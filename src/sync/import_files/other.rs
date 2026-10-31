/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Sieve scripts with the one that was active, the file tree with its
//! media types and links, and the identities, which only the manifest
//! holds.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::db::{blobs, defaults, files_ids};
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::keys::{identity_key, participant_identity_key};
use crate::types::ObjectType;

use super::mail::row_exists;
use super::manifest::{Files, Identity, ParticipantIdentity, Script};
use super::{Rerun, mtime_rfc3339, paths};

const SCRIPT_COLUMNS: &str = "blob_id, is_active";
const NODE_COLUMNS: &str = "parent_id, node_type, blob_id, target, name, media_type, modified";
const IDENTITY_COLUMNS: &str = "name, email, reply_to, bcc, text_signature, html_signature";
const PARTICIPANT_COLUMNS: &str = "name, calendar_address, is_default";

pub fn import_sieve(
    conn: &mut Connection,
    root: &Path,
    source_id: i64,
    scripts: &[Script],
    logger: Logger,
) -> Result<TypeCounts, Error> {
    let mut counts = TypeCounts::default();
    let mut rerun = Rerun::default();
    let tx = conn.transaction()?;
    let mut active: Option<&str> = None;
    for script in scripts {
        let path = match paths::resolve(root, &script.file, "sieve") {
            Ok(path) if paths::is_real_file(&path) => path,
            Ok(_) => {
                logger.warn(&format!("{}: not a file", script.file));
                counts.failed += 1;
                continue;
            }
            Err(reason) => {
                logger.warn(&format!("Sieve script {:?}: {reason}", script.name));
                counts.failed += 1;
                continue;
            }
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                logger.warn(&format!("{}: {e}", script.file));
                counts.failed += 1;
                continue;
            }
        };
        let is_active = script.active && active.is_none();
        if script.active && !is_active {
            logger.warn(&format!(
                "Sieve script {:?}: the manifest marks more than one script active; only \
                 {:?} is",
                script.name,
                active.unwrap_or_default()
            ));
        }
        if is_active {
            active = Some(&script.name);
        }
        let blob = blobs::intern_blob(&tx, &bytes)?;
        let existing =
            match files_ids::local_for(&tx, source_id, files_ids::SIEVE_SCRIPT, &script.name)? {
                Some(id) if row_exists(&tx, "sieve_scripts", id)? => Some(id),
                // Names are unique in the archive.
                _ => tx
                    .query_row(
                        "SELECT id FROM sieve_scripts WHERE name = ?1",
                        params![script.name],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?,
            };
        if let Some(id) = existing {
            rerun.before(&tx, "sieve_scripts", SCRIPT_COLUMNS, id)?;
        }
        if is_active {
            // One script runs at a time.
            tx.execute(
                "UPDATE sieve_scripts SET is_active = 0 WHERE is_active = 1 AND id IS NOT ?1",
                params![existing],
            )?;
        }
        let id = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE sieve_scripts SET blob_id = ?1, is_active = ?2 WHERE id = ?3",
                    params![blob, is_active as i64, id],
                )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO sieve_scripts (name, is_active, blob_id) VALUES (?1, ?2, ?3)",
                    params![script.name, is_active as i64, blob],
                )?;
                counts.created += 1;
                counts.fetched += 1;
                tx.last_insert_rowid()
            }
        };
        files_ids::record(&tx, source_id, files_ids::SIEVE_SCRIPT, &script.name, id)?;
    }
    rerun.tally(&tx, "sieve_scripts", SCRIPT_COLUMNS, &mut counts)?;
    tx.commit()?;
    Ok(counts)
}

/// The file tree, walked without following links, then the links the
/// manifest lists, made as archive nodes and never on disk.
pub fn import_files(
    conn: &mut Connection,
    root: &Path,
    source_id: i64,
    files: &Files,
    created: &str,
    logger: Logger,
) -> Result<TypeCounts, Error> {
    let mut counts = TypeCounts::default();
    let part = match paths::segments(&files.path).as_deref() {
        Ok([part]) => (*part).to_owned(),
        _ => {
            return Err(Error::Usage(format!(
                "manifest.json: files path {:?} is not a directory at the export's top",
                files.path
            )));
        }
    };
    let tx = conn.transaction()?;
    let mut tree = FileTree {
        conn: &tx,
        source_id,
        files,
        dirs: HashMap::new(),
        rerun: Rerun::default(),
        created,
        logger,
    };
    // A writer with no files to write may not make the directory at all.
    let base = root.join(&part);
    match fs::symlink_metadata(&base) {
        Ok(meta) if meta.is_dir() => tree.walk(&base, &part, None, &mut counts)?,
        Ok(_) => {
            logger.warn(&format!("{part}/ is not a directory and is not read"));
            counts.failed += 1;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Io(e)),
    }
    for link in &files.links {
        if let Err(reason) = tree.link(&part, &link.path, &link.target, &mut counts) {
            logger.warn(&format!("link {:?}: {reason}", link.path));
            counts.failed += 1;
        }
    }
    let rerun = std::mem::take(&mut tree.rerun);
    rerun.tally(&tx, "file_nodes", NODE_COLUMNS, &mut counts)?;
    tx.commit()?;
    Ok(counts)
}

