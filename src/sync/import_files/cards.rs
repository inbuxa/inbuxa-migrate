/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Address books from `contacts/*.vcf` and calendars from
//! `calendars/*.ics`, each named and set up as the manifest says: its real
//! name, description, color, time zone and whether it was the default. A
//! card or event in two files is one row in both collections.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::db::{cards, defaults, files_ids};
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::import_dav::calcard;
use crate::sync::import_takeout::contacts::split_vcards;
use crate::types::ObjectType;

use super::mail::row_exists;
use super::manifest::Collection;
use super::{Rerun, paths};

pub struct Outcome {
    pub collection: TypeCounts,
    pub item: TypeCounts,
}

const BOOK_COLUMNS: &str = "name, description, sort_order, is_default";
const CARD_COLUMNS: &str = "address_book_ids, data";
const CALENDAR_COLUMNS: &str = "name, description, color, time_zone, sort_order, is_default";
const EVENT_COLUMNS: &str = "calendar_ids, is_draft, use_default_alerts, data, data_type";

pub fn import_contacts(
    conn: &mut Connection,
    root: &Path,
    source_id: i64,
    books: &[Collection],
    logger: Logger,
) -> Result<Outcome, Error> {
    let mut out = Outcome {
        collection: TypeCounts::default(),
        item: TypeCounts::default(),
    };
    let mut collections = Rerun::default();
    let mut items = Rerun::default();
    let mut seen: HashMap<String, i64> = HashMap::new();
    for (order, book) in books.iter().enumerate() {
        let tx = conn.transaction()?;
        let book_id = ensure_collection(
            &tx,
            source_id,
            Kind::Contacts,
            book,
            order,
            &mut collections,
            &mut out.collection,
            logger,
        )?;
        let Some(text) = read_part_file(root, &book.file, "contacts", logger) else {
            out.item.failed += 1;
            tx.commit()?;
            continue;
        };
        let chunks = split_vcards(&text);
        for (ordinal, chunk) in chunks.iter().enumerate() {
            let card = match calcard::vcard_to_jscontact(chunk, &format!("{}#{ordinal}", book.file))
            {
                Ok(card) => card,
                Err(e) => {
                    logger.warn(&format!("{} card {ordinal}: {e}", book.file));
                    out.item.failed += 1;
                    continue;
                }
            };
            let data = card.data.to_string();
            if let Some(&id) = seen.get(&card.uid) {
                // The same card in another address book.
                add_to_collection(&tx, "contact_cards", "address_book_ids", id, book_id)?;
                continue;
            }
            let ours =
                match files_ids::local_for(&tx, source_id, files_ids::CONTACT_CARD, &card.uid)? {
                    Some(id) if row_exists(&tx, "contact_cards", id)? => Some(id),
                    _ => None,
                };
            // The uid is the archive's key for a card, whoever put it there.
            let theirs = match ours {
                Some(_) => None,
                None => tx
                    .query_row(
                        "SELECT id, address_book_ids FROM contact_cards WHERE uid = ?1",
                        params![card.uid],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()?,
            };
            let id = match (ours, theirs) {
                (Some(id), _) => {
                    // Read on an earlier run: now in the books it is in now.
                    items.before(&tx, "contact_cards", CARD_COLUMNS, id)?;
                    tx.execute(
                        "UPDATE contact_cards SET address_book_ids = ?1, data = ?2 WHERE id = ?3",
                        params![format!("[{book_id}]"), data, id],
                    )?;
                    id
                }
                (None, Some((id, books))) => {
                    // Another import's card: it keeps the books it was in.
                    items.before(&tx, "contact_cards", CARD_COLUMNS, id)?;
                    tx.execute(
                        "UPDATE contact_cards SET address_book_ids = ?1, data = ?2 WHERE id = ?3",
                        params![cards::with_book(&books, book_id), data, id],
                    )?;
                    id
                }
                (None, None) => {
                    tx.execute(
                        "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
                        params![card.uid, format!("[{book_id}]"), data],
                    )?;
                    out.item.created += 1;
                    out.item.fetched += 1;
                    tx.last_insert_rowid()
                }
            };
            files_ids::record(&tx, source_id, files_ids::CONTACT_CARD, &card.uid, id)?;
            seen.insert(card.uid, id);
        }
        tx.commit()?;
        listed_and_found(logger, &book.file, "cards", book.cards, chunks.len());
    }
    let tx = conn.transaction()?;
    collections.tally(&tx, "address_books", BOOK_COLUMNS, &mut out.collection)?;
    items.tally(&tx, "contact_cards", CARD_COLUMNS, &mut out.item)?;
    tx.commit()?;
    Ok(out)
}

pub fn import_calendars(
    conn: &mut Connection,
    root: &Path,
    source_id: i64,
    calendars: &[Collection],
    logger: Logger,
) -> Result<Outcome, Error> {
    let mut out = Outcome {
        collection: TypeCounts::default(),
        item: TypeCounts::default(),
    };
    let mut collections = Rerun::default();
    let mut items = Rerun::default();
    let mut seen: HashMap<String, i64> = HashMap::new();
    for (order, calendar) in calendars.iter().enumerate() {
        let tx = conn.transaction()?;
        let calendar_id = ensure_collection(
            &tx,
            source_id,
            Kind::Calendars,
            calendar,
            order,
            &mut collections,
            &mut out.collection,
            logger,
        )?;
        let Some(text) = read_part_file(root, &calendar.file, "calendars", logger) else {
            out.item.failed += 1;
            tx.commit()?;
            continue;
        };
        let entries = match calcard::ical_to_jscalendar_entries(&text) {
            Ok(entries) => entries,
            // A calendar with no events is written as an empty VCALENDAR.
            Err(calcard::CalcardError::NoEntries) => Vec::new(),
            Err(e) => {
                logger.warn(&format!("{}: {e}", calendar.file));
                out.item.failed += 1;
                tx.commit()?;
                continue;
            }
        };
        let found = entries.len();
        for (ordinal, mut entry) in entries.into_iter().enumerate() {
            let (is_draft, use_default_alerts, uid) =
                calcard::strip_extracted_fields_from_event(&mut entry.data);
            let uid = uid
                .map(|u| u.trim().to_owned())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| {
                    calcard::synthesise_uid(&format!("{}#{ordinal}", calendar.file))
                });
            // An occurrence sent on its own shares its series' uid.
            let key = match entry.data.get("recurrenceId").and_then(Value::as_str) {
                Some(recurrence) => serde_json::to_string(&[&uid, recurrence]).unwrap_or_default(),
                None => uid,
            };
            if let Some(&id) = seen.get(&key) {
                add_to_collection(&tx, "calendar_events", "calendar_ids", id, calendar_id)?;
                continue;
            }
            let ids = format!("[{calendar_id}]");
            let data = entry.data.to_string();
            let data_type = entry.data_type.as_column();
            let id = match files_ids::local_for(&tx, source_id, files_ids::CALENDAR_EVENT, &key)? {
                Some(id) if row_exists(&tx, "calendar_events", id)? => {
                    items.before(&tx, "calendar_events", EVENT_COLUMNS, id)?;
                    tx.execute(
                        "UPDATE calendar_events SET calendar_ids = ?1, is_draft = ?2,
                             use_default_alerts = ?3, data = ?4, data_type = ?5
                         WHERE id = ?6",
                        params![
                            ids,
                            is_draft as i64,
                            use_default_alerts as i64,
                            data,
                            data_type,
                            id
                        ],
                    )?;
                    id
                }
                _ => {
                    tx.execute(
                        "INSERT INTO calendar_events
                             (calendar_ids, is_draft, use_default_alerts, data, data_type)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            ids,
                            is_draft as i64,
                            use_default_alerts as i64,
                            data,
                            data_type
                        ],
                    )?;
                    out.item.created += 1;
                    out.item.fetched += 1;
                    tx.last_insert_rowid()
                }
            };
            files_ids::record(&tx, source_id, files_ids::CALENDAR_EVENT, &key, id)?;
            seen.insert(key, id);
        }
        tx.commit()?;
        listed_and_found(logger, &calendar.file, "events", calendar.events, found);
    }
    let tx = conn.transaction()?;
    collections.tally(&tx, "calendars", CALENDAR_COLUMNS, &mut out.collection)?;
    items.tally(&tx, "calendar_events", EVENT_COLUMNS, &mut out.item)?;
    tx.commit()?;
    Ok(out)
}

