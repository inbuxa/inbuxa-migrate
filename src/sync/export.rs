/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use crate::db;
use crate::error::Error;
use crate::jmap::blobxfer;
use crate::jmap::connect::{self, Connected};
use crate::jmap::error::JmapError;
use crate::jmap::http::HttpClient;
use crate::jmap::request::{Request, SetRequest, get_all, get_objects, query_all_ids, set_call};
use crate::jmap::session::{Limits, Session};
use crate::jmap::wire::JmapId;
use crate::logging::{LEVEL_DEFAULT, Logger};
use crate::sync::import_jmap::mapping::{BlobBytes, TargetResolver};
use crate::sync::{CommonConfig, Context, ExportConfig, Summary, TypeCounts};
use crate::types::ObjectType;

const EXPORT_ORDER: [ObjectType; 10] = [
    ObjectType::Mailbox,
    ObjectType::AddressBook,
    ObjectType::Calendar,
    ObjectType::FileNode,
    ObjectType::Identity,
    ObjectType::SieveScript,
    ObjectType::ParticipantIdentity,
    ObjectType::Email,
    ObjectType::ContactCard,
    ObjectType::CalendarEvent,
];

type IdMap = HashMap<i64, JmapId>;

#[derive(Default)]
struct Maps {
    m: HashMap<ObjectType, IdMap>,
}

impl Maps {
    fn insert(&mut self, ty: ObjectType, local: i64, target: JmapId) {
        self.m.entry(ty).or_default().insert(local, target);
    }

    /// Every target id this run mapped for `ty`: the objects it migrated.
    fn targets_of(&self, ty: ObjectType) -> HashSet<String> {
        self.m
            .get(&ty)
            .map(|m| m.values().map(|id| id.0.clone()).collect())
            .unwrap_or_default()
    }
}

impl TargetResolver for Maps {
    fn target(&self, ty: ObjectType, local_id: i64) -> Option<JmapId> {
        self.m.get(&ty)?.get(&local_id).cloned()
    }
}

struct Uploader<'a> {
    net: &'a Net,
    conn: &'a Connection,
    cache: HashMap<i64, JmapId>,
    touched: Vec<i64>,
}

