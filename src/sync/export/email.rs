/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::common::{jid, target_query_get};
use super::{Maps, Net, Plan, Uploader};
use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::request::{
    MethodCall, Request, SetRequest, check_method_error, get_objects, retry_method_call, set_call,
};
use crate::jmap::retry::MethodCallKind;
use crate::jmap::wire::JmapId;
use crate::logging::Logger;
use crate::sync::import_jmap::mapping::{EMAIL_SELECT, EmailRow, TargetResolver, row_to_email};
use crate::sync::keys::{EmailIndex, EmailKey, email_index, email_keys, index_from_json};
use crate::sync::{Context, TypeCounts};
use crate::types::ObjectType;

fn server_index(v: &Value) -> EmailIndex {
    let arr = |k: &str| {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("email").and_then(Value::as_str).map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let mids: Vec<String> = v
        .get("messageId")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    email_index(
        &mids,
        &arr("from"),
        v.get("subject").and_then(Value::as_str).unwrap_or(""),
        v.get("sentAt").and_then(Value::as_str).unwrap_or(""),
        &arr("to"),
    )
}

pub fn reconcile(
    ctx: &Context,
    net: &Net,
    maps: &mut Maps,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Result<Plan, Error> {
    let ty = ObjectType::Email;

    let target_min = target_query_get(net, ty, Some(&["messageId", "size", "mailboxIds"]))
        .map_err(Error::from)?;
    let mut indices: Vec<EmailIndex> = target_min.iter().map(server_index).collect();

    let fallback_ids: Vec<JmapId> = target_min
        .iter()
        .zip(indices.iter())
        .filter(|(_, i)| i.mids.is_empty())
        .filter_map(|(v, _)| jid(v).map(JmapId))
        .collect();
    if !fallback_ids.is_empty() {
        let got = get_objects::<Value>(
            &net.client,
            &net.api,
            &net.account,
            ty.jmap_name(),
            &fallback_ids,
            Some(&["messageId", "from", "subject", "sentAt", "to"]),
            &net.limits,
        )
        .map_err(Error::from)?;
        let by_id: HashMap<String, &Value> = got
            .list
            .iter()
            .filter_map(|v| jid(v).map(|i| (i, v)))
            .collect();
        for (v, slot) in target_min.iter().zip(indices.iter_mut()) {
            if let Some(full) = jid(v).and_then(|i| by_id.get(&i)) {
                *slot = server_index(full);
            }
        }
    }
    let targets: Vec<TargetEmail> = target_min.iter().map(TargetEmail::from_value).collect();
    let target_keys = email_keys(&indices);

    let mut local: Vec<(i64, EmailRow)> = {
        let mut stmt = ctx
            .conn
            .prepare(EMAIL_SELECT)
            .map_err(|e| Error::Partial(e.to_string()))?;
        stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            Ok((id, row_to_email(row)))
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .map_err(|e| Error::Partial(e.to_string()))?
        .into_iter()
        .map(|(id, r)| Ok((id, r.map_err(Error::from)?)))
        .collect::<Result<_, Error>>()?
    };
    local.sort_by_key(|(id, _)| *id);
    let units = fold_by_blob(local);

    let local_indices: Vec<EmailIndex> = units
        .iter()
        .map(|u| index_from_json(&u.row.message_match))
        .collect();
    let local_keys = email_keys(&local_indices);

    let mut uploader = Uploader::new(net, &ctx.conn);
    let sizes: Vec<Option<u64>> = units
        .iter()
        .map(|u| uploader.blob_len(u.row.blob_local_id))
        .collect();
    let pairs = pair_with_targets(&local_keys, &sizes, &target_keys, &targets);

    let mut membership_updates: Vec<(String, Value)> = Vec::new();
    for (i, unit) in units.iter().enumerate() {
        match pairs[i] {
            Some(t) => match missing_memberships(&unit.row, &targets[t], maps) {
                Some(patch) => membership_updates.push((targets[t].id.clone(), patch)),
                None => counts.skipped += 1,
            },
            None => export_one(
                net,
                &mut uploader,
                maps,
                unit.local_id,
                &unit.row,
                counts,
                logger,
            ),
        }
    }
    send_membership_updates(net, membership_updates, counts, logger);

    Ok(Plan::default())
}

/// One message to write: the archive rows that hold the same bytes, folded
/// together. A source that files one message in several folders (IMAP and
/// Maildir copies, Gmail labels) leaves one archive row per folder; on the
/// target it is one email in all of them. Keywords are the union of the
/// copies', so a message read or flagged in any folder stays so.
struct Unit {
    local_id: i64,
    row: EmailRow,
}

fn fold_by_blob(local: Vec<(i64, EmailRow)>) -> Vec<Unit> {
    let mut order: Vec<i64> = Vec::new();
    let mut by_blob: HashMap<i64, Unit> = HashMap::new();
    for (local_id, row) in local {
        match by_blob.get_mut(&row.blob_local_id) {
            Some(unit) => {
                for m in row.mailbox_locals {
                    if !unit.row.mailbox_locals.contains(&m) {
                        unit.row.mailbox_locals.push(m);
                    }
                }
                for k in row.keywords {
                    if !unit.row.keywords.contains(&k) {
                        unit.row.keywords.push(k);
                    }
                }
            }
            None => {
                order.push(row.blob_local_id);
                by_blob.insert(row.blob_local_id, Unit { local_id, row });
            }
        }
    }
    order
        .into_iter()
        .filter_map(|b| by_blob.remove(&b))
        .collect()
}

/// What export needs to know about an email already on the target. `size`
/// and `mailboxes` are `None` when the server did not return them, and then
/// nothing is inferred from their absence.
struct TargetEmail {
    id: String,
    size: Option<u64>,
    mailboxes: Option<HashSet<String>>,
}

impl TargetEmail {
    fn from_value(v: &Value) -> TargetEmail {
        TargetEmail {
            id: jid(v).unwrap_or_default(),
            size: v.get("size").and_then(Value::as_u64),
            mailboxes: v
                .get("mailboxIds")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect()),
        }
    }
}

/// Pairs each local message with at most one target email. Messages are
/// matched by key (Message-ID, or the fallback digest); where several share a
/// key -- genuinely different messages with the same Message-ID, or copies
/// already on the target -- equal size decides first, then order, so two
/// different messages are never folded onto one target email.
fn pair_with_targets(
    local_keys: &[EmailKey],
    local_sizes: &[Option<u64>],
    target_keys: &[EmailKey],
    targets: &[TargetEmail],
) -> Vec<Option<usize>> {
    let mut by_key: HashMap<&EmailKey, Vec<usize>> = HashMap::new();
    for (t, key) in target_keys.iter().enumerate() {
        by_key.entry(key).or_default().push(t);
    }
    let mut groups: HashMap<&EmailKey, Vec<usize>> = HashMap::new();
    for (i, key) in local_keys.iter().enumerate() {
        groups.entry(key).or_default().push(i);
    }
    let mut out = vec![None; local_keys.len()];
    for (key, members) in groups {
        let Some(candidates) = by_key.get_mut(key) else {
            continue;
        };
        for &i in &members {
            let Some(size) = local_sizes[i] else { continue };
            if let Some(pos) = candidates
                .iter()
                .position(|&t| targets[t].size == Some(size))
            {
                out[i] = Some(candidates.remove(pos));
            }
        }
        for &i in &members {
            if out[i].is_none() && !candidates.is_empty() {
                out[i] = Some(candidates.remove(0));
            }
        }
    }
    out
}

/// The `Email/set` patch adding the folders a matched email is missing on the
/// target, or `None` when it is already in all of them. Folders that exist
/// only on the target are left alone.
fn missing_memberships(row: &EmailRow, target: &TargetEmail, maps: &Maps) -> Option<Value> {
    let have = target.mailboxes.as_ref()?;
    let mut patch = Map::new();
    for ml in &row.mailbox_locals {
        if let Some(t) = maps.target(ObjectType::Mailbox, *ml)
            && !have.contains(&t.0)
        {
            patch.insert(format!("mailboxIds/{}", t.0), Value::Bool(true));
        }
    }
    (!patch.is_empty()).then_some(Value::Object(patch))
}

fn send_membership_updates(
    net: &Net,
    updates: Vec<(String, Value)>,
    counts: &mut TypeCounts,
    logger: &Logger,
) {
    if updates.is_empty() {
        return;
    }
    if net.dry_run {
        counts.updated += updates.len() as u64;
        return;
    }
    let total = updates.len() as u64;
    let mut map = Map::new();
    for (id, patch) in updates {
        map.insert(id, patch);
    }
    match set_call(
        &net.client,
        &net.api,
        &net.account,
        ObjectType::Email.jmap_name(),
        SetRequest {
            update: Some(Value::Object(map)),
            ..Default::default()
        },
        &net.limits,
    ) {
        Ok(outcome) => {
            counts.updated += outcome.updated.len() as u64;
            for (id, err) in &outcome.not_updated {
                logger.warn(&format!("Email/set {id}: folders not added: {err}"));
                counts.failed += 1;
            }
        }
        Err(e) => {
            logger.warn(&format!(
                "Email/set: adding folders to {total} email(s) failed: {e}"
            ));
            counts.failed += total;
        }
    }
}

fn build_mailbox_ids(row: &EmailRow, maps: &Maps) -> Option<Map<String, Value>> {
    let mut mids = Map::new();
    for ml in &row.mailbox_locals {
        let t = maps.target(ObjectType::Mailbox, *ml)?;
        mids.insert(t.0, Value::Bool(true));
    }
    Some(mids)
}

fn build_keywords(row: &EmailRow) -> Map<String, Value> {
    let mut kw = Map::new();
    for k in &row.keywords {
        kw.insert(k.clone(), Value::Bool(true));
    }
    kw
}

fn blob_hint(uploader: &Uploader, row: &EmailRow) -> String {
    let idx = index_from_json(&row.message_match);
    let mut s = match idx.mids.first() {
        Some(mid) => format!("message-id <{mid}>"),
        None => "no message-id".to_owned(),
    };
    if let Some(len) = uploader.blob_len(row.blob_local_id) {
        use std::fmt::Write;
        let _ = write!(s, ", {}", crate::inspect::format_bytes(len));
    }
    s
}

fn size_note(e: &JmapError) -> &'static str {
    if matches!(
        e,
        JmapError::RequestTooLarge | JmapError::SingleObjectTooLarge(_)
    ) {
        "; exceeds the target server size limit, so this message is skipped and re-running will not migrate it"
    } else {
        ""
    }
}

