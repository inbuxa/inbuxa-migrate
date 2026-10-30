/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Address books as `.vcf` files and calendars as `.ics` files, one per
//! collection, converted back from the JSContact and JSCalendar the archive
//! holds.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use calcard::jscalendar::JSCalendar;
use calcard::jscontact::JSContact;
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::db::blobs;
use crate::error::Error;
use crate::jmap::blob::{BlobWalkError, InlineShape, inline_blob_data_uris};
use crate::jmap::error::JmapError;
use crate::logging::Logger;
use crate::sync::TypeCounts;

use super::paths::{Names, safe_component};

pub struct Outcome {
    pub collection: TypeCounts,
    pub item: TypeCounts,
    pub manifest: Value,
}

pub(crate) struct Collection {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) color: Option<String>,
    pub(crate) time_zone: Option<String>,
    pub(crate) is_default: bool,
}

pub fn write_contacts(
    conn: &Connection,
    root: &Path,
    dry_run: bool,
    logger: Logger,
) -> Result<Outcome, Error> {
    let books = read_collections(
        conn,
        "SELECT id, name, description, NULL, NULL, is_default FROM address_books ORDER BY sort_order, id",
    )?;
    let mut per_book: HashMap<i64, Vec<String>> = HashMap::new();
    let mut item = TypeCounts::default();
    let mut stmt =
        conn.prepare("SELECT id, uid, address_book_ids, data FROM contact_cards ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (id, uid, book_ids, data) = row?;
        let text = match vcard_text(conn, &uid, &data) {
            Ok(text) => text,
            Err(reason) => {
                logger.warn(&format!(
                    "contact {id} ({uid}): not written as a vCard: {reason}"
                ));
                item.failed += 1;
                continue;
            }
        };
        let book_ids: Vec<i64> = serde_json::from_str(&book_ids).unwrap_or_default();
        for book in book_ids
            .iter()
            .filter(|b| books.iter().any(|c| c.id == **b))
        {
            per_book.entry(*book).or_default().push(text.clone());
        }
        item.created += 1;
    }

    let mut names = Names::default();
    let mut manifest = Vec::new();
    for book in &books {
        let file = names.unique(&safe_component(&book.name), ".vcf");
        let cards = per_book.remove(&book.id).unwrap_or_default();
        if !dry_run {
            fs::create_dir_all(root)?;
            fs::write(root.join(&file), cards.concat())?;
        }
        manifest.push(json!({
            "name": book.name,
            "description": book.description,
            "file": format!("contacts/{file}"),
            "default": book.is_default,
            "cards": cards.len(),
        }));
    }
    Ok(Outcome {
        collection: TypeCounts {
            created: books.len() as u64,
            ..Default::default()
        },
        item,
        manifest: Value::Array(manifest),
    })
}

pub fn write_calendars(
    conn: &Connection,
    root: &Path,
    dry_run: bool,
    logger: Logger,
) -> Result<Outcome, Error> {
    let calendars = read_collections(
        conn,
        "SELECT id, name, description, color, time_zone, is_default FROM calendars ORDER BY sort_order, id",
    )?;
    let mut per_calendar: HashMap<i64, Vec<String>> = HashMap::new();
    let mut item = TypeCounts::default();
    let mut stmt =
        conn.prepare("SELECT id, calendar_ids, data, data_type FROM calendar_events ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (id, calendar_ids, data, data_type) = row?;
        let text = match icalendar_text(conn, &data, &data_type) {
            Ok(text) => text,
            Err(reason) => {
                logger.warn(&format!("event {id}: not written as iCalendar: {reason}"));
                item.failed += 1;
                continue;
            }
        };
        let calendar_ids: Vec<i64> = serde_json::from_str(&calendar_ids).unwrap_or_default();
        for calendar in calendar_ids
            .iter()
            .filter(|c| calendars.iter().any(|k| k.id == **c))
        {
            per_calendar
                .entry(*calendar)
                .or_default()
                .push(text.clone());
        }
        item.created += 1;
    }

    let mut names = Names::default();
    let mut manifest = Vec::new();
    for calendar in &calendars {
        let file = names.unique(&safe_component(&calendar.name), ".ics");
        let events = per_calendar.remove(&calendar.id).unwrap_or_default();
        if !dry_run {
            fs::create_dir_all(root)?;
            fs::write(root.join(&file), merge_calendar(calendar, &events))?;
        }
        manifest.push(json!({
            "name": calendar.name,
            "description": calendar.description,
            "color": calendar.color,
            "timeZone": calendar.time_zone,
            "file": format!("calendars/{file}"),
            "default": calendar.is_default,
            "events": events.len(),
        }));
    }
    Ok(Outcome {
        collection: TypeCounts {
            created: calendars.len() as u64,
            ..Default::default()
        },
        item,
        manifest: Value::Array(manifest),
    })
}

