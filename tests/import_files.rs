/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `import files`: an account export read back, manifest and all, so what
//! the open formats can't hold comes back too. An archive exported and
//! imported again holds the same account, and exporting that gives the
//! same tree.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use inbuxa_migrate::db::{blobs, init};
use inbuxa_migrate::export_files::{self, ExportFilesConfig};
use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::import_dav::calcard;
use inbuxa_migrate::sync::import_files::{self, FilesImportConfig};
use inbuxa_migrate::sync::{CommonConfig, Summary, TypeCounts};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

const VCARD_ALICE: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-alice\r\nFN:Alice Smith\r\n\
EMAIL:alice@example.com\r\nEND:VCARD\r\n";
const VCARD_BOB: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-bob\r\nFN:Bob Jones\r\n\
TEL:+1 555 0100\r\nEND:VCARD\r\n";
const STANDUP: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\nBEGIN:VEVENT\r\nUID:ev-standup\r\n\
DTSTAMP:20240101T000000Z\r\nDTSTART;TZID=Europe/Berlin:20240301T090000\r\nDURATION:PT15M\r\n\
RRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
const OFFSITE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\nBEGIN:VEVENT\r\nUID:ev-offsite\r\n\
DTSTAMP:20240101T000000Z\r\nDTSTART;VALUE=DATE:20240510\r\nSUMMARY:Offsite\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

fn message(subject: &str) -> Vec<u8> {
    format!(
        "From: dave@example.net\r\nTo: alice@example.org\r\nSubject: {subject}\r\n\
         Message-ID: <{}@example.net>\r\nDate: Fri, 01 Mar 2024 12:00:00 +0000\r\n\r\n{subject}\r\n",
        subject.replace(' ', "-")
    )
    .into_bytes()
}

