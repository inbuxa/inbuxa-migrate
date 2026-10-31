/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `import files`: an account export (the layout `export-files` writes,
//! specified at <https://coffeylabs.org/specs/account-export/>) read back
//! into an archive, manifest and all. `import maildir` and `import takeout`
//! read the open formats; this reads what only the manifest holds as well:
//! real folder names, roles and subscriptions, which address book and
//! calendar were the default, their colors and time zones, the active Sieve
//! script, media types and links, and the identities with their signatures.
//!
//! Paths in the manifest are data. Each one is checked to stay inside its
//! part of the export, and nothing that is a symbolic link is followed. A
//! run that is repeated brings the archive up to date with the export; it
//! never deletes.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use crate::db;
use crate::db::sources::SourceKey;
use crate::error::Error;
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};
use crate::sync::import_dav::calcard;
use crate::sync::import_takeout::contacts::split_vcards;
use crate::sync::{CommonConfig, RunOutcome, Summary, TypeCounts};

pub mod cards;
pub mod mail;
pub mod manifest;
pub mod other;
pub mod paths;

use manifest::Manifest;

pub const SOURCE_KIND: &str = "files";

#[derive(Debug)]
pub struct FilesImportConfig {
    pub dir: PathBuf,
    pub allow_source_change: bool,
}

pub fn run(common: CommonConfig, config: FilesImportConfig) -> Result<Summary, Error> {
    run_reporting(common, config).into_result()
}

pub fn run_reporting(common: CommonConfig, config: FilesImportConfig) -> RunOutcome {
    let mut summary = Summary::default();
    let error = run_into(common, config, &mut summary).err();
    RunOutcome { summary, error }
}

fn run_into(
    common: CommonConfig,
    config: FilesImportConfig,
    summary: &mut Summary,
) -> Result<(), Error> {
    let logger = common.logger;
    let root = fs::canonicalize(&config.dir)
        .map_err(|e| Error::Usage(format!("{}: {e}", config.dir.display())))?;
    if !root.is_dir() {
        return Err(Error::Usage(format!("not a directory: {}", root.display())));
    }
    let manifest = manifest::read(&root)?;
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "account export {} written by {} at {}",
            root.display(),
            manifest
                .created_by
                .as_deref()
                .unwrap_or("an unnamed program"),
            manifest.created.as_deref().unwrap_or("an unrecorded time"),
        );
    }

    if common.dry_run {
        *summary = count_only(&root, &manifest, logger)?;
        logger.warn("dry run: nothing was written; the counts are what the export holds");
        return Ok(());
    }

    let mut conn = db::init::open(&common.archive)?;
    let session_url = Url::from_file_path(&root)
        .map(|u| u.to_string())
        .map_err(|_| Error::Usage(format!("cannot name {} as a file: URL", root.display())))?;
    let account_id = root.to_string_lossy().into_owned();
    if let Some((existing_url, _)) =
        db::sources::conflicting_source(&conn, SOURCE_KIND, &session_url, &account_id)?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records the account export at {existing_url}; re-run with \
             --allow-source-change or use a fresh archive"
        )));
    }
    let original = manifest.sources.first();
    let account_name = original
        .and_then(|s| s.account_name.clone().or_else(|| s.username.clone()))
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()));
    if logger.enabled(LEVEL_PROGRESS)
        && let Some(source) = original
    {
        eprintln!(
            "the account came from {} ({})",
            source.url.as_deref().unwrap_or("an unrecorded address"),
            source.kind.as_deref().unwrap_or("an unrecorded kind"),
        );
    }
    let source_id = db::sources::upsert_source(
        &conn,
        &SourceKey {
            kind: SOURCE_KIND.to_owned(),
            session_url,
            account_id,
        },
        account_name.as_deref(),
        "",
    )?;

    let result = import_parts(&mut conn, &root, source_id, &manifest, logger, summary);
    if result.is_ok() && !summary.any_failed() {
        let tx = conn.unchecked_transaction()?;
        db::blobs::gc_orphan_blobs(&tx)?;
        tx.commit()?;
    }
    if logger.enabled(LEVEL_DEFAULT) {
        let line: Vec<String> = summary
            .per_type
            .iter()
            .map(|(name, c)| {
                format!(
                    "{name} c={} u={} s={} f={}",
                    c.created, c.updated, c.skipped, c.failed
                )
            })
            .collect();
        eprintln!("files: {}", line.join(" | "));
    }
    result
}