impl<'a> Uploader<'a> {
    fn new(net: &'a Net, conn: &'a Connection) -> Uploader<'a> {
        Uploader {
            net,
            conn,
            cache: HashMap::new(),
            touched: Vec::new(),
        }
    }

    fn upload_with(&mut self, local_id: i64, content_type: &str) -> Result<JmapId, JmapError> {
        self.touched.push(local_id);
        if let Some(id) = self.cache.get(&local_id) {
            return Ok(id.clone());
        }
        let id = if self.net.dry_run {
            let len = db::blobs::blob_len(self.conn, local_id)?
                .ok_or_else(|| JmapError::malformed(format!("blob local id {local_id} missing")))?;
            self.net.check_upload_size(len)?;
            JmapId(format!("dryrun-blob-{local_id}"))
        } else {
            let bytes = db::blobs::blob_bytes(self.conn, local_id)?
                .ok_or_else(|| JmapError::malformed(format!("blob local id {local_id} missing")))?;
            blobxfer::upload_bytes(
                &self.net.client,
                &self.net.session,
                &self.net.account,
                content_type,
                &bytes,
            )?
        };
        self.cache.insert(local_id, id.clone());
        Ok(id)
    }

    /// As `upload_with`, but sends `bytes` in place of the stored blob: for
    /// content rewritten on its way to the target. Cached under the same
    /// local id, so a retry sends the rewritten bytes again.
    fn upload_bytes_as(
        &mut self,
        local_id: i64,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<JmapId, JmapError> {
        self.touched.push(local_id);
        if let Some(id) = self.cache.get(&local_id) {
            return Ok(id.clone());
        }
        let id = if self.net.dry_run {
            self.net.check_upload_size(bytes.len() as u64)?;
            JmapId(format!("dryrun-blob-{local_id}"))
        } else {
            blobxfer::upload_bytes(
                &self.net.client,
                &self.net.session,
                &self.net.account,
                content_type,
                bytes,
            )?
        };
        self.cache.insert(local_id, id.clone());
        Ok(id)
    }

    /// Uploads several stored blobs at once, on up to `Net::upload_workers`
    /// threads, and returns each one's result in the order given. Each thread
    /// reads its blobs through its own read-only connection to the archive, so
    /// at most one blob per thread is held in memory. With one worker, or in a
    /// dry run, it is `upload_with` in a loop.
    fn upload_many(
        &mut self,
        local_ids: &[i64],
        content_type: &str,
    ) -> Vec<Result<JmapId, JmapError>> {
        if self.net.dry_run || self.net.upload_workers <= 1 {
            return local_ids
                .iter()
                .map(|id| self.upload_with(*id, content_type))
                .collect();
        }
        self.touched.extend_from_slice(local_ids);
        let mut todo: Vec<i64> = Vec::new();
        for id in local_ids {
            if !self.cache.contains_key(id) && !todo.contains(id) {
                todo.push(*id);
            }
        }
        let net = self.net;
        let results = run_bounded(
            &todo,
            net.upload_workers,
            || {
                rusqlite::Connection::open_with_flags(
                    &net.archive,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
            },
            |conn, local_id| {
                let conn = conn
                    .as_ref()
                    .map_err(|e| JmapError::malformed(format!("archive not readable: {e}")))?;
                let bytes = db::blobs::blob_bytes(conn, *local_id)?.ok_or_else(|| {
                    JmapError::malformed(format!("blob local id {local_id} missing"))
                })?;
                blobxfer::upload_bytes(
                    &net.client,
                    &net.session,
                    &net.account,
                    content_type,
                    &bytes,
                )
            },
        );
        let mut failed: HashMap<i64, JmapError> = HashMap::new();
        for (local_id, result) in todo.into_iter().zip(results) {
            match result {
                Ok(id) => {
                    self.cache.insert(local_id, id);
                }
                Err(e) => {
                    failed.insert(local_id, e);
                }
            }
        }
        local_ids
            .iter()
            .map(|id| match self.cache.get(id) {
                Some(blob) => Ok(blob.clone()),
                None => Err(failed.get(id).map(clone_error).unwrap_or_else(|| {
                    JmapError::malformed(format!("blob local id {id} not uploaded"))
                })),
            })
            .collect()
    }

    fn invalidate(&mut self, local_id: i64) {
        self.cache.remove(&local_id);
    }

    fn blob_len(&self, local_id: i64) -> Option<u64> {
        db::blobs::blob_len(self.conn, local_id).ok().flatten()
    }

    fn take_touched(&mut self) -> Vec<i64> {
        std::mem::take(&mut self.touched)
    }
}

/// A copy of an upload error for each archive row that shares the blob. The
/// errors that carry meaning for the caller -- the size limits -- keep their
/// kind; the rest keep their message.
fn clone_error(e: &JmapError) -> JmapError {
    match e {
        JmapError::RequestTooLarge => JmapError::RequestTooLarge,
        JmapError::SingleObjectTooLarge(m) => JmapError::SingleObjectTooLarge(m.clone()),
        other => JmapError::Transport(other.to_string()),
    }
}

/// Runs `f` over `jobs` on at most `workers` threads and returns the results
/// in job order. Each thread builds its own state once with `init`, such as a
/// connection of its own to the archive.
fn run_bounded<J, S, R>(
    jobs: &[J],
    workers: usize,
    init: impl Fn() -> S + Sync,
    f: impl Fn(&mut S, &J) -> R + Sync,
) -> Vec<R>
where
    J: Sync,
    R: Send,
{
    let workers = workers.clamp(1, jobs.len().max(1));
    if workers == 1 {
        let mut state = init();
        return jobs.iter().map(|j| f(&mut state, j)).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<R>>> = Mutex::new((0..jobs.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut state = init();
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(job) = jobs.get(i) else { break };
                    let r = f(&mut state, job);
                    slots.lock().expect("result slots")[i] = Some(r);
                }
            });
        }
    });
    slots
        .into_inner()
        .expect("result slots")
        .into_iter()
        .map(|r| r.expect("every job ran"))
        .collect()
}

impl BlobBytes for Uploader<'_> {
    fn bytes(&self, local_id: i64) -> Result<Vec<u8>, JmapError> {
        db::blobs::blob_bytes(self.conn, local_id)?
            .ok_or_else(|| JmapError::malformed(format!("blob local id {local_id} missing")))
    }
}