#[derive(Clone, Copy)]
enum Kind {
    Contacts,
    Calendars,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Kind::Contacts => "address_books",
            Kind::Calendars => "calendars",
        }
    }

    fn type_name(self) -> &'static str {
        match self {
            Kind::Contacts => files_ids::ADDRESS_BOOK,
            Kind::Calendars => files_ids::CALENDAR,
        }
    }

    fn object(self) -> ObjectType {
        match self {
            Kind::Contacts => ObjectType::AddressBook,
            Kind::Calendars => ObjectType::Calendar,
        }
    }
}

/// The address book or calendar for one manifest entry, keyed by its file.
#[allow(clippy::too_many_arguments)]
fn ensure_collection(
    conn: &Connection,
    source_id: i64,
    kind: Kind,
    entry: &Collection,
    order: usize,
    rerun: &mut Rerun,
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<i64, Error> {
    let table = kind.table();
    let existing = match files_ids::local_for(conn, source_id, kind.type_name(), &entry.file)? {
        Some(id) if row_exists(conn, table, id)? => Some(id),
        _ => None,
    };
    let is_default = defaults::unique_default(conn, kind.object(), entry.default, existing)?;
    if entry.default && !is_default {
        logger.warn(&format!(
            "{}: another {} is already the default, so this one is not",
            entry.file,
            match kind {
                Kind::Contacts => "address book",
                Kind::Calendars => "calendar",
            }
        ));
    }
    let blank = |v: &Option<String>| v.clone().filter(|s| !s.is_empty());
    let id = match (kind, existing) {
        (Kind::Contacts, Some(id)) => {
            rerun.before(conn, table, BOOK_COLUMNS, id)?;
            conn.execute(
                "UPDATE address_books SET name = ?1, description = ?2, sort_order = ?3,
                     is_default = ?4
                 WHERE id = ?5",
                params![
                    entry.name,
                    blank(&entry.description),
                    order as i64,
                    is_default as i64,
                    id
                ],
            )?;
            id
        }
        (Kind::Contacts, None) => {
            conn.execute(
                "INSERT INTO address_books (name, description, sort_order, is_default, is_subscribed)
                 VALUES (?1, ?2, ?3, ?4, 1)",
                params![
                    entry.name,
                    blank(&entry.description),
                    order as i64,
                    is_default as i64
                ],
            )?;
            counts.created += 1;
            counts.fetched += 1;
            conn.last_insert_rowid()
        }
        (Kind::Calendars, Some(id)) => {
            rerun.before(conn, table, CALENDAR_COLUMNS, id)?;
            conn.execute(
                "UPDATE calendars SET name = ?1, description = ?2, color = ?3, time_zone = ?4,
                     sort_order = ?5, is_default = ?6
                 WHERE id = ?7",
                params![
                    entry.name,
                    blank(&entry.description),
                    blank(&entry.color),
                    blank(&entry.time_zone),
                    order as i64,
                    is_default as i64,
                    id
                ],
            )?;
            id
        }
        (Kind::Calendars, None) => {
            conn.execute(
                "INSERT INTO calendars
                     (name, description, color, time_zone, sort_order, is_subscribed,
                      is_visible, is_default)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, 1, ?6)",
                params![
                    entry.name,
                    blank(&entry.description),
                    blank(&entry.color),
                    blank(&entry.time_zone),
                    order as i64,
                    is_default as i64
                ],
            )?;
            counts.created += 1;
            counts.fetched += 1;
            conn.last_insert_rowid()
        }
    };
    files_ids::record(conn, source_id, kind.type_name(), &entry.file, id)?;
    Ok(id)
}

