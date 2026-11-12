/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! The counts-only `--estimate` pass for a Microsoft Graph source.
//!
//! Graph can enumerate an item's id cheaply (`$select=id`), but a size would
//! cost a body fetch per item, which an estimate does not do. So this reports
//! how many mail, calendar events and contacts are new since the archive was
//! last filled -- the same id enumeration and archive diff the import runs --
//! and no bytes. It reads folder lists and id pages only; it writes nothing.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde_json::Value;

use super::coordinator::{GraphCoordinator, enumerate_mail_folders};
use super::folders::default_contact_folder;
use crate::db::exchange_graph_ids;
use crate::error::Error;
use crate::exchange_graph::api::{self, PREFER_TIMEZONE_UTC};
use crate::exchange_graph::calendar_map::{EventType, classify_event_type};
use crate::exchange_graph::types::{MailboxKind, Surfaces};
use crate::sync::{Summary, TypeCounts};

/// Builds the estimate summary: a per-type row of new objects for each surface
/// the run asks for. Mirrors which surfaces the real import would touch (an
/// archive mailbox carries mail only).
pub fn run(
    conn: &Connection,
    ctx: &GraphCoordinator<'_>,
    mailbox_kind: MailboxKind,
    surfaces: &Surfaces,
) -> Result<Summary, Error> {
    let mut summary = Summary::default();
    let primary = !matches!(mailbox_kind, MailboxKind::Archive);
    if surfaces.mail {
        summary
            .per_type
            .push(("email", estimate_mail(conn, ctx, mailbox_kind)?));
    }
    if surfaces.calendar && primary {
        summary
            .per_type
            .push(("calendarevent", estimate_calendar(conn, ctx)?));
    }
    if surfaces.contacts && primary {
        summary
            .per_type
            .push(("contactcard", estimate_contacts(conn, ctx)?));
    }
    Ok(summary)
}

/// Counts the mail that is new across the full folder tree. Enumerates message
/// ids folder by folder -- the same read-only tree walk a dry run does -- and
/// counts those not already in the archive, deduplicated across folders.
fn estimate_mail(
    conn: &Connection,
    ctx: &GraphCoordinator<'_>,
    mailbox_kind: MailboxKind,
) -> Result<TypeCounts, Error> {
    let local = exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::EMAIL)?;
    let folders = enumerate_mail_folders(ctx.client, ctx.endpoints, mailbox_kind, ctx.top)
        .map_err(Error::from)?;
    let mut counts = TypeCounts::default();
    let mut planned: HashSet<String> = HashSet::new();
    for folder in &folders {
        let Some(fid) = folder.get("id").and_then(Value::as_str) else {
            continue;
        };
        let url = ctx.endpoints.folder_messages_ids(fid, ctx.top);
        let stubs = match api::collect_all_values(ctx.client, &url, &[]) {
            Ok(v) => v,
            Err(e) => {
                ctx.logger
                    .warn(&format!("estimate: mail folder {fid} enumerate: {e}"));
                counts.failed += 1;
                continue;
            }
        };
        for stub in &stubs {
            count_new(
                stub.get("id").and_then(Value::as_str),
                &local,
                &mut planned,
                &mut counts,
            );
        }
    }
    Ok(counts)
}

/// Counts the calendar events that are new. Enumerates event ids per calendar
/// and skips single `Occurrence` items exactly as the import does -- it stores
/// the series master, not each occurrence -- so a re-estimate finds nothing
/// new once imported.
fn estimate_calendar(conn: &Connection, ctx: &GraphCoordinator<'_>) -> Result<TypeCounts, Error> {
    let local =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::CALENDAR_EVENT)?;
    let calendars = api::collect_all_values(ctx.client, &ctx.endpoints.calendars(ctx.top), &[])
        .map_err(Error::from)?;
    let mut counts = TypeCounts::default();
    let mut planned: HashSet<String> = HashSet::new();
    for cal in &calendars {
        let Some(cid) = cal.get("id").and_then(Value::as_str) else {
            continue;
        };
        let url = ctx.endpoints.calendar_events_ids(cid, ctx.top);
        let stubs = match api::collect_all_values(ctx.client, &url, &[PREFER_TIMEZONE_UTC]) {
            Ok(v) => v,
            Err(e) => {
                ctx.logger
                    .warn(&format!("estimate: calendar {cid} enumerate: {e}"));
                counts.failed += 1;
                continue;
            }
        };
        for stub in &stubs {
            if matches!(classify_event_type(stub), EventType::Occurrence) {
                continue;
            }
            count_new(
                stub.get("id").and_then(Value::as_str),
                &local,
                &mut planned,
                &mut counts,
            );
        }
    }
    Ok(counts)
}