fn import_item(
    blob: String,
    mids: Map<String, Value>,
    kw: Map<String, Value>,
    received_at: &str,
) -> Value {
    json!({
        "blobId": blob,
        "mailboxIds": Value::Object(mids),
        "keywords": Value::Object(kw),
        "receivedAt": received_at,
    })
}

fn export_one(
    net: &Net,
    uploader: &mut Uploader,
    maps: &Maps,
    local_id: i64,
    row: &EmailRow,
    counts: &mut TypeCounts,
    logger: &Logger,
) {
    let cid = format!("e{local_id}");
    let mids = match build_mailbox_ids(row, maps) {
        Some(m) => m,
        None => {
            logger.warn(&format!(
                "Email/import {cid} ({}) skipped: mailbox not on target",
                blob_hint(uploader, row)
            ));
            counts.failed += 1;
            return;
        }
    };
    let blob = match uploader.upload_with(row.blob_local_id, "message/rfc822") {
        Ok(b) => b.0,
        Err(e) => {
            logger.warn(&format!(
                "Email/import {cid} ({}) blob upload failed: {e}{}",
                blob_hint(uploader, row),
                size_note(&e)
            ));
            counts.failed += 1;
            return;
        }
    };
    if net.dry_run {
        counts.created += 1;
        return;
    }
    let item = import_item(blob, mids, build_keywords(row), &row.received_at);
    match send_single_import(net, &cid, item, logger) {
        Ok(SingleImport::Created) => counts.created += 1,
        Ok(SingleImport::Skipped) => counts.skipped += 1,
        Ok(SingleImport::NotCreated { error_type, .. }) if error_type == "blobNotFound" => {
            retry_after_reupload(net, uploader, maps, &cid, row, counts, logger);
        }
        Ok(SingleImport::NotCreated { detail, .. }) => {
            logger.warn(&format!(
                "Email/import {cid} ({}) failed: {detail}",
                blob_hint(uploader, row)
            ));
            counts.failed += 1;
        }
        Err(e) => {
            logger.warn(&format!(
                "Email/import {cid} ({}) send failed: {e}{}",
                blob_hint(uploader, row),
                size_note(&e)
            ));
            counts.failed += 1;
        }
    }
}

