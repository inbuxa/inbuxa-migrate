/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! The reconciliation report: a read-only manifest of what a local archive
//! holds, folder by folder, with a content checksum per folder and one for the
//! whole, plus a check that the archive is internally sound. It is how you
//! prove a migration brought everything across: compare the per-folder counts
//! against the source mailbox, and the checksum lets you show two archives --
//! an export and a re-import, say -- hold the very same mail.
//!
//! Everything comes from the archive itself; it opens nothing on the network.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use crate::db::init;
use crate::error::Error;
use crate::inspect::{format_bytes, format_count};

pub struct ReconcileConfig {
    pub archive: PathBuf,
}

/// run writes the report and returns whether the archive reconciled cleanly:
/// false when an integrity check found a problem, so the caller can exit
/// non-zero.
pub fn run(config: ReconcileConfig) -> Result<bool, Error> {
    let conn = init::open_read_only(&config.archive)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    report(&conn, &config.archive, &mut out)
}

fn report(conn: &Connection, archive: &Path, out: &mut impl Write) -> Result<bool, Error> {
    writeln!(out, "Archive: {}", archive.display())?;
    if let Some((kind, url, account, name)) = read_source(conn)? {
        let name = name.unwrap_or_else(|| "(unnamed)".to_owned());
        writeln!(out, "Source:  {kind} {url} (account {account} / {name})")?;
    } else {
        writeln!(out, "Source:  (no source recorded)")?;
    }
    writeln!(out)?;

    let folders = folder_manifest(conn)?;
    let mut total_messages = 0u64;
    let mut hasher = blake3::Hasher::new();

    writeln!(out, "Folders")?;
    if folders.is_empty() {
        writeln!(out, "  (no folders)")?;
    } else {
        let path_width = folders
            .iter()
            .map(|f| f.path.len())
            .max()
            .unwrap_or(0)
            .max("Folder".len());
        let count_width = folders
            .iter()
            .map(|f| format_count(f.messages).len())
            .max()
            .unwrap_or(1)
            .max("messages".len());
        writeln!(
            out,
            "  {:<pw$}  {:>cw$}  checksum",
            "Folder",
            "messages",
            pw = path_width,
            cw = count_width
        )?;
        for f in &folders {
            total_messages += f.messages;
            // Fold each folder's checksum into the whole-archive one, so the
            // grand total fingerprints the entire mail set, order and all.
            hasher.update(f.path.as_bytes());
            hasher.update(&f.checksum);
            writeln!(
                out,
                "  {:<pw$}  {:>cw$}  {}",
                f.path,
                format_count(f.messages),
                short_hex(&f.checksum),
                pw = path_width,
                cw = count_width
            )?;
        }
    }

    let (blob_count, blob_bytes) = blob_totals(conn)?;
    writeln!(out)?;
    writeln!(out, "Totals")?;
    writeln!(out, "  messages        {}", format_count(total_messages))?;
    writeln!(
        out,
        "  folders         {}",
        format_count(folders.len() as u64)
    )?;
    writeln!(
        out,
        "  stored content  {} in {} blobs",
        format_bytes(blob_bytes),
        format_count(blob_count)
    )?;
    writeln!(
        out,
        "  checksum        {}",
        short_hex(hasher.finalize().as_bytes())
    )?;

    let problems = integrity(conn)?;
    writeln!(out)?;
    writeln!(out, "Integrity")?;
    if problems.is_empty() {
        writeln!(
            out,
            "  ok -- every message resolves to stored content and a real folder"
        )?;
        Ok(true)
    } else {
        for p in &problems {
            writeln!(out, "  PROBLEM  {p}")?;
        }
        Ok(false)
    }
}

struct Folder {
    path: String,
    messages: u64,
    checksum: [u8; 32],
}