/// Counts the contacts that are new across every address book, including nested
/// folders and the default folder, which is the same set the import walks.
fn estimate_contacts(conn: &Connection, ctx: &GraphCoordinator<'_>) -> Result<TypeCounts, Error> {
    let local =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::CONTACT_CARD)?;
    let books = discover_contact_folder_ids(ctx)?;
    let mut counts = TypeCounts::default();
    let mut planned: HashSet<String> = HashSet::new();
    for bid in &books {
        let url = ctx.endpoints.contact_folder_contacts_ids(bid, ctx.top);
        let ids = match api::collect_all_ids(ctx.client, &url, &[]) {
            Ok(v) => v,
            Err(e) => {
                ctx.logger
                    .warn(&format!("estimate: contact folder {bid} enumerate: {e}"));
                counts.failed += 1;
                continue;
            }
        };
        for id in &ids {
            count_new(Some(id.as_str()), &local, &mut planned, &mut counts);
        }
    }
    Ok(counts)
}

/// The read-only address-book discovery the import does: the listed folders,
/// the default folder (which may not be listed), and every nested child.
fn discover_contact_folder_ids(ctx: &GraphCoordinator<'_>) -> Result<Vec<String>, Error> {
    let server = api::collect_all_values(ctx.client, &ctx.endpoints.contact_folders(ctx.top), &[])
        .map_err(Error::from)?;
    let mut seen: HashSet<String> = server
        .iter()
        .filter_map(|f| f.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    if let Some(default) = default_contact_folder(ctx)
        && let Some(id) = default.get("id").and_then(Value::as_str)
    {
        seen.insert(id.to_owned());
    }
    let mut frontier: Vec<String> = seen.iter().cloned().collect();
    while let Some(parent) = frontier.pop() {
        let url = ctx.endpoints.contact_folder_children(&parent, ctx.top);
        let children = api::collect_all_values(ctx.client, &url, &[]).map_err(Error::from)?;
        for child in children {
            if let Some(id) = child.get("id").and_then(Value::as_str)
                && seen.insert(id.to_owned())
            {
                frontier.push(id.to_owned());
            }
        }
    }
    Ok(seen.into_iter().collect())
}

/// Counts an enumerated id as new when it is neither already in the archive nor
/// already seen in this pass (the same id can appear in two folders).
fn count_new(
    id: Option<&str>,
    local: &HashMap<String, i64>,
    planned: &mut HashSet<String>,
    counts: &mut TypeCounts,
) {
    let Some(id) = id else { return };
    if local.contains_key(id) {
        return;
    }
    if planned.insert(id.to_owned()) {
        counts.created += 1;
        counts.fetched += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_new_skips_archived_and_dedups_within_a_pass() {
        let mut local: HashMap<String, i64> = HashMap::new();
        local.insert("archived".to_owned(), 1);
        let mut planned: HashSet<String> = HashSet::new();
        let mut counts = TypeCounts::default();

        // Already in the archive: not new.
        count_new(Some("archived"), &local, &mut planned, &mut counts);
        // New: counted once...
        count_new(Some("A"), &local, &mut planned, &mut counts);
        // ...and the same id in another folder does not count again.
        count_new(Some("A"), &local, &mut planned, &mut counts);
        // Another new one.
        count_new(Some("B"), &local, &mut planned, &mut counts);
        // A stub with no id is ignored.
        count_new(None, &local, &mut planned, &mut counts);

        assert_eq!(counts.fetched, 2, "A and B are new; A counts once");
        assert_eq!(counts.created, 2);
        assert_eq!(counts.bytes, 0, "counts-only: no sizes");
    }
}
