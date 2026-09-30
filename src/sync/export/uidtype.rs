/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::common::{create_batch, jid, target_query_get, update_batch};
use super::{Maps, Net, Plan, Uploader};
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::import_jmap::mapping::{
    CALENDAR_EVENT_SELECT, CONTACT_CARD_SELECT, calendar_event_to_wire, contact_card_to_wire,
};
use crate::sync::prune::{TargetObj, candidates};
use crate::sync::{Context, TypeCounts};
use crate::types::ObjectType;

fn target_uid(v: &Value) -> Option<String> {
    v.get("uid").and_then(Value::as_str).map(str::to_owned)
}

fn describe(ty: ObjectType, local: i64, uid: &str) -> String {
    let mut out = String::new();
    let _ = write!(out, "{} local {local}", ty.jmap_name());
    if !uid.is_empty() {
        let _ = write!(out, " (uid {uid})");
    }
    out
}

pub fn reconcile(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    maps: &mut Maps,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Result<Plan, Error> {
    let targets = target_query_get(net, ty, None).map_err(Error::from)?;
    let mut by_uid: HashMap<String, (String, &Value)> = HashMap::new();
    for t in &targets {
        if let (Some(uid), Some(id)) = (target_uid(t), jid(t)) {
            by_uid.entry(uid).or_insert((id, t));
        }
    }

    let select = if ty == ObjectType::ContactCard {
        CONTACT_CARD_SELECT
    } else {
        CALENDAR_EVENT_SELECT
    };
    let rows: Vec<(i64, String)> = {
        let mut stmt = ctx
            .conn
            .prepare(select)
            .map_err(|e| Error::Partial(e.to_string()))?;
        stmt.query_map([], |r| {
            if ty == ObjectType::ContactCard {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            } else {
                let data: String = r.get(4)?;
                let uid = serde_json::from_str::<Value>(&data)
                    .ok()
                    .and_then(|v| v.get("uid").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                Ok((r.get::<_, i64>(0)?, uid))
            }
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .map_err(|e| Error::Partial(e.to_string()))?
    };

    let mut matched_uids: HashSet<String> = HashSet::new();
    let mut updates: Vec<(String, Value)> = Vec::new();
    let blobs = Uploader::new(net, &ctx.conn);
    for (local, uid) in &rows {
        if let Some((tid, existing)) = by_uid.get(uid) {
            maps.insert(ty, *local, crate::jmap::wire::JmapId(tid.clone()));
            matched_uids.insert(uid.clone());
            match build_wire(ctx, ty, *local, maps, &blobs) {
                Ok(wire) => match changed_properties(&wire, existing) {
                    Some(patch) => updates.push((tid.clone(), patch)),
                    None => counts.skipped += 1,
                },
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    logger.warn(&format!("{} not compared: {e}", describe(ty, *local, uid)));
                    counts.skipped += 1;
                }
            }
            continue;
        }
        let cid = format!("c{local}");
        let wire = match build_wire(ctx, ty, *local, maps, &blobs) {
            Ok(w) => w,
            Err(e) if e.aborts_run() => return Err(e),
            Err(e) => {
                logger.warn(&format!("{} skipped: {e}", describe(ty, *local, uid)));
                counts.failed += 1;
                continue;
            }
        };
        let outcome = match create_batch(net, ty, vec![(cid.clone(), wire)]) {
            Ok(o) => o,
            Err(e) => {
                let mapped = Error::from(e);
                if mapped.aborts_run() {
                    return Err(mapped);
                }
                logger.warn(&format!(
                    "{} not created: {mapped}",
                    describe(ty, *local, uid)
                ));
                counts.failed += 1;
                continue;
            }
        };
        for (cid, v) in &outcome.created {
            if let Some(parsed) = cid.strip_prefix('c').and_then(|s| s.parse::<i64>().ok())
                && let Some(id) = jid(v)
            {
                maps.insert(ty, parsed, crate::jmap::wire::JmapId(id));
                counts.created += 1;
            }
        }
        for (cid, err) in &outcome.not_created {
            logger.warn(&format!("{} {cid} not created: {err}", ty.jmap_name()));
            counts.failed += 1;
        }
    }

    update_batch(net, ty, updates, counts, logger);

    let objs: Vec<TargetObj> = targets
        .iter()
        .filter_map(|t| {
            let id = jid(t)?;
            let uid = target_uid(t);
            Some(TargetObj {
                id: id.clone(),
                matched: uid.map(|u| matched_uids.contains(&u)).unwrap_or(false),
                protected: false,
                may_delete: true,
                parent: None,
            })
        })
        .collect();
    Ok(Plan {
        prune_candidates: candidates(&objs, false),
        active_sieve_target: None,
    })
}

fn build_wire(
    ctx: &Context,
    ty: ObjectType,
    local: i64,
    maps: &Maps,
    blobs: &Uploader<'_>,
) -> Result<Value, Error> {
    if ty == ObjectType::ContactCard {
        let (uid, abids, data): (String, String, String) = ctx
            .conn
            .query_row(
                &format!("{CONTACT_CARD_SELECT} WHERE id = ?1"),
                rusqlite::params![local],
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
        contact_card_to_wire(&uid, &abids, &data, maps, blobs).map_err(Error::from)
    } else {
        let (cal, dr, ud, data): (String, i64, i64, String) = ctx
            .conn
            .query_row(
                &format!("{CALENDAR_EVENT_SELECT} AND id = ?1"),
                rusqlite::params![local],
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
        calendar_event_to_wire(&cal, dr != 0, ud != 0, &data, maps, blobs).map_err(Error::from)
    }
}

/// An `updated` value as a point in time, so that offsets and fractional
/// seconds compare as the same instant. `None` if it does not parse.
fn parse_updated(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

/// The update that makes `target` match the archive's `wire` object, or
/// `None` when nothing changed. When both carry `updated`, it decides: the
/// archive's copy wins only if it is newer. Otherwise each property the
/// archive writes is compared, and those that differ are sent whole.
fn changed_properties(wire: &Value, target: &Value) -> Option<Value> {
    let wire = wire.as_object()?;
    let stamp = |v: Option<&Value>| v.and_then(Value::as_str).and_then(parse_updated);
    if let (Some(ours), Some(theirs)) = (stamp(wire.get("updated")), stamp(target.get("updated")))
        && ours <= theirs
    {
        return None;
    }
    let mut patch = Map::new();
    for (k, v) in wire {
        if k == "uid" || k == "id" {
            continue;
        }
        if target.get(k) != Some(v) {
            patch.insert(k.clone(), v.clone());
        }
    }
    (!patch.is_empty()).then_some(Value::Object(patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unchanged_object_needs_no_update() {
        let wire = json!({"uid": "u", "name": {"full": "Ann"}, "addressBookIds": {"A": true}});
        let target = json!({"id": "T", "uid": "u", "name": {"full": "Ann"},
                            "addressBookIds": {"A": true}, "extra": 1});
        assert_eq!(changed_properties(&wire, &target), None);
    }

    #[test]
    fn changed_properties_are_sent_whole() {
        let wire = json!({"uid": "u", "name": {"full": "Ann B"}, "addressBookIds": {"A": true}});
        let target = json!({"id": "T", "uid": "u", "name": {"full": "Ann"},
                            "addressBookIds": {"A": true}});
        assert_eq!(
            changed_properties(&wire, &target),
            Some(json!({"name": {"full": "Ann B"}}))
        );
    }

    #[test]
    fn updated_compares_instants_not_strings() {
        let target = json!({"uid": "u", "title": "old", "updated": "2026-01-02T00:00:00Z"});
        let same_instant = json!({"uid": "u", "title": "new",
                                  "updated": "2026-01-02T01:00:00.000+01:00"});
        assert_eq!(changed_properties(&same_instant, &target), None);
        let later = json!({"uid": "u", "title": "new", "updated": "2026-01-02T00:00:00.5Z"});
        assert!(changed_properties(&later, &target).is_some());
    }

    #[test]
    fn updated_decides_when_both_sides_carry_it() {
        let target = json!({"uid": "u", "title": "old", "updated": "2026-01-02T00:00:00Z"});
        let older = json!({"uid": "u", "title": "new", "updated": "2026-01-01T00:00:00Z"});
        assert_eq!(changed_properties(&older, &target), None, "target is newer");
        let newer = json!({"uid": "u", "title": "new", "updated": "2026-01-03T00:00:00Z"});
        assert_eq!(
            changed_properties(&newer, &target),
            Some(json!({"title": "new", "updated": "2026-01-03T00:00:00Z"}))
        );
    }
}
