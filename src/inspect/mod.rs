/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::init;
use crate::error::Error;
use crate::types::ObjectType;

pub mod list;
pub mod tree;

pub struct InspectConfig {
    pub archive: PathBuf,
    pub target: Option<ObjectType>,
    pub limit: Option<usize>,
    pub offset: usize,
}

pub fn run(config: InspectConfig) -> Result<(), Error> {
    let conn = init::open_read_only(&config.archive)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    dispatch(&conn, &config, &mut out)?;
    Ok(())
}

fn dispatch(conn: &Connection, cfg: &InspectConfig, out: &mut impl Write) -> Result<(), Error> {
    match cfg.target {
        None => write_summary(conn, &cfg.archive, out),
        Some(ObjectType::Mailbox) => {
            warn_if_paged(out, cfg, "mailbox")?;
            tree::write_mailboxes(conn, out)
        }
        Some(ObjectType::FileNode) => {
            warn_if_paged(out, cfg, "filenode")?;
            tree::write_file_nodes(conn, out)
        }
        Some(ty) => list::write_list(conn, ty, cfg.limit, cfg.offset, out),
    }
}

fn warn_if_paged(out: &mut impl Write, cfg: &InspectConfig, token: &str) -> Result<(), Error> {
    if cfg.limit.is_some() || cfg.offset > 0 {
        writeln!(
            out,
            "note: --limit/--offset are ignored for {token} (tree view always shows the full hierarchy)"
        )?;
    }
    Ok(())
}

fn write_summary(conn: &Connection, archive: &Path, out: &mut impl Write) -> Result<(), Error> {
    writeln!(out, "Archive: {}", archive.display())?;
    match read_source(conn)? {
        Some(src) => {
            let name = src.account_name.unwrap_or_else(|| "(unnamed)".to_owned());
            writeln!(
                out,
                "Source:  {} {} (account {} / {})",
                src.kind, src.session_url, src.account_id, name
            )?;
        }
        None => writeln!(out, "Source:  (no source recorded)")?,
    }
    writeln!(out)?;
    let counts = read_type_counts(conn)?;
    let label_width = ObjectType::ALL
        .iter()
        .map(|t| t.token().len())
        .max()
        .unwrap_or(0);
    let count_width = counts
        .values()
        .map(|n| format_count(*n).len())
        .max()
        .unwrap_or(1)
        .max("blobs".len());
    for ty in ObjectType::ALL {
        let n = counts.get(&ty).copied().unwrap_or(0);
        writeln!(
            out,
            "  {:<lw$}  {:>cw$}",
            ty.token(),
            format_count(n),
            lw = label_width,
            cw = count_width
        )?;
    }
    let (blob_count, blob_bytes) = read_blob_totals(conn)?;
    writeln!(out)?;
    writeln!(
        out,
        "  {:<lw$}  {:>cw$}  ({})",
        "blobs",
        format_count(blob_count),
        format_bytes(blob_bytes),
        lw = label_width,
        cw = count_width
    )?;
    Ok(())
}

struct SourceRow {
    kind: String,
    session_url: String,
    account_id: String,
    account_name: Option<String>,
}

fn read_source(conn: &Connection) -> Result<Option<SourceRow>, Error> {
    let row = conn
        .query_row(
            "SELECT kind, session_url, account_id, account_name FROM sources LIMIT 1",
            [],
            |r| {
                Ok(SourceRow {
                    kind: r.get(0)?,
                    session_url: r.get(1)?,
                    account_id: r.get(2)?,
                    account_name: r.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

fn read_type_counts(conn: &Connection) -> Result<HashMap<ObjectType, u64>, Error> {
    let mut out = HashMap::new();
    for ty in ObjectType::ALL {
        let sql = format!("SELECT COUNT(*) FROM {}", crate::sync::table_name(ty));
        let n: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
        out.insert(ty, n as u64);
    }
    Ok(out)
}

fn read_blob_totals(conn: &Connection) -> Result<(u64, u64), Error> {
    let (count, bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(data)), 0) FROM blobs",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((count as u64, bytes.max(0) as u64))
}

pub(crate) fn format_count(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*ch as char);
    }
    out
}

pub(crate) fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut i = 0;
    while value >= 1024.0 && i + 1 < UNITS.len() {
        value /= 1024.0;
        i += 1;
    }
    format!("{value:.1} {}", UNITS[i])
}

pub(crate) fn blob_summary(conn: &Connection, blob_id: i64) -> Result<String, Error> {
    let row = conn
        .query_row(
            "SELECT length(data), substr(lower(hex(hash)), 1, 12) FROM blobs WHERE id = ?1",
            params![blob_id],
            |r| {
                let len: i64 = r.get(0)?;
                let hex: String = r.get(1)?;
                Ok((len, hex))
            },
        )
        .optional()?;
    match row {
        Some((len, hex)) => Ok(format!("{} blake3={hex}", format_bytes(len.max(0) as u64))),
        None => Ok(format!("(missing blob #{blob_id})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init::apply_schema(&c).unwrap();
        c
    }

    #[test]
    fn format_count_inserts_grouping_commas() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(42), "42");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(12_345), "12,345");
        assert_eq!(format_count(1_234_567_890), "1,234,567,890");
    }

    #[test]
    fn format_bytes_picks_appropriate_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1_536), "1.5 KB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.0 MB");
    }

    #[test]
    fn blob_summary_reports_size_and_short_hash() {
        let c = mem();
        let id = crate::db::blobs::intern_blob(&c, b"hello world").unwrap();
        let s = blob_summary(&c, id).unwrap();
        assert!(s.starts_with("11 B blake3="), "got {s}");
        assert!(s.len() > "11 B blake3=".len());
    }

    #[test]
    fn blob_summary_handles_missing_id() {
        let c = mem();
        let s = blob_summary(&c, 9_999).unwrap();
        assert_eq!(s, "(missing blob #9999)");
    }

    #[test]
    fn summary_lists_every_type_and_blob_totals() {
        let c = mem();
        crate::db::sources::upsert_source(
            &c,
            &crate::db::sources::SourceKey {
                kind: "jmap".to_owned(),
                session_url: "https://example.test/jmap".to_owned(),
                account_id: "u-1".to_owned(),
            },
            Some("alice@example.test"),
            "alice@example.test",
        )
        .unwrap();
        c.execute("INSERT INTO mailboxes (name) VALUES (?1)", params!["Inbox"])
            .unwrap();
        crate::db::blobs::intern_blob(&c, b"some bytes").unwrap();
        let mut buf = Vec::new();
        write_summary(&c, Path::new("test.sqlite"), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Archive: test.sqlite"));
        assert!(s.contains("Source:  jmap https://example.test/jmap"));
        assert!(s.contains("alice@example.test"));
        for ty in ObjectType::ALL {
            assert!(s.contains(ty.token()), "missing {} in:\n{s}", ty.token());
        }
        assert!(s.contains("blobs"));
        assert!(s.contains("10 B"));
    }

    #[test]
    fn summary_without_source_reports_none() {
        let c = mem();
        let mut buf = Vec::new();
        write_summary(&c, Path::new("x.sqlite"), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("(no source recorded)"));
    }
}
