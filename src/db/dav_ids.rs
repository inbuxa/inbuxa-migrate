/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

pub const CALENDAR: &str = "calendar";
pub const CALENDAR_EVENT: &str = "calendarevent";
pub const ADDRESS_BOOK: &str = "addressbook";
pub const CONTACT_CARD: &str = "contactcard";
pub const FILE_NODE: &str = "filenode";

#[derive(Debug, Clone)]
pub struct ItemRow {
    pub item_href: String,
    pub etag: String,
    pub local_id: i64,
}

pub fn insert(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    collection_href: &str,
    item_href: &str,
    etag: &str,
    local_id: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO sync_id_dav
         (source_id, type_name, collection_href, item_href, etag, local_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            source_id,
            type_name,
            collection_href,
            item_href,
            etag,
            local_id
        ],
    )?;
    Ok(())
}

pub fn update_etag(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    item_href: &str,
    etag: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE sync_id_dav
         SET etag = ?4
         WHERE source_id = ?1 AND type_name = ?2 AND item_href = ?3",
        params![source_id, type_name, item_href, etag],
    )?;
    Ok(())
}

pub fn local_for_item(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    item_href: &str,
) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT local_id FROM sync_id_dav
         WHERE source_id = ?1 AND type_name = ?2 AND item_href = ?3",
        params![source_id, type_name, item_href],
        |row| row.get(0),
    )
    .optional()
}

pub fn items_in_collection(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    collection_href: &str,
) -> Result<Vec<ItemRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT item_href, etag, local_id FROM sync_id_dav
         WHERE source_id = ?1 AND type_name = ?2 AND collection_href = ?3",
    )?;
    let rows = stmt.query_map(params![source_id, type_name, collection_href], |row| {
        Ok(ItemRow {
            item_href: row.get(0)?,
            etag: row.get(1)?,
            local_id: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn collections_of_type(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
) -> Result<HashMap<String, i64>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT item_href, local_id FROM sync_id_dav
         WHERE source_id = ?1 AND type_name = ?2",
    )?;
    let rows = stmt.query_map(params![source_id, type_name], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut map = HashMap::new();
    for r in rows {
        let (href, id) = r?;
        map.insert(href, id);
    }
    Ok(map)
}

/// How many server items stand for one local row: a card in two address
/// books has two.
pub fn items_for_local(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    local_id: i64,
) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT count(*) FROM sync_id_dav
         WHERE source_id = ?1 AND type_name = ?2 AND local_id = ?3",
        params![source_id, type_name, local_id],
        |row| row.get(0),
    )
}

pub fn delete_item(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    item_href: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM sync_id_dav
         WHERE source_id = ?1 AND type_name = ?2 AND item_href = ?3",
        params![source_id, type_name, item_href],
    )?;
    Ok(())
}

