/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-carddav` and `export-caldav` against a small in-memory DAV
//! server: collections are matched by name or made, a stale card is
//! replaced under its etag, a refused card is counted without stopping the
//! rest, and a second run writes nothing.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use inbuxa_migrate::db::init;
use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::export_dav::{self, ExportDavConfig};
use inbuxa_migrate::sync::import_dav::{DavAuth, DavKindArg, calcard};
use inbuxa_migrate::sync::{CommonConfig, Summary, TypeCounts};
use rusqlite::params;
use tempfile::TempDir;

const ALICE: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-alice\r\nFN:Alice Smith\r\n\
EMAIL:alice@example.com\r\nEND:VCARD\r\n";
const BOB: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-bob\r\nFN:Bob Jones\r\nEND:VCARD\r\n";
const CAROL: &str =
    "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-carol\r\nFN:Carol Refused\r\nEND:VCARD\r\n";
const EVENT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\nBEGIN:VEVENT\r\nUID:ev-standup\r\n\
DTSTAMP:20240101T000000Z\r\nDTSTART:20240301T090000Z\r\nDURATION:PT15M\r\n\
SUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

/// What the server holds, by path: a collection's name, or an item's body
/// and etag.
#[derive(Clone)]
enum Resource {
    Collection { name: String, calendar: bool },
    Item { body: String, etag: u32 },
}

#[derive(Default)]
struct Store {
    resources: BTreeMap<String, Resource>,
    next_etag: u32,
    /// Each write as (method, path, condition header).
    writes: Vec<(String, String, String)>,
}

impl Store {
    fn put_item(&mut self, path: &str, body: &str) {
        self.next_etag += 1;
        self.resources.insert(
            path.to_owned(),
            Resource::Item {
                body: body.to_owned(),
                etag: self.next_etag,
            },
        );
    }
}

fn props(path: &str, resource: Option<&Resource>) -> String {
    let inner = match resource {
        None => "<d:resourcetype><d:collection/></d:resourcetype>".to_owned(),
        Some(Resource::Collection { name, calendar }) => format!(
            "<d:resourcetype><d:collection/>{}</d:resourcetype><d:displayname>{name}</d:displayname>",
            if *calendar {
                "<c:calendar/>"
            } else {
                "<card:addressbook/>"
            }
        ),
        Some(Resource::Item { etag, .. }) => {
            format!("<d:resourcetype/><d:getetag>\"{etag}\"</d:getetag>")
        }
    };
    format!(
        "<d:response><d:href>{path}</d:href><d:propstat><d:prop>{inner}</d:prop>\
         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
    )
}

fn multistatus(responses: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\" \
         xmlns:c=\"urn:ietf:params:xml:ns:caldav\" \
         xmlns:card=\"urn:ietf:params:xml:ns:carddav\">{responses}</d:multistatus>"
    )
}

fn displayname(body: &str) -> String {
    let start = body.find("displayname>").map(|i| i + "displayname>".len());
    start
        .and_then(|s| body[s..].find('<').map(|e| body[s..s + e].to_owned()))
        .unwrap_or_default()
}

/// One request against the store, as (status, etag, body).
fn handle(
    store: &mut Store,
    method: &str,
    path: &str,
    headers: &BTreeMap<String, String>,
    body: &str,
) -> (u16, Option<u32>, String) {
    match method {
        "PROPFIND" if headers.get("depth").map(String::as_str) == Some("0") => {
            let home = "<d:current-user-principal><d:href>/p/</d:href></d:current-user-principal>\
                <c:calendar-home-set><d:href>/cal/</d:href></c:calendar-home-set>\
                <card:addressbook-home-set><d:href>/card/</d:href></card:addressbook-home-set>";
            let response = format!(
                "<d:response><d:href>{path}</d:href><d:propstat><d:prop>{home}</d:prop>\
                 <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
            );
            (207, None, multistatus(&response))
        }
        "PROPFIND" => {
            let mut out = props(path, store.resources.get(path));
            for (child, resource) in store.resources.range(path.to_owned()..) {
                let Some(rest) = child.strip_prefix(path) else {
                    break;
                };
                let direct = !rest.is_empty() && !rest.trim_end_matches('/').contains('/');
                if direct {
                    out.push_str(&props(child, Some(resource)));
                }
            }
            (207, None, multistatus(&out))
        }
        "MKCALENDAR" | "MKCOL" => {
            store
                .writes
                .push((method.to_owned(), path.to_owned(), String::new()));
            if store.resources.contains_key(path) {
                return (405, None, String::new());
            }
            store.resources.insert(
                path.to_owned(),
                Resource::Collection {
                    name: displayname(body),
                    calendar: method == "MKCALENDAR",
                },
            );
            (201, None, String::new())
        }
        "GET" => match store.resources.get(path) {
            Some(Resource::Item { body, etag }) => (200, Some(*etag), body.clone()),
            _ => (404, None, String::new()),
        },
        "PUT" => {
            let condition = headers
                .get("if-none-match")
                .map(|v| format!("If-None-Match: {v}"))
                .or_else(|| headers.get("if-match").map(|v| format!("If-Match: {v}")))
                .unwrap_or_default();
            store
                .writes
                .push(("PUT".to_owned(), path.to_owned(), condition));
            if path.contains("card-carol") {
                return (415, None, String::new());
            }
            let current = match store.resources.get(path) {
                Some(Resource::Item { etag, .. }) => Some(format!("\"{etag}\"")),
                _ => None,
            };
            let allowed = match (headers.get("if-none-match"), headers.get("if-match")) {
                (Some(_), _) => current.is_none(),
                (_, Some(want)) => current.as_deref() == Some(want.as_str()),
                _ => false,
            };
            if !allowed {
                return (412, None, String::new());
            }
            let status = if current.is_some() { 204 } else { 201 };
            store.put_item(path, body);
            (status, Some(store.next_etag), String::new())
        }
        _ => (405, None, String::new()),
    }
}

/// A DAV server on a local port, one request per connection.
fn serve(store: Arc<Mutex<Store>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_owned();
            let path = parts.next().unwrap_or_default().to_owned();
            let mut headers = BTreeMap::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
                }
            }
            let length: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8_lossy(&body);
            let (status, etag, out) =
                handle(&mut store.lock().unwrap(), &method, &path, &headers, &body);
            let etag = etag
                .map(|e| format!("ETag: \"{e}\"\r\n"))
                .unwrap_or_default();
            let kind = if status == 207 {
                "Content-Type: application/xml; charset=utf-8\r\n"
            } else {
                ""
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\n{etag}{kind}Content-Length: {}\r\nConnection: close\r\n\r\n{out}",
                out.len()
            );
        }
    });
    url
}

fn common(archive: &Path) -> CommonConfig {
    CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: Logger::from_flags(false, 0),
    }
}

fn export(kind: DavKindArg, url: &str, archive: &Path) -> Summary {
    export_dav::run(
        common(archive),
        ExportDavConfig {
            kind,
            url: url.to_owned(),
            auth: DavAuth::Basic {
                user: "u".into(),
                password: "p".into(),
            },
            allow_cleartext: true,
        },
    )
    .expect("export")
}

fn counts<'a>(summary: &'a Summary, ty: &str) -> &'a TypeCounts {
    &summary.per_type.iter().find(|(t, _)| *t == ty).unwrap().1
}

fn tuple(c: &TypeCounts) -> (u64, u64, u64, u64) {
    (c.created, c.updated, c.skipped, c.failed)
}

fn seed(archive: &Path) {
    let conn = init::open(archive).unwrap();
    conn.execute(
        "INSERT INTO address_books (id, name, is_default) VALUES (1, 'Personal', 1), (2, 'Work', 0)",
        [],
    )
    .unwrap();
    for (vcard, books) in [(ALICE, "[1]"), (BOB, "[2]"), (CAROL, "[2]")] {
        let card = calcard::vcard_to_jscontact(vcard, "/x").unwrap();
        conn.execute(
            "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
            params![card.uid, books, card.data.to_string()],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO calendars (id, name, is_default) VALUES (1, 'Team', 1)",
        [],
    )
    .unwrap();
    let mut entry = calcard::ical_to_jscalendar_entries(EVENT)
        .unwrap()
        .remove(0);
    calcard::strip_extracted_fields_from_event(&mut entry.data);
    conn.execute(
        "INSERT INTO calendar_events (calendar_ids, data) VALUES ('[1]', ?1)",
        params![entry.data.to_string()],
    )
    .unwrap();
}

#[test]
fn carddav_export_matches_by_name_replaces_under_etag_and_converges() {
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("archive.sqlite");
    seed(&archive);

    let store = Arc::new(Mutex::new(Store::default()));
    {
        // The target already has a "Personal" book holding an older Alice.
        let mut s = store.lock().unwrap();
        s.resources.insert(
            "/card/contacts/".into(),
            Resource::Collection {
                name: "Personal".into(),
                calendar: false,
            },
        );
        s.put_item(
            "/card/contacts/card-alice.vcf",
            "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-alice\r\nFN:Alice Old\r\nEND:VCARD\r\n",
        );
    }
    let url = serve(store.clone());

    let first = export(DavKindArg::Carddav, &url, &archive);
    assert_eq!(
        tuple(counts(&first, "addressbook")),
        (1, 0, 1, 0),
        "{first:?}"
    );
    assert_eq!(
        tuple(counts(&first, "contactcard")),
        (1, 1, 0, 1),
        "{first:?}"
    );
    {
        let s = store.lock().unwrap();
        let writes: Vec<(&str, &str, &str)> = s
            .writes
            .iter()
            .map(|(m, p, c)| (m.as_str(), p.as_str(), c.as_str()))
            .collect();
        assert_eq!(
            writes,
            [
                ("PUT", "/card/contacts/card-alice.vcf", "If-Match: \"1\""),
                ("MKCOL", "/card/work/", ""),
                ("PUT", "/card/work/card-bob.vcf", "If-None-Match: *"),
                ("PUT", "/card/work/card-carol.vcf", "If-None-Match: *"),
            ]
        );
        let Some(Resource::Collection { name, .. }) = s.resources.get("/card/work/") else {
            panic!("no Work book");
        };
        assert_eq!(name, "Work");
        let Some(Resource::Item { body, .. }) = s.resources.get("/card/contacts/card-alice.vcf")
        else {
            panic!("no Alice");
        };
        assert!(body.contains("Alice Smith"), "{body}");
    }

    store.lock().unwrap().writes.clear();
    let again = export(DavKindArg::Carddav, &url, &archive);
    assert_eq!(
        tuple(counts(&again, "addressbook")),
        (0, 0, 2, 0),
        "{again:?}"
    );
    assert_eq!(
        tuple(counts(&again, "contactcard")),
        (0, 0, 2, 1),
        "{again:?}"
    );
    let writes = store.lock().unwrap().writes.clone();
    assert_eq!(
        writes.len(),
        1,
        "only the refused card is tried again: {writes:?}"
    );
}

#[test]
fn caldav_export_makes_the_calendar_and_converges() {
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("archive.sqlite");
    seed(&archive);
    let store = Arc::new(Mutex::new(Store::default()));
    let url = serve(store.clone());

    let first = export(DavKindArg::Caldav, &url, &archive);
    assert_eq!(tuple(counts(&first, "calendar")), (1, 0, 0, 0), "{first:?}");
    assert_eq!(
        tuple(counts(&first, "calendarevent")),
        (1, 0, 0, 0),
        "{first:?}"
    );
    {
        let s = store.lock().unwrap();
        let Some(Resource::Item { body, .. }) = s.resources.get("/cal/team/ev-standup.ics") else {
            panic!("no event: {:?}", s.resources.keys().collect::<Vec<_>>());
        };
        assert!(body.contains("SUMMARY:Standup"), "{body}");
    }

    let again = export(DavKindArg::Caldav, &url, &archive);
    assert_eq!(tuple(counts(&again, "calendar")), (0, 0, 1, 0), "{again:?}");
    assert_eq!(
        tuple(counts(&again, "calendarevent")),
        (0, 0, 1, 0),
        "{again:?}"
    );
}
