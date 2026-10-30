/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! A contact card is one row however many address books hold it, with the
//! books in `address_book_ids`. Sources that list a card once per book
//! (CardDAV, Takeout) add each book they find it in to the same row.

use rusqlite::{Connection, params};

/// `address_book_ids` with `book` added, in the order first seen.
pub fn with_book(ids_json: &str, book: i64) -> String {
    let mut ids: Vec<i64> = serde_json::from_str(ids_json).unwrap_or_default();
    if !ids.contains(&book) {
        ids.push(book);
    }
    serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_owned())
}

/// Takes `book` off one card, leaving the card in its other books.
pub fn leave_book(conn: &Connection, card: i64, book: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE contact_cards
         SET address_book_ids = (SELECT json_group_array(value) FROM json_each(address_book_ids)
                                 WHERE value != ?2)
         WHERE id = ?1",
        params![card, book],
    )?;
    Ok(())
}

/// An address book going away: the cards only it held go with it, and the
/// others stay in their other books.
pub fn remove_book(conn: &Connection, book: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM contact_cards
         WHERE EXISTS (SELECT 1 FROM json_each(address_book_ids) WHERE value = ?1)
           AND NOT EXISTS (SELECT 1 FROM json_each(address_book_ids) WHERE value != ?1)",
        params![book],
    )?;
    conn.execute(
        "UPDATE contact_cards
         SET address_book_ids = (SELECT json_group_array(value) FROM json_each(address_book_ids)
                                 WHERE value != ?1)
         WHERE EXISTS (SELECT 1 FROM json_each(address_book_ids) WHERE value = ?1)",
        params![book],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;

    fn books(conn: &Connection, uid: &str) -> Option<String> {
        conn.query_row(
            "SELECT address_book_ids FROM contact_cards WHERE uid = ?1",
            [uid],
            |r| r.get(0),
        )
        .ok()
    }

    #[test]
    fn a_book_is_added_once() {
        assert_eq!(with_book("[1]", 2), "[1,2]");
        assert_eq!(with_book("[1,2]", 1), "[1,2]");
        assert_eq!(with_book("not json", 3), "[3]");
    }

    #[test]
    fn removing_a_book_keeps_cards_another_book_holds() {
        let conn = Connection::open_in_memory().unwrap();
        init::apply_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES
               ('only-1', '[1]', '{}'), ('both', '[1,2]', '{}'), ('only-2', '[2]', '{}');",
        )
        .unwrap();
        remove_book(&conn, 1).unwrap();
        assert_eq!(books(&conn, "only-1"), None);
        assert_eq!(books(&conn, "both").as_deref(), Some("[2]"));
        assert_eq!(books(&conn, "only-2").as_deref(), Some("[2]"));

        let id: i64 = conn
            .query_row("SELECT id FROM contact_cards WHERE uid = 'both'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "UPDATE contact_cards SET address_book_ids = '[2,3]' WHERE id = ?1",
            [id],
        )
        .unwrap();
        leave_book(&conn, id, 2).unwrap();
        assert_eq!(books(&conn, "both").as_deref(), Some("[3]"));
    }
}