#[derive(Clone)]
struct Net {
    client: HttpClient,
    api: String,
    account: String,
    limits: Limits,
    session: Session,
    dry_run: bool,
    /// The archive's path, for the upload threads' own connections.
    archive: PathBuf,
    /// Blobs uploaded at once: the server's `maxConcurrentUpload`, and no
    /// more than `--threads`.
    upload_workers: usize,
    /// In a dry run, what would fail and why, for the plan.
    would_fail: std::sync::Arc<Mutex<Vec<String>>>,
}

impl Net {
    /// In a dry run, a blob of `len` bytes that the target would refuse:
    /// over its `maxSizeUpload`.
    fn check_upload_size(&self, len: u64) -> Result<(), JmapError> {
        let cap = self.limits.max_size_upload;
        if cap > 0 && len > cap {
            return Err(JmapError::SingleObjectTooLarge(format!(
                "{} is larger than the target accepts ({} maxSizeUpload)",
                crate::inspect::format_bytes(len),
                crate::inspect::format_bytes(cap)
            )));
        }
        Ok(())
    }

    /// Notes, in a dry run, that `what` would fail and why. A real run
    /// reports failures as they happen and keeps no list.
    fn would_fail(&self, what: impl Into<String>) {
        if self.dry_run {
            self.would_fail
                .lock()
                .expect("would-fail list")
                .push(what.into());
        }
    }
}

fn has_rows(conn: &Connection, ty: ObjectType) -> bool {
    let table = crate::sync::table_name(ty);
    conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n != 0)
    .unwrap_or(false)
}