/// An account with everything the manifest exists for: folder names Maildir
/// can't hold, roles, an unsubscribed folder, a message in two folders,
/// custom keywords, default address book and calendar, a card and an event
/// in two collections, colors and time zones, an active Sieve script,
/// media types, a link, identities with signatures, a calendar identity.
fn seed(path: &Path) {
    let conn = init::open(path).unwrap();
    conn.execute(
        "INSERT INTO sources (id, kind, session_url, account_id, account_name, username)
         VALUES (1, 'jmap', 'https://mail.example.org/jmap/session', 'a1', 'alice@example.org', 'alice')",
        [],
    )
    .unwrap();
    for (id, name, parent, role, subscribed) in [
        (1, "Inbox", None, Some("inbox"), 1),
        (2, "Clients", Some(1), None, 1),
        (3, "Archive", None, Some("archive"), 1),
        (4, "2024", Some(3), None, 1),
        (5, "Équipe & co", None, None, 1),
        (6, "v1.0 notes", None, None, 0),
        (7, "Sent Items", None, Some("sent"), 1),
    ] {
        conn.execute(
            "INSERT INTO mailboxes (id, name, parent_id, role, is_subscribed)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, parent, role, subscribed],
        )
        .unwrap();
    }
    for (subject, mailboxes, keywords) in [
        ("Seen and flagged", "[1]", r#"["$flagged","$seen"]"#),
        ("Junk in two folders", "[1,3]", r#"["$Junk"]"#),
        ("Client note", "[2]", r#"["$answered","project-x"]"#),
        ("Old report", "[4]", "[]"),
        ("Bonjour", "[5]", r#"["$seen"]"#),
        ("Release notes", "[6]", "[]"),
        ("Sent reply", "[7]", r#"["$seen"]"#),
    ] {
        let blob = blobs::intern_blob(&conn, &message(subject)).unwrap();
        conn.execute(
            "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords)
             VALUES (?1, '2024-03-01T12:00:00Z', ?2, ?3)",
            params![blob, mailboxes, keywords],
        )
        .unwrap();
    }

    conn.execute(
        "INSERT INTO address_books (id, name, description, is_default)
         VALUES (1, 'Personal', 'Friends and family', 0), (2, 'Work', NULL, 1)",
        [],
    )
    .unwrap();
    for (vcard, books) in [(VCARD_ALICE, "[1,2]"), (VCARD_BOB, "[2]")] {
        let card = calcard::vcard_to_jscontact(vcard, "/x").unwrap();
        conn.execute(
            "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
            params![card.uid, books, card.data.to_string()],
        )
        .unwrap();
    }

    conn.execute(
        "INSERT INTO calendars (id, name, description, color, time_zone, is_default)
         VALUES (1, 'Team, Berlin', 'Stand-ups', '#3366ff', 'Europe/Berlin', 1),
                (2, 'Company', NULL, '#ff9900', 'America/Phoenix', 0)",
        [],
    )
    .unwrap();
    for (ics, calendars) in [(STANDUP, "[1]"), (OFFSITE, "[1,2]")] {
        let mut entry = calcard::ical_to_jscalendar_entries(ics).unwrap().remove(0);
        calcard::strip_extracted_fields_from_event(&mut entry.data);
        conn.execute(
            "INSERT INTO calendar_events (calendar_ids, data) VALUES (?1, ?2)",
            params![calendars, entry.data.to_string()],
        )
        .unwrap();
    }

    for (name, active, script) in [
        (
            "filters",
            1,
            "require \"fileinto\";\r\nif header :contains \"subject\" \"report\" { fileinto \"Archive\"; }\r\n",
        ),
        ("old", 0, "keep;\r\n"),
    ] {
        let blob = blobs::intern_blob(&conn, script.as_bytes()).unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (name, is_active, blob_id) VALUES (?1, ?2, ?3)",
            params![name, active, blob],
        )
        .unwrap();
    }

    let text = blobs::intern_blob(&conn, b"hello\n").unwrap();
    let pdf = blobs::intern_blob(&conn, b"%PDF-1.7 not really\n").unwrap();
    conn.execute(
        "INSERT INTO file_nodes (id, parent_id, node_type, blob_id, target, name, media_type, created, modified)
         VALUES (1, NULL, 'directory', NULL, NULL, 'Docs', NULL, '2024-01-01T00:00:00Z', NULL),
                (2, 1, 'file', ?1, NULL, 'a.txt', 'text/plain', '2024-01-01T00:00:00Z', '2024-02-01T10:00:00Z'),
                (3, 1, 'directory', NULL, NULL, 'Taxes', NULL, '2024-01-01T00:00:00Z', NULL),
                (4, 3, 'file', ?2, NULL, '2025.pdf', 'application/pdf', '2024-01-01T00:00:00Z', '2024-04-15T09:30:00Z'),
                (5, 1, 'symlink', NULL, '{\"path\":\"Taxes/2025.pdf\"}', 'latest', NULL, '2024-01-01T00:00:00Z', NULL)",
        params![text, pdf],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO identities (name, email, reply_to, bcc, text_signature, html_signature)
         VALUES ('Alice Smith', 'alice@example.org', '[{\"name\":\"Alice\",\"email\":\"replies@example.org\"}]',
                 NULL, 'Alice\n-- \nExample Org', '<p>Alice</p>'),
                ('Alice (support)', 'support@example.org', NULL, '[{\"name\":null,\"email\":\"log@example.org\"}]', '', '')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO participant_identities (name, calendar_address, is_default)
         VALUES ('Alice Smith', 'mailto:alice@example.org', 1),
                ('Alice (support)', 'mailto:support@example.org', 0)",
        [],
    )
    .unwrap();
}

struct Fixture {
    _td: TempDir,
    root: PathBuf,
    archive: PathBuf,
    out: PathBuf,
}

fn exported() -> Fixture {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().to_path_buf();
    let archive = root.join("archive.sqlite");
    let out = root.join("out");
    seed(&archive);
    export(&archive, &out);
    Fixture {
        _td: td,
        root,
        archive,
        out,
    }
}

fn export(archive: &Path, out: &Path) {
    let summary = export_files::run(ExportFilesConfig {
        archive: archive.to_path_buf(),
        dir: out.to_path_buf(),
        objects: None,
        dry_run: false,
        logger: Logger::from_flags(true, 0),
    })
    .unwrap();
    assert!(!summary.any_failed(), "{:?}", summary.per_type);
}

fn common(archive: &Path, dry_run: bool) -> CommonConfig {
    CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    }
}

fn import(dir: &Path, archive: &Path) -> Result<Summary, inbuxa_migrate::error::Error> {
    import_files::run(
        common(archive, false),
        FilesImportConfig {
            dir: dir.to_path_buf(),
            allow_source_change: false,
        },
    )
}

fn counts<'a>(summary: &'a Summary, type_name: &str) -> &'a TypeCounts {
    &summary
        .per_type
        .iter()
        .find(|(t, _)| *t == type_name)
        .unwrap_or_else(|| panic!("no {type_name} in {:?}", summary.per_type))
        .1
}

fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| {
        (0..n)
            .map(|i| {
                Ok(match r.get::<_, rusqlite::types::Value>(i)? {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(v) => json!(v),
                    rusqlite::types::Value::Real(v) => json!(v),
                    rusqlite::types::Value::Text(v) => json!(v),
                    rusqlite::types::Value::Blob(v) => json!(blake3::hash(&v).to_hex().to_string()),
                })
            })
            .collect()
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// An archive's account in terms no row id appears in: every collection by
/// its names, every item by what it is and where it is.
fn account(path: &Path) -> BTreeMap<&'static str, BTreeSet<String>> {
    let conn = Connection::open(path).unwrap();
    let mut out: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();

    let boxes: BTreeMap<i64, (String, Option<i64>)> =
        rows(&conn, "SELECT id, name, parent_id FROM mailboxes")
            .into_iter()
            .map(|r| {
                (
                    r[0].as_i64().unwrap(),
                    (r[1].as_str().unwrap().to_owned(), r[2].as_i64()),
                )
            })
            .collect();
    let path_of = |mut id: i64| {
        let mut parts = Vec::new();
        loop {
            let (name, parent) = &boxes[&id];
            parts.insert(0, name.clone());
            match parent {
                Some(p) => id = *p,
                None => break parts.join("/"),
            }
        }
    };
    for r in rows(&conn, "SELECT id, role, is_subscribed FROM mailboxes") {
        out.entry("mailboxes").or_default().insert(format!(
            "{} role={} subscribed={}",
            path_of(r[0].as_i64().unwrap()),
            r[1],
            r[2]
        ));
    }
    let ids = |v: &Value| -> Vec<i64> { serde_json::from_str(v.as_str().unwrap()).unwrap() };
    for r in rows(
        &conn,
        "SELECT b.data, e.mailbox_ids, e.keywords, e.received_at FROM emails e JOIN blobs b ON b.id = e.blob_id",
    ) {
        let mut folders: Vec<String> = ids(&r[1]).into_iter().map(path_of).collect();
        folders.sort();
        let mut keywords: Vec<String> = serde_json::from_str(r[2].as_str().unwrap()).unwrap();
        keywords.sort();
        out.entry("emails").or_default().insert(format!(
            "{} in {folders:?} keywords={keywords:?} received={}",
            r[0], r[3]
        ));
    }

    let names = |sql: &str| -> BTreeMap<i64, String> {
        rows(&conn, sql)
            .into_iter()
            .map(|r| (r[0].as_i64().unwrap(), r[1].as_str().unwrap().to_owned()))
            .collect()
    };
    let books = names("SELECT id, name FROM address_books");
    for r in rows(
        &conn,
        "SELECT name, description, is_default FROM address_books",
    ) {
        out.entry("address_books")
            .or_default()
            .insert(format!("{r:?}"));
    }
    for r in rows(
        &conn,
        "SELECT uid, address_book_ids, data FROM contact_cards",
    ) {
        let mut in_books: Vec<&str> = ids(&r[1]).iter().map(|b| books[b].as_str()).collect();
        in_books.sort();
        out.entry("cards")
            .or_default()
            .insert(format!("{} in {in_books:?}", r[0]));
    }
    let calendars = names("SELECT id, name FROM calendars");
    for r in rows(
        &conn,
        "SELECT name, description, color, time_zone, is_default FROM calendars",
    ) {
        out.entry("calendars").or_default().insert(format!("{r:?}"));
    }
    for r in rows(
        &conn,
        "SELECT calendar_ids, data, data_type FROM calendar_events",
    ) {
        let data: Value = serde_json::from_str(r[1].as_str().unwrap()).unwrap();
        let mut in_calendars: Vec<&str> =
            ids(&r[0]).iter().map(|c| calendars[c].as_str()).collect();
        in_calendars.sort();
        out.entry("events").or_default().insert(format!(
            "{} {} {} in {in_calendars:?}",
            data["uid"], data["title"], r[2]
        ));
    }
    for r in rows(
        &conn,
        "SELECT s.name, s.is_active, b.data FROM sieve_scripts s JOIN blobs b ON b.id = s.blob_id",
    ) {
        out.entry("sieve").or_default().insert(format!("{r:?}"));
    }
    let nodes = names("SELECT id, name FROM file_nodes");
    let parents: BTreeMap<i64, Option<i64>> = rows(&conn, "SELECT id, parent_id FROM file_nodes")
        .into_iter()
        .map(|r| (r[0].as_i64().unwrap(), r[1].as_i64()))
        .collect();
    for r in rows(
        &conn,
        "SELECT n.id, n.node_type, n.media_type, n.target, b.data, n.modified
         FROM file_nodes n LEFT JOIN blobs b ON b.id = n.blob_id",
    ) {
        let mut id = r[0].as_i64().unwrap();
        let mut path = nodes[&id].clone();
        while let Some(Some(p)) = parents.get(&id) {
            path = format!("{}/{path}", nodes[p]);
            id = *p;
        }
        // A link has no file whose time the export could set.
        let modified = if r[1] == "symlink" {
            Value::Null
        } else {
            r[5].clone()
        };
        out.entry("files").or_default().insert(format!(
            "{path} {} {} {} {} {modified}",
            r[1], r[2], r[3], r[4]
        ));
    }
    for r in rows(
        &conn,
        "SELECT name, email, reply_to, bcc, text_signature, html_signature FROM identities",
    ) {
        // Addresses are JSON; the same addresses may be written in any key order.
        let mut r = r;
        for v in &mut r[2..4] {
            if let Some(text) = v.as_str() {
                *v = serde_json::from_str(text).unwrap();
            }
        }
        out.entry("identities")
            .or_default()
            .insert(format!("{r:?}"));
    }
    for r in rows(
        &conn,
        "SELECT name, calendar_address, is_default FROM participant_identities",
    ) {
        out.entry("participant_identities")
            .or_default()
            .insert(format!("{r:?}"));
    }
    out
}

#[test]
fn an_exported_account_comes_back_whole() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    let summary = import(&f.out, &back).unwrap();
    assert!(!summary.any_failed(), "{:?}", summary.per_type);

    let original = account(&f.archive);
    let read_back = account(&back);
    for part in original.keys() {
        assert_eq!(read_back.get(part), original.get(part), "{part}");
    }
    assert_eq!(read_back, original);

    // One message in two folders came back as one message, not two.
    assert_eq!(counts(&summary, "email").created, 7);
    // Folders Maildir can't name kept their real names.
    let mailboxes = &read_back["mailboxes"];
    assert!(
        mailboxes.contains("v1.0 notes role=null subscribed=0"),
        "{mailboxes:?}"
    );
    assert!(
        mailboxes.contains("Équipe & co role=null subscribed=1"),
        "{mailboxes:?}"
    );

    let conn = Connection::open(&back).unwrap();
    let source: (String, String) = conn
        .query_row("SELECT kind, account_name FROM sources", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(source, ("files".to_owned(), "alice@example.org".to_owned()));
}

#[test]
fn exporting_what_was_imported_gives_the_same_tree() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    import(&f.out, &back).unwrap();
    let again = f.root.join("again");
    export(&back, &again);

    let mut first = manifest(&f.out);
    let mut second = manifest(&again);
    for m in [&mut first, &mut second] {
        let m = m.as_object_mut().unwrap();
        m.remove("created");
        m.remove("sources");
    }
    assert_eq!(second, first);
    assert_eq!(tree(&again), tree(&f.out));
}

