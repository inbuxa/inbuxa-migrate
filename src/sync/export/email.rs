/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::common::{jid, target_query_get, update_batch};
use super::{Maps, Net, Plan, Uploader};
use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::request::{
    MethodCall, Request, check_method_error, get_objects, retry_method_call,
};
use crate::jmap::retry::MethodCallKind;
use crate::jmap::wire::JmapId;
use crate::logging::Logger;
use crate::sync::import_jmap::mapping::{EMAIL_SELECT, EmailRow, TargetResolver, row_to_email};
use crate::sync::keys::{EmailIndex, EmailKey, email_index, email_keys, index_from_json};
use crate::sync::progress::Progress;
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
    let (targets, target_keys) = target_emails(net).map_err(Error::from)?;

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

    let migrated = maps.targets_of(ObjectType::Mailbox);
    let mut updates: Vec<(String, Value)> = Vec::new();
    let mut creates: Vec<usize> = Vec::new();
    for (i, unit) in units.iter().enumerate() {
        match pairs[i] {
            Some(t) => match email_patch(&unit.row, &targets[t], maps, &migrated) {
                Some(patch) => updates.push((targets[t].id.clone(), patch)),
                None => counts.skipped += 1,
            },
            None => creates.push(i),
        }
    }
    let mut progress = Progress::new("export: Email", creates.len() as u64, logger);
    import_units(
        net,
        &mut uploader,
        maps,
        &units,
        &creates,
        counts,
        logger,
        &mut progress,
    );
    update_batch(net, ty, updates, counts, logger);

    Ok(Plan::default())
}

/// The emails already on the target, and the key each one matches by.
fn target_emails(net: &Net) -> Result<(Vec<TargetEmail>, Vec<EmailKey>), JmapError> {
    let ty = ObjectType::Email;
    let target_min = target_query_get(
        net,
        ty,
        Some(&["messageId", "size", "mailboxIds", "keywords"]),
    )?;
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
        )?;
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
    let keys = email_keys(&indices);
    Ok((targets, keys))
}

/// The most emails one `Email/import` carries: the server's
/// `maxObjectsInSet`, but no more than this, so that a request that fails
/// without a clear answer leaves few messages to check.
const IMPORT_BATCH_CAP: usize = 50;

/// One message ready to import: its creation id, its place in `units`, and
/// the `Email/import` entry.
struct Pending {
    cid: String,
    unit: usize,
    item: Value,
}

/// Writes the messages the target does not have yet. They go in batches of
/// up to `maxObjectsInSet` (capped by `IMPORT_BATCH_CAP`), their blobs
/// uploaded at once up to `maxConcurrentUpload`. Each message is still
/// counted on its own: one rejected in a batch fails alone. A batch that
/// fails without a clear answer is never resent blindly -- the target is
/// checked first, and only what did not arrive is imported again.
#[allow(clippy::too_many_arguments)]
fn import_units(
    net: &Net,
    uploader: &mut Uploader,
    maps: &Maps,
    units: &[Unit],
    creates: &[usize],
    counts: &mut TypeCounts,
    logger: &Logger,
    progress: &mut Progress,
) {
    let batch = (net.limits.max_objects_in_set as usize).clamp(1, IMPORT_BATCH_CAP);
    let mut unclear: Vec<Pending> = Vec::new();
    for chunk in creates.chunks(batch) {
        let mut ready: Vec<(usize, Map<String, Value>)> = Vec::new();
        for &i in chunk {
            let row = &units[i].row;
            match build_mailbox_ids(row, maps) {
                Some(mids) => ready.push((i, mids)),
                None => {
                    logger.warn(&format!(
                        "Email/import e{} ({}) skipped: mailbox not on target",
                        units[i].local_id,
                        blob_hint(uploader, row)
                    ));
                    counts.failed += 1;
                }
            }
        }
        let blobs: Vec<i64> = ready
            .iter()
            .map(|(i, _)| units[*i].row.blob_local_id)
            .collect();
        let uploaded = uploader.upload_many(&blobs, "message/rfc822");
        let mut pending: Vec<Pending> = Vec::new();
        for ((i, mids), result) in ready.into_iter().zip(uploaded) {
            let row = &units[i].row;
            let cid = format!("e{}", units[i].local_id);
            match result {
                Ok(blob) => pending.push(Pending {
                    cid,
                    unit: i,
                    item: import_item(blob.0, mids, build_keywords(row), &row.received_at),
                }),
                Err(e) => {
                    logger.warn(&format!(
                        "Email/import {cid} ({}) blob upload failed: {e}{}",
                        blob_hint(uploader, row),
                        size_note(&e)
                    ));
                    counts.failed += 1;
                }
            }
        }
        if net.dry_run {
            counts.created += pending.len() as u64;
        } else {
            send_batch(
                net,
                uploader,
                maps,
                units,
                pending,
                counts,
                logger,
                &mut unclear,
            );
        }
        progress.add(chunk.len() as u64);
    }
    if !unclear.is_empty() {
        settle_unclear(net, uploader, maps, units, unclear, counts, logger);
    }
}

