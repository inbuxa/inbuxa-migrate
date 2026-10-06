/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Connection;

pub const SCHEMA_SQL: &str = include_str!("schema.sql");

pub fn open(path: &std::path::Path) -> Result<Connection, OpenError> {
    private_archive(path)?;
    let conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    apply_schema(&conn)?;
    Ok(conn)
}

pub fn apply_schema(conn: &Connection) -> Result<(), OpenError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(SCHEMA_SQL)?;
    ensure_calendar_events_data_type(&tx)?;
    ensure_graph_ids_accept_file_nodes(&tx)?;
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
}
