/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! The id map for a POP3 source: which archive row each UIDL became, and the
//! single INBOX mailbox. POP3 is flat, so there is no folder column -- unlike
//! the maildir map it mirrors.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

const MAILBOX: &str = "mailbox";
const EMAIL: &str = "email";

pub fn insert_mailbox(
    conn: &Connection,
    source_id: i64,
    local_id: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO sync_id_pop3 (source_id, type_name, unique_id, local_id)
         VALUES (?1, ?2, '', ?3)",
        params![source_id, MAILBOX, local_id],
    )?;
    Ok(())
}

pub fn local_for_mailbox(
    conn: &Connection,
    source_id: i64,
) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT local_id FROM sync_id_pop3
         WHERE source_id = ?1 AND type_name = ?2 AND unique_id = ''",
        params![source_id, MAILBOX],
        |row| row.get(0),
    )
    .optional()
}

pub fn insert_email(
    conn: &Connection,
    source_id: i64,
    unique_id: &str,
    local_id: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO sync_id_pop3 (source_id, type_name, unique_id, local_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![source_id, EMAIL, unique_id, local_id],
    )?;
    Ok(())
}

/// Every UIDL already stored for this source, mapped to its archive row, so a
/// later run can tell what is new.
pub fn email_ids(
    conn: &Connection,
    source_id: i64,
) -> Result<HashMap<String, i64>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT unique_id, local_id FROM sync_id_pop3
         WHERE source_id = ?1 AND type_name = ?2",
    )?;
    let rows = stmt.query_map(params![source_id, EMAIL], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut map = HashMap::new();
    for r in rows {
        let (u, l) = r?;
        map.insert(u, l);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;
    use crate::db::sources::{SourceKey, upsert_source};

    fn archive() -> (Connection, i64) {
        let c = Connection::open_in_memory().unwrap();
        init::apply_schema(&c).unwrap();
        let sid = upsert_source(
            &c,
            &SourceKey {
                kind: "pop3".to_owned(),
                session_url: "pop3s://mail.example.org:995".to_owned(),
                account_id: "alice".to_owned(),
            },
            Some("alice"),
            "alice",
        )
        .unwrap();
        (c, sid)
    }

    #[test]
    fn mailbox_round_trips() {
        let (c, sid) = archive();
        assert_eq!(local_for_mailbox(&c, sid).unwrap(), None);
        insert_mailbox(&c, sid, 42).unwrap();
        assert_eq!(local_for_mailbox(&c, sid).unwrap(), Some(42));
    }

    #[test]
    fn emails_map_by_uidl() {
        let (c, sid) = archive();
        insert_email(&c, sid, "uid-a", 1).unwrap();
        insert_email(&c, sid, "uid-b", 2).unwrap();
        let map = email_ids(&c, sid).unwrap();
        assert_eq!(map.get("uid-a"), Some(&1));
        assert_eq!(map.get("uid-b"), Some(&2));
        assert_eq!(map.len(), 2);
    }
}