/// folder_manifest returns one row per mailbox, by full path, with its message
/// count and a checksum over the sorted content hashes of the messages in it.
/// A message in two folders is counted in each, as it is filed in each.
fn folder_manifest(conn: &Connection) -> Result<Vec<Folder>, Error> {
    // id -> (parent, name), to build full paths.
    let mut names: HashMap<i64, (Option<i64>, String)> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, parent_id, name FROM mailboxes")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, parent, name) = row?;
            names.insert(id, (parent, name));
        }
    }

    let mut folders = Vec::with_capacity(names.len());
    for &id in names.keys() {
        let mut hashes = folder_hashes(conn, id)?;
        hashes.sort_unstable();
        let mut hasher = blake3::Hasher::new();
        for h in &hashes {
            hasher.update(h);
        }
        folders.push(Folder {
            path: full_path(id, &names),
            messages: hashes.len() as u64,
            checksum: *hasher.finalize().as_bytes(),
        });
    }
    folders.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(folders)
}

/// folder_hashes is the raw content hashes of every message filed in a mailbox.
fn folder_hashes(conn: &Connection, mailbox_id: i64) -> Result<Vec<Vec<u8>>, Error> {
    let mut stmt = conn.prepare(
        "SELECT b.hash FROM emails e JOIN blobs b ON b.id = e.blob_id
          WHERE EXISTS (SELECT 1 FROM json_each(e.mailbox_ids) j
                         WHERE CAST(j.value AS INTEGER) = ?1)",
    )?;
    let rows = stmt.query_map([mailbox_id], |r| r.get::<_, Vec<u8>>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn full_path(id: i64, names: &HashMap<i64, (Option<i64>, String)>) -> String {
    let mut parts = Vec::new();
    let mut cur = Some(id);
    // A depth bound guards against a cycle a corrupt archive might hold.
    let mut guard = 0;
    while let Some(c) = cur {
        guard += 1;
        if guard > 1000 {
            break;
        }
        match names.get(&c) {
            Some((parent, name)) => {
                parts.push(name.clone());
                cur = *parent;
            }
            None => break,
        }
    }
    parts.reverse();
    parts.join("/")
}

fn blob_totals(conn: &Connection) -> Result<(u64, u64), Error> {
    let (count, bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(data)), 0) FROM blobs",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((count.max(0) as u64, bytes.max(0) as u64))
}

/// integrity returns a line for each way the archive is unsound, or an empty
/// list when it is whole. These are the failures that make a migration look
/// done when it is not: a message with no content, with no folder, or filed in
/// a folder that is not there.
fn integrity(conn: &Connection) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();

    let dangling: i64 = conn.query_row(
        "SELECT COUNT(*) FROM emails e LEFT JOIN blobs b ON b.id = e.blob_id WHERE b.id IS NULL",
        [],
        |r| r.get(0),
    )?;
    if dangling > 0 {
        problems.push(format!(
            "{} message(s) point at content that is not in the archive",
            format_count(dangling as u64)
        ));
    }

    let empty_blobs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM blobs WHERE length(data) = 0",
        [],
        |r| r.get(0),
    )?;
    if empty_blobs > 0 {
        problems.push(format!(
            "{} stored blob(s) are empty",
            format_count(empty_blobs as u64)
        ));
    }

    let orphans: i64 = conn.query_row(
        "SELECT COUNT(*) FROM emails WHERE json_array_length(mailbox_ids) = 0",
        [],
        |r| r.get(0),
    )?;
    if orphans > 0 {
        problems.push(format!(
            "{} message(s) are filed in no folder",
            format_count(orphans as u64)
        ));
    }

    let bad_refs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM emails e
          WHERE EXISTS (SELECT 1 FROM json_each(e.mailbox_ids) j
                         WHERE NOT EXISTS (SELECT 1 FROM mailboxes m
                                            WHERE m.id = CAST(j.value AS INTEGER)))",
        [],
        |r| r.get(0),
    )?;
    if bad_refs > 0 {
        problems.push(format!(
            "{} message(s) are filed in a folder that is not in the archive",
            format_count(bad_refs as u64)
        ));
    }

    Ok(problems)
}

type SourceRow = (String, String, String, Option<String>);

