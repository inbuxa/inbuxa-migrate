/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::db::takeout_ids;
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::import_dav::calcard;

const DEFAULT_CALENDAR: &str = "Imported";

pub fn process_file(
    conn: &mut Connection,
    file_path: &Path,
    source_id: i64,
    calendar_cache: &mut HashMap<String, i64>,
    calendar_counts: &mut TypeCounts,
    event_counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    let text = match std::fs::read_to_string(file_path) {
        Ok(s) => s,
        Err(e) => {
            logger.warn(&format!("{file_path:?}: read: {e}"));
            event_counts.failed += 1;
            return Ok(());
        }
    };

    let entries = match calcard::ical_to_jscalendar_entries(&text) {
        Ok(v) => v,
        Err(calcard::CalcardError::NoEntries) => {
            logger.warn(&format!("{file_path:?}: iCalendar contains no events"));
            return Ok(());
        }
        Err(e) => {
            logger.warn(&format!("{file_path:?}: iCalendar parse: {e}"));
            event_counts.failed += 1;
            return Ok(());
        }
    };

    let metadata = parse_calendar_metadata(&text);
    let container_name = metadata
        .x_wr_calname
        .clone()
        .unwrap_or_else(|| DEFAULT_CALENDAR.to_owned());

    let tx = conn.transaction()?;
    let calendar_local = ensure_calendar(
        &tx,
        source_id,
        &container_name,
        &metadata,
        calendar_cache,
        calendar_counts,
    )?;

    for (ordinal, mut entry) in entries.into_iter().enumerate() {
        match insert_event(
            &tx,
            source_id,
            calendar_local,
            file_path,
            ordinal,
            &mut entry,
            event_counts,
        ) {
            Ok(()) => {}
            Err(e) => {
                logger.warn(&format!("{file_path:?}#{ordinal}: event insert: {e}"));
                event_counts.failed += 1;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Default, Clone)]
struct CalendarMetadata {
    x_wr_calname: Option<String>,
    x_wr_caldesc: Option<String>,
    x_wr_timezone: Option<String>,
}

fn ensure_calendar(
    tx: &Transaction<'_>,
    source_id: i64,
    name: &str,
    metadata: &CalendarMetadata,
    cache: &mut HashMap<String, i64>,
    counts: &mut TypeCounts,
) -> Result<i64, Error> {
    if let Some(&id) = cache.get(name) {
        return Ok(id);
    }
    if let Some(id) = takeout_ids::local_for(tx, source_id, takeout_ids::CALENDAR, name)? {
        cache.insert(name.to_owned(), id);
        return Ok(id);
    }
    tx.execute(
        "INSERT INTO calendars
           (name, description, time_zone, sort_order, is_subscribed, is_visible, is_default)
         VALUES (?1, ?2, ?3, 0, 1, 1, 0)",
        params![name, metadata.x_wr_caldesc, metadata.x_wr_timezone],
    )?;
    let id = tx.last_insert_rowid();
    takeout_ids::insert(tx, source_id, takeout_ids::CALENDAR, name, id)?;
    cache.insert(name.to_owned(), id);
    counts.created += 1;
    counts.fetched += 1;
    Ok(id)
}

fn insert_event(
    tx: &Transaction<'_>,
    source_id: i64,
    calendar_local: i64,
    file_path: &Path,
    ordinal: usize,
    entry: &mut calcard::JsCalendarEntry,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let (is_draft, use_default_alerts, uid_field) =
        calcard::strip_extracted_fields_from_event(&mut entry.data);
    let uid = match uid_field
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(u) => u.to_owned(),
        None => calcard::synthesise_uid(&format!("{}#{ordinal}", file_path.to_string_lossy())),
    };
    let data_type = entry.data_type.as_column();
    let data_json = entry.data.to_string();

    let existing = takeout_ids::local_for(tx, source_id, takeout_ids::CALENDAR_EVENT, &uid)?;

    if let Some(local) = existing {
        let calendar_ids =
            merge_container_id(tx, "calendar_events", "calendar_ids", local, calendar_local)?;
        let current: (String, i64, i64, String, String) = tx.query_row(
            "SELECT calendar_ids, is_draft, use_default_alerts, data, data_type
             FROM calendar_events WHERE id = ?1",
            params![local],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let want = (
            calendar_ids.clone(),
            is_draft as i64,
            use_default_alerts as i64,
            data_json.clone(),
            data_type.to_owned(),
        );
        if current == want {
            counts.skipped += 1;
            return Ok(());
        }
        tx.execute(
            "UPDATE calendar_events SET calendar_ids = ?1, is_draft = ?2,
                                          use_default_alerts = ?3, data = ?4, data_type = ?5
             WHERE id = ?6",
            params![
                calendar_ids,
                is_draft as i64,
                use_default_alerts as i64,
                data_json,
                data_type,
                local,
            ],
        )?;
        counts.fetched += 1;
        return Ok(());
    }

    let calendar_ids = format!("[{calendar_local}]");
    tx.execute(
        "INSERT INTO calendar_events
            (calendar_ids, is_draft, use_default_alerts, data, data_type)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            calendar_ids,
            is_draft as i64,
            use_default_alerts as i64,
            data_json,
            data_type,
        ],
    )?;
    let local_id = tx.last_insert_rowid();
    takeout_ids::insert(tx, source_id, takeout_ids::CALENDAR_EVENT, &uid, local_id)?;
    counts.created += 1;
    counts.fetched += 1;
    Ok(())
}

fn merge_container_id(
    tx: &Transaction<'_>,
    table: &str,
    column: &str,
    local_id: i64,
    new_id: i64,
) -> Result<String, Error> {
    let current: String = tx
        .query_row(
            &format!("SELECT {column} FROM {table} WHERE id = ?1"),
            params![local_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "[]".to_owned());
    let mut ids: Vec<i64> = match serde_json::from_str::<Value>(&current) {
        Ok(Value::Array(arr)) => arr.into_iter().filter_map(|v| v.as_i64()).collect(),
        _ => Vec::new(),
    };
    if !ids.contains(&new_id) {
        ids.push(new_id);
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(Value::Array(ids.into_iter().map(Value::from).collect()).to_string())
}

fn parse_calendar_metadata(text: &str) -> CalendarMetadata {
    let mut out = CalendarMetadata::default();
    let mut inside_vevent = false;
    for line in unfolded_lines(text) {
        let line = line.as_str();
        if line.starts_with("BEGIN:VEVENT")
            || line.starts_with("BEGIN:VTODO")
            || line.starts_with("BEGIN:VJOURNAL")
        {
            inside_vevent = true;
            continue;
        }
        if line.starts_with("END:VEVENT")
            || line.starts_with("END:VTODO")
            || line.starts_with("END:VJOURNAL")
        {
            inside_vevent = false;
            continue;
        }
        if inside_vevent {
            continue;
        }
        if let Some(rest) = line.strip_prefix("X-WR-CALNAME:") {
            out.x_wr_calname = Some(unescape_text(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("X-WR-CALDESC:") {
            out.x_wr_caldesc = Some(unescape_text(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("X-WR-TIMEZONE:") {
            out.x_wr_timezone = Some(rest.trim().to_owned());
        }
    }
    out
}

/// Content lines with RFC 5545 folding undone.
fn unfolded_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        match (line.strip_prefix([' ', '\t']), out.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => out.push(line.to_owned()),
        }
    }
    out
}

/// An iCalendar TEXT value with its `\\`, `\;`, `\,` and `\n` escapes undone.
fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_metadata_unfolds_and_unescapes() {
        let meta = parse_calendar_metadata(
            "BEGIN:VCALENDAR\r\nX-WR-CALNAME:Team\\, Berlin\\; o\r\n ps\r\n\
             X-WR-CALDESC:one\\ntwo\\\\three\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(meta.x_wr_calname.as_deref(), Some("Team, Berlin; ops"));
        assert_eq!(meta.x_wr_caldesc.as_deref(), Some("one\ntwo\\three"));
    }

    #[test]
    fn parse_metadata_finds_xwr_fields() {
        let ical = "BEGIN:VCALENDAR\r\n\
            PRODID:-//Google Inc//Google Calendar 70.9054//EN\r\n\
            X-WR-CALNAME:My Calendar\r\n\
            X-WR-CALDESC:Some description\r\n\
            X-WR-TIMEZONE:Europe/Madrid\r\n\
            BEGIN:VEVENT\r\nUID:e1@google.com\r\nDTSTAMP:20250101T000000Z\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let m = parse_calendar_metadata(ical);
        assert_eq!(m.x_wr_calname.as_deref(), Some("My Calendar"));
        assert_eq!(m.x_wr_caldesc.as_deref(), Some("Some description"));
        assert_eq!(m.x_wr_timezone.as_deref(), Some("Europe/Madrid"));
    }

    #[test]
    fn parse_metadata_ignores_xwr_inside_vevent() {
        let ical = "BEGIN:VCALENDAR\r\n\
            BEGIN:VEVENT\r\nUID:e1\r\nX-WR-CALNAME:NOT THIS\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let m = parse_calendar_metadata(ical);
        assert!(m.x_wr_calname.is_none());
    }

    #[test]
    fn parse_metadata_empty_when_missing() {
        let ical = "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n";
        let m = parse_calendar_metadata(ical);
        assert!(m.x_wr_calname.is_none());
        assert!(m.x_wr_caldesc.is_none());
        assert!(m.x_wr_timezone.is_none());
    }

    #[test]
    fn parse_metadata_handles_lf_only_line_endings() {
        let ical = "BEGIN:VCALENDAR\nX-WR-CALNAME:Foo\nEND:VCALENDAR\n";
        let m = parse_calendar_metadata(ical);
        assert_eq!(m.x_wr_calname.as_deref(), Some("Foo"));
    }
}
