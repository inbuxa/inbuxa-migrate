/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-carddav` and `export-caldav` against a small in-memory DAV
//! server: collections are matched by name or made, a stale card is
//! replaced under its etag, a refused card is counted without stopping the
//! rest, and a second run writes nothing.

mod fake_dav;

use std::path::Path;
use std::sync::{Arc, Mutex};

use fake_dav::{Resource, Store, serve};

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
        s.collection("/card/contacts/", "Personal", false);
        s.refuse.push("/card/work/card-carol.vcf".into());
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