fn retry_after_reupload(
    net: &Net,
    uploader: &mut Uploader,
    maps: &Maps,
    cid: &str,
    row: &EmailRow,
    counts: &mut TypeCounts,
    logger: &Logger,
) {
    uploader.invalidate(row.blob_local_id);
    let blob = match uploader.upload_with(row.blob_local_id, "message/rfc822") {
        Ok(b) => b.0,
        Err(e) => {
            logger.warn(&format!(
                "Email/import {cid} ({}) blob re-upload failed: {e}{}",
                blob_hint(uploader, row),
                size_note(&e)
            ));
            counts.failed += 1;
            return;
        }
    };
    let mids = match build_mailbox_ids(row, maps) {
        Some(m) => m,
        None => {
            logger.warn(&format!(
                "Email/import {cid} ({}) skipped: mailbox not on target",
                blob_hint(uploader, row)
            ));
            counts.failed += 1;
            return;
        }
    };
    let item = import_item(blob, mids, build_keywords(row), &row.received_at);
    match send_single_import(net, cid, item, logger) {
        Ok(SingleImport::Created) => counts.created += 1,
        Ok(SingleImport::Skipped) => counts.skipped += 1,
        Ok(SingleImport::NotCreated { detail, .. }) => {
            logger.warn(&format!(
                "Email/import {cid} ({}) failed after blob re-upload: {detail}",
                blob_hint(uploader, row)
            ));
            counts.failed += 1;
        }
        Err(e) => {
            logger.warn(&format!(
                "Email/import {cid} ({}) send failed after blob re-upload: {e}{}",
                blob_hint(uploader, row),
                size_note(&e)
            ));
            counts.failed += 1;
        }
    }
}