struct FileTree<'a> {
    conn: &'a Connection,
    source_id: i64,
    files: &'a Files,
    /// Each directory made, by its path in the export.
    dirs: HashMap<String, i64>,
    rerun: Rerun,
    /// When the export was written: the time for what has none of its own.
    created: &'a str,
    logger: Logger,
}

struct Node<'a> {
    parent: Option<i64>,
    node_type: &'a str,
    blob: Option<i64>,
    target: Option<String>,
    name: &'a str,
    media_type: Option<&'a str>,
    created: String,
    modified: Option<String>,
}

impl FileTree<'_> {
    fn walk(
        &mut self,
        dir: &Path,
        rel: &str,
        parent: Option<i64>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let mut entries: Vec<fs::DirEntry> =
            match fs::read_dir(dir).and_then(|d| d.collect::<Result<_, _>>()) {
                Ok(entries) => entries,
                Err(e) => {
                    self.logger.warn(&format!("{rel}/: {e}"));
                    counts.failed += 1;
                    return Ok(());
                }
            };
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                self.logger.warn(&format!(
                    "{rel}/{}: the name is not UTF-8 and is skipped",
                    entry.file_name().to_string_lossy()
                ));
                counts.failed += 1;
                continue;
            };
            let rel_path = format!("{rel}/{name}");
            let Ok(file_type) = entry.file_type() else {
                self.logger
                    .warn(&format!("{rel_path}: its type can't be read"));
                counts.failed += 1;
                continue;
            };
            if file_type.is_symlink() {
                self.logger.warn(&format!(
                    "{rel_path} is a symbolic link and is not followed; an export's links \
                     are listed in its manifest"
                ));
                counts.skipped += 1;
                continue;
            }
            if file_type.is_dir() {
                // A writer sets the times of files, not of directories, so a
                // directory's time is only when the export was written.
                let id = self.node(
                    &rel_path,
                    Node {
                        parent,
                        node_type: "directory",
                        blob: None,
                        target: None,
                        name: &name,
                        media_type: None,
                        created: self.created.to_owned(),
                        modified: None,
                    },
                    counts,
                )?;
                self.dirs.insert(rel_path.clone(), id);
                self.walk(&entry.path(), &rel_path, Some(id), counts)?;
            } else if file_type.is_file() {
                let modified = entry.metadata().ok().and_then(|m| mtime_rfc3339(&m));
                let created = modified.clone().unwrap_or_else(|| self.created.to_owned());
                let bytes = match fs::read(entry.path()) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        self.logger.warn(&format!("{rel_path}: {e}"));
                        counts.failed += 1;
                        continue;
                    }
                };
                let blob = blobs::intern_blob(self.conn, &bytes)?;
                let media_type = self.files.media_types.get(&rel_path).map(String::as_str);
                self.node(
                    &rel_path,
                    Node {
                        parent,
                        node_type: "file",
                        blob: Some(blob),
                        target: None,
                        name: &name,
                        media_type,
                        created,
                        modified,
                    },
                    counts,
                )?;
            } else {
                self.logger
                    .warn(&format!("{rel_path} is not a file or directory; skipped"));
                counts.skipped += 1;
            }
        }
        Ok(())
    }

    fn link(
        &mut self,
        part: &str,
        rel: &str,
        target: &Value,
        counts: &mut TypeCounts,
    ) -> Result<(), String> {
        let segments = paths::segments(rel)?;
        let [first, parents @ .., name] = segments.as_slice() else {
            return Err("names no file".to_owned());
        };
        if *first != part {
            return Err(format!("is outside {part}/"));
        }
        let parent = if parents.is_empty() {
            None
        } else {
            let parent_rel = format!("{part}/{}", parents.join("/"));
            match self.dirs.get(&parent_rel) {
                Some(id) => Some(*id),
                None => return Err(format!("its directory {parent_rel} is not in the export")),
            }
        };
        let target = (!target.is_null()).then(|| target.to_string());
        self.node(
            rel,
            Node {
                parent,
                node_type: "symlink",
                blob: None,
                target,
                name,
                media_type: None,
                created: self.created.to_owned(),
                modified: None,
            },
            counts,
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    fn node(&mut self, key: &str, node: Node<'_>, counts: &mut TypeCounts) -> Result<i64, Error> {
        let existing =
            match files_ids::local_for(self.conn, self.source_id, files_ids::FILE_NODE, key)? {
                Some(id) if row_exists(self.conn, "file_nodes", id)? => Some(id),
                // Names are unique within a directory.
                _ => self
                    .conn
                    .query_row(
                        "SELECT id FROM file_nodes WHERE parent_id IS ?1 AND name = ?2",
                        params![node.parent, node.name],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?,
            };
        let id = match existing {
            Some(id) => {
                self.rerun
                    .before(self.conn, "file_nodes", NODE_COLUMNS, id)?;
                self.conn.execute(
                    "UPDATE file_nodes SET parent_id = ?1, node_type = ?2, blob_id = ?3,
                         target = ?4, name = ?5, media_type = ?6, modified = ?7
                     WHERE id = ?8",
                    params![
                        node.parent,
                        node.node_type,
                        node.blob,
                        node.target,
                        node.name,
                        node.media_type,
                        node.modified,
                        id
                    ],
                )?;
                id
            }
            None => {
                self.conn.execute(
                    "INSERT INTO file_nodes
                         (parent_id, node_type, blob_id, target, name, media_type, created, modified)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        node.parent,
                        node.node_type,
                        node.blob,
                        node.target,
                        node.name,
                        node.media_type,
                        node.created,
                        node.modified
                    ],
                )?;
                counts.created += 1;
                counts.fetched += 1;
                self.conn.last_insert_rowid()
            }
        };
        files_ids::record(self.conn, self.source_id, files_ids::FILE_NODE, key, id)?;
        Ok(id)
    }
}

pub fn import_identities(
    conn: &mut Connection,
    source_id: i64,
    identities: &[Identity],
) -> Result<TypeCounts, Error> {
    let mut counts = TypeCounts::default();
    let mut rerun = Rerun::default();
    let tx = conn.transaction()?;
    for identity in identities {
        let key = hex(&identity_key(&identity.name, &identity.email));
        let addresses = |v: &Value| (!v.is_null()).then(|| v.to_string());
        let (reply_to, bcc) = (addresses(&identity.reply_to), addresses(&identity.bcc));
        let id = match files_ids::local_for(&tx, source_id, files_ids::IDENTITY, &key)? {
            Some(id) if row_exists(&tx, "identities", id)? => {
                rerun.before(&tx, "identities", IDENTITY_COLUMNS, id)?;
                tx.execute(
                    "UPDATE identities SET name = ?1, email = ?2, reply_to = ?3, bcc = ?4,
                         text_signature = ?5, html_signature = ?6
                     WHERE id = ?7",
                    params![
                        identity.name,
                        identity.email,
                        reply_to,
                        bcc,
                        identity.text_signature,
                        identity.html_signature,
                        id
                    ],
                )?;
                id
            }
            _ => {
                tx.execute(
                    "INSERT INTO identities
                         (name, email, reply_to, bcc, text_signature, html_signature)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        identity.name,
                        identity.email,
                        reply_to,
                        bcc,
                        identity.text_signature,
                        identity.html_signature
                    ],
                )?;
                counts.created += 1;
                counts.fetched += 1;
                tx.last_insert_rowid()
            }
        };
        files_ids::record(&tx, source_id, files_ids::IDENTITY, &key, id)?;
    }
    rerun.tally(&tx, "identities", IDENTITY_COLUMNS, &mut counts)?;
    tx.commit()?;
    Ok(counts)
}

