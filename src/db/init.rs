/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, OpenFlags};

pub const SCHEMA_SQL: &str = include_str!("schema.sql");

pub fn open(path: &std::path::Path) -> Result<Connection, OpenError> {
    private_archive(path)?;
    let conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    apply_schema(&conn)?;
    Ok(conn)
}

/// An existing archive, for reading only: nothing is created, migrated or
/// written. A closed archive has no `-wal` file, since every run folds it
/// in when it ends, and is opened as immutable, so SQLite adds no lock or
/// index files beside it either. With a `-wal` there (a run is going, or
/// one was interrupted) it is opened read-only, so what that run wrote is
/// shown; SQLite may then add its `-shm` index. An archive from an older release
/// that is missing part of today's schema is refused rather than brought up
/// to date, since that would write to it.
pub fn open_read_only(path: &std::path::Path) -> Result<Connection, OpenError> {
    if !path.is_file() {
        return Err(OpenError::NotAnArchive(format!(
            "no archive at {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    if let Some(warning) = permission_warning(path) {
        eprintln!("warning: {warning}");
    }
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let closed = !std::path::Path::new(&wal).exists();
    let conn = Connection::open_with_flags(
        read_only_uri(path, closed),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "query_only", "ON")?;
    let present: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
        .map_err(|e| match e.sqlite_error_code() {
            Some(rusqlite::ErrorCode::NotADatabase) => {
                OpenError::NotAnArchive(format!("{} is not an archive", path.display()))
            }
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                OpenError::NotAnArchive(format!(
                    "{} is in use by another inbuxa-migrate run; inspect it once that run ends",
                    path.display()
                ))
            }
            _ => OpenError::Sqlite(e),
        })?;
    if !present.iter().any(|t| t == "sources") {
        return Err(OpenError::NotAnArchive(format!(
            "{} is not an archive",
            path.display()
        )));
    }
    let missing: Vec<&str> = schema_tables()
        .filter(|t| !present.iter().any(|p| p == t))
        .collect();
    let has_data_type = conn
        .prepare("SELECT 1 FROM pragma_table_info('calendar_events') WHERE name = 'data_type'")?
        .exists([])?;
    if !missing.is_empty() || !has_data_type {
        return Err(OpenError::NotAnArchive(format!(
            "{} was written by an older inbuxa-migrate and lacks part of today's layout; \
             the next import into it brings it up to date",
            path.display()
        )));
    }
    Ok(conn)
}

/// A `file:` URI for `path`, read-only, and immutable when asked.
fn read_only_uri(path: &std::path::Path, immutable: bool) -> String {
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut out = String::from("file:");
    let windows_drive = bytes.len() > 1 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic();
    if bytes.first() == Some(&b'/') {
        out.push_str("//");
    } else if windows_drive {
        out.push_str("///");
    }
    for &b in bytes {
        match b {
            b'\\' => out.push('/'),
            b if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) => out.push(b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out.push_str("?mode=ro");
    if immutable {
        out.push_str("&immutable=1");
    }
    out
}

/// Every table the schema creates.
fn schema_tables() -> impl Iterator<Item = &'static str> {
    SCHEMA_SQL
        .split("CREATE TABLE IF NOT EXISTS")
        .skip(1)
        .filter_map(|rest| {
            rest.split(|c: char| c.is_whitespace() || c == '(')
                .find(|s| !s.is_empty())
        })
}

pub fn apply_schema(conn: &Connection) -> Result<(), OpenError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(SCHEMA_SQL)?;
    ensure_calendar_events_data_type(&tx)?;
    ensure_graph_ids_accept_file_nodes(&tx)?;
    ensure_dav_ids_share_local_rows(&tx)?;
    tx.commit()?;
    Ok(())
}

fn ensure_calendar_events_data_type(conn: &Connection) -> Result<(), OpenError> {
    let mut stmt = conn.prepare("PRAGMA table_info(calendar_events)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let has_column = rows.filter_map(|r| r.ok()).any(|name| name == "data_type");
    if !has_column {
        conn.execute(
            "ALTER TABLE calendar_events ADD COLUMN data_type TEXT NOT NULL DEFAULT 'Event'",
            [],
        )?;
    }
    Ok(())
}

fn ensure_graph_ids_accept_file_nodes(conn: &Connection) -> Result<(), OpenError> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' \
             AND name = 'sync_id_exchange_graph'",
            [],
            |row| row.get(0),
        )
        .ok();
    let Some(sql) = sql else { return Ok(()) };
    if sql.contains("'filenode'") {
        return Ok(());
    }
    conn.execute_batch(
        "ALTER TABLE sync_id_exchange_graph RENAME TO sync_id_exchange_graph_old;
         CREATE TABLE sync_id_exchange_graph (
             source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
             type_name   TEXT    NOT NULL CHECK (type_name IN (
                                                     'mailbox','email',
                                                     'calendar','calendarevent',
                                                     'addressbook','contactcard',
                                                     'filenode')),
             graph_id    TEXT    NOT NULL,
             local_id    INTEGER NOT NULL,
             PRIMARY KEY (source_id, type_name, graph_id),
             UNIQUE (source_id, type_name, local_id)
         );
         INSERT INTO sync_id_exchange_graph SELECT * FROM sync_id_exchange_graph_old;
         DROP TABLE sync_id_exchange_graph_old;
         CREATE INDEX IF NOT EXISTS sync_id_exchange_graph_type_idx
             ON sync_id_exchange_graph (source_id, type_name);",
    )?;
    Ok(())
}

/// A card in two address books is one archive row reached from two server
/// items, so the DAV map no longer holds a local row to one item. An
/// archive from before that is rebuilt without the constraint.
fn ensure_dav_ids_share_local_rows(conn: &Connection) -> Result<(), OpenError> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'sync_id_dav'",
            [],
            |row| row.get(0),
        )
        .ok();
    let Some(sql) = sql else { return Ok(()) };
    if !sql.contains("UNIQUE (source_id, type_name, local_id)") {
        return Ok(());
    }
    conn.execute_batch(
        "DROP INDEX IF EXISTS sync_id_dav_collection_idx;
         DROP INDEX IF EXISTS sync_id_dav_local_idx;
         ALTER TABLE sync_id_dav RENAME TO sync_id_dav_old;
         CREATE TABLE sync_id_dav (
             source_id        INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
             type_name        TEXT    NOT NULL CHECK (type_name IN (
                                                          'calendar','calendarevent',
                                                          'addressbook','contactcard',
                                                          'filenode')),
             collection_href  TEXT    NOT NULL,
             item_href        TEXT    NOT NULL,
             etag             TEXT    NOT NULL DEFAULT '',
             local_id         INTEGER NOT NULL,
             PRIMARY KEY (source_id, type_name, item_href)
         );
         INSERT INTO sync_id_dav SELECT * FROM sync_id_dav_old;
         DROP TABLE sync_id_dav_old;
         CREATE INDEX sync_id_dav_collection_idx
             ON sync_id_dav (source_id, type_name, collection_href);
         CREATE INDEX sync_id_dav_local_idx
             ON sync_id_dav (source_id, type_name, local_id);",
    )?;
    Ok(())
}