/// Each part the manifest lists, in turn; one that is missing wasn't
/// exported and is left as the archive has it.
fn import_parts(
    conn: &mut Connection,
    root: &Path,
    source_id: i64,
    manifest: &Manifest,
    logger: Logger,
    summary: &mut Summary,
) -> Result<(), Error> {
    if let Some(mail) = &manifest.mail {
        let out = mail::import(conn, root, source_id, mail, logger)?;
        summary.per_type.push(("mailbox", out.mailbox));
        summary.per_type.push(("email", out.email));
    }
    if let Some(books) = &manifest.contacts {
        let out = cards::import_contacts(conn, root, source_id, books, logger)?;
        summary.per_type.push(("addressbook", out.collection));
        summary.per_type.push(("contactcard", out.item));
    }
    if let Some(calendars) = &manifest.calendars {
        let out = cards::import_calendars(conn, root, source_id, calendars, logger)?;
        summary.per_type.push(("calendar", out.collection));
        summary.per_type.push(("calendarevent", out.item));
    }
    if let Some(scripts) = &manifest.sieve {
        let counts = other::import_sieve(conn, root, source_id, scripts, logger)?;
        summary.per_type.push(("sievescript", counts));
    }
    if let Some(files) = &manifest.files {
        let created = manifest
            .created
            .clone()
            .filter(|c| OffsetDateTime::parse(c, &Rfc3339).is_ok())
            .unwrap_or_else(now_rfc3339);
        let counts = other::import_files(conn, root, source_id, files, &created, logger)?;
        summary.per_type.push(("filenode", counts));
    }
    if let Some(identities) = &manifest.identities {
        let counts = other::import_identities(conn, source_id, identities)?;
        summary.per_type.push(("identity", counts));
    }
    if let Some(identities) = &manifest.participant_identities {
        let counts = other::import_participant_identities(conn, source_id, identities, logger)?;
        summary.per_type.push(("participantidentity", counts));
    }
    Ok(())
}