enum SingleImport {
    Created,
    Skipped,
    NotCreated { error_type: String, detail: String },
}

fn send_single_import(
    net: &Net,
    cid: &str,
    item: Value,
    logger: &Logger,
) -> Result<SingleImport, JmapError> {
    let mut emails = Map::new();
    emails.insert(cid.to_owned(), item);
    let mut req = Request::new();
    req.call(
        "Email/import",
        json!({ "accountId": net.account, "emails": Value::Object(emails) }),
        "i",
    );
    req.fits(&net.limits)?;
    retry_method_call(
        &net.client,
        MethodCallKind::SingleObjectWrite,
        logger,
        || interpret_import(req.send(&net.client, &net.api)?.first()?, cid),
    )
}

fn interpret_import(mr: &MethodCall, cid: &str) -> Result<SingleImport, JmapError> {
    check_method_error(mr)?;
    if let Some(err) = mr
        .args
        .get("notCreated")
        .and_then(Value::as_object)
        .and_then(|nc| nc.get(cid))
    {
        let error_type = err
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if error_type == "alreadyExists" {
            return Ok(SingleImport::Skipped);
        }
        return Ok(SingleImport::NotCreated {
            error_type,
            detail: err.to_string(),
        });
    }
    if mr
        .args
        .get("created")
        .and_then(Value::as_object)
        .is_some_and(|c| !c.is_empty())
    {
        return Ok(SingleImport::Created);
    }
    Ok(SingleImport::NotCreated {
        error_type: String::new(),
        detail: format!("Email/import returned neither created nor notCreated for {cid}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(blob: i64, mailboxes: &[i64], keywords: &[&str]) -> EmailRow {
        EmailRow {
            blob_local_id: blob,
            received_at: "2020-01-01T00:00:00Z".to_owned(),
            mailbox_locals: mailboxes.to_vec(),
            keywords: keywords.iter().map(|k| (*k).to_owned()).collect(),
            message_match: "{}".to_owned(),
        }
    }

    fn target(id: &str, size: Option<u64>, mailboxes: Option<&[&str]>) -> TargetEmail {
        TargetEmail {
            id: id.to_owned(),
            size,
            mailboxes: mailboxes.map(|m| m.iter().map(|s| (*s).to_owned()).collect()),
        }
    }

    fn mid(m: &str) -> EmailKey {
        EmailKey::MessageId(m.to_owned())
    }

    #[test]
    fn copies_of_one_message_fold_into_one_unit_in_every_folder() {
        let units = fold_by_blob(vec![
            (1, row(10, &[1], &["$seen"])),
            (2, row(10, &[2], &["$flagged", "$seen"])),
            (3, row(11, &[1], &[])),
        ]);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].local_id, 1);
        assert_eq!(units[0].row.mailbox_locals, vec![1, 2]);
        assert_eq!(units[0].row.keywords, vec!["$seen", "$flagged"]);
        assert_eq!(units[1].row.blob_local_id, 11);
    }

    #[test]
    fn different_messages_sharing_a_message_id_pair_by_size() {
        let pairs = pair_with_targets(
            &[mid("m@h"), mid("m@h")],
            &[Some(100), Some(200)],
            &[mid("m@h")],
            &[target("T", Some(200), None)],
        );
        assert_eq!(
            pairs,
            vec![None, Some(0)],
            "only the 200-byte one is on the target"
        );
    }

    #[test]
    fn size_pairing_ignores_target_order() {
        let pairs = pair_with_targets(
            &[mid("m@h"), mid("m@h")],
            &[Some(100), Some(200)],
            &[mid("m@h"), mid("m@h")],
            &[target("A", Some(200), None), target("B", Some(100), None)],
        );
        assert_eq!(pairs, vec![Some(1), Some(0)]);
    }

    #[test]
    fn a_lone_message_pairs_by_key_when_the_target_gives_no_size() {
        let pairs = pair_with_targets(
            &[mid("m@h"), mid("n@h")],
            &[Some(100), Some(50)],
            &[mid("m@h")],
            &[target("T", None, None)],
        );
        assert_eq!(pairs, vec![Some(0), None]);
    }

    #[test]
    fn missing_memberships_adds_only_the_absent_migrated_folders() {
        let mut maps = Maps::default();
        maps.insert(ObjectType::Mailbox, 1, JmapId("T1".into()));
        maps.insert(ObjectType::Mailbox, 2, JmapId("T2".into()));
        let r = row(10, &[1, 2, 3], &[]);
        let patch = missing_memberships(&r, &target("E", None, Some(&["T1", "Own"])), &maps)
            .expect("T2 is missing");
        assert_eq!(patch, json!({ "mailboxIds/T2": true }));
        assert!(missing_memberships(&r, &target("E", None, Some(&["T1", "T2"])), &maps).is_none());
        assert!(
            missing_memberships(&r, &target("E", None, None), &maps).is_none(),
            "unknown membership is left alone"
        );
    }
}