pub fn delete_collection(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    collection_href: &str,
) -> Result<usize, rusqlite::Error> {
    let removed = conn.execute(
        "DELETE FROM sync_id_dav
         WHERE source_id = ?1 AND type_name = ?2
           AND (item_href = ?3 OR collection_href = ?3)",
        params![source_id, type_name, collection_href],
    )?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;
    use crate::db::sources::{SourceKey, upsert_source};

    fn setup() -> (rusqlite::Connection, i64) {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init::apply_schema(&conn).unwrap();
        let key = SourceKey {
            kind: "caldav".to_owned(),
            session_url: "https://x/".to_owned(),
            account_id: "u".to_owned(),
        };
        let sid = upsert_source(&conn, &key, Some("u"), "u").unwrap();
        (conn, sid)
    }

    #[test]
    fn insert_and_lookup_round_trip() {
        let (conn, sid) = setup();
        insert(
            &conn,
            sid,
            CALENDAR_EVENT,
            "/cal/a/",
            "/cal/a/e.ics",
            "\"v1\"",
            7,
        )
        .unwrap();
        let id = local_for_item(&conn, sid, CALENDAR_EVENT, "/cal/a/e.ics")
            .unwrap()
            .unwrap();
        assert_eq!(id, 7);
    }

    #[test]
    fn replace_keeps_local_id_on_etag_change() {
        let (conn, sid) = setup();
        insert(
            &conn,
            sid,
            CALENDAR_EVENT,
            "/cal/a/",
            "/cal/a/e.ics",
            "\"v1\"",
            7,
        )
        .unwrap();
        update_etag(&conn, sid, CALENDAR_EVENT, "/cal/a/e.ics", "\"v2\"").unwrap();
        let rows = items_in_collection(&conn, sid, CALENDAR_EVENT, "/cal/a/").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].etag, "\"v2\"");
        assert_eq!(rows[0].local_id, 7);
    }

    #[test]
    fn items_in_collection_filters_by_collection_href() {
        let (conn, sid) = setup();
        insert(
            &conn,
            sid,
            CALENDAR_EVENT,
            "/cal/a/",
            "/cal/a/e1.ics",
            "v1",
            1,
        )
        .unwrap();
        insert(
            &conn,
            sid,
            CALENDAR_EVENT,
            "/cal/a/",
            "/cal/a/e2.ics",
            "v2",
            2,
        )
        .unwrap();
        insert(
            &conn,
            sid,
            CALENDAR_EVENT,
            "/cal/b/",
            "/cal/b/e3.ics",
            "v3",
            3,
        )
        .unwrap();
        let rows = items_in_collection(&conn, sid, CALENDAR_EVENT, "/cal/a/").unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn delete_item_removes_only_one_row() {
        let (conn, sid) = setup();
        insert(
            &conn,
            sid,
            CALENDAR_EVENT,
            "/cal/a/",
            "/cal/a/e1.ics",
            "v",
            1,
        )
        .unwrap();
        insert(
            &conn,
            sid,
            CALENDAR_EVENT,
            "/cal/a/",
            "/cal/a/e2.ics",
            "v",
            2,
        )
        .unwrap();
        delete_item(&conn, sid, CALENDAR_EVENT, "/cal/a/e1.ics").unwrap();
        let rows = items_in_collection(&conn, sid, CALENDAR_EVENT, "/cal/a/").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].item_href, "/cal/a/e2.ics");
    }

    #[test]
    fn delete_collection_removes_collection_row_and_items() {
        let (conn, sid) = setup();
        insert(&conn, sid, CALENDAR, "/cal/a/", "/cal/a/", "", 10).unwrap();
        insert(
            &conn,
            sid,
            CALENDAR_EVENT,
            "/cal/a/",
            "/cal/a/e.ics",
            "v",
            1,
        )
        .unwrap();
        let removed = delete_collection(&conn, sid, CALENDAR_EVENT, "/cal/a/").unwrap();
        assert_eq!(removed, 1);
        let rows = items_in_collection(&conn, sid, CALENDAR_EVENT, "/cal/a/").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn collections_of_type_lists_all_known_collections() {
        let (conn, sid) = setup();
        insert(&conn, sid, CALENDAR, "/cal/a/", "/cal/a/", "", 1).unwrap();
        insert(&conn, sid, CALENDAR, "/cal/b/", "/cal/b/", "", 2).unwrap();
        let map = collections_of_type(&conn, sid, CALENDAR).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("/cal/a/"), Some(&1));
        assert_eq!(map.get("/cal/b/"), Some(&2));
    }

    #[test]
    fn two_items_can_stand_for_one_card() {
        let (conn, sid) = setup();
        for (book, href) in [("/card/a/", "/card/a/x.vcf"), ("/card/b/", "/card/b/x.vcf")] {
            insert(&conn, sid, CONTACT_CARD, book, href, "v", 1).unwrap();
        }
        assert_eq!(items_for_local(&conn, sid, CONTACT_CARD, 1).unwrap(), 2);
        delete_item(&conn, sid, CONTACT_CARD, "/card/a/x.vcf").unwrap();
        assert_eq!(items_for_local(&conn, sid, CONTACT_CARD, 1).unwrap(), 1);
        assert_eq!(items_for_local(&conn, sid, CALENDAR_EVENT, 1).unwrap(), 0);
    }
}