fn add_to_collection(
    conn: &Connection,
    table: &str,
    column: &str,
    id: i64,
    collection: i64,
) -> Result<(), Error> {
    let ids: String = conn.query_row(
        &format!("SELECT {column} FROM {table} WHERE id = ?1"),
        params![id],
        |r| r.get(0),
    )?;
    conn.execute(
        &format!("UPDATE {table} SET {column} = ?1 WHERE id = ?2"),
        params![cards::with_book(&ids, collection), id],
    )?;
    Ok(())
}

/// A text file the manifest names, if it is safely inside `part`.
pub(super) fn read_part_file(root: &Path, rel: &str, part: &str, logger: Logger) -> Option<String> {
    let path = match paths::resolve(root, rel, part) {
        Ok(path) if paths::is_real_file(&path) => path,
        Ok(_) => {
            logger.warn(&format!("{rel}: not a file"));
            return None;
        }
        Err(reason) => {
            logger.warn(&reason);
            return None;
        }
    };
    match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) => {
            logger.warn(&format!("{rel}: {e}"));
            None
        }
    }
}

fn listed_and_found(logger: Logger, file: &str, what: &str, listed: Option<u64>, found: usize) {
    if let Some(listed) = listed
        && listed != found as u64
    {
        logger.warn(&format!(
            "{file}: the manifest lists {listed} {what} and {found} were found"
        ));
    }
}