/// An archive holds a whole mailbox, so it is created readable by its owner
/// only. SQLite gives its `-wal` and `-shm` files the database file's mode,
/// so they follow. An existing archive others can read is left as it is,
/// with a warning and the command that fixes it.
#[cfg(unix)]
fn private_archive(path: &std::path::Path) -> Result<(), OpenError> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(warning) = permission_warning(path) {
                eprintln!("warning: {warning}");
            }
            Ok(())
        }
        Err(e) => Err(OpenError::Create(e)),
    }
}

#[cfg(not(unix))]
fn private_archive(_path: &std::path::Path) -> Result<(), OpenError> {
    Ok(())
}

/// The warning for an archive that someone other than its owner can read.
#[cfg(unix)]
fn permission_warning(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o077 != 0).then(|| {
        format!(
            "archive {} can be read by other users (mode {:o}); run: chmod 600 {}",
            path.display(),
            mode & 0o777,
            path.display()
        )
    })
}

fn apply_pragmas(conn: &Connection) -> Result<(), OpenError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot create the archive: {0}")]
    Create(std::io::Error),
    #[error("{0}")]
    NotAnArchive(String),
}

#[cfg(all(test, unix))]
mod permission_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("inbuxa-migrate-perm-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("a.sqlite")
    }

    #[test]
    fn a_new_archive_and_its_wal_are_private() {
        let path = scratch("new");
        let conn = open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
        assert_eq!(mode(&path), 0o600);
        let wal = path.with_extension("sqlite-wal");
        assert!(wal.exists(), "WAL mode writes a -wal file");
        assert_eq!(mode(&wal), 0o600);
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_existing_readable_archive_is_warned_about_not_changed() {
        let path = scratch("existing");
        drop(open(&path).unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let warning = permission_warning(&path).expect("warns");
        assert!(warning.contains("chmod 600"), "{warning}");
        assert!(warning.contains("644"), "{warning}");
        drop(open(&path).unwrap());
        assert_eq!(mode(&path), 0o644, "left as it is");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(permission_warning(&path).is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Each file in `dir` by name, with its length and hash.
    fn files_in(dir: &std::path::Path) -> Vec<(String, usize, String)> {
        let mut out: Vec<(String, usize, String)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let bytes = std::fs::read(e.path()).unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    bytes.len(),
                    blake3::hash(&bytes).to_hex().to_string(),
                )
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn reading_an_archive_changes_nothing_on_disk() {
        let path = scratch("read-only");
        let conn = open(&path).unwrap();
        conn.execute("INSERT INTO mailboxes (name) VALUES ('Inbox')", [])
            .unwrap();
        drop(conn);
        let before = files_in(path.parent().unwrap());
        let conn = open_read_only(&path).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM mailboxes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(conn.execute("DELETE FROM mailboxes", []).is_err());
        drop(conn);
        assert_eq!(files_in(path.parent().unwrap()), before);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_or_foreign_file_is_refused_and_not_created() {
        let path = scratch("missing");
        let err = open_read_only(&path).unwrap_err().to_string();
        assert!(err.contains("no archive at"), "{err}");
        assert!(!path.exists());

        let other = Connection::open(&path).unwrap();
        other.execute_batch("CREATE TABLE notes(x);").unwrap();
        drop(other);
        let err = open_read_only(&path).unwrap_err().to_string();
        assert!(err.contains("is not an archive"), "{err}");

        std::fs::write(&path, b"plain text").unwrap();
        let err = open_read_only(&path).unwrap_err().to_string();
        assert!(err.contains("is not an archive"), "{err}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_older_layout_is_refused_rather_than_upgraded() {
        let path = scratch("older");
        drop(open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE file_nodes;").unwrap();
        drop(conn);
        let err = open_read_only(&path).unwrap_err().to_string();
        assert!(err.contains("older inbuxa-migrate"), "{err}");
        let tables: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'file_nodes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "left as it was");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_path_becomes_a_read_only_uri() {
        use std::path::Path;
        assert_eq!(
            read_only_uri(Path::new("/srv/a b/x?#%.sqlite"), true),
            "file:///srv/a%20b/x%3F%23%25.sqlite?mode=ro&immutable=1"
        );
        assert_eq!(
            read_only_uri(Path::new("a.sqlite"), false),
            "file:a.sqlite?mode=ro"
        );
    }

    #[test]
    fn an_interrupted_archive_is_read_with_its_wal() {
        let path = scratch("interrupted");
        drop(open(&path).unwrap());
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        // A run that hasn't closed: its rows are still only in the -wal.
        let writer = Connection::open(&path).unwrap();
        writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        writer
            .execute("INSERT INTO mailboxes (name) VALUES ('Inbox')", [])
            .unwrap();
        assert!(std::path::Path::new(&wal).exists());
        let conn = open_read_only(&path).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM mailboxes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(conn);
        drop(writer);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_archive_a_run_holds_is_reported_as_in_use() {
        let path = scratch("in-use");
        let run = open(&path).unwrap();
        run.execute("INSERT INTO mailboxes (name) VALUES ('Inbox')", [])
            .unwrap();
        let err = open_read_only(&path).unwrap_err().to_string();
        assert!(err.contains("in use"), "{err}");
        drop(run);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn every_schema_table_is_found() {
        let tables: Vec<&str> = schema_tables().collect();
        assert!(tables.contains(&"sources"), "{tables:?}");
        assert!(tables.contains(&"contact_cards"), "{tables:?}");
        assert!(
            tables
                .iter()
                .all(|t| t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        );
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn an_older_dav_map_is_rebuilt_to_share_local_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sources (id INTEGER PRIMARY KEY);
             INSERT INTO sources (id) VALUES (1);
             CREATE TABLE sync_id_dav (
                 source_id        INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
                 type_name        TEXT    NOT NULL,
                 collection_href  TEXT    NOT NULL,
                 item_href        TEXT    NOT NULL,
                 etag             TEXT    NOT NULL DEFAULT '',
                 local_id         INTEGER NOT NULL,
                 PRIMARY KEY (source_id, type_name, item_href),
                 UNIQUE (source_id, type_name, local_id)
             );
             INSERT INTO sync_id_dav VALUES (1, 'contactcard', '/a/', '/a/x.vcf', 'e1', 7);",
        )
        .unwrap();
        ensure_dav_ids_share_local_rows(&conn).unwrap();
        conn.execute(
            "INSERT INTO sync_id_dav VALUES (1, 'contactcard', '/b/', '/b/x.vcf', 'e2', 7)",
            [],
        )
        .expect("two items may stand for one card");
        let rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM sync_id_dav WHERE local_id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "the old row was kept");
        // A second pass finds nothing to do.
        ensure_dav_ids_share_local_rows(&conn).unwrap();
        // And a full schema pass over it is clean.
        apply_schema(&conn).unwrap();
    }
}