/// Every file in an export by path, with its content; messages by folder
/// and content, since their names carry archive ids.
fn tree(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == "manifest.json" {
                continue;
            }
            let bytes = fs::read(&path).unwrap();
            // iCalendar gives the order of a calendar's events no meaning.
            let bytes = if rel.ends_with(".ics") {
                components_sorted(&String::from_utf8(bytes).unwrap()).into_bytes()
            } else {
                bytes
            };
            let content = blake3::hash(&bytes).to_hex().to_string();
            let key = match rel.split_once("/cur/") {
                Some((folder, name)) => {
                    let flags = name.rsplit_once(":2,").map(|(_, f)| f).unwrap_or("");
                    format!("{folder}/cur/{content}:2,{flags}")
                }
                None => rel,
            };
            out.insert(key, content);
        }
    }
    out
}

/// An iCalendar object with its components in sorted order, each kept as
/// written.
fn components_sorted(ics: &str) -> String {
    let mut head = String::new();
    let mut components: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in ics.split_inclusive('\n') {
        let bare = line.trim_end();
        match &mut current {
            None if bare.starts_with("BEGIN:") && bare != "BEGIN:VCALENDAR" => {
                current = Some(line.to_owned());
            }
            None => head.push_str(line),
            Some(text) => {
                text.push_str(line);
                if bare.starts_with("END:") && text.starts_with(&format!("BEGIN:{}", &bare[4..])) {
                    components.push(current.take().unwrap());
                }
            }
        }
    }
    components.sort();
    head + &components.concat()
}

