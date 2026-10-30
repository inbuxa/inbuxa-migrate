/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `import carddav` with one card in two address books: it is one card in
//! both books, every run succeeds, and taking it out of one book, or
//! deleting that book, leaves it in the other.

mod fake_dav;

use std::path::Path;
use std::sync::{Arc, Mutex};

use fake_dav::{Store, serve};
use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::import_dav::{self, DavAuth, DavImportConfig, DavKindArg};
use inbuxa_migrate::sync::{CommonConfig, Summary, TypeCounts};
use rusqlite::Connection;
use tempfile::TempDir;

const ALICE: &str =
    "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-alice\r\nFN:Alice Smith\r\nEND:VCARD\r\n";
const BOB: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-bob\r\nFN:Bob Jones\r\nEND:VCARD\r\n";

fn import(url: &str, archive: &Path) -> Summary {
    let summary = import_dav::run(
        CommonConfig {
            archive: archive.to_path_buf(),
            threads: 1,
            dry_run: false,
            max_retries: 0,
            allow_invalid_certs: false,
            logger: Logger::from_flags(false, 0),
        },
        DavImportConfig {
            kind: DavKindArg::Carddav,
            url: url.to_owned(),
            auth: DavAuth::Basic {
                user: "u".into(),
                password: "p".into(),
            },
            allow_cleartext: true,
            dav_connections: 1,
            multiget_batch: 25,
            source_change: inbuxa_migrate::sync::import_dav::SourceChange::Reject,
        },
    )
    .expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    summary
}

fn cards(summary: &Summary) -> &TypeCounts {
    &summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "contactcard")
        .unwrap()
        .1
}

/// Each card's UID with the names of the books it is in.
fn books(archive: &Path) -> Vec<(String, Vec<String>)> {
    let conn = Connection::open(archive).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT c.uid, coalesce((SELECT json_group_array(name) FROM
                 (SELECT b.name FROM json_each(c.address_book_ids) j
                  JOIN address_books b ON b.id = j.value ORDER BY b.name)), '[]')
             FROM contact_cards c ORDER BY c.uid",
        )
        .unwrap();
    stmt.query_map([], |r| {
        let names: String = r.get(1)?;
        Ok((r.get(0)?, serde_json::from_str(&names).unwrap()))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn names(list: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    list.iter()
        .map(|(uid, books)| {
            (
                uid.to_string(),
                books.iter().map(|b| b.to_string()).collect(),
            )
        })
        .collect()
}

#[test]
fn a_card_in_two_address_books_is_one_card_in_both() {
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("archive.sqlite");
    let store = Arc::new(Mutex::new(Store::default()));
    {
        let mut s = store.lock().unwrap();
        s.collection("/card/personal/", "Personal", false);
        s.collection("/card/work/", "Work", false);
        s.put_item("/card/personal/alice.vcf", ALICE);
        s.put_item("/card/work/alice-copy.vcf", ALICE);
        s.put_item("/card/work/bob.vcf", BOB);
    }
    let url = serve(store.clone());

    let first = import(&url, &archive);
    assert_eq!(cards(&first).fetched, 3, "{first:?}");
    let both = names(&[
        ("card-alice", &["Personal", "Work"]),
        ("card-bob", &["Work"]),
    ]);
    assert_eq!(books(&archive), both);

    let again = import(&url, &archive);
    assert_eq!(cards(&again).fetched, 0, "nothing changed: {again:?}");
    assert_eq!(books(&archive), both);

    // Out of Work only: still in Personal.
    store.lock().unwrap().remove("/card/work/alice-copy.vcf");
    let left = import(&url, &archive);
    assert_eq!(cards(&left).deleted, 1, "{left:?}");
    assert_eq!(
        books(&archive),
        names(&[("card-alice", &["Personal"]), ("card-bob", &["Work"])])
    );

    // Back in Work, then Personal deleted as a whole: still in Work.
    store
        .lock()
        .unwrap()
        .put_item("/card/work/alice-copy.vcf", ALICE);
    import(&url, &archive);
    assert_eq!(books(&archive), both);
    store.lock().unwrap().remove("/card/personal/");
    import(&url, &archive);
    assert_eq!(
        books(&archive),
        names(&[("card-alice", &["Work"]), ("card-bob", &["Work"])])
    );

    // Gone from every book: gone.
    store.lock().unwrap().remove("/card/work/alice-copy.vcf");
    import(&url, &archive);
    assert_eq!(books(&archive), names(&[("card-bob", &["Work"])]));
}
