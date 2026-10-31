/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! The mail part: a Maildir++ tree, read with the manifest's real folder
//! names, hierarchy, roles and subscriptions in place of what the directory
//! names could hold. A message kept in several folders is written to each;
//! copies with the same content become one message in several mailboxes,
//! as the archive holds them. File names are only read for their Maildir
//! flags: the specification says nothing else in them can be relied on.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{blobs, cards, files_ids, roles};
use crate::error::Error;
use crate::logging::{LEVEL_PROGRESS, Logger};
use crate::sync::TypeCounts;
use crate::sync::emailmeta::email_meta_from_blob;
use crate::sync::import_maildir::keywords::{
    custom_keywords_from_filename, flags_from_filename, read_keyword_names, translate_flags,
    with_custom,
};
use crate::sync::keys::index_to_json;

use super::manifest::{Folder, Mail};
use super::{Rerun, mtime_rfc3339, now_rfc3339, paths};

pub struct Outcome {
    pub mailbox: TypeCounts,
    pub email: TypeCounts,
}

pub fn import(
    conn: &mut Connection,
    root: &Path,
    source_id: i64,
    mail: &Mail,
    logger: Logger,
) -> Result<Outcome, Error> {
    let mut out = Outcome {
        mailbox: TypeCounts::default(),
        email: TypeCounts::default(),
    };
    if paths::segments(&mail.path).map(|s| s.len()) != Ok(1) {
        return Err(Error::Usage(format!(
            "manifest.json: mail path {:?} is not a directory at the export's top",
            mail.path
        )));
    }

    // Parents before children, so each folder finds its parent made.
    let mut folders: Vec<&Folder> = mail.folders.iter().collect();
    folders.sort_by_key(|f| real_path(f).len());
    let mut planned: Vec<(&Folder, i64)> = Vec::new();
    {
        let tx = conn.transaction()?;
        let mut tree = Tree {
            conn: &tx,
            source_id,
            made: HashMap::new(),
            rerun: Rerun::default(),
            logger,
        };
        for folder in folders {
            match tree.ensure(&real_path(folder), Some(folder), &mut out.mailbox) {
                Ok(id) => planned.push((folder, id)),
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    logger.warn(&format!("folder {:?}: {e}", folder.name));
                    out.mailbox.failed += 1;
                }
            }
        }
        tree.rerun
            .tally(&tx, "mailboxes", MAILBOX_COLUMNS, &mut out.mailbox)?;
        tx.commit()?;
    }

    let mut messages = Messages {
        source_id,
        seen: HashMap::new(),
        rerun: Rerun::default(),
        copies: 0,
        logger,
    };
    for (folder, mailbox) in &planned {
        let dir = match paths::resolve(root, &folder.dir, &mail.path) {
            Ok(dir) if paths::is_real_dir(&dir) => dir,
            Ok(_) => {
                logger.warn(&format!(
                    "folder {:?}: {} is not a directory",
                    folder.name, folder.dir
                ));
                out.email.failed += 1;
                continue;
            }
            Err(reason) => {
                logger.warn(&format!("folder {:?}: {reason}", folder.name));
                out.email.failed += 1;
                continue;
            }
        };
        let tx = conn.transaction()?;
        let found = match messages.folder(&tx, &dir, *mailbox, &folder.name, &mut out.email) {
            Ok(found) => found,
            Err(e) if e.aborts_run() => return Err(e),
            Err(e) => {
                logger.warn(&format!("folder {:?}: {e}", folder.name));
                out.email.failed += 1;
                0
            }
        };
        tx.commit()?;
        if let Some(listed) = folder.messages
            && listed != found
        {
            logger.warn(&format!(
                "folder {:?}: the manifest lists {listed} messages and {found} were found",
                folder.name
            ));
        }
        if let Some(dropped) = folder.keywords_dropped.filter(|n| *n > 0) {
            logger.warn(&format!(
                "folder {:?}: the export had no room for {dropped} keywords on its messages \
                 (Maildir holds 26 per folder), so those are not on them",
                folder.name
            ));
        }
    }
    {
        let tx = conn.transaction()?;
        messages
            .rerun
            .tally(&tx, "emails", EMAIL_COLUMNS, &mut out.email)?;
        tx.commit()?;
    }
    if messages.copies > 0 && logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "mail: {} copies of messages kept in more than one folder were read as one \
             message in each of those folders",
            messages.copies
        );
    }
    Ok(out)
}

const MAILBOX_COLUMNS: &str = "name, parent_id, role, is_subscribed";
const EMAIL_COLUMNS: &str = "mailbox_ids, keywords";

