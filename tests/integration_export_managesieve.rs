/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-managesieve` into a real Dovecot (Pigeonhole), read back with
//! `import managesieve`: a dry run checks every script and writes nothing,
//! scripts and the active one arrive exactly, a second run writes nothing, a changed script is all a third run sends, and a
//! script the server refuses is counted and doesn't stop the rest.

mod integration;

use std::collections::BTreeMap;

use integration::dovecot::Dovecot;
use integration::validate::{cleanup, common, open_archive, tmp_archive};

use inbuxa_migrate::db::{blobs, init};
use inbuxa_migrate::sync::export_managesieve::{self, ExportManageSieveConfig};
use inbuxa_migrate::sync::import_managesieve::{self, ManageSieveAuth, ManageSieveImportConfig};
use inbuxa_migrate::sync::{Summary, TypeCounts};
use rusqlite::{Connection, params};

const VACATION: &str = "require \"vacation\";\r\nvacation :days 7 \"Away until Monday.\";\r\n";
const FILTER: &str = "require \"fileinto\";\r\nif header :contains \"subject\" \"[list]\" {\r\n  fileinto \"INBOX\";\r\n}\r\n";
const BROKEN: &str = "this is not sieve;\r\n";

fn seed(conn: &Connection, scripts: &[(&str, &str, bool)]) {
    for (name, body, active) in scripts {
        let blob = blobs::intern_blob(conn, body.as_bytes()).unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (name, is_active, blob_id) VALUES (?1, ?2, ?3)",
            params![name, active, blob],
        )
        .unwrap();
    }
}

fn counts(summary: &Summary) -> &TypeCounts {
    &summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "sievescript")
        .unwrap()
        .1
}

/// Each script as name → (active, body).
fn scripts(conn: &Connection) -> BTreeMap<String, (bool, String)> {
    conn.prepare(
        "SELECT s.name, s.is_active, b.data FROM sieve_scripts s JOIN blobs b ON b.id = s.blob_id",
    )
    .unwrap()
    .query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            (
                r.get::<_, bool>(1)?,
                String::from_utf8(r.get::<_, Vec<u8>>(2)?).unwrap(),
            ),
        ))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

#[test]
#[ignore = "requires Docker"]
fn dovecot_export_managesieve_round_trip_converges() {
    let d = Dovecot::start().expect("dovecot start");
    let account = d.accounts.last().expect("account").clone();
    let auth = ManageSieveAuth::Basic {
        user: account.username.clone(),
        password: account.password.clone(),
    };
    let url = format!("sieve://{}:{}", d.sieve.host, d.sieve.port);
    let config = || ExportManageSieveConfig {
        url: url.clone(),
        auth: auth.clone(),
        allow_cleartext: true,
    };

    let archive = tmp_archive("export-sieve-src");
    seed(
        &init::open(&archive).unwrap(),
        &[
            ("vacation", VACATION, false),
            ("filter", FILTER, true),
            ("broken", BROKEN, false),
        ],
    );

    let mut dry = common(&archive);
    dry.dry_run = true;
    let planned = export_managesieve::run(dry, config()).expect("dry run");
    let c = counts(&planned);
    // CHECKSCRIPT finds the broken one before anything is written.
    assert_eq!((c.created, c.failed), (2, 1), "{planned:?}");

    // Created, not skipped: the dry run wrote nothing.
    let first = export_managesieve::run(common(&archive), config()).expect("first export");
    let c = counts(&first);
    assert_eq!((c.created, c.failed), (2, 1), "{first:?}");

    let again = export_managesieve::run(common(&archive), config()).expect("second export");
    let c = counts(&again);
    assert_eq!((c.created, c.updated, c.skipped, c.failed), (0, 0, 2, 1));

    let back = tmp_archive("export-sieve-back");
    let imported = import_managesieve::run(
        common(&back),
        ManageSieveImportConfig {
            url: url.clone(),
            auth: auth.clone(),
            allow_cleartext: true,
            allow_source_change: false,
        },
    )
    .expect("import back");
    assert!(!imported.any_failed(), "{imported:?}");
    let mut expected = scripts(&open_archive(&archive));
    expected.remove("broken");
    assert_eq!(scripts(&open_archive(&back)), expected);

    let changed = blobs::intern_blob(&open_archive(&archive), b"keep;\r\n").unwrap();
    open_archive(&archive)
        .execute(
            "UPDATE sieve_scripts SET blob_id = ?1 WHERE name = 'vacation'",
            params![changed],
        )
        .unwrap();
    let third = export_managesieve::run(common(&archive), config()).expect("third export");
    let c = counts(&third);
    assert_eq!((c.created, c.updated, c.skipped), (0, 1, 1), "{third:?}");

    cleanup(&archive);
    cleanup(&back);
    d.stop().expect("dovecot stop");
}