/// Sends one `Email/import` for `batch` and counts each message's outcome.
/// A request too large for the server is split in two; a method error that
/// rejects the whole call is retried one message at a time, so the one at
/// fault fails alone. Anything that leaves it unclear whether the server
/// applied the call goes to `unclear`.
#[allow(clippy::too_many_arguments)]
fn send_batch(
    net: &Net,
    uploader: &mut Uploader,
    maps: &Maps,
    units: &[Unit],
    batch: Vec<Pending>,
    counts: &mut TypeCounts,
    logger: &Logger,
    unclear: &mut Vec<Pending>,
) {
    if batch.is_empty() {
        return;
    }
    let mut emails = Map::new();
    for p in &batch {
        emails.insert(p.cid.clone(), p.item.clone());
    }
    let mut req = Request::new();
    req.call(
        "Email/import",
        json!({ "accountId": net.account, "emails": Value::Object(emails) }),
        "i",
    );
    let cids: Vec<&str> = batch.iter().map(|p| p.cid.as_str()).collect();
    let sent = req.fits(&net.limits).and_then(|()| {
        retry_method_call(
            &net.client,
            MethodCallKind::SingleObjectWrite,
            logger,
            || {
                let resp = req.send_once(&net.client, &net.api)?;
                let mr = resp.first()?;
                check_method_error(mr)?;
                Ok(cids
                    .iter()
                    .map(|cid| interpret_import_for(mr, cid, cids.len()))
                    .collect::<Vec<_>>())
            },
        )
    });
    match sent {
        Ok(outcomes) => {
            for (p, outcome) in batch.into_iter().zip(outcomes) {
                let row = &units[p.unit].row;
                match outcome {
                    SingleImport::Created => counts.created += 1,
                    SingleImport::Skipped => counts.skipped += 1,
                    SingleImport::NotCreated { error_type, .. } if error_type == "blobNotFound" => {
                        retry_after_reupload(net, uploader, maps, &p.cid, row, counts, logger);
                    }
                    SingleImport::NotCreated { detail, .. } => {
                        logger.warn(&format!(
                            "Email/import {} ({}) failed: {detail}",
                            p.cid,
                            blob_hint(uploader, row)
                        ));
                        counts.failed += 1;
                    }
                }
            }
        }
        Err(JmapError::RequestTooLarge | JmapError::SingleObjectTooLarge(_)) if batch.len() > 1 => {
            let mut batch = batch;
            let second = batch.split_off(batch.len() / 2);
            send_batch(net, uploader, maps, units, batch, counts, logger, unclear);
            send_batch(net, uploader, maps, units, second, counts, logger, unclear);
        }
        Err(e) if applied_unknown(&e) => {
            logger.warn(&format!(
                "Email/import of {} message(s) ended without a clear answer ({e}); the target is checked before any is sent again",
                batch.len()
            ));
            unclear.extend(batch);
        }
        Err(JmapError::Method { .. }) if batch.len() > 1 => {
            for p in batch {
                send_batch(net, uploader, maps, units, vec![p], counts, logger, unclear);
            }
        }
        Err(e) => {
            for p in batch {
                logger.warn(&format!(
                    "Email/import {} ({}) send failed: {e}{}",
                    p.cid,
                    blob_hint(uploader, &units[p.unit].row),
                    size_note(&e)
                ));
                counts.failed += 1;
            }
        }
    }
}