/// The folder's real names from the top down; the manifest may leave the
/// path out of a top-level folder.
fn real_path(folder: &Folder) -> Vec<String> {
    if folder.path.is_empty() {
        vec![folder.name.clone()]
    } else {
        folder.path.clone()
    }
}

struct Tree<'a> {
    conn: &'a Connection,
    source_id: i64,
    made: HashMap<Vec<String>, i64>,
    rerun: Rerun,
    logger: Logger,
}

impl Tree<'_> {
    /// The mailbox for `path`, made or brought up to date. A parent the
    /// manifest doesn't list is made too, so its children have one.
    fn ensure(
        &mut self,
        path: &[String],
        folder: Option<&Folder>,
        counts: &mut TypeCounts,
    ) -> Result<i64, Error> {
        if folder.is_none()
            && let Some(id) = self.made.get(path)
        {
            return Ok(*id);
        }
        let Some((name, parents)) = path.split_last() else {
            return Err(Error::Partial("a folder with no name".to_owned()));
        };
        let parent = if parents.is_empty() {
            None
        } else {
            Some(self.ensure(parents, None, counts)?)
        };
        let key = serde_json::to_string(path).unwrap_or_default();
        let existing =
            match files_ids::local_for(self.conn, self.source_id, files_ids::MAILBOX, &key)? {
                Some(id) if row_exists(self.conn, "mailboxes", id)? => Some(id),
                _ => self
                    .conn
                    .query_row(
                        "SELECT id FROM mailboxes WHERE parent_id IS ?1 AND name = ?2",
                        params![parent, name],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?,
            };
        let Some(folder) = folder else {
            // A parent only: one the manifest doesn't list.
            if let Some(id) = existing {
                self.made.insert(path.to_vec(), id);
                files_ids::record(self.conn, self.source_id, files_ids::MAILBOX, &key, id)?;
                return Ok(id);
            }
            if self.logger.enabled(LEVEL_PROGRESS) {
                eprintln!(
                    "folder {:?}: not in the manifest; made so its folders have a parent",
                    path.join("/")
                );
            }
            self.conn.execute(
                "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
                 VALUES (?1, ?2, NULL, 0, 1)",
                params![name, parent],
            )?;
            let id = self.conn.last_insert_rowid();
            files_ids::record(self.conn, self.source_id, files_ids::MAILBOX, &key, id)?;
            self.made.insert(path.to_vec(), id);
            counts.created += 1;
            counts.fetched += 1;
            return Ok(id);
        };

        let wanted_role = folder
            .role
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_ascii_lowercase);
        let role = roles::unique_role(self.conn, wanted_role.as_deref(), existing)?;
        if let (Some(wanted), None) = (&wanted_role, &role) {
            self.logger.warn(&format!(
                "folder {:?}: another folder already has the role {wanted:?}, so this one \
                 is left without it",
                path.join("/")
            ));
        }
        let subscribed = folder.subscribed as i64;
        let id = match existing {
            Some(id) => {
                self.rerun
                    .before(self.conn, "mailboxes", MAILBOX_COLUMNS, id)?;
                self.conn.execute(
                    "UPDATE mailboxes SET name = ?1, parent_id = ?2, role = ?3, is_subscribed = ?4
                     WHERE id = ?5",
                    params![name, parent, role, subscribed, id],
                )?;
                id
            }
            None => {
                self.conn.execute(
                    "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
                     VALUES (?1, ?2, ?3, 0, ?4)",
                    params![name, parent, role, subscribed],
                )?;
                counts.created += 1;
                counts.fetched += 1;
                self.conn.last_insert_rowid()
            }
        };
        files_ids::record(self.conn, self.source_id, files_ids::MAILBOX, &key, id)?;
        self.made.insert(path.to_vec(), id);
        Ok(id)
    }
}

struct Messages {
    source_id: i64,
    /// Each message read this run, by content hash.
    seen: HashMap<String, i64>,
    rerun: Rerun,
    copies: u64,
    logger: Logger,
}

