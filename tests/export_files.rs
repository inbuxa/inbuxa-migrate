/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-files`: an archive written out as open formats, and those files
//! read back by `import maildir` and `import takeout` with nothing lost
//! that the formats can hold.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use inbuxa_migrate::db::{blobs, init};
use inbuxa_migrate::export_files::{ExportFilesConfig, run};
use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::CommonConfig;
use inbuxa_migrate::sync::import_dav::calcard;
use inbuxa_migrate::sync::import_maildir::{self, MaildirImportConfig};
use inbuxa_migrate::sync::import_takeout::{self, TakeoutImportConfig};
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;

const VCARD_ALICE: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-alice\r\nFN:Alice Smith\r\n\
EMAIL:alice@example.com\r\nEND:VCARD\r\n";
const VCARD_BOB: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-bob\r\nFN:Bob Jones\r\n\
TEL:+1 555 0100\r\nEND:VCARD\r\n";
const EVENT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\nBEGIN:VEVENT\r\nUID:ev-standup\r\n\
DTSTAMP:20240101T000000Z\r\nDTSTART;TZID=Europe/Berlin:20240301T090000\r\nDURATION:PT15M\r\n\
RRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

fn message(subject: &str) -> Vec<u8> {
    format!(
        "From: dave@example.net\r\nTo: alice@example.org\r\nSubject: {subject}\r\n\
         Message-ID: <{}@example.net>\r\nDate: Fri, 01 Mar 2024 12:00:00 +0000\r\n\r\n{subject}\r\n",
        subject.replace(' ', "-")
    )
    .into_bytes()
}

/// An archive as an import would leave it: mail in nested and non-ASCII
/// folders, a message in two folders, standard and custom keywords, two
/// address books sharing a card, a calendar, Sieve scripts, files and a link.
fn seed(path: &Path) {
    let conn = init::open(path).unwrap();
    conn.execute(
        "INSERT INTO sources (id, kind, session_url, account_id, account_name, username)
         VALUES (1, 'jmap', 'https://mail.example.org/jmap/session', 'a1', 'alice', 'alice')",
        [],
    )
    .unwrap();
    for (id, name, parent, role) in [
        (1, "Inbox", None, Some("inbox")),
        (2, "Clients", Some(1), None),
        (3, "Archive", None, Some("archive")),
        (4, "2024", Some(3), None),
        (5, "Équipe & co", None, None),
        (6, "v1.0 notes", None, None),
    ] {
        conn.execute(
            "INSERT INTO mailboxes (id, name, parent_id, role) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, parent, role],
        )
        .unwrap();
    }
    for (subject, mailboxes, keywords) in [
        ("Seen and flagged", "[1]", r#"["$seen","$flagged"]"#),
        ("Junk in two folders", "[1,3]", r#"["$Junk"]"#),
        ("Client note", "[2]", r#"["$answered","project-x"]"#),
        ("Old report", "[4]", "[]"),
        ("Bonjour", "[5]", r#"["$seen"]"#),
        ("Release notes", "[6]", "[]"),
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
        "INSERT INTO address_books (id, name, is_default) VALUES (1, 'Personal', 1), (2, 'Work', 0)",
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
         VALUES (1, 'Team, Berlin', 'Stand-ups', '#3366ff', 'Europe/Berlin', 1)",
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

    let file_blob = blobs::intern_blob(&conn, b"hello\n").unwrap();
    conn.execute(
        "INSERT INTO file_nodes (id, parent_id, node_type, blob_id, target, name, media_type, created)
         VALUES (1, NULL, 'directory', NULL, NULL, 'Docs', NULL, '2024-01-01T00:00:00Z'),
                (2, 1, 'file', ?1, NULL, 'a.txt', 'text/plain', '2024-01-01T00:00:00Z'),
                (3, NULL, 'symlink', NULL, '{\"path\":\"/etc/passwd\"}', 'link', NULL, '2024-01-01T00:00:00Z'),
                (4, NULL, 'file', ?1, NULL, '../escape', NULL, '2024-01-01T00:00:00Z')",
        params![file_blob],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO identities (name, email) VALUES ('Alice Smith', 'alice@example.org')",
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
    let summary = run(ExportFilesConfig {
        archive: archive.clone(),
        dir: out.clone(),
        objects: None,
        dry_run: false,
        logger: Logger::from_flags(true, 0),
    })
    .unwrap();
    assert!(!summary.any_failed(), "{:?}", summary.per_type);
    Fixture {
        _td: td,
        root,
        archive,
        out,
    }
}

fn common(archive: &Path) -> CommonConfig {
    CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    }
}

fn manifest(out: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap()
}

#[test]
fn writes_open_formats_and_a_manifest() {
    let f = exported();
    let mail = f.out.join("mail");
    assert_eq!(fs::read_dir(mail.join("cur")).unwrap().count(), 2);
    for dir in [
        ".INBOX.Clients",
        ".Archive",
        ".Archive.2024",
        ".&AMk-quipe &- co",
        ".v1_0 notes",
    ] {
        assert!(mail.join(dir).join("cur").is_dir(), "{dir}");
        assert!(mail.join(dir).join("maildirfolder").is_file(), "{dir}");
    }
    assert_eq!(
        fs::read_to_string(mail.join("dovecot-keywords")).unwrap(),
        "0 $Junk\n"
    );

    let personal = fs::read_to_string(f.out.join("contacts/Personal.vcf")).unwrap();
    assert!(personal.contains("UID:card-alice") && !personal.contains("card-bob"));
    let work = fs::read_to_string(f.out.join("contacts/Work.vcf")).unwrap();
    assert_eq!(work.matches("BEGIN:VCARD").count(), 2);

    let ics = fs::read_to_string(f.out.join("calendars/Team, Berlin.ics")).unwrap();
    assert!(ics.contains("X-WR-CALNAME:Team\\, Berlin\r\n"), "{ics}");
    assert!(ics.contains("UID:ev-standup"), "{ics}");
    assert_eq!(ics.matches("BEGIN:VCALENDAR").count(), 1);

    assert!(f.out.join("sieve/filters.sieve").is_file());
    assert_eq!(
        fs::read(f.out.join("files/Docs/a.txt")).unwrap(),
        b"hello\n"
    );
    assert!(f.out.join("files/_.._escape").is_file());
    assert!(!f.out.join("files/link").exists());
    assert!(!f.root.join("escape").exists());

    let m = manifest(&f.out);
    assert_eq!(m["format"], "inbuxa-migrate-files");
    assert_eq!(m["formatVersion"], 1);
    assert_eq!(m["sources"][0]["kind"], "jmap");
    let folders = m["mail"]["folders"].as_array().unwrap();
    let inbox = folders.iter().find(|x| x["role"] == "inbox").unwrap();
    assert_eq!(inbox["dir"], "mail");
    assert_eq!(inbox["name"], "Inbox");
    let equipe = folders
        .iter()
        .find(|x| x["dir"] == "mail/.&AMk-quipe &- co")
        .unwrap();
    assert_eq!(equipe["name"], "Équipe & co");
    let dotted = folders
        .iter()
        .find(|x| x["dir"] == "mail/.v1_0 notes")
        .unwrap();
    assert_eq!(dotted["name"], "v1.0 notes");
    let sieve = m["sieve"].as_array().unwrap();
    assert!(
        sieve
            .iter()
            .any(|s| s["name"] == "filters" && s["active"] == true)
    );
    assert_eq!(m["files"]["links"][0]["path"], "files/link");
    assert_eq!(m["identities"][0]["email"], "alice@example.org");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&f.out).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "export readable by others: {mode:o}");
    }
}

/// Each message as (folder path, subject) → keywords, from an archive.
fn mail_by_folder(conn: &Connection) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut paths: BTreeMap<i64, String> = BTreeMap::new();
    let mut stmt = conn
        .prepare("SELECT id, name, parent_id, role FROM mailboxes")
        .unwrap();
    let rows: Vec<(i64, String, Option<i64>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let name_of = |id: i64| {
        let (_, name, _, role) = rows.iter().find(|r| r.0 == id).unwrap();
        if role.as_deref() == Some("inbox") {
            "INBOX".to_owned()
        } else {
            name.clone()
        }
    };
    for (id, _, _, _) in &rows {
        let mut parts = vec![name_of(*id)];
        let mut parent = rows.iter().find(|r| r.0 == *id).unwrap().2;
        while let Some(p) = parent {
            parts.insert(0, name_of(p));
            parent = rows.iter().find(|r| r.0 == p).unwrap().2;
        }
        paths.insert(*id, parts.join("/"));
    }
    let mut out = BTreeMap::new();
    let mut stmt = conn
        .prepare("SELECT b.data, e.mailbox_ids, e.keywords FROM emails e JOIN blobs b ON b.id = e.blob_id")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (data, mailboxes, keywords) = row.unwrap();
        let text = String::from_utf8(data).unwrap();
        let subject = text
            .lines()
            .find_map(|l| l.strip_prefix("Subject: "))
            .unwrap()
            .to_owned();
        let keywords: BTreeSet<String> = serde_json::from_str::<Vec<String>>(&keywords)
            .unwrap()
            .into_iter()
            .map(|k| k.to_lowercase())
            .collect();
        for mailbox in serde_json::from_str::<Vec<i64>>(&mailboxes).unwrap() {
            out.insert((paths[&mailbox].clone(), subject.clone()), keywords.clone());
        }
    }
    out
}

#[test]
fn mail_reads_back_with_import_maildir() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    import_maildir::run(
        common(&back),
        MaildirImportConfig {
            maildir: f.out.join("mail"),
            include: Vec::new(),
            exclude: Vec::new(),
            folder: Vec::new(),
            automap: true,
            include_deleted: false,
            allow_source_change: false,
        },
    )
    .unwrap();

    let original = mail_by_folder(&Connection::open(&f.archive).unwrap());
    let read_back = mail_by_folder(&Connection::open(&back).unwrap());
    // Maildir++ has no `.` inside a folder name; the manifest keeps it.
    let expected: BTreeMap<_, _> = original
        .into_iter()
        .map(|((path, subject), keywords)| ((path.replace("v1.0", "v1_0"), subject), keywords))
        .collect();
    assert_eq!(read_back, expected);

    let conn = Connection::open(&back).unwrap();
    let received: String = conn
        .query_row("SELECT received_at FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(received, "2024-03-01T12:00:00Z");
}

#[test]
fn contacts_and_calendars_read_back_with_import_takeout() {
    let f = exported();
    let back = f.root.join("back.sqlite");
    import_takeout::run(
        common(&back),
        TakeoutImportConfig {
            takeout_root: f.out.clone(),
            allow_source_change: false,
            automap: true,
        },
    )
    .unwrap();
    let conn = Connection::open(&back).unwrap();
    let uids: BTreeSet<String> = conn
        .prepare("SELECT uid FROM contact_cards")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        uids,
        BTreeSet::from(["card-alice".to_owned(), "card-bob".to_owned()])
    );
    let calendar: String = conn
        .query_row("SELECT name FROM calendars", [], |r| r.get(0))
        .unwrap();
    assert_eq!(calendar, "Team, Berlin");
    let event: String = conn
        .query_row("SELECT data FROM calendar_events", [], |r| r.get(0))
        .unwrap();
    let event: Value = serde_json::from_str(&event).unwrap();
    assert_eq!(event["uid"], "ev-standup");
    assert_eq!(event["title"], "Standup");
    assert_eq!(event["timeZone"], "Europe/Berlin");
    assert_eq!(event["start"], "2024-03-01T09:00:00");
    assert_eq!(event["recurrenceRule"]["frequency"], "weekly");
}

#[test]
fn refuses_a_directory_that_is_not_empty() {
    let td = tempfile::tempdir().unwrap();
    let archive = td.path().join("archive.sqlite");
    seed(&archive);
    let out = td.path().join("out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("keep.txt"), "mine").unwrap();
    let err = run(ExportFilesConfig {
        archive,
        dir: out.clone(),
        objects: None,
        dry_run: false,
        logger: Logger::from_flags(true, 0),
    })
    .unwrap_err();
    assert_eq!(err.exit_code(), 1, "{err}");
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
}

#[test]
fn a_dry_run_counts_and_writes_nothing() {
    let td = tempfile::tempdir().unwrap();
    let archive = td.path().join("archive.sqlite");
    seed(&archive);
    let out = td.path().join("out");
    let summary = run(ExportFilesConfig {
        archive,
        dir: out.clone(),
        objects: None,
        dry_run: true,
        logger: Logger::from_flags(true, 0),
    })
    .unwrap();
    assert!(!out.exists());
    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "email")
        .unwrap();
    assert_eq!(email.1.created, 6);
}

#[test]
fn objects_limits_what_is_written() {
    let td = tempfile::tempdir().unwrap();
    let archive = td.path().join("archive.sqlite");
    seed(&archive);
    let out = td.path().join("out");
    run(ExportFilesConfig {
        archive,
        dir: out.clone(),
        objects: Some(vec![inbuxa_migrate::types::ObjectType::SieveScript]),
        dry_run: false,
        logger: Logger::from_flags(true, 0),
    })
    .unwrap();
    assert!(out.join("sieve").is_dir());
    assert!(!out.join("mail").exists());
    assert!(manifest(&out).get("mail").is_none());
}

#[test]
fn a_missing_archive_is_a_usage_error_and_creates_nothing() {
    let td = tempfile::tempdir().unwrap();
    let archive = td.path().join("nope.sqlite");
    let err = run(ExportFilesConfig {
        archive: archive.clone(),
        dir: td.path().join("out"),
        objects: None,
        dry_run: false,
        logger: Logger::from_flags(true, 0),
    })
    .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert!(!archive.exists());
}
