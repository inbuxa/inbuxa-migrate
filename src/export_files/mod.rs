/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-files`: an archive written out as plain files in open formats,
//! so an account can leave for any server or app without this tool or any
//! vendor's store format. Mail is a Maildir++ tree, contacts are vCard,
//! calendars are iCalendar, filters are Sieve, files are files, and
//! `manifest.json` records what doesn't fit those formats. It never opens a
//! network connection.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::db::{blobs, init};
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::{Summary, TypeCounts};
use crate::types::ObjectType;

pub mod cards;
pub mod mail;
pub mod paths;

use paths::{Names, safe_component};

/// The layout this release writes; raised when `manifest.json` or the tree
/// changes in a way a reader has to know about.
pub const FORMAT_VERSION: u32 = 1;

pub struct ExportFilesConfig {
    pub archive: PathBuf,
    pub dir: PathBuf,
    pub objects: Option<Vec<ObjectType>>,
    pub dry_run: bool,
    pub logger: Logger,
}

pub fn run(config: ExportFilesConfig) -> Result<Summary, Error> {
    if !config.archive.is_file() {
        return Err(Error::Usage(format!(
            "no archive at {}",
            config.archive.display()
        )));
    }
    check_target(&config.dir)?;
    let conn = init::open(&config.archive)?;
    let wanted = |types: &[ObjectType]| match &config.objects {
        None => true,
        Some(list) => types.iter().any(|t| list.contains(t)),
    };
    let dry_run = config.dry_run;
    let logger = config.logger;
    let dir = config.dir.as_path();
    if !dry_run {
        make_private_dir(dir)?;
    }

    let mut summary = Summary::default();
    let mut manifest = json!({
        "format": "inbuxa-migrate-files",
        "formatVersion": FORMAT_VERSION,
        "createdBy": format!("inbuxa-migrate {}", crate::version::shown()),
        "created": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default(),
        "sources": read_sources(&conn)?,
    });

    if wanted(&[ObjectType::Mailbox, ObjectType::Email]) {
        let out = mail::write(&conn, &dir.join("mail"), dry_run, logger)?;
        summary.per_type.push(("mailbox", out.mailbox));
        summary.per_type.push(("email", out.email));
        manifest["mail"] = out.manifest;
    }
    if wanted(&[ObjectType::AddressBook, ObjectType::ContactCard]) {
        let out = cards::write_contacts(&conn, &dir.join("contacts"), dry_run, logger)?;
        summary.per_type.push(("addressbook", out.collection));
        summary.per_type.push(("contactcard", out.item));
        manifest["contacts"] = out.manifest;
    }
    if wanted(&[ObjectType::Calendar, ObjectType::CalendarEvent]) {
        let out = cards::write_calendars(&conn, &dir.join("calendars"), dry_run, logger)?;
        summary.per_type.push(("calendar", out.collection));
        summary.per_type.push(("calendarevent", out.item));
        manifest["calendars"] = out.manifest;
    }
    if wanted(&[ObjectType::SieveScript]) {
        let (counts, entries) = write_sieve(&conn, &dir.join("sieve"), dry_run, logger)?;
        summary.per_type.push(("sievescript", counts));
        manifest["sieve"] = entries;
    }
    if wanted(&[ObjectType::FileNode]) {
        let (counts, entries) = write_files(&conn, &dir.join("files"), dry_run, logger)?;
        summary.per_type.push(("filenode", counts));
        manifest["files"] = entries;
    }
    if wanted(&[ObjectType::Identity]) {
        let (counts, entries) = read_identities(&conn)?;
        summary.per_type.push(("identity", counts));
        manifest["identities"] = entries;
    }
    if wanted(&[ObjectType::ParticipantIdentity]) {
        let (counts, entries) = read_participant_identities(&conn)?;
        summary.per_type.push(("participantidentity", counts));
        manifest["participantIdentities"] = entries;
    }

    if dry_run {
        logger.warn(&format!(
            "dry run: nothing was written; {} would hold what is counted below",
            dir.display()
        ));
    } else {
        // Written last, so a tree with a manifest is a finished one.
        let text = serde_json::to_string_pretty(&manifest).map_err(std::io::Error::other)?;
        fs::write(dir.join("manifest.json"), text + "\n")?;
    }
    Ok(summary)
}

/// Only a new or empty directory: what is written must be exactly the
/// archive, with nothing of an earlier run mixed in.
fn check_target(dir: &Path) -> Result<(), Error> {
    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Error::Usage(format!(
                    "{} is not empty; give a new or empty directory",
                    dir.display()
                )));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Err(Error::Usage(format!(
            "{} is not a directory",
            dir.display()
        ))),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Mail, contacts and calendars are as private as the archive they came
/// from.
fn make_private_dir(dir: &Path) -> Result<(), Error> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn read_sources(conn: &Connection) -> Result<Value, Error> {
    let mut stmt = conn.prepare(
        "SELECT kind, session_url, account_id, account_name, username FROM sources ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "kind": r.get::<_, String>(0)?,
            "url": r.get::<_, String>(1)?,
            "accountId": r.get::<_, String>(2)?,
            "accountName": r.get::<_, Option<String>>(3)?,
            "username": r.get::<_, String>(4)?,
        }))
    })?;
    Ok(Value::Array(rows.collect::<Result<_, _>>()?))
}

fn write_sieve(
    conn: &Connection,
    root: &Path,
    dry_run: bool,
    logger: Logger,
) -> Result<(TypeCounts, Value), Error> {
    let mut counts = TypeCounts::default();
    let mut names = Names::default();
    let mut entries = Vec::new();
    let mut stmt =
        conn.prepare("SELECT id, name, is_active, blob_id FROM sieve_scripts ORDER BY name, id")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, i64>(2)? != 0,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (id, name, is_active, blob_id) = row?;
        let name = name.unwrap_or_else(|| format!("script-{id}"));
        let Some(bytes) = blobs::blob_bytes(conn, blob_id)? else {
            logger.warn(&format!(
                "Sieve script {name:?}: its content is missing from the archive"
            ));
            counts.failed += 1;
            continue;
        };
        let file = names.unique(&safe_component(&name), ".sieve");
        if !dry_run {
            fs::create_dir_all(root)?;
            fs::write(root.join(&file), &bytes)?;
        }
        entries.push(json!({
            "name": name,
            "file": format!("sieve/{file}"),
            "active": is_active,
        }));
        counts.created += 1;
    }
    Ok((counts, Value::Array(entries)))
}

struct FileNode {
    id: i64,
    parent_id: Option<i64>,
    node_type: String,
    blob_id: Option<i64>,
    target: Option<String>,
    name: String,
    media_type: Option<String>,
    modified: Option<String>,
    created: String,
}

/// The file tree as directories and files. Links are listed in the manifest
/// and never made: the source chose their targets, and one could point
/// anywhere on this machine.
fn write_files(
    conn: &Connection,
    root: &Path,
    dry_run: bool,
    logger: Logger,
) -> Result<(TypeCounts, Value), Error> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, node_type, blob_id, target, name, media_type, modified, created
         FROM file_nodes ORDER BY name, id",
    )?;
    let nodes: Vec<FileNode> = stmt
        .query_map([], |r| {
            Ok(FileNode {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                node_type: r.get(2)?,
                blob_id: r.get(3)?,
                target: r.get(4)?,
                name: r.get(5)?,
                media_type: r.get(6)?,
                modified: r.get(7)?,
                created: r.get(8)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut children: HashMap<Option<i64>, Vec<&FileNode>> = HashMap::new();
    let known: std::collections::HashSet<i64> = nodes.iter().map(|n| n.id).collect();
    for node in &nodes {
        let parent = node
            .parent_id
            .filter(|p| known.contains(p) && *p != node.id);
        children.entry(parent).or_default().push(node);
    }
    let mut state = FilesState {
        conn,
        children: &children,
        dry_run,
        logger,
        counts: TypeCounts::default(),
        links: Vec::new(),
        media_types: serde_json::Map::new(),
        visited: std::collections::HashSet::new(),
    };
    state.write_dir(None, root, "files")?;
    let manifest = json!({
        "path": "files",
        "mediaTypes": state.media_types,
        "links": state.links,
    });
    Ok((state.counts, manifest))
}

struct FilesState<'a> {
    conn: &'a Connection,
    children: &'a HashMap<Option<i64>, Vec<&'a FileNode>>,
    dry_run: bool,
    logger: Logger,
    counts: TypeCounts,
    links: Vec<Value>,
    media_types: serde_json::Map<String, Value>,
    visited: std::collections::HashSet<i64>,
}

impl FilesState<'_> {
    fn write_dir(&mut self, parent: Option<i64>, dir: &Path, rel: &str) -> Result<(), Error> {
        let Some(nodes) = self.children.get(&parent) else {
            return Ok(());
        };
        let mut names = Names::default();
        for node in nodes.iter().copied() {
            if !self.visited.insert(node.id) {
                continue;
            }
            let name = names.unique(&safe_component(&node.name), "");
            let path = dir.join(&name);
            let rel_path = format!("{rel}/{name}");
            match node.node_type.as_str() {
                "directory" => {
                    if !self.dry_run {
                        fs::create_dir_all(&path)?;
                    }
                    self.counts.created += 1;
                    self.write_dir(Some(node.id), &path, &rel_path)?;
                }
                "file" => {
                    let bytes = match node.blob_id {
                        Some(blob) => blobs::blob_bytes(self.conn, blob)?,
                        None => Some(Vec::new()),
                    };
                    let Some(bytes) = bytes else {
                        self.logger.warn(&format!(
                            "{rel_path}: its content is missing from the archive"
                        ));
                        self.counts.failed += 1;
                        continue;
                    };
                    if !self.dry_run {
                        fs::create_dir_all(dir)?;
                        fs::write(&path, &bytes)?;
                        if let Some(when) = node
                            .modified
                            .as_deref()
                            .or(Some(node.created.as_str()))
                            .and_then(|t| OffsetDateTime::parse(t, &Rfc3339).ok())
                            .filter(|t| t.unix_timestamp() > 0)
                        {
                            let _ = fs::File::options().write(true).open(&path).and_then(|f| {
                                f.set_modified(
                                    UNIX_EPOCH + Duration::from_secs(when.unix_timestamp() as u64),
                                )
                            });
                        }
                    }
                    if let Some(media_type) = &node.media_type {
                        self.media_types
                            .insert(rel_path.clone(), Value::String(media_type.clone()));
                    }
                    self.counts.created += 1;
                }
                _ => {
                    let target = node
                        .target
                        .as_deref()
                        .and_then(|t| serde_json::from_str::<Value>(t).ok());
                    self.links
                        .push(json!({ "path": rel_path, "target": target }));
                    self.counts.skipped += 1;
                }
            }
        }
        Ok(())
    }
}

fn read_identities(conn: &Connection) -> Result<(TypeCounts, Value), Error> {
    let mut stmt = conn.prepare(
        "SELECT name, email, reply_to, bcc, text_signature, html_signature
         FROM identities ORDER BY id",
    )?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            let json_column = |i: usize| -> rusqlite::Result<Value> {
                Ok(r.get::<_, Option<String>>(i)?
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or(Value::Null))
            };
            Ok(json!({
                "name": r.get::<_, String>(0)?,
                "email": r.get::<_, String>(1)?,
                "replyTo": json_column(2)?,
                "bcc": json_column(3)?,
                "textSignature": r.get::<_, String>(4)?,
                "htmlSignature": r.get::<_, String>(5)?,
            }))
        })?
        .collect::<Result<_, _>>()?;
    let counts = TypeCounts {
        created: rows.len() as u64,
        ..Default::default()
    };
    Ok((counts, Value::Array(rows)))
}

fn read_participant_identities(conn: &Connection) -> Result<(TypeCounts, Value), Error> {
    let mut stmt = conn.prepare(
        "SELECT name, calendar_address, is_default FROM participant_identities ORDER BY id",
    )?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "name": r.get::<_, String>(0)?,
                "calendarAddress": r.get::<_, String>(1)?,
                "default": r.get::<_, i64>(2)? != 0,
            }))
        })?
        .collect::<Result<_, _>>()?;
    let counts = TypeCounts {
        created: rows.len() as u64,
        ..Default::default()
    };
    Ok((counts, Value::Array(rows)))
}
