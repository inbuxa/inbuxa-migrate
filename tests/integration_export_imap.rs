/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-imap` into a real Dovecot, read back with `import imap`: folders
//! (nested, non-ASCII, one with the separator in its name), a message kept
//! in two folders, one with no Message-ID, standard and custom flags. A
//! second run writes nothing, and a changed flag is all a third run sends.

mod integration;

use std::collections::{BTreeMap, BTreeSet};

use integration::dovecot::Dovecot;
use integration::validate::{cleanup, common, open_archive, tmp_archive};

use inbuxa_migrate::db::{blobs, init};
use inbuxa_migrate::sync::export_imap::{self, ExportImapConfig};
use inbuxa_migrate::sync::import_imap::{self, ImapAuth, ImapImportConfig};
use inbuxa_migrate::sync::{Summary, TypeCounts};
use rusqlite::{Connection, params};

fn message(subject: &str, with_id: bool) -> Vec<u8> {
    let id = if with_id {
        format!(
            "Message-ID: <{}@export-imap.test>\r\n",
            subject.replace(' ', "-")
        )
    } else {
        String::new()
    };
    format!(
        "From: dave@example.net\r\nTo: user@example.org\r\nSubject: {subject}\r\n{id}\
         Date: Fri, 01 Mar 2024 12:00:00 +0000\r\n\r\n{subject}\r\n"
    )
    .into_bytes()
}

fn seed(path: &std::path::Path) {
    let conn = init::open(path).unwrap();
    for (id, name, parent, role) in [
        (1, "Inbox", None, Some("inbox")),
        (2, "Clients", Some(1), None),
        (3, "Archive", None, Some("archive")),
        (4, "2024", Some(3), None),
        (5, "Équipe & co", None, None),
        (6, "Q1/Q2", None, None),
    ] {
        conn.execute(
            "INSERT INTO mailboxes (id, name, parent_id, role) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, parent, role],
        )
        .unwrap();
    }
    for (subject, with_id, mailboxes, keywords) in [
        ("Seen and flagged", true, "[1]", r#"["$seen","$flagged"]"#),
        ("Junk in two folders", true, "[1,3]", r#"["$Junk"]"#),
        ("Client note", true, "[2]", r#"["$answered","project-x"]"#),
        ("Old report", true, "[4]", "[]"),
        ("Bonjour", true, "[5]", r#"["$seen"]"#),
        ("No message id", false, "[6]", r#"["$seen"]"#),
    ] {
        let blob = blobs::intern_blob(&conn, &message(subject, with_id)).unwrap();
        conn.execute(
            "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords)
             VALUES (?1, '2024-03-01T12:00:00Z', ?2, ?3)",
            params![blob, mailboxes, keywords],
        )
        .unwrap();
    }
}

fn counts<'a>(summary: &'a Summary, ty: &str) -> &'a TypeCounts {
    &summary.per_type.iter().find(|(t, _)| *t == ty).unwrap().1
}

/// Each message as (folder path, subject) → keywords, lowercased.
fn mail_by_folder(conn: &Connection) -> BTreeMap<(String, String), BTreeSet<String>> {
    let rows: Vec<(i64, String, Option<i64>, Option<String>)> = conn
        .prepare("SELECT id, name, parent_id, role FROM mailboxes")
        .unwrap()
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
    let path_of = |id: i64| {
        let mut parts = vec![name_of(id)];
        let mut parent = rows.iter().find(|r| r.0 == id).unwrap().2;
        while let Some(p) = parent {
            parts.insert(0, name_of(p));
            parent = rows.iter().find(|r| r.0 == p).unwrap().2;
        }
        parts.join("/")
    };
    let mut out = BTreeMap::new();
    let mut stmt = conn
        .prepare("SELECT b.data, e.mailbox_ids, e.keywords FROM emails e JOIN blobs b ON b.id = e.blob_id")
        .unwrap();
    let rows_e = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap();
    for row in rows_e {
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
            out.insert((path_of(mailbox), subject.clone()), keywords.clone());
        }
    }
    out
}

#[test]
#[ignore = "requires Docker"]
fn dovecot_export_imap_round_trip_converges() {
    let d = Dovecot::start().expect("dovecot start");
    let account = d.accounts.last().expect("account").clone();
    let url = format!("imap://{}:{}", d.imap.host, d.imap.port);
    let auth = ImapAuth::Basic {
        user: account.username.clone(),
        password: account.password.clone(),
    };
    let config = || ExportImapConfig {
        url: url.clone(),
        auth: auth.clone(),
        allow_cleartext: false,
    };

    let archive = tmp_archive("export-imap-src");
    seed(&archive);

    let first = export_imap::run(common(&archive), config()).expect("first export");
    assert!(!first.any_failed(), "{first:?}");
    assert_eq!(counts(&first, "email").created, 7, "{first:?}");
    assert_eq!(counts(&first, "mailbox").created, 5, "{first:?}");

    let again = export_imap::run(common(&archive), config()).expect("second export");
    let email = counts(&again, "email");
    assert_eq!(
        (email.created, email.updated, email.skipped),
        (0, 0, 7),
        "{again:?}"
    );
    assert_eq!(counts(&again, "mailbox").created, 0);

    let back = tmp_archive("export-imap-back");
    let imported = import_imap::run(
        common(&back),
        ImapImportConfig {
            url: url.clone(),
            auth: auth.clone(),
            allow_cleartext: false,
            compress: false,
            include: Vec::new(),
            exclude: Vec::new(),
            exclude_special: Vec::new(),
            folder: Vec::new(),
            subscribed_only: false,
            automap: true,
            include_deleted: false,
            fetch_batch: 64,
            fetch_batch_bytes: inbuxa_migrate::sync::batch::DEFAULT_BATCH_BYTES,
            imap_connections: 1,
            allow_source_change: false,
        },
    )
    .expect("import back");
    assert!(!imported.any_failed(), "{imported:?}");

    let expected: BTreeMap<_, _> = mail_by_folder(&open_archive(&archive))
        .into_iter()
        // The target's separator is `/`, so it can't sit inside a name.
        .map(|((path, subject), kw)| ((path.replace("Q1/Q2", "Q1_Q2"), subject), kw))
        .collect();
    assert_eq!(mail_by_folder(&open_archive(&back)), expected);
    let received: String = open_archive(&back)
        .query_row("SELECT received_at FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(received, "2024-03-01T12:00:00Z");

    open_archive(&archive)
        .execute(
            "UPDATE emails SET keywords = '[\"$seen\"]' WHERE keywords LIKE '%flagged%'",
            [],
        )
        .unwrap();
    let third = export_imap::run(common(&archive), config()).expect("third export");
    let email = counts(&third, "email");
    assert_eq!(
        (email.created, email.updated, email.skipped),
        (0, 1, 6),
        "{third:?}"
    );

    cleanup(&archive);
    cleanup(&back);
    d.stop().expect("dovecot stop");
}