pub fn import_participant_identities(
    conn: &mut Connection,
    source_id: i64,
    identities: &[ParticipantIdentity],
    logger: Logger,
) -> Result<TypeCounts, Error> {
    let mut counts = TypeCounts::default();
    let mut rerun = Rerun::default();
    let tx = conn.transaction()?;
    for identity in identities {
        let key = hex(&participant_identity_key(
            &identity.calendar_address,
            &identity.name,
        ));
        let existing =
            match files_ids::local_for(&tx, source_id, files_ids::PARTICIPANT_IDENTITY, &key)? {
                Some(id) if row_exists(&tx, "participant_identities", id)? => Some(id),
                _ => None,
            };
        let is_default = defaults::unique_default(
            &tx,
            ObjectType::ParticipantIdentity,
            identity.default,
            existing,
        )?;
        if identity.default && !is_default {
            logger.warn(&format!(
                "calendar identity {:?}: another is already the default, so this one is not",
                identity.calendar_address
            ));
        }
        let id = match existing {
            Some(id) => {
                rerun.before(&tx, "participant_identities", PARTICIPANT_COLUMNS, id)?;
                tx.execute(
                    "UPDATE participant_identities SET name = ?1, calendar_address = ?2,
                         is_default = ?3
                     WHERE id = ?4",
                    params![
                        identity.name,
                        identity.calendar_address,
                        is_default as i64,
                        id
                    ],
                )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO participant_identities (name, calendar_address, is_default)
                     VALUES (?1, ?2, ?3)",
                    params![identity.name, identity.calendar_address, is_default as i64],
                )?;
                counts.created += 1;
                counts.fetched += 1;
                tx.last_insert_rowid()
            }
        };
        files_ids::record(&tx, source_id, files_ids::PARTICIPANT_IDENTITY, &key, id)?;
    }
    rerun.tally(
        &tx,
        "participant_identities",
        PARTICIPANT_COLUMNS,
        &mut counts,
    )?;
    tx.commit()?;
    Ok(counts)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
