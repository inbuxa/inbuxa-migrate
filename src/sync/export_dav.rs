/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-caldav` and `export-carddav`: an archive's calendars and address
//! books written into any CalDAV or CardDAV server, converted back to
//! iCalendar and vCard as `export-files` writes them. Like `export-imap`,
//! it keeps no state. A collection is found by its display name or made;
//! each item goes to a name taken from its UID, so every run finds what the
//! last one wrote, writes what is missing or differs, and leaves the rest.
//! Nothing is deleted.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::dav::client::{DavClient, PutCondition};
use crate::dav::discover::{DavKind, discover_home};
use crate::dav::href::{Href, absolute_url, normalise};
use crate::dav::xml;
use crate::error::Error;
use crate::export_files::cards::{
    Collection, icalendar_entries_text, icalendar_text, read_collections, vcard_text,
};
use crate::jmap::error::JmapError;
use crate::jmap::http::RetryPolicy;
use crate::logging::{LEVEL_PROGRESS, Logger};
use crate::net::CertOverride;
use crate::sync::import_dav::coordinator::enforce_tls_policy;
use crate::sync::import_dav::{DavAuth, DavKindArg};
use crate::sync::keys::blake3_bytes;
use crate::sync::{CommonConfig, Summary, TypeCounts};

pub struct ExportDavConfig {
    /// `Caldav` or `Carddav`; files are not written over WebDAV.
    pub kind: DavKindArg,
    pub url: String,
    pub auth: DavAuth,
    pub allow_cleartext: bool,
}

/// What differs between the two kinds.
struct Kind {
    dav: DavKind,
    collection_type: &'static str,
    item_type: &'static str,
    collections_sql: &'static str,
    extension: &'static str,
    content_type: &'static str,
    make_method: &'static str,
}

const CALDAV: Kind = Kind {
    dav: DavKind::Caldav,
    collection_type: "calendar",
    item_type: "calendarevent",
    collections_sql: "SELECT id, name, description, color, time_zone, is_default \
                      FROM calendars ORDER BY sort_order, id",
    extension: ".ics",
    content_type: "text/calendar; charset=utf-8",
    make_method: "MKCALENDAR",
};

const CARDDAV: Kind = Kind {
    dav: DavKind::Carddav,
    collection_type: "addressbook",
    item_type: "contactcard",
    collections_sql: "SELECT id, name, description, NULL, NULL, is_default \
                      FROM address_books ORDER BY sort_order, id",
    extension: ".vcf",
    content_type: "text/vcard; charset=utf-8",
    make_method: "MKCOL",
};

/// One card or event as the server will get it.
struct Item {
    uid: String,
    text: String,
    collections: Vec<i64>,
    /// An event's `(data, data type)`, for writing it with the other
    /// occurrences that share its UID.
    event: Option<(String, String)>,
}

/// id, uid (cards), collection ids, data, data type (events).
type Row = (i64, Option<String>, String, String, Option<String>);

fn read_items(
    conn: &Connection,
    kind: &Kind,
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<Vec<Item>, Error> {
    let sql = if matches!(kind.dav, DavKind::Caldav) {
        "SELECT id, NULL, calendar_ids, data, data_type FROM calendar_events ORDER BY id"
    } else {
        "SELECT id, uid, address_book_ids, data, NULL FROM contact_cards ORDER BY id"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, uid, collections, data, data_type) in rows {
        let converted = match &uid {
            Some(uid) => vcard_text(conn, uid, &data),
            None => icalendar_text(conn, &data, data_type.as_deref().unwrap_or("Event")),
        };
        let text = match converted {
            Ok(text) => text,
            Err(reason) => {
                logger.warn(&format!(
                    "{} {id}: can't be converted: {reason}",
                    kind.item_type
                ));
                counts.failed += 1;
                continue;
            }
        };
        let Some(uid) = uid.or_else(|| first_uid(&text)) else {
            logger.warn(&format!("{} {id}: has no UID; left out", kind.item_type));
            counts.failed += 1;
            continue;
        };
        let event = data_type.map(|t| (data, t));
        out.push(Item {
            uid,
            text,
            collections: serde_json::from_str(&collections).unwrap_or_default(),
            event,
        });
    }
    Ok(out)
}

/// iCalendar or vCard lines with folded lines joined back up.
fn unfolded(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(rest);
            continue;
        }
        if !line.is_empty() {
            lines.push(line.to_owned());
        }
    }
    lines
}

fn first_uid(text: &str) -> Option<String> {
    unfolded(text).into_iter().find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.eq_ignore_ascii_case("UID")
            .then(|| value.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

/// A content line with its name and parameter names in capitals and its
/// parameters sorted, since servers write them in their own order, and
/// without empty components at the end of its value, which some servers
/// drop (vCard 4's `N` has seven, vCard 3's five). `N` and `ADR` are
/// compared on the parts vCard 3 has: a server that keeps only those cuts
/// the rest off every time, so sending them again would change nothing. A
/// quoted parameter value keeps its `;` and `:`.
fn canonical_line(line: &str) -> String {
    let mut parts: Vec<String> = vec![String::new()];
    let mut quoted = false;
    let mut value = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => {
                quoted = !quoted;
                parts.last_mut().unwrap().push(c);
            }
            ';' if !quoted => parts.push(String::new()),
            ':' if !quoted => {
                value = Some(&line[i + 1..]);
                break;
            }
            c => parts.last_mut().unwrap().push(c),
        }
    }
    let name = parts.remove(0).to_ascii_uppercase();
    let mut params: Vec<String> = parts
        .into_iter()
        .map(|p| match p.split_once('=') {
            Some((k, v)) => format!("{}={v}", k.to_ascii_uppercase()),
            None => p.to_ascii_uppercase(),
        })
        .collect();
    params.sort();
    let mut out = name;
    for p in params {
        out.push(';');
        out.push_str(&p);
    }
    let mut value = value.unwrap_or("");
    let parts = match out.split(';').next() {
        Some("N") => Some(5),
        Some("ADR") => Some(7),
        _ => None,
    };
    if let Some(n) = parts {
        value = first_components(value, n);
    }
    out.push(':');
    while let Some(rest) = value.strip_suffix(';') {
        if rest.ends_with('\\') {
            break;
        }
        value = rest;
    }
    out.push_str(value);
    out
}

/// A structured value up to its `n`th unescaped `;`.
fn first_components(value: &str, n: usize) -> &str {
    let mut seen = 0;
    let mut escaped = false;
    for (i, c) in value.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            ';' if !escaped => {
                seen += 1;
                if seen == n {
                    return &value[..i];
                }
            }
            _ => escaped = false,
        }
    }
    value
}

/// The same object for this purpose: the same lines in any order, leaving
/// out the ones a server or a converter stamps on its own.
fn same_object(a: &str, b: &str) -> bool {
    let comparable = |text: &str| {
        let mut lines: Vec<String> = unfolded(text)
            .iter()
            .map(|l| canonical_line(l))
            .filter(|l| {
                let name = l.split([':', ';']).next().unwrap_or("");
                !matches!(name, "PRODID" | "DTSTAMP" | "REV")
            })
            .collect();
        lines.sort();
        lines
    };
    comparable(a) == comparable(b)
}

/// A path segment for a name the source chose: kept when it is plain and
/// short, else replaced by its hash, so the same name always lands on the
/// same segment.
fn segment(name: &str) -> String {
    let plain = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if plain {
        name.to_owned()
    } else {
        blake3_bytes(name.as_bytes())[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

fn slug(name: &str) -> String {
    let lowered: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = lowered.trim_matches('-');
    if trimmed.is_empty() {
        segment(name)
    } else {
        segment(trimmed)
    }
}

/// The server's answer to one write, as this run counts it.
enum Write {
    Done,
    Refused(String),
}

fn write_result(r: Result<impl Sized, JmapError>) -> Result<Write, Error> {
    match r {
        Ok(_) => Ok(Write::Done),
        Err(JmapError::HttpStatus { status, body }) => Ok(Write::Refused(format!(
            "http {status}{}",
            if body.is_empty() {
                String::new()
            } else {
                format!(": {}", body.trim())
            }
        ))),
        Err(e) => Err(e.into()),
    }
}

struct Target {
    url: String,
    /// Each item already there, by href, with its etag.
    items: HashMap<String, Option<String>>,
}

pub fn run(common: CommonConfig, config: ExportDavConfig) -> Result<Summary, Error> {
    let kind = match config.kind {
        DavKindArg::Caldav => &CALDAV,
        DavKindArg::Carddav => &CARDDAV,
        DavKindArg::Webdav => {
            return Err(Error::Usage(
                "files are written with export-files, not over WebDAV".to_owned(),
            ));
        }
    };
    if !common.archive.is_file() {
        return Err(Error::Usage(format!(
            "no archive at {}",
            common.archive.display()
        )));
    }
    enforce_tls_policy(&config.url, config.allow_cleartext)?;
    let conn = crate::db::init::open(&common.archive)?;
    let logger = common.logger;
    let dry_run = common.dry_run;
    let mut collection_counts = TypeCounts::default();
    let mut item_counts = TypeCounts::default();
    let collections = read_collections(&conn, kind.collections_sql)?;
    let items = read_items(&conn, kind, &mut item_counts, logger)?;

    let client = DavClient::new(
        config.auth.to_jmap_auth(),
        RetryPolicy::new(common.max_retries),
        CertOverride::for_url(common.allow_invalid_certs, &config.url),
    );
    client.set_logger(logger);
    let home = discover_home(&client, kind.dav, &config.url)
        .map_err(|e| Error::Connection(format!("finding where collections go: {e}")))?;
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "{} home {} holds {} collection(s)",
            kind.collection_type,
            home.home_set_url,
            home.collections.len()
        );
    }
    let home_url = if home.home_set_url.ends_with('/') {
        home.home_set_url.clone()
    } else {
        format!("{}/", home.home_set_url)
    };
    // Every name in the home is taken, not only this kind's collections: a
    // server may keep calendars and address books side by side.
    let mut taken: HashSet<String> = client
        .propfind_responses(&home_url, 1, &xml::propfind_dav_items(), &home_url)?
        .responses
        .into_iter()
        .map(|r| r.href.as_str().trim_end_matches('/').to_owned())
        .chain(
            home.collections
                .iter()
                .map(|c| c.href.as_str().trim_end_matches('/').to_owned()),
        )
        .collect();
    let mut used: HashSet<usize> = HashSet::new();

    for collection in &collections {
        let found = home.collections.iter().enumerate().find(|(i, c)| {
            !used.contains(i) && c.props.displayname.as_deref() == Some(collection.name.as_str())
        });
        let target_url = match found {
            Some((i, c)) => {
                used.insert(i);
                collection_counts.skipped += 1;
                Some(c.url.clone())
            }
            None => make_collection(
                &client,
                kind,
                collection,
                &home_url,
                &mut taken,
                dry_run,
                &mut collection_counts,
                logger,
            )?,
        };
        let mine: Vec<&Item> = items
            .iter()
            .filter(|i| i.collections.contains(&collection.id))
            .collect();
        let Some(url) = target_url else {
            item_counts.failed += mine.len() as u64;
            continue;
        };
        let target = if found.is_some() {
            list_items(&client, &url)?
        } else {
            Target {
                url,
                items: HashMap::new(),
            }
        };
        for group in by_uid(&mine) {
            let merged;
            let item = match group.as_slice() {
                [one] => *one,
                many => {
                    merged = occurrences(&conn, many);
                    match &merged {
                        Ok(item) => item,
                        Err(reason) => {
                            logger.warn(&format!(
                                "{} {}: can't be converted: {reason}",
                                kind.item_type, many[0].uid
                            ));
                            item_counts.failed += many.len() as u64;
                            continue;
                        }
                    }
                }
            };
            let before = item_counts.clone();
            write_item(
                &client,
                kind,
                &target,
                item,
                dry_run,
                &mut item_counts,
                logger,
            )?;
            // Counted per row, as the archive holds them.
            item_counts.repeat_last(&before, group.len() as u64 - 1);
        }
    }

    if dry_run {
        logger.warn("dry run: nothing was written");
    }
    Ok(Summary {
        per_type: vec![
            (kind.collection_type, collection_counts),
            (kind.item_type, item_counts),
        ],
        retries_observed: client.retries_observed(),
        retry_after_sleeps: client.retry_after_sleeps(),
    })
}

/// Items in archive order, those sharing a UID together: one CalDAV
/// resource holds every event with its UID.
fn by_uid<'a>(items: &[&'a Item]) -> Vec<Vec<&'a Item>> {
    let mut groups: Vec<Vec<&Item>> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    for item in items {
        match at.get(item.uid.as_str()) {
            Some(&i) => groups[i].push(item),
            None => {
                at.insert(item.uid.as_str(), groups.len());
                groups.push(vec![item]);
            }
        }
    }
    groups
}

/// Occurrences sharing a UID as one item.
fn occurrences(conn: &Connection, many: &[&Item]) -> Result<Item, String> {
    let entries: Vec<(&str, &str)> = many
        .iter()
        .filter_map(|i| i.event.as_ref().map(|(d, t)| (d.as_str(), t.as_str())))
        .collect();
    if entries.len() != many.len() {
        return Err("two cards share a UID".to_owned());
    }
    Ok(Item {
        uid: many[0].uid.clone(),
        text: icalendar_entries_text(conn, &entries)?,
        collections: Vec::new(),
        event: None,
    })
}

/// A new collection under the home, named after the archive's, or `None`
/// when the server won't make it.
#[allow(clippy::too_many_arguments)]
fn make_collection(
    client: &DavClient,
    kind: &Kind,
    collection: &Collection,
    home_url: &str,
    taken: &mut HashSet<String>,
    dry_run: bool,
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<Option<String>, Error> {
    let base = slug(&collection.name);
    let mut name = base.clone();
    let mut n = 2;
    let href_of = |name: &str| -> Result<String, Error> {
        let href = normalise(home_url, name).map_err(|e| Error::Connection(e.to_string()))?;
        Ok(href.as_str().trim_end_matches('/').to_owned())
    };
    while taken.contains(&href_of(&name)?) {
        name = format!("{base}-{n}");
        n += 1;
    }
    taken.insert(href_of(&name)?);
    let url = format!("{home_url}{name}/");
    let description = collection.description.as_deref().filter(|d| !d.is_empty());
    if dry_run {
        counts.created += 1;
        return Ok(Some(url));
    }
    let body = if matches!(kind.dav, DavKind::Caldav) {
        let color = collection.color.as_deref().filter(|c| !c.is_empty());
        xml::mkcalendar(&collection.name, description, color)
    } else {
        xml::mkcol_addressbook(&collection.name, description)
    };
    match write_result(client.make_collection(kind.make_method, &url, &body))? {
        Write::Done => {
            counts.created += 1;
            if logger.enabled(LEVEL_PROGRESS) {
                eprintln!(
                    "{} {:?}: created at {url}",
                    kind.collection_type, collection.name
                );
            }
            Ok(Some(url))
        }
        Write::Refused(reason) => {
            logger.warn(&format!(
                "{} {:?} could not be made ({reason}); its items are left out",
                kind.collection_type, collection.name
            ));
            counts.failed += 1;
            Ok(None)
        }
    }
}

fn list_items(client: &DavClient, url: &str) -> Result<Target, Error> {
    let listing = client.propfind_responses(url, 1, &xml::propfind_dav_items(), url)?;
    let own = normalise(url, "")
        .map(Href::into_string)
        .unwrap_or_default();
    let items = listing
        .responses
        .into_iter()
        .filter(|r| !r.props.is_collection && r.href.as_str() != own)
        .map(|r| (r.href.into_string(), r.props.etag))
        .collect();
    Ok(Target {
        url: url.to_owned(),
        items,
    })
}

fn write_item(
    client: &DavClient,
    kind: &Kind,
    target: &Target,
    item: &Item,
    dry_run: bool,
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    let name = format!("{}{}", segment(&item.uid), kind.extension);
    let href = normalise(&target.url, &name).map_err(|e| Error::Connection(e.to_string()))?;
    let url = absolute_url(&target.url, &href).map_err(|e| Error::Connection(e.to_string()))?;
    let existing = target.items.get(href.as_str());
    let etag: Option<String>;
    let condition = match existing {
        None => PutCondition::Create,
        Some(listed) => {
            let theirs = client.get(&url)?;
            if same_object(&String::from_utf8_lossy(&theirs.body), &item.text) {
                counts.skipped += 1;
                return Ok(());
            }
            etag = listed.clone().or(theirs.etag);
            match etag.as_deref() {
                Some(etag) => PutCondition::Replace(etag),
                None => {
                    logger.warn(&format!(
                        "{} {}: the server gives no etag, so it isn't overwritten",
                        kind.item_type, item.uid
                    ));
                    counts.failed += 1;
                    return Ok(());
                }
            }
        }
    };
    let replacing = matches!(condition, PutCondition::Replace(_));
    if dry_run {
        if replacing {
            counts.updated += 1;
        } else {
            counts.created += 1;
        }
        return Ok(());
    }
    let put = client.put(&url, item.text.as_bytes(), kind.content_type, condition);
    match write_result(put)? {
        Write::Done if replacing => counts.updated += 1,
        Write::Done => counts.created += 1,
        Write::Refused(reason) => {
            logger.warn(&format!(
                "{} {} was refused: {reason}",
                kind.item_type, item.uid
            ));
            counts.failed += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_lines_and_stamps_dont_count_as_changes() {
        let ours = "BEGIN:VEVENT\r\nUID:a\r\nDTSTAMP:20260101T000000Z\r\nSUMMARY:Lunch with the\r\n  team\r\nEND:VEVENT\r\n";
        let theirs = "BEGIN:VEVENT\nSUMMARY:Lunch with the team\nUID:a\nDTSTAMP:20261030T120000Z\nEND:VEVENT\n";
        assert!(same_object(ours, theirs));
        assert!(!same_object(ours, &theirs.replace("Lunch", "Dinner")));
    }

    #[test]
    fn parameter_order_doesnt_count_as_a_change() {
        let ours = "ATTENDEE;PARTSTAT=ACCEPTED;CN=\"Smith; Jo: PhD\":mailto:jo@example.org\r\n";
        let theirs = "attendee;cn=\"Smith; Jo: PhD\";partstat=ACCEPTED:mailto:jo@example.org\n";
        assert!(same_object(ours, theirs));
        assert_eq!(
            canonical_line("ATTENDEE;PARTSTAT=ACCEPTED;CN=\"a;b:c\":mailto:x@y"),
            "ATTENDEE;CN=\"a;b:c\";PARTSTAT=ACCEPTED:mailto:x@y"
        );
        assert!(same_object("N:Doe;Jo;;;;;\r\n", "N:Doe;Jo;;;\n"));
        assert!(!same_object("N:Doe;Jo\\;\r\n", "N:Doe;Jo\n"));
        assert!(!same_object("N:Doe;Jo;;;Jr.\r\n", "N:Doe;Jo\n"));
        assert!(same_object(
            "ADR;TYPE=HOME:;;5 Main St;Town;OR;97477;US;;;;;5 Main St\r\n",
            "ADR;TYPE=HOME:;;5 Main St;Town;OR;97477;US\n"
        ));
        assert!(!same_object(
            "ADR:;;5 Main St;Town;OR;97477;US\r\n",
            "ADR:;;6 Main St;Town;OR;97477;US\n"
        ));
        assert_eq!(first_components("a\\;b;c;d", 2), "a\\;b;c");
        let other = "ATTENDEE;PARTSTAT=DECLINED;CN=\"Smith; Jo: PhD\":mailto:jo@example.org\r\n";
        assert!(!same_object(ours, other));
    }

    #[test]
    fn the_uid_is_read_unfolded() {
        let text =
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:abc\r\n def@example.org\r\nEND:VEVENT\r\n";
        assert_eq!(first_uid(text).as_deref(), Some("abcdef@example.org"));
        assert_eq!(first_uid("BEGIN:VEVENT\r\nEND:VEVENT\r\n"), None);
    }

    #[test]
    fn names_from_the_source_become_safe_segments() {
        assert_eq!(segment("abc-123_x.y"), "abc-123_x.y");
        let hashed = segment("ab/../c@example.org");
        assert_eq!(hashed.len(), 32);
        assert_eq!(hashed, segment("ab/../c@example.org"));
        assert_eq!(segment(".hidden").len(), 32);
        assert_eq!(slug("Family & Friends"), "family---friends");
        assert_eq!(slug("Équipe"), "quipe");
        assert_eq!(slug("日本").len(), 32);
        assert_eq!(slug("Work"), "work");
    }
}