fn manifest(out: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap()
}

#[test]
fn a_second_run_finds_everything_in_place() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    import(&f.out, &back).unwrap();
    let before = account(&back);
    let summary = import(&f.out, &back).unwrap();
    for (type_name, c) in &summary.per_type {
        assert_eq!(
            (c.created, c.updated, c.failed),
            (0, 0, 0),
            "{type_name}: {c:?}"
        );
    }
    assert_eq!(counts(&summary, "email").skipped, 7);
    assert_eq!(account(&back), before);
}

#[test]
fn a_changed_export_updates_the_archive_and_deletes_nothing() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    import(&f.out, &back).unwrap();

    // Read in Dovecot since: one message in the archive folder is now seen.
    let archive_cur = f.out.join("mail/.Archive/cur");
    let name = fs::read_dir(&archive_cur)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();
    fs::rename(
        archive_cur.join(&name),
        archive_cur.join(format!("{name}S")),
    )
    .unwrap();
    // And the Sieve script that runs is now the other one.
    let mut m = manifest(&f.out);
    for script in m["sieve"].as_array_mut().unwrap() {
        let active = script["name"] == "old";
        script["active"] = json!(active);
    }
    fs::write(f.out.join("manifest.json"), m.to_string()).unwrap();

    let summary = import(&f.out, &back).unwrap();
    assert_eq!(
        counts(&summary, "email").updated,
        1,
        "{:?}",
        summary.per_type
    );
    assert_eq!(counts(&summary, "sievescript").updated, 2);
    let conn = Connection::open(&back).unwrap();
    let active: String = conn
        .query_row(
            "SELECT name FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, "old");
    let junk: String = conn
        .query_row(
            "SELECT keywords FROM emails WHERE json_array_length(mailbox_ids) = 2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // The copy in the inbox wasn't marked; the one in Archive was, and a
    // message has one set of keywords.
    assert_eq!(junk, r#"["$Junk","$seen"]"#);
    let emails: i64 = conn
        .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(emails, 7);
}

#[test]
fn an_unfinished_export_or_a_later_version_is_refused() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    let text = fs::read_to_string(f.out.join("manifest.json")).unwrap();

    fs::remove_file(f.out.join("manifest.json")).unwrap();
    let e = import(&f.out, &back).unwrap_err();
    assert_eq!(e.exit_code(), 1);
    assert!(e.to_string().contains("interrupted"), "{e}");
    assert!(
        !back.exists(),
        "nothing is created for an export that can't be read"
    );

    let mut m: Value = serde_json::from_str(&text).unwrap();
    m["formatVersion"] = json!(2);
    fs::write(f.out.join("manifest.json"), m.to_string()).unwrap();
    let e = import(&f.out, &back).unwrap_err();
    assert!(e.to_string().contains("version 2"), "{e}");

    // What 27.1.0-rc.2 wrote is the same format under its old name.
    m["formatVersion"] = json!(1);
    m["format"] = json!("inbuxa-migrate-files");
    fs::write(f.out.join("manifest.json"), m.to_string()).unwrap();
    import(&f.out, &back).unwrap();
}

#[test]
fn a_dry_run_reads_the_export_and_writes_nothing() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    let summary = import_files::run(
        common(&back, true),
        FilesImportConfig {
            dir: f.out.clone(),
            allow_source_change: false,
        },
    )
    .unwrap();
    assert!(!back.exists());
    assert_eq!(counts(&summary, "mailbox").fetched, 7);
    // The message in two folders is a file in each.
    assert_eq!(counts(&summary, "email").fetched, 8);
    assert_eq!(counts(&summary, "contactcard").fetched, 3);
    assert_eq!(counts(&summary, "calendarevent").fetched, 3);
    assert_eq!(counts(&summary, "filenode").fetched, 5);
}