impl Messages {
    /// Every message in one folder's `cur/` and `new/`; returns how many
    /// were found.
    fn folder(
        &mut self,
        conn: &Connection,
        dir: &Path,
        mailbox: i64,
        folder_name: &str,
        counts: &mut TypeCounts,
    ) -> Result<u64, Error> {
        let keyword_file = dir.join("dovecot-keywords");
        let names = if paths::is_real_file(&keyword_file) {
            read_keyword_names(dir)
        } else {
            if keyword_file.symlink_metadata().is_ok() {
                self.logger.warn(&format!(
                    "folder {folder_name:?}: its dovecot-keywords is not a plain file and \
                     is not read"
                ));
            }
            vec![None; 26]
        };
        let mut found = 0;
        for sub in ["cur", "new"] {
            let sub_dir = dir.join(sub);
            if !paths::is_real_dir(&sub_dir) {
                if sub_dir.symlink_metadata().is_ok() {
                    self.logger.warn(&format!(
                        "folder {folder_name:?}: {sub}/ is not a directory and is not read"
                    ));
                    counts.failed += 1;
                }
                continue;
            }
            let mut entries: Vec<fs::DirEntry> = fs::read_dir(&sub_dir)
                .and_then(|d| d.collect::<Result<_, _>>())
                .map_err(|e| Error::Partial(format!("reading {sub}/: {e}")))?;
            entries.sort_by_key(fs::DirEntry::file_name);
            for entry in entries {
                let file_name = entry.file_name().to_string_lossy().into_owned();
                if file_name.starts_with('.') {
                    continue;
                }
                let file_type = entry
                    .file_type()
                    .map_err(|e| Error::Partial(format!("{sub}/{file_name}: {e}")))?;
                if file_type.is_symlink() {
                    self.logger.warn(&format!(
                        "folder {folder_name:?}: {sub}/{file_name} is a symbolic link and is \
                         not followed"
                    ));
                    counts.skipped += 1;
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                found += 1;
                if let Err(e) = self.message(conn, &entry, &file_name, &names, mailbox, counts) {
                    if e.aborts_run() {
                        return Err(e);
                    }
                    self.logger
                        .warn(&format!("folder {folder_name:?}: {sub}/{file_name}: {e}"));
                    counts.failed += 1;
                }
            }
        }
        Ok(found)
    }

    fn message(
        &mut self,
        conn: &Connection,
        entry: &fs::DirEntry,
        file_name: &str,
        keyword_names: &[Option<String>],
        mailbox: i64,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let bytes = fs::read(entry.path()).map_err(|e| Error::Partial(e.to_string()))?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let keywords: BTreeSet<String> = with_custom(
            translate_flags(&flags_from_filename(file_name), true),
            &custom_keywords_from_filename(file_name, keyword_names),
        )
        .keywords
        .into_iter()
        .collect();

        if let Some(&id) = self.seen.get(&hash) {
            // Another copy of a message already read this run.
            let (mailbox_ids, current): (String, String) = conn.query_row(
                "SELECT mailbox_ids, keywords FROM emails WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let mut merged: BTreeSet<String> = serde_json::from_str::<Vec<String>>(&current)
                .unwrap_or_default()
                .into_iter()
                .collect();
            merged.extend(keywords);
            conn.execute(
                "UPDATE emails SET mailbox_ids = ?1, keywords = ?2 WHERE id = ?3",
                params![
                    cards::with_book(&mailbox_ids, mailbox),
                    keywords_json(&merged),
                    id
                ],
            )?;
            self.copies += 1;
            return Ok(());
        }

        let mailbox_ids = format!("[{mailbox}]");
        if let Some(id) = files_ids::local_for(conn, self.source_id, files_ids::EMAIL, &hash)?
            && row_exists(conn, "emails", id)?
        {
            // Read on an earlier run: now in the folders it is in now.
            self.rerun.before(conn, "emails", EMAIL_COLUMNS, id)?;
            conn.execute(
                "UPDATE emails SET mailbox_ids = ?1, keywords = ?2 WHERE id = ?3",
                params![mailbox_ids, keywords_json(&keywords), id],
            )?;
            self.seen.insert(hash, id);
            return Ok(());
        }

        let (index, date_header) = email_meta_from_blob(&bytes);
        // The file's modification time is when the message arrived.
        let received_at = entry
            .metadata()
            .ok()
            .and_then(|m| mtime_rfc3339(&m))
            .or(date_header)
            .unwrap_or_else(now_rfc3339);
        let blob = blobs::intern_blob(conn, &bytes)?;
        conn.execute(
            "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords, message_match)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob,
                received_at,
                mailbox_ids,
                keywords_json(&keywords),
                index_to_json(&index)
            ],
        )?;
        let id = conn.last_insert_rowid();
        files_ids::record(conn, self.source_id, files_ids::EMAIL, &hash, id)?;
        self.seen.insert(hash, id);
        counts.created += 1;
        counts.fetched += 1;
        Ok(())
    }
}

fn keywords_json(keywords: &BTreeSet<String>) -> String {
    serde_json::to_string(keywords).unwrap_or_else(|_| "[]".to_owned())
}

pub(super) fn row_exists(conn: &Connection, table: &str, id: i64) -> Result<bool, Error> {
    Ok(conn
        .prepare(&format!("SELECT 1 FROM {table} WHERE id = ?1"))?
        .exists(params![id])?)
}
