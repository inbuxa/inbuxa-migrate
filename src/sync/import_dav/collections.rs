/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::dav::discover::DiscoveredCollection;
use crate::dav::href::{Href, last_path_component};
use crate::db::dav_ids;
use crate::error::Error;
use crate::logging::{LEVEL_PROGRESS, Logger};
use crate::sync::TypeCounts;

pub fn reconcile_calendars(
    conn: &mut Connection,
    source_id: i64,
    discovered: &[DiscoveredCollection],
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<Vec<(String, i64)>, Error> {
    reconcile_calendar_like(
        conn,
        source_id,
        discovered,
        &RowOps {
            type_name: dav_ids::CALENDAR,
            upsert: upsert_calendar_row,
            delete: delete_calendar_row,
        },
        counts,
        logger,
    )
}

pub fn reconcile_address_books(
    conn: &mut Connection,
    source_id: i64,
    discovered: &[DiscoveredCollection],
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<Vec<(String, i64)>, Error> {
    reconcile_calendar_like(
        conn,
        source_id,
        discovered,
        &RowOps {
            type_name: dav_ids::ADDRESS_BOOK,
            upsert: upsert_address_book_row,
            delete: delete_address_book_row,
        },
        counts,
        logger,
    )
}

type RowUpsert =
    fn(&Connection, &DiscoveredCollection, Option<i64>) -> Result<i64, rusqlite::Error>;
type RowDelete = fn(&Connection, i64) -> Result<(), rusqlite::Error>;

struct RowOps {
    type_name: &'static str,
    upsert: RowUpsert,
    delete: RowDelete,
}

fn reconcile_calendar_like(
    conn: &mut Connection,
    source_id: i64,
    discovered: &[DiscoveredCollection],
    ops: &RowOps,
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<Vec<(String, i64)>, Error> {
    let type_name = ops.type_name;
    let upsert_row = ops.upsert;
    let delete_row = ops.delete;
    let existing: HashMap<String, i64> = dav_ids::collections_of_type(conn, source_id, type_name)
        .map_err(|e| Error::Partial(e.to_string()))?;

    let mut upserted: Vec<(String, i64)> = Vec::new();
    let server_hrefs: std::collections::HashSet<String> = discovered
        .iter()
        .map(|c| c.href.as_str().to_owned())
        .collect();

    for coll in discovered {
        let collection_href = coll.href.as_str().to_owned();
        let existing_local = existing.get(&collection_href).copied();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| Error::Partial(e.to_string()))?;
        match upsert_row(&tx, coll, existing_local) {
            Ok(local_id) => {
                if existing_local.is_none() {
                    dav_ids::insert(
                        &tx,
                        source_id,
                        type_name,
                        &collection_href,
                        &collection_href,
                        "",
                        local_id,
                    )
                    .map_err(|e| Error::Partial(e.to_string()))?;
                    counts.created += 1;
                } else {
                    counts.fetched += 1;
                }
                tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
                upserted.push((collection_href.clone(), local_id));
                if logger.enabled(LEVEL_PROGRESS) {
                    eprintln!("collection upserted: {collection_href} -> {local_id}");
                }
            }
            Err(e) => {
                let _ = tx.rollback();
                logger.warn(&format!(
                    "collection upsert {collection_href:?} failed: {e}"
                ));
                counts.failed += 1;
            }
        }
    }

    for (collection_href, local_id) in &existing {
        if server_hrefs.contains(collection_href) {
            continue;
        }
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| Error::Partial(e.to_string()))?;
        let item_type = item_type_for(type_name);
        if let Some(item_type) = item_type {
            let _ = dav_ids::delete_collection(&tx, source_id, item_type, collection_href);
        }
        let _ = dav_ids::delete_collection(&tx, source_id, type_name, collection_href);
        match delete_row(&tx, *local_id) {
            Ok(()) => {
                counts.deleted += 1;
            }
            Err(e) => {
                logger.warn(&format!(
                    "vanished collection {collection_href:?} delete failed: {e}"
                ));
                counts.failed += 1;
            }
        }
        tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    }

    Ok(upserted)
}

fn item_type_for(collection_type: &str) -> Option<&'static str> {
    match collection_type {
        dav_ids::CALENDAR => Some(dav_ids::CALENDAR_EVENT),
        dav_ids::ADDRESS_BOOK => Some(dav_ids::CONTACT_CARD),
        _ => None,
    }
}

fn upsert_calendar_row(
    conn: &Connection,
    coll: &DiscoveredCollection,
    existing_local: Option<i64>,
) -> Result<i64, rusqlite::Error> {
    let name = display_or_fallback(&coll.props.displayname, &coll.href);
    let description = coll.props.calendar_description.as_deref();
    let color = coll.props.calendar_color.as_deref();
    let sort_order = coll.props.calendar_order.unwrap_or(0).max(0);
    let time_zone = parse_tzid_from_vtimezone(coll.props.calendar_timezone.as_deref());

    if let Some(local) = existing_local {
        conn.execute(
            "UPDATE calendars SET name = ?1, description = ?2, color = ?3, sort_order = ?4,
                                  time_zone = ?5
             WHERE id = ?6",
            params![name, description, color, sort_order, time_zone, local],
        )?;
        Ok(local)
    } else {
        conn.execute(
            "INSERT INTO calendars (name, description, color, sort_order, is_subscribed,
                                    is_visible, is_default, include_in_availability,
                                    default_alerts_with_time, default_alerts_without_time, time_zone)
             VALUES (?1, ?2, ?3, ?4, 1, 1, 0, 'all', NULL, NULL, ?5)",
            params![name, description, color, sort_order, time_zone],
        )?;
        Ok(conn.last_insert_rowid())
    }
}