pub(crate) fn read_collections(conn: &Connection, sql: &str) -> Result<Vec<Collection>, Error> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(Collection {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            color: row.get(3)?,
            time_zone: row.get(4)?,
            is_default: row.get::<_, i64>(5)? != 0,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The archive keeps a card's uid beside its JSContact, and photos and
/// other media as blobs; the vCard needs both back inside.
pub(crate) fn vcard_text(conn: &Connection, uid: &str, data: &str) -> Result<String, String> {
    let mut value: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
    let map = value.as_object_mut().ok_or("not a JSON object")?;
    map.insert("uid".to_owned(), Value::String(uid.to_owned()));
    map.entry("@type").or_insert_with(|| json!("Card"));
    inline_blobs(conn, &mut value, InlineShape::JsContactResource)?;
    let json = value.to_string();
    let card = JSContact::<String, String>::parse(&json)?;
    Ok(card.into_vcard().ok_or("no vCard came of it")?.to_string())
}

/// calcard writes a VCALENDAR from a Group, so each event goes in one, with
/// its attachments inlined from the archive's blobs.
pub(crate) fn icalendar_text(
    conn: &Connection,
    data: &str,
    data_type: &str,
) -> Result<String, String> {
    let mut value: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
    value
        .as_object_mut()
        .ok_or("not a JSON object")?
        .entry("@type")
        .or_insert_with(|| json!(data_type));
    inline_blobs(conn, &mut value, InlineShape::JsCalendarLink)?;
    let group = if data_type == "Group" {
        value
    } else {
        json!({ "@type": "Group", "entries": [value] })
    };
    group_text(group)
}

/// Several events as one iCalendar object, each `(data, data type)`: the
/// occurrences of one series that were sent on their own, which a CalDAV
/// resource holds together under their UID.
pub(crate) fn icalendar_entries_text(
    conn: &Connection,
    entries: &[(&str, &str)],
) -> Result<String, String> {
    let mut values = Vec::with_capacity(entries.len());
    for (data, data_type) in entries {
        let mut value: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
        value
            .as_object_mut()
            .ok_or("not a JSON object")?
            .entry("@type")
            .or_insert_with(|| json!(data_type));
        inline_blobs(conn, &mut value, InlineShape::JsCalendarLink)?;
        values.push(value);
    }
    group_text(json!({ "@type": "Group", "entries": values }))
}

fn group_text(group: Value) -> Result<String, String> {
    let json = group.to_string();
    let event = JSCalendar::<String, String>::parse(&json)?;
    Ok(event
        .into_icalendar()
        .ok_or("no iCalendar came of it")?
        .to_string())
}

/// Blobs the archive refers to, as `data:` URIs, the way `export` sends them.
fn inline_blobs(conn: &Connection, value: &mut Value, shape: InlineShape) -> Result<(), String> {
    inline_blob_data_uris(value, shape, |local_id| {
        match blobs::blob_bytes(conn, local_id) {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(BlobWalkError::resolver(JmapError::Transport(format!(
                "blob {local_id} is missing from the archive"
            )))),
            Err(e) => Err(BlobWalkError::resolver(JmapError::Transport(e.to_string()))),
        }
    })
    .map_err(|e| e.to_string())
}

/// One VCALENDAR for a whole calendar: its name and details as the
/// `X-WR-*` properties calendar apps read, then every event's components,
/// each time zone once.
fn merge_calendar(calendar: &Collection, events: &[String]) -> String {
    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n");
    out.push_str("PRODID:-//Coffey Labs//inbuxa-migrate//EN\r\n");
    push_property(&mut out, "X-WR-CALNAME", &calendar.name);
    if let Some(description) = calendar.description.as_deref().filter(|d| !d.is_empty()) {
        push_property(&mut out, "X-WR-CALDESC", description);
    }
    if let Some(tz) = calendar.time_zone.as_deref().filter(|t| !t.is_empty()) {
        push_property(&mut out, "X-WR-TIMEZONE", tz);
    }
    if let Some(color) = calendar.color.as_deref().filter(|c| !c.is_empty()) {
        push_property(&mut out, "X-APPLE-CALENDAR-COLOR", color);
    }
    let mut zones: HashSet<String> = HashSet::new();
    for event in events {
        for component in components(event) {
            if let Some(tzid) = component.tzid
                && !zones.insert(tzid)
            {
                continue;
            }
            out.push_str(&component.text);
        }
    }
    out.push_str("END:VCALENDAR\r\n");
    out
}

struct Component {
    tzid: Option<String>,
    text: String,
}

/// The components directly inside an iCalendar object's VCALENDAR, each
/// with its lines as they were (folded lines stay folded).
fn components(ics: &str) -> Vec<Component> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut current: Option<(String, Component)> = None;
    for line in ics.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        let upper = bare.to_ascii_uppercase();
        let continuation = bare.starts_with([' ', '\t']);
        if !continuation && upper.starts_with("BEGIN:") {
            depth += 1;
            if depth == 2 {
                current = Some((
                    upper[6..].to_owned(),
                    Component {
                        tzid: None,
                        text: String::new(),
                    },
                ));
            }
        }
        if depth >= 2
            && let Some((name, component)) = current.as_mut()
        {
            component.text.push_str(bare);
            component.text.push_str("\r\n");
            if depth == 2 && name == "VTIMEZONE" && !continuation && upper.starts_with("TZID") {
                component.tzid = bare.split_once(':').map(|(_, v)| v.to_owned());
            }
        }
        if !continuation && upper.starts_with("END:") {
            if depth == 2
                && let Some((_, component)) = current.take()
            {
                out.push(component);
            }
            depth = depth.saturating_sub(1);
        }
    }
    out
}

