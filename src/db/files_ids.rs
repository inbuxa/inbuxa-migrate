/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! What `import files` has read from an account export, by a key of its
//! own for each thing: a folder's path of real names, a message's content
//! hash, a collection's file, a card's uid. Several keys may lead to one
//! archive row: a message kept in two folders, a card in two address books.

use rusqlite::{Connection, OptionalExtension, params};

pub const MAILBOX: &str = "mailbox";
pub const EMAIL: &str = "email";
pub const ADDRESS_BOOK: &str = "addressbook";
pub const CONTACT_CARD: &str = "contactcard";
pub const CALENDAR: &str = "calendar";
pub const CALENDAR_EVENT: &str = "calendarevent";
pub const SIEVE_SCRIPT: &str = "sievescript";
pub const FILE_NODE: &str = "filenode";
pub const IDENTITY: &str = "identity";
pub const PARTICIPANT_IDENTITY: &str = "participantidentity";

pub fn local_for(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    key: &str,
) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT local_id FROM sync_id_files
         WHERE source_id = ?1 AND type_name = ?2 AND item_key = ?3",
        params![source_id, type_name, key],
        |row| row.get(0),
    )
    .optional()
}

/// Records `key` as `local_id`; a key already recorded is pointed at it.
pub fn record(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    key: &str,
    local_id: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO sync_id_files (source_id, type_name, item_key, local_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (source_id, type_name, item_key) DO UPDATE SET local_id = excluded.local_id",
        params![source_id, type_name, key, local_id],
    )?;
    Ok(())
}

pub fn count(conn: &Connection, source_id: i64, type_name: &str) -> Result<u64, rusqlite::Error> {
    conn.query_row(
        "SELECT count(DISTINCT local_id) FROM sync_id_files
         WHERE source_id = ?1 AND type_name = ?2",
        params![source_id, type_name],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;

    #[test]
    fn two_keys_may_lead_to_one_row_and_a_key_moves() {
        let conn = Connection::open_in_memory().unwrap();
        init::apply_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO sources (id, kind, session_url, account_id, username)
             VALUES (1, 'files', 'file:///e', '/e', '')",
            [],
        )
        .unwrap();
        record(&conn, 1, EMAIL, "a", 7).unwrap();
        record(&conn, 1, EMAIL, "b", 7).unwrap();
        assert_eq!(local_for(&conn, 1, EMAIL, "a").unwrap(), Some(7));
        assert_eq!(count(&conn, 1, EMAIL).unwrap(), 1);
        record(&conn, 1, EMAIL, "a", 8).unwrap();
        assert_eq!(local_for(&conn, 1, EMAIL, "a").unwrap(), Some(8));
        assert_eq!(local_for(&conn, 1, MAILBOX, "a").unwrap(), None);
    }
}