/// Whether the server may have applied a call that failed with `e`: the
/// connection broke after the request was sent, the answer was unreadable, or
/// the server said it applied part of it.
fn applied_unknown(e: &JmapError) -> bool {
    match e {
        JmapError::Transport(_)
        | JmapError::RetriesExhausted(_)
        | JmapError::Malformed(_)
        | JmapError::HttpStatus { .. } => true,
        JmapError::Method { error_type, .. } => error_type == "serverPartialFail",
        _ => false,
    }
}

/// Settles messages whose import ended without a clear answer: reads the
/// target again, counts those that arrived as created, and imports the rest
/// one at a time. If the target cannot be read, they are counted as failed --
/// the next export matches whatever did arrive, so none is ever doubled.
fn settle_unclear(
    net: &Net,
    uploader: &mut Uploader,
    maps: &Maps,
    units: &[Unit],
    unclear: Vec<Pending>,
    counts: &mut TypeCounts,
    logger: &Logger,
) {
    let (targets, target_keys) = match target_emails(net) {
        Ok(t) => t,
        Err(e) => {
            logger.warn(&format!(
                "could not read the target to settle {} message(s) ({e}); they count as failed, and the next export matches whatever arrived",
                unclear.len()
            ));
            counts.failed += unclear.len() as u64;
            return;
        }
    };
    let keys: Vec<EmailKey> = email_keys(
        &unclear
            .iter()
            .map(|p| index_from_json(&units[p.unit].row.message_match))
            .collect::<Vec<_>>(),
    );
    let sizes: Vec<Option<u64>> = unclear
        .iter()
        .map(|p| uploader.blob_len(units[p.unit].row.blob_local_id))
        .collect();
    let pairs = pair_with_targets(&keys, &sizes, &target_keys, &targets);
    for (p, found) in unclear.into_iter().zip(pairs) {
        if found.is_some() {
            counts.created += 1;
            continue;
        }
        let unit = &units[p.unit];
        export_one(
            net,
            uploader,
            maps,
            unit.local_id,
            &unit.row,
            counts,
            logger,
        );
    }
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
    keywords: Option<HashSet<String>>,
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
            keywords: v
                .get("keywords")
                .and_then(Value::as_object)
                .map(|m| m.keys().map(|k| k.to_lowercase()).collect()),
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

/// The `Email/set` patch that brings a matched email on the target in line
/// with the archive, or `None` when it already is. The source is taken as
/// the truth for what it covers: keywords are added and removed to match, and
/// so are memberships of folders this run migrated. Folders that exist only
/// on the target are left alone, an email is never left in no folder, and
/// whatever the server did not report is not touched.
fn email_patch(
    row: &EmailRow,
    target: &TargetEmail,
    maps: &Maps,
    migrated: &HashSet<String>,
) -> Option<Value> {
    let mut patch = Map::new();
    if let Some(have) = &target.mailboxes {
        let want: HashSet<String> = row
            .mailbox_locals
            .iter()
            .filter_map(|ml| maps.target(ObjectType::Mailbox, *ml).map(|t| t.0))
            .collect();
        let add: Vec<&String> = want.iter().filter(|t| !have.contains(*t)).collect();
        let remove: Vec<&String> = have
            .iter()
            .filter(|t| migrated.contains(*t) && !want.contains(*t))
            .collect();
        let left = have.len() - remove.len() + add.len();
        for t in add {
            patch.insert(
                format!("mailboxIds/{}", pointer_escape(t)),
                Value::Bool(true),
            );
        }
        if left > 0 {
            for t in remove {
                patch.insert(format!("mailboxIds/{}", pointer_escape(t)), Value::Null);
            }
        }
    }
    if let Some(have) = &target.keywords {
        let want: HashSet<String> = row.keywords.iter().map(|k| k.to_lowercase()).collect();
        for k in want.difference(have) {
            patch.insert(format!("keywords/{}", pointer_escape(k)), Value::Bool(true));
        }
        for k in have.difference(&want) {
            patch.insert(format!("keywords/{}", pointer_escape(k)), Value::Null);
        }
    }
    (!patch.is_empty()).then_some(Value::Object(patch))
}

/// Escapes one JSON Pointer segment (RFC 6901), as JMAP patch paths use.
fn pointer_escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
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
    Ok(interpret_import_for(mr, cid, 1))
}