pub fn run(common: CommonConfig, config: ExportConfig) -> Result<Summary, Error> {
    let logger = common.logger;
    let ctx = Context::open(common, &config.connect)?;
    let connected = connect::prepare(&ctx, &config.connect)?;

    let net = Net {
        client: ctx.client.clone(),
        api: connected.session.api_url.clone(),
        account: connected.account_id.clone(),
        limits: connected.limits,
        session: connected.session.clone(),
        dry_run: ctx.dry_run(),
        archive: ctx.common.archive.clone(),
        upload_workers: (connected.limits.max_concurrent_upload as usize)
            .min(ctx.common.threads)
            .max(1),
        would_fail: Default::default(),
    };

    let work = work_list(&ctx.conn, &config, &connected, &logger);
    let mut maps = Maps::default();
    let mut summary = Summary::default();
    let mut dry_rows: Vec<(&'static str, u64, u64, u64)> = Vec::new();
    let mut plans: HashMap<ObjectType, Plan> = HashMap::new();
    let mut counts_per_type: HashMap<ObjectType, TypeCounts> = HashMap::new();

    for ty in &work {
        if logger.enabled(LEVEL_DEFAULT) {
            eprintln!("export: {} ...", ty.jmap_name());
        }
        let started = std::time::Instant::now();
        let mut counts = TypeCounts::default();
        let res = reconcile_type(
            &ctx,
            &net,
            *ty,
            &mut maps,
            &logger,
            &mut counts,
            &mut dry_rows,
        );
        let plan = match res {
            Ok(p) => p,
            Err(e) if e.aborts_run() => return Err(e),
            Err(e) => {
                logger.warn(&format!("type {} aborted: {e}", ty.jmap_name()));
                counts.failed += 1;
                Plan::default()
            }
        };
        if logger.enabled(LEVEL_DEFAULT) && !ctx.dry_run() {
            eprintln!(
                "{}",
                crate::sync::progress::done_line(
                    &format!("export: {}", ty.jmap_name()),
                    &counts,
                    started.elapsed()
                )
            );
        }
        plans.insert(*ty, plan);
        counts_per_type.insert(*ty, counts);
    }

    if config.prune {
        prune_phase(
            &ctx,
            &net,
            &work,
            &plans,
            &config,
            &logger,
            &mut counts_per_type,
        )?;
    }

    for ty in &work {
        if let Some(counts) = counts_per_type.remove(ty) {
            summary.per_type.push((ty.jmap_name(), counts));
        }
    }

    if ctx.dry_run() {
        let would_fail = net.would_fail.lock().expect("would-fail list").clone();
        print_plan(&summary, &dry_rows, &would_fail, config.prune);
        return Ok(summary);
    }
    summary.retries_observed = ctx.client.retries_observed();
    summary.retry_after_sleeps = ctx.client.retry_after_sleeps();
    Ok(summary)
}

fn prune_phase(
    ctx: &Context,
    net: &Net,
    work: &[ObjectType],
    plans: &HashMap<ObjectType, Plan>,
    config: &ExportConfig,
    logger: &Logger,
    counts_per_type: &mut HashMap<ObjectType, TypeCounts>,
) -> Result<(), Error> {
    let totals: Vec<(ObjectType, &Plan)> = work
        .iter()
        .filter_map(|ty| plans.get(ty).map(|p| (*ty, p)))
        .filter(|(_, p)| !p.prune_candidates.is_empty())
        .collect();
    if totals.is_empty() {
        return Ok(());
    }
    eprintln!("prune plan:");
    let total: usize = totals.iter().map(|(_, p)| p.prune_candidates.len()).sum();
    for (ty, p) in &totals {
        eprintln!(
            "  {:<22} {:>6} candidate(s); sample: {}",
            ty.jmap_name(),
            p.prune_candidates.len(),
            sample(&p.prune_candidates),
        );
    }
    eprintln!("  {:<22} {:>6} total", "(all types)", total);
    if ctx.dry_run() {
        return Ok(());
    }
    if !config.yes && std::io::stdin().is_terminal() {
        eprint!("destroy all {total} objects across all types? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| Error::Partial(e.to_string()))?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            return Err(Error::PruneAborted);
        }
    }
    for ty in work.iter().rev() {
        if let Some(plan) = plans.get(ty)
            && !plan.prune_candidates.is_empty()
            && let Some(counts) = counts_per_type.get_mut(ty)
        {
            do_destroy(net, *ty, plan, logger, counts);
        }
    }
    Ok(())
}

fn work_list(
    conn: &Connection,
    config: &ExportConfig,
    connected: &Connected,
    logger: &Logger,
) -> Vec<ObjectType> {
    let selected = config.objects.as_ref();
    EXPORT_ORDER
        .into_iter()
        .filter(|ty| selected.map(|s| s.contains(ty)).unwrap_or(true))
        .filter(|ty| has_rows(conn, *ty))
        .filter(|ty| {
            if connected.supports(*ty) {
                true
            } else {
                logger.warn(&format!(
                    "target does not support {}; skipping",
                    ty.jmap_name()
                ));
                false
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn reconcile_type(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    maps: &mut Maps,
    logger: &Logger,
    counts: &mut TypeCounts,
    dry_rows: &mut Vec<(&'static str, u64, u64, u64)>,
) -> Result<Plan, Error> {
    let plan = match ty {
        ObjectType::Mailbox | ObjectType::FileNode => {
            tree::reconcile(ctx, net, ty, maps, counts, logger)
        }
        ObjectType::AddressBook | ObjectType::Calendar => {
            flat::reconcile(ctx, net, ty, maps, counts, logger)
        }
        ObjectType::Identity => keyed::reconcile_identity(ctx, net, maps, counts, logger),
        ObjectType::ParticipantIdentity => {
            keyed::reconcile_participant(ctx, net, maps, counts, logger)
        }
        ObjectType::SieveScript => sieve::reconcile(ctx, net, maps, counts, logger),
        ObjectType::ContactCard | ObjectType::CalendarEvent => {
            uidtype::reconcile(ctx, net, ty, maps, counts, logger)
        }
        ObjectType::Email => email::reconcile(ctx, net, maps, counts, logger),
    }?;

    if ctx.dry_run() {
        dry_rows.push((
            ty.jmap_name(),
            counts.created,
            counts.skipped,
            plan.prune_candidates.len() as u64,
        ));
    }

    Ok(plan)
}

#[derive(Default)]
pub struct Plan {
    pub prune_candidates: Vec<String>,
    pub active_sieve_target: Option<String>,
}

fn do_destroy(net: &Net, ty: ObjectType, plan: &Plan, logger: &Logger, counts: &mut TypeCounts) {
    if ty == ObjectType::SieveScript {
        deactivate_active_sieve_script(net, logger);
    }
    let destroy = Value::Array(
        plan.prune_candidates
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect(),
    );
    let extra = destroy_contents_arg(ty);
    match set_call(
        &net.client,
        &net.api,
        &net.account,
        ty.jmap_name(),
        SetRequest {
            destroy: Some(destroy),
            extra_args: &extra,
            ..Default::default()
        },
        &net.limits,
    ) {
        Ok(outcome) => {
            counts.deleted += outcome.destroyed.len() as u64;
            for (id, err) in &outcome.not_destroyed {
                logger.warn(&format!(
                    "prune: {} {id} not destroyed: {err}",
                    ty.jmap_name()
                ));
                counts.skipped += 1;
            }
        }
        Err(e) => {
            logger.warn(&format!(
                "prune {}: destroy request failed: {e}",
                ty.jmap_name()
            ));
            counts.failed += plan.prune_candidates.len() as u64;
        }
    }
}

fn deactivate_active_sieve_script(net: &Net, logger: &Logger) {
    let mut req = Request::new();
    req.call(
        "SieveScript/set",
        json!({ "accountId": net.account, "onSuccessDeactivateScript": true }),
        "d",
    );
    let outcome = req.send(&net.client, &net.api).and_then(|resp| {
        let mr = resp.first()?;
        crate::jmap::request::check_method_error(mr)
    });
    if let Err(e) = outcome {
        logger.warn(&format!(
            "prune: SieveScript deactivation failed before destroy: {e}"
        ));
    }
}

fn destroy_contents_arg(ty: ObjectType) -> Vec<(&'static str, Value)> {
    match ty {
        ObjectType::AddressBook => vec![("onDestroyRemoveContents", Value::Bool(false))],
        ObjectType::Calendar => vec![("onDestroyRemoveEvents", Value::Bool(false))],
        ObjectType::FileNode => vec![("onDestroyRemoveChildren", Value::Bool(false))],
        _ => Vec::new(),
    }
}

fn sample(ids: &[String]) -> String {
    let n = ids.len().min(5);
    ids[..n].join(", ")
}

/// The dry run's report, in plain words: per type, what would be created,
/// updated, left as it is and would fail; then why each failure would happen.
fn print_plan(
    summary: &Summary,
    dry_rows: &[(&'static str, u64, u64, u64)],
    would_fail: &[String],
    prune: bool,
) {
    print!("{}", plan_text(summary, dry_rows, would_fail, prune));
}

fn plan_text(
    summary: &Summary,
    dry_rows: &[(&'static str, u64, u64, u64)],
    would_fail: &[String],
    prune: bool,
) -> String {
    use crate::sync::progress::thousands;
    let mut out = String::from("Dry run: nothing was written to the target. The plan:\n");
    for (ty, c) in &summary.per_type {
        let mut parts: Vec<String> = Vec::new();
        if c.created > 0 {
            parts.push(format!("{} to create", thousands(c.created)));
        }
        if c.updated > 0 {
            parts.push(format!("{} to update", thousands(c.updated)));
        }
        if c.skipped > 0 {
            parts.push(format!("{} unchanged", thousands(c.skipped)));
        }
        if c.failed > 0 {
            parts.push(format!("{} would fail", thousands(c.failed)));
        }
        if prune {
            let gone = dry_rows
                .iter()
                .find(|(t, ..)| t == ty)
                .map(|(.., d)| *d)
                .unwrap_or(0);
            if gone > 0 {
                parts.push(format!("{} to delete (--prune)", thousands(gone)));
            }
        }
        if parts.is_empty() {
            parts.push("nothing to do".to_owned());
        }
        out.push_str(&format!("  {ty:<20} {}\n", parts.join(", ")));
    }
    let failed: u64 = summary.per_type.iter().map(|(_, c)| c.failed).sum();
    if failed > 0 {
        out.push_str("Would fail:\n");
        for line in would_fail {
            out.push_str(&format!("  {line}\n"));
        }
        let unexplained = failed.saturating_sub(would_fail.len() as u64);
        if unexplained > 0 {
            out.push_str(&format!(
                "  {} more; the warnings above say why\n",
                thousands(unexplained)
            ));
        }
    }
    out
}

mod tree;

mod flat;

mod keyed;

mod sieve;

mod sieve_names;

mod uidtype;

mod email;

mod common {
    use super::*;

    pub fn target_query_get(
        net: &Net,
        ty: ObjectType,
        props: Option<&[&str]>,
    ) -> Result<Vec<Value>, JmapError> {
        let ids = query_all_ids(
            &net.client,
            &net.api,
            &net.account,
            ty.jmap_name(),
            &net.limits,
        )?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let got = get_objects::<Value>(
            &net.client,
            &net.api,
            &net.account,
            ty.jmap_name(),
            &ids,
            props,
            &net.limits,
        )?;
        Ok(got.list)
    }

    pub fn target_get_all(net: &Net, ty: ObjectType) -> Result<Vec<Value>, JmapError> {
        Ok(get_all::<Value>(&net.client, &net.api, &net.account, ty.jmap_name())?.list)
    }

    pub fn jid(v: &Value) -> Option<String> {
        v.get("id").and_then(Value::as_str).map(str::to_owned)
    }

    pub fn create_batch(
        net: &Net,
        ty: ObjectType,
        creates: Vec<(String, Value)>,
    ) -> Result<crate::jmap::request::SetOutcome, JmapError> {
        if net.dry_run {
            return Ok(synthesize_dry_run_outcome(net, ty, &creates));
        }
        let mut map = Map::new();
        for (cid, obj) in creates {
            map.insert(cid, obj);
        }
        set_call(
            &net.client,
            &net.api,
            &net.account,
            ty.jmap_name(),
            SetRequest {
                create: Some(Value::Object(map)),
                ..Default::default()
            },
            &net.limits,
        )
    }

    /// Sends `updates` (target id, patch) as batched `/set` calls and counts
    /// the result into `counts`. A dry run counts them as updated and sends
    /// nothing.
    pub fn update_batch(
        net: &Net,
        ty: ObjectType,
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
            ty.jmap_name(),
            SetRequest {
                update: Some(Value::Object(map)),
                ..Default::default()
            },
            &net.limits,
        ) {
            Ok(outcome) => {
                counts.updated += outcome.updated.len() as u64;
                for (id, err) in &outcome.not_updated {
                    logger.warn(&format!("{}/set {id} not updated: {err}", ty.jmap_name()));
                    counts.failed += 1;
                }
            }
            Err(e) => {
                logger.warn(&format!(
                    "{}/set: updating {total} object(s) failed: {e}",
                    ty.jmap_name()
                ));
                counts.failed += total;
            }
        }
    }

    fn blob_not_found(outcome: &crate::jmap::request::SetOutcome, cid: &str) -> bool {
        outcome.not_created.iter().any(|(c, err)| {
            c == cid && err.get("type").and_then(Value::as_str) == Some("blobNotFound")
        })
    }

    pub fn retry_if_blob_missing<F>(
        net: &Net,
        ty: ObjectType,
        cid: &str,
        uploader: &mut Uploader<'_>,
        touched: Vec<i64>,
        outcome: crate::jmap::request::SetOutcome,
        mut rebuild: F,
    ) -> Result<crate::jmap::request::SetOutcome, Error>
    where
        F: FnMut(&mut Uploader<'_>) -> Result<Value, Error>,
    {
        if !blob_not_found(&outcome, cid) {
            return Ok(outcome);
        }
        for id in &touched {
            uploader.invalidate(*id);
        }
        let _ = uploader.take_touched();
        let wire = rebuild(uploader)?;
        let _ = uploader.take_touched();
        create_batch(net, ty, vec![(cid.to_owned(), wire)]).map_err(Error::from)
    }

    /// Room left in a request for everything but the object itself: the
    /// envelope, the method name and the arguments around it.
    const REQUEST_OVERHEAD: u64 = 512;

    /// What a dry run predicts for `creates`: each one created, except an
    /// object too big to fit in one request under the target's
    /// `maxSizeRequest`, which a real run could not send either.
    fn synthesize_dry_run_outcome(
        net: &Net,
        ty: ObjectType,
        creates: &[(String, Value)],
    ) -> crate::jmap::request::SetOutcome {
        let mut outcome = crate::jmap::request::SetOutcome::default();
        let cap = net.limits.max_size_request;
        for (cid, obj) in creates {
            let size = serde_json::to_vec(obj).map(|v| v.len() as u64).unwrap_or(0);
            if cap > 0 && size + REQUEST_OVERHEAD > cap {
                let why = format!(
                    "{} is larger than one request to the target may be ({} maxSizeRequest)",
                    crate::inspect::format_bytes(size),
                    crate::inspect::format_bytes(cap)
                );
                net.would_fail(format!("{} {cid}: {why}", ty.jmap_name()));
                outcome.not_created.push((
                    cid.clone(),
                    serde_json::json!({ "type": "tooLarge", "description": why }),
                ));
                continue;
            }
            let synthetic = serde_json::json!({
                "id": format!("dryrun-{}-{cid}", ty.jmap_name())
            });
            outcome.created.push((cid.clone(), synthetic));
        }
        outcome
    }
}

#[cfg(test)]
mod pool_tests {
    use super::run_bounded;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn never_more_workers_at_once_than_the_cap_and_results_keep_job_order() {
        let jobs: Vec<usize> = (0..24).collect();
        let running = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        let inits = AtomicUsize::new(0);
        let out = run_bounded(
            &jobs,
            3,
            || inits.fetch_add(1, Ordering::SeqCst),
            |_, j| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(5));
                running.fetch_sub(1, Ordering::SeqCst);
                j * 2
            },
        );
        assert_eq!(out, jobs.iter().map(|j| j * 2).collect::<Vec<_>>());
        let most = most.load(Ordering::SeqCst);
        assert!(most <= 3, "{most} ran at once");
        assert!(most > 1, "work ran in parallel");
        assert_eq!(
            inits.load(Ordering::SeqCst),
            3,
            "state built once per worker"
        );
    }

    #[test]
    fn one_worker_or_one_job_runs_in_place() {
        let out = run_bounded(&[1, 2, 3], 1, || (), |_, j| j + 1);
        assert_eq!(out, vec![2, 3, 4]);
        let out = run_bounded(&[7], 8, || (), |_, j| j + 1);
        assert_eq!(out, vec![8]);
        let out: Vec<i32> = run_bounded(&[], 4, || (), |_, j: &i32| *j);
        assert!(out.is_empty());
    }
}

#[cfg(test)]
mod plan_tests {
    use super::plan_text;
    use crate::sync::{Summary, TypeCounts};

    fn summary(rows: &[(&'static str, u64, u64, u64, u64)]) -> Summary {
        Summary {
            per_type: rows
                .iter()
                .map(|(t, created, updated, skipped, failed)| {
                    (
                        *t,
                        TypeCounts {
                            created: *created,
                            updated: *updated,
                            skipped: *skipped,
                            failed: *failed,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_plan_reads_in_plain_words_and_says_why_things_would_fail() {
        let s = summary(&[
            ("Mailbox", 0, 0, 12, 0),
            ("Email", 1200, 40, 5000, 2),
            ("SieveScript", 0, 0, 0, 0),
        ]);
        let text = plan_text(
            &s,
            &[],
            &["Email e7 (message-id <a@b>): 61 MB is larger than the target accepts".to_owned()],
            false,
        );
        assert!(
            text.starts_with("Dry run: nothing was written to the target."),
            "{text}"
        );
        assert!(text.contains("Mailbox              12 unchanged"), "{text}");
        assert!(
            text.contains(
                "Email                1,200 to create, 40 to update, 5,000 unchanged, 2 would fail"
            ),
            "{text}"
        );
        assert!(
            text.contains("SieveScript          nothing to do"),
            "{text}"
        );
        assert!(text.contains("Would fail:\n  Email e7"), "{text}");
        assert!(
            text.contains("1 more; the warnings above say why"),
            "{text}"
        );
    }

    #[test]
    fn prune_counts_appear_only_with_prune() {
        let s = summary(&[("ContactCard", 0, 0, 3, 0)]);
        let rows = [("ContactCard", 0, 3, 4)];
        assert!(plan_text(&s, &rows, &[], true).contains("3 unchanged, 4 to delete (--prune)"));
        assert!(!plan_text(&s, &rows, &[], false).contains("delete"));
        assert!(!plan_text(&s, &rows, &[], false).contains("Would fail"));
    }
}