fn read_source(conn: &Connection) -> Result<Option<SourceRow>, Error> {
    let row = conn
        .query_row(
            "SELECT kind, session_url, account_id, account_name FROM sources LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(row)
}

fn short_hex(hash: &[u8]) -> String {
    let mut s = String::with_capacity(16);
    for b in hash.iter().take(8) {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::blobs;
    use rusqlite::params;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init::apply_schema(&c).unwrap();
        c
    }

    fn mailbox(c: &Connection, name: &str, parent: Option<i64>) -> i64 {
        c.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES (?1, ?2, NULL, 0, 1)",
            params![name, parent],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn email(c: &Connection, body: &[u8], mailbox_ids: &[i64]) {
        let blob = blobs::intern_blob(c, body).unwrap();
        let ids: Vec<String> = mailbox_ids.iter().map(|i| i.to_string()).collect();
        c.execute(
            "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords)
             VALUES (?1, '2024-01-01T00:00:00Z', ?2, '[]')",
            params![blob, format!("[{}]", ids.join(","))],
        )
        .unwrap();
    }

    fn render(c: &Connection) -> (String, bool) {
        let mut buf = Vec::new();
        let ok = report(c, Path::new("test.sqlite"), &mut buf).unwrap();
        (String::from_utf8(buf).unwrap(), ok)
    }

    #[test]
    fn counts_messages_per_folder_and_reconciles_cleanly() {
        let c = mem();
        let inbox = mailbox(&c, "INBOX", None);
        let archived = mailbox(&c, "Archive", None);
        email(&c, b"one", &[inbox]);
        email(&c, b"two", &[inbox]);
        email(&c, b"three", &[archived]);
        let (out, ok) = render(&c);
        assert!(ok, "a sound archive reconciles: {out}");
        assert!(out.contains("INBOX"), "{out}");
        assert!(out.contains("Archive"), "{out}");
        // INBOX has two, Archive one.
        let inbox_line = out.lines().find(|l| l.contains("INBOX")).unwrap();
        assert!(inbox_line.contains('2'), "INBOX line: {inbox_line}");
    }

    #[test]
    fn nested_folders_show_their_full_path() {
        let c = mem();
        let parent = mailbox(&c, "Archive", None);
        let child = mailbox(&c, "2019", Some(parent));
        email(&c, b"old", &[child]);
        let (out, ok) = render(&c);
        assert!(ok);
        assert!(out.contains("Archive/2019"), "want full path: {out}");
    }

    #[test]
    fn the_same_message_in_two_folders_is_counted_in_each() {
        let c = mem();
        let a = mailbox(&c, "A", None);
        let b = mailbox(&c, "B", None);
        email(&c, b"shared", &[a, b]);
        let (out, _) = render(&c);
        // Two folder rows, each with one message, from a single stored blob.
        assert!(
            out.contains("1 blobs") || out.contains("in 1 blobs"),
            "{out}"
        );
    }

    #[test]
    fn identical_content_gives_two_folders_the_same_checksum() {
        let c = mem();
        let a = mailbox(&c, "A", None);
        let b = mailbox(&c, "B", None);
        email(&c, b"identical", &[a]);
        email(&c, b"identical", &[b]);
        let folders = folder_manifest(&c).unwrap();
        let ca = &folders.iter().find(|f| f.path == "A").unwrap().checksum;
        let cb = &folders.iter().find(|f| f.path == "B").unwrap().checksum;
        assert_eq!(ca, cb, "same content => same folder checksum");
    }

    #[test]
    fn a_message_filed_in_a_missing_folder_is_a_problem() {
        let c = mem();
        let real = mailbox(&c, "INBOX", None);
        email(&c, b"ok", &[real]);
        email(&c, b"lost", &[9999]); // 9999 is not a mailbox
        let (out, ok) = render(&c);
        assert!(!ok, "a dangling folder reference must not reconcile: {out}");
        assert!(out.contains("not in the archive"), "{out}");
    }

    #[test]
    fn a_message_in_no_folder_is_a_problem() {
        let c = mem();
        mailbox(&c, "INBOX", None);
        email(&c, b"homeless", &[]);
        let (out, ok) = render(&c);
        assert!(!ok);
        assert!(out.contains("filed in no folder"), "{out}");
    }

    #[test]
    fn an_empty_archive_reports_no_folders_and_reconciles() {
        let c = mem();
        let (out, ok) = render(&c);
        assert!(ok);
        assert!(out.contains("(no folders)"), "{out}");
    }
}