/// One message's outcome in an `Email/import` answer. With a single message
/// in the call, any `created` entry is taken as its own, as servers may key it
/// differently.
fn interpret_import_for(mr: &MethodCall, cid: &str, in_call: usize) -> SingleImport {
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
            return SingleImport::Skipped;
        }
        return SingleImport::NotCreated {
            error_type,
            detail: err.to_string(),
        };
    }
    let created = mr.args.get("created").and_then(Value::as_object);
    if created.is_some_and(|c| c.contains_key(cid) || (in_call == 1 && !c.is_empty())) {
        return SingleImport::Created;
    }
    SingleImport::NotCreated {
        error_type: String::new(),
        detail: format!("Email/import returned neither created nor notCreated for {cid}"),
    }
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
            keywords: None,
        }
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
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

    fn two_folder_maps() -> Maps {
        let mut maps = Maps::default();
        maps.insert(ObjectType::Mailbox, 1, JmapId("T1".into()));
        maps.insert(ObjectType::Mailbox, 2, JmapId("T2".into()));
        maps
    }

    #[test]
    fn patch_adds_the_absent_migrated_folders() {
        let maps = two_folder_maps();
        let r = row(10, &[1, 2, 3], &[]);
        let patch = email_patch(
            &r,
            &target("E", None, Some(&["T1", "Own"])),
            &maps,
            &set(&["T1", "T2"]),
        )
        .expect("T2 is missing");
        assert_eq!(patch, json!({ "mailboxIds/T2": true }));
        assert!(
            email_patch(
                &r,
                &target("E", None, Some(&["T1", "T2"])),
                &maps,
                &set(&["T1", "T2"])
            )
            .is_none()
        );
        assert!(
            email_patch(&r, &target("E", None, None), &maps, &set(&["T1", "T2"])).is_none(),
            "unknown membership is left alone"
        );
    }

    #[test]
    fn patch_moves_between_migrated_folders_but_keeps_target_only_ones() {
        let maps = two_folder_maps();
        let r = row(10, &[2], &[]);
        let patch = email_patch(
            &r,
            &target("E", None, Some(&["T1", "Own"])),
            &maps,
            &set(&["T1", "T2"]),
        )
        .unwrap();
        assert_eq!(
            patch,
            json!({ "mailboxIds/T2": true, "mailboxIds/T1": null })
        );
    }

    #[test]
    fn patch_never_leaves_an_email_in_no_folder() {
        let maps = two_folder_maps();
        let r = row(10, &[9], &[]);
        assert!(
            email_patch(
                &r,
                &target("E", None, Some(&["T1"])),
                &maps,
                &set(&["T1", "T2"])
            )
            .is_none(),
            "the only folder is not removed when nothing replaces it"
        );
    }

    #[test]
    fn patch_syncs_keywords_both_ways_case_insensitively() {
        let maps = two_folder_maps();
        let r = row(10, &[1], &["$Seen", "work/urgent"]);
        let mut t = target("E", None, Some(&["T1"]));
        t.keywords = Some(set(&["$seen", "$flagged"]));
        let patch = email_patch(&r, &t, &maps, &set(&["T1", "T2"])).unwrap();
        assert_eq!(
            patch,
            json!({ "keywords/work~1urgent": true, "keywords/$flagged": null })
        );
        t.keywords = Some(set(&["$seen", "work/urgent"]));
        assert!(email_patch(&r, &t, &maps, &set(&["T1", "T2"])).is_none());
    }
}