fn push_property(out: &mut String, name: &str, value: &str) {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            ';' => escaped.push_str("\\;"),
            ',' => escaped.push_str("\\,"),
            '\n' => escaped.push_str("\\n"),
            '\r' => {}
            c => escaped.push(c),
        }
    }
    out.push_str(&fold(&format!("{name}:{escaped}")));
}

/// RFC 5545 folding: lines of at most 75 octets, never splitting a
/// character.
fn fold(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 8);
    let mut width = 0;
    for c in line.chars() {
        let len = c.len_utf8();
        if width + len > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += len;
    }
    out.push_str("\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT_A: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\n\
BEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nBEGIN:STANDARD\r\nDTSTART:19701025T030000\r\n\
TZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:A long\r\n  folded summary\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\n\
END:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    const EVENT_B: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
BEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nUID:b\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn archive() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init::apply_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn a_photo_kept_as_a_blob_goes_into_the_vcard() {
        let conn = archive();
        let blob = blobs::intern_blob(&conn, b"\x89PNG fake").unwrap();
        let data = json!({
            "@type": "Card",
            "name": { "full": "Carol" },
            "media": { "p": { "kind": "photo", "@blob": blob, "mediaType": "image/png" } }
        });
        let text = vcard_text(&conn, "card-carol", &data.to_string()).unwrap();
        assert!(text.contains("PHOTO"), "{text}");
        // calcard escapes the comma inside the URI, as it does for CardDAV.
        assert!(text.contains("data:image/png;base64"), "{text}");
        assert!(!text.contains("@blob"), "{text}");
    }

    #[test]
    fn a_missing_blob_fails_the_item_with_a_reason() {
        let data = json!({ "@type": "Card", "media": { "p": { "kind": "photo", "@blob": 99 } } });
        let err = vcard_text(&archive(), "x", &data.to_string()).unwrap_err();
        assert!(err.contains("blob 99"), "{err}");
    }

    fn calendar() -> Collection {
        Collection {
            id: 1,
            name: "Team, Berlin; ops".to_owned(),
            description: Some("Line one\nline two".to_owned()),
            color: Some("#ff0000".to_owned()),
            time_zone: Some("Europe/Berlin".to_owned()),
            is_default: true,
        }
    }

    #[test]
    fn events_merge_into_one_calendar_with_each_zone_once() {
        let ics = merge_calendar(&calendar(), &[EVENT_A.to_owned(), EVENT_B.to_owned()]);
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n"));
        assert!(ics.ends_with("END:VCALENDAR\r\n"));
        assert_eq!(ics.matches("BEGIN:VCALENDAR").count(), 1);
        assert_eq!(ics.matches("BEGIN:VTIMEZONE").count(), 1);
        assert_eq!(ics.matches("BEGIN:VEVENT").count(), 2);
        assert_eq!(ics.matches("PRODID:").count(), 1);
        assert!(ics.contains("SUMMARY:A long\r\n  folded summary\r\n"));
        assert!(ics.contains("BEGIN:VALARM\r\nACTION:DISPLAY\r\nEND:VALARM\r\n"));
        assert!(ics.contains("X-WR-CALNAME:Team\\, Berlin\\; ops\r\n"));
        assert!(ics.contains("X-WR-CALDESC:Line one\\nline two\r\n"));
        assert!(ics.contains("X-WR-TIMEZONE:Europe/Berlin\r\n"));
    }

    #[test]
    fn long_lines_fold_on_character_boundaries() {
        let folded = fold(&format!("X-WR-CALNAME:{}", "é".repeat(60)));
        for line in folded.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(line.len() <= 75, "{} octets", line.len());
        }
        let unfolded = folded.replace("\r\n ", "");
        assert_eq!(unfolded, format!("X-WR-CALNAME:{}\r\n", "é".repeat(60)));
    }

    #[test]
    fn a_card_round_trips_through_jscontact() {
        let card = crate::sync::import_dav::calcard::vcard_to_jscontact(
            "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:abc-123\r\nFN:Alice Smith\r\n\
EMAIL:alice@example.com\r\nEND:VCARD\r\n",
            "/x",
        )
        .unwrap();
        let text = vcard_text(&archive(), &card.uid, &card.data.to_string()).unwrap();
        assert!(text.contains("VERSION:4.0"), "{text}");
        assert!(text.contains("UID:abc-123"), "{text}");
        assert!(text.contains("FN:Alice Smith"), "{text}");
        assert!(text.contains("alice@example.com"), "{text}");
    }

    #[test]
    fn an_event_round_trips_through_jscalendar() {
        let entries = crate::sync::import_dav::calcard::ical_to_jscalendar_entries(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\nBEGIN:VEVENT\r\nUID:ev-1\r\n\
DTSTAMP:20240101T000000Z\r\nDTSTART;TZID=Europe/Berlin:20240301T090000\r\n\
DURATION:PT1H\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n",
        )
        .unwrap();
        let mut data = entries[0].data.clone();
        crate::sync::import_dav::calcard::strip_extracted_fields_from_event(&mut data);
        let text = icalendar_text(&archive(), &data.to_string(), "Event").unwrap();
        assert!(text.contains("UID:ev-1"), "{text}");
        assert!(text.contains("SUMMARY:Standup"), "{text}");
        assert!(text.contains("FREQ=WEEKLY"), "{text}");
        assert!(text.contains("Europe/Berlin"), "{text}");
    }
}