fn delete_calendar_row(conn: &Connection, local_id: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM calendar_events
         WHERE EXISTS (
           SELECT 1 FROM json_each(calendar_ids) WHERE value = ?1
         )",
        params![local_id],
    )?;
    conn.execute("DELETE FROM calendars WHERE id = ?1", params![local_id])?;
    Ok(())
}

fn upsert_address_book_row(
    conn: &Connection,
    coll: &DiscoveredCollection,
    existing_local: Option<i64>,
) -> Result<i64, rusqlite::Error> {
    let name = display_or_fallback(&coll.props.displayname, &coll.href);
    let description = coll.props.addressbook_description.as_deref();

    if let Some(local) = existing_local {
        conn.execute(
            "UPDATE address_books SET name = ?1, description = ?2 WHERE id = ?3",
            params![name, description, local],
        )?;
        Ok(local)
    } else {
        conn.execute(
            "INSERT INTO address_books (name, description, sort_order, is_default, is_subscribed)
             VALUES (?1, ?2, 0, 0, 1)",
            params![name, description],
        )?;
        Ok(conn.last_insert_rowid())
    }
}

fn delete_address_book_row(conn: &Connection, local_id: i64) -> Result<(), rusqlite::Error> {
    crate::db::cards::remove_book(conn, local_id)?;
    conn.execute("DELETE FROM address_books WHERE id = ?1", params![local_id])?;
    Ok(())
}

fn display_or_fallback(name: &Option<String>, href: &Href) -> String {
    if let Some(n) = name
        && !n.trim().is_empty()
    {
        return n.clone();
    }
    let comp = last_path_component(href);
    if comp.is_empty() {
        "Untitled".to_owned()
    } else {
        comp
    }
}

fn parse_tzid_from_vtimezone(vt: Option<&str>) -> Option<String> {
    let body = vt?;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("TZID:") {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tzid_from_vtimezone_extracts_first() {
        let v = "BEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nEND:VTIMEZONE\r\n";
        assert_eq!(
            parse_tzid_from_vtimezone(Some(v)).as_deref(),
            Some("Europe/Berlin")
        );
    }

    #[test]
    fn parse_tzid_returns_none_when_absent() {
        assert!(parse_tzid_from_vtimezone(Some("nothing here")).is_none());
        assert!(parse_tzid_from_vtimezone(None).is_none());
    }

    #[test]
    fn display_or_fallback_uses_href_when_displayname_empty() {
        let href = Href::from_normalised("/dav/cal/u/work/".to_owned());
        let name: Option<String> = None;
        assert_eq!(display_or_fallback(&name, &href), "work");
    }

    #[test]
    fn display_or_fallback_uses_displayname_when_present() {
        let href = Href::from_normalised("/dav/cal/u/default/".to_owned());
        let name = Some("Default".to_owned());
        assert_eq!(display_or_fallback(&name, &href), "Default");
    }

    #[test]
    fn delete_calendar_row_cascades_events_and_sync_id_rows() {
        use crate::db::init;
        use crate::db::sources::{SourceKey, upsert_source};
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init::apply_schema(&conn).unwrap();
        let sid = upsert_source(
            &conn,
            &SourceKey {
                kind: "caldav".to_owned(),
                session_url: "https://x".to_owned(),
                account_id: "u".to_owned(),
            },
            Some("u"),
            "u",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendars (name, sort_order, is_subscribed, is_visible, is_default,
                                     include_in_availability)
             VALUES ('Work', 0, 1, 1, 0, 'all')",
            [],
        )
        .unwrap();
        let cal_local = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO calendar_events (calendar_ids, data, data_type)
             VALUES (?1, '{}', 'Event')",
            rusqlite::params![format!("[{cal_local}]")],
        )
        .unwrap();
        let evt_local = conn.last_insert_rowid();
        dav_ids::insert(
            &conn,
            sid,
            dav_ids::CALENDAR,
            "/cal/work/",
            "/cal/work/",
            "",
            cal_local,
        )
        .unwrap();
        dav_ids::insert(
            &conn,
            sid,
            dav_ids::CALENDAR_EVENT,
            "/cal/work/",
            "/cal/work/e.ics",
            "\"v\"",
            evt_local,
        )
        .unwrap();
        let _ = dav_ids::delete_collection(&conn, sid, dav_ids::CALENDAR_EVENT, "/cal/work/");
        let _ = dav_ids::delete_collection(&conn, sid, dav_ids::CALENDAR, "/cal/work/");
        delete_calendar_row(&conn, cal_local).unwrap();
        let cal_count: i64 = conn
            .query_row("SELECT count(*) FROM calendars", [], |r| r.get(0))
            .unwrap();
        let evt_count: i64 = conn
            .query_row("SELECT count(*) FROM calendar_events", [], |r| r.get(0))
            .unwrap();
        let dav_count: i64 = conn
            .query_row("SELECT count(*) FROM sync_id_dav", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cal_count, 0, "calendar row gone");
        assert_eq!(evt_count, 0, "event rows cascaded");
        assert_eq!(dav_count, 0, "sync_id_dav rows for both types cleared");
    }
}