/// What a run would read, counted from the export without opening the
/// archive. A message kept in several folders counts once per folder.
fn count_only(root: &Path, manifest: &Manifest, logger: Logger) -> Result<Summary, Error> {
    let found = |n: usize| TypeCounts {
        fetched: n as u64,
        ..Default::default()
    };
    let mut summary = Summary::default();
    if let Some(mail) = &manifest.mail {
        let mut messages = 0;
        let mut failed = 0;
        for folder in &mail.folders {
            match paths::resolve(root, &folder.dir, &mail.path) {
                Ok(dir) => {
                    for sub in ["cur", "new"] {
                        let sub_dir = dir.join(sub);
                        if paths::is_real_dir(&sub_dir) {
                            messages += fs::read_dir(&sub_dir)?
                                .filter_map(Result::ok)
                                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                                .count();
                        }
                    }
                }
                Err(reason) => {
                    logger.warn(&format!("folder {:?}: {reason}", folder.name));
                    failed += 1;
                }
            }
        }
        summary
            .per_type
            .push(("mailbox", found(mail.folders.len())));
        let mut email = found(messages);
        email.failed = failed;
        summary.per_type.push(("email", email));
    }
    for (part, books, collection, item) in [
        ("contacts", &manifest.contacts, "addressbook", "contactcard"),
        (
            "calendars",
            &manifest.calendars,
            "calendar",
            "calendarevent",
        ),
    ] {
        let Some(books) = books else { continue };
        let mut items = TypeCounts::default();
        for book in books {
            match cards::read_part_file(root, &book.file, part, logger) {
                Some(text) if part == "contacts" => {
                    items.fetched += split_vcards(&text).len() as u64
                }
                Some(text) => match calcard::ical_to_jscalendar_entries(&text) {
                    Ok(entries) => items.fetched += entries.len() as u64,
                    Err(calcard::CalcardError::NoEntries) => {}
                    Err(e) => {
                        logger.warn(&format!("{}: {e}", book.file));
                        items.failed += 1;
                    }
                },
                None => items.failed += 1,
            }
        }
        summary.per_type.push((collection, found(books.len())));
        summary.per_type.push((item, items));
    }
    if let Some(scripts) = &manifest.sieve {
        let mut counts = TypeCounts::default();
        for script in scripts {
            match paths::resolve(root, &script.file, "sieve") {
                Ok(path) if paths::is_real_file(&path) => counts.fetched += 1,
                Ok(_) => counts.failed += 1,
                Err(reason) => {
                    logger.warn(&reason);
                    counts.failed += 1;
                }
            }
        }
        summary.per_type.push(("sievescript", counts));
    }
    if let Some(files) = &manifest.files {
        let base = root.join(&files.path);
        let nodes = if paths::segments(&files.path).map(|s| s.len()) == Ok(1)
            && paths::is_real_dir(&base)
        {
            count_tree(&base)?
        } else {
            0
        };
        summary
            .per_type
            .push(("filenode", found(nodes + files.links.len())));
    }
    if let Some(identities) = &manifest.identities {
        summary.per_type.push(("identity", found(identities.len())));
    }
    if let Some(identities) = &manifest.participant_identities {
        summary
            .per_type
            .push(("participantidentity", found(identities.len())));
    }
    Ok(summary)
}

fn count_tree(dir: &Path) -> Result<usize, Error> {
    let mut n = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            n += 1 + count_tree(&entry.path())?;
        } else if file_type.is_file() {
            n += 1;
        }
    }
    Ok(n)
}

/// Rows a run found already in the archive, as they were before it changed
/// them, so the end of the run can count which it actually changed.
#[derive(Default)]
pub(crate) struct Rerun {
    before: HashMap<i64, Vec<rusqlite::types::Value>>,
}

impl Rerun {
    pub(crate) fn before(
        &mut self,
        conn: &Connection,
        table: &str,
        columns: &str,
        id: i64,
    ) -> Result<(), Error> {
        if self.before.contains_key(&id) {
            return Ok(());
        }
        if let Some(row) = read_row(conn, table, columns, id)? {
            self.before.insert(id, row);
        }
        Ok(())
    }

    pub(crate) fn tally(
        &self,
        conn: &Connection,
        table: &str,
        columns: &str,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        for (id, was) in &self.before {
            match read_row(conn, table, columns, *id)? {
                Some(now) if now == *was => counts.skipped += 1,
                _ => counts.updated += 1,
            }
        }
        Ok(())
    }
}

fn read_row(
    conn: &Connection,
    table: &str,
    columns: &str,
    id: i64,
) -> Result<Option<Vec<rusqlite::types::Value>>, Error> {
    let mut stmt = conn.prepare(&format!("SELECT {columns} FROM {table} WHERE id = ?1"))?;
    let n = stmt.column_count();
    Ok(stmt
        .query_row(params![id], |r| {
            (0..n)
                .map(|i| r.get::<_, rusqlite::types::Value>(i))
                .collect()
        })
        .optional()?)
}

/// A file's modification time as RFC 3339, if it has a usable one.
pub(crate) fn mtime_rfc3339(meta: &fs::Metadata) -> Option<String> {
    let secs = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    if secs == 0 {
        return None;
    }
    OffsetDateTime::from_unix_timestamp(i64::try_from(secs).ok()?)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

pub(crate) fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