#[test]
fn another_export_needs_allow_source_change() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    import(&f.out, &back).unwrap();
    let copy = f.root.join("copy");
    export(&back, &copy);
    let e = import(&copy, &back).unwrap_err();
    assert_eq!(e.exit_code(), 4, "{e}");
}

#[cfg(unix)]
#[test]
fn nothing_outside_the_export_is_read() {
    use std::os::unix::fs::symlink;

    let f = exported();
    let outside = f.root.join("outside");
    fs::create_dir_all(outside.join("cur")).unwrap();
    fs::create_dir_all(outside.join("new")).unwrap();
    fs::create_dir_all(outside.join("tmp")).unwrap();
    let secret = b"Subject: not yours\r\n\r\nsecret\r\n";
    fs::write(outside.join("cur/1.x:2,S"), secret).unwrap();
    fs::write(
        outside.join("secret.vcf"),
        VCARD_BOB.replace("card-bob", "card-secret"),
    )
    .unwrap();

    let mut m = manifest(&f.out);
    // Paths that climb out, by name and through links.
    m["mail"]["folders"].as_array_mut().unwrap().push(json!({
        "name": "Escape", "path": ["Escape"], "dir": "mail/../outside",
        "role": null, "subscribed": true, "messages": 1
    }));
    symlink(&outside, f.out.join("mail/.Linked")).unwrap();
    m["mail"]["folders"].as_array_mut().unwrap().push(json!({
        "name": "Linked", "path": ["Linked"], "dir": "mail/.Linked",
        "role": null, "subscribed": true, "messages": 1
    }));
    symlink(outside.join("cur/1.x:2,S"), f.out.join("mail/cur/2.x:2,S")).unwrap();
    m["contacts"].as_array_mut().unwrap().push(json!({
        "name": "Stolen", "description": null, "file": "/etc/passwd", "default": false, "cards": 1
    }));
    symlink(
        outside.join("secret.vcf"),
        f.out.join("contacts/Linked.vcf"),
    )
    .unwrap();
    m["contacts"].as_array_mut().unwrap().push(json!({
        "name": "Linked", "description": null, "file": "contacts/Linked.vcf", "default": false, "cards": 1
    }));
    symlink(&outside, f.out.join("files/Docs/elsewhere")).unwrap();
    m["files"]["links"].as_array_mut().unwrap().push(json!({
        "path": "files/../../x", "target": {"path": "/"}
    }));
    fs::write(f.out.join("manifest.json"), m.to_string()).unwrap();

    let back = f.root.join("back.sqlite");
    let summary = import(&f.out, &back).unwrap();
    assert!(
        counts(&summary, "email").failed >= 2,
        "{:?}",
        summary.per_type
    );
    assert!(counts(&summary, "contactcard").failed >= 2);
    assert!(counts(&summary, "filenode").failed >= 1);

    let conn = Connection::open(&back).unwrap();
    let leaked: i64 = conn
        .query_row(
            "SELECT count(*) FROM blobs WHERE instr(data, 'secret') > 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0);
    let cards: i64 = conn
        .query_row(
            "SELECT count(*) FROM contact_cards WHERE uid = 'card-secret'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cards, 0);
    let elsewhere: i64 = conn
        .query_row(
            "SELECT count(*) FROM file_nodes WHERE name = 'elsewhere'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(elsewhere, 0);
    // Everything that was safely inside still came in.
    assert_eq!(counts(&summary, "email").created, 7);
}

#[test]
fn a_card_another_import_put_in_the_archive_keeps_its_books() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    {
        let conn = init::open(&back).unwrap();
        conn.execute(
            "INSERT INTO sources (id, kind, session_url, account_id, account_name, username)
             VALUES (1, 'carddav', 'https://dav.example.org/', 'alice', 'alice', 'alice')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO address_books (id, name) VALUES (1, 'Phone')",
            [],
        )
        .unwrap();
        let card = calcard::vcard_to_jscontact(VCARD_ALICE, "/x").unwrap();
        conn.execute(
            "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, '[1]', ?2)",
            params![card.uid, card.data.to_string()],
        )
        .unwrap();
    }
    import(&f.out, &back).unwrap();
    let cards = &account(&back)["cards"];
    assert!(
        cards.contains(r#""card-alice" in ["Personal", "Phone", "Work"]"#),
        "{cards:?}"
    );
}
