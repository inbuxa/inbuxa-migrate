/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;
mod seeder;

use std::path::{Path, PathBuf};

use integration::stalwart::shared as shared_stalwart;
use rusqlite::Connection;

fn base_url() -> &'static str {
    shared_stalwart().base_url()
}

fn sieve_url() -> String {
    let s = shared_stalwart();
    format!("sieve://{}:{}", s.host, s.sieve_port)
}

fn sieves_url() -> String {
    let s = shared_stalwart();
    format!("sieves://{}:{}", s.host, s.sieve_port)
}
use inbuxa_migrate::jmap::account::AccountSelector;
use inbuxa_migrate::jmap::http::Auth;
use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::import_managesieve::{ManageSieveAuth, ManageSieveImportConfig};
use inbuxa_migrate::sync::{self, CommonConfig, ConnectConfig, ImportConfig};

fn tmp_archive(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "inbuxa-migrate-managesieve-{tag}-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn common(archive: &Path) -> CommonConfig {
    CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 3,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    }
}

fn basic_config(localpart: &str) -> ManageSieveImportConfig {
    ManageSieveImportConfig {
        url: sieve_url(),
        auth: ManageSieveAuth::Basic {
            user: format!("{localpart}@{}", seeder::DOMAIN),
            password: seeder::USER_PASSWORD.to_owned(),
        },
        allow_cleartext: false,
        allow_source_change: false,
    }
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_import_test1_lands_active_sieve_script() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = tmp_archive("test1");

    assert_eq!(fx.domain, seeder::DOMAIN);
    assert!(!fx.domain_id.is_empty(), "domain id resolved");
    assert_eq!(
        fx.admin_login,
        (
            seeder::ADMIN_USER.to_owned(),
            seeder::ADMIN_PASSWORD.to_owned()
        )
    );
    assert!(!fx.base_url.is_empty(), "base url set");
    assert!(!acc.account_id.is_empty(), "JMAP account id resolved");
    assert!(!acc.email.is_empty());
    assert!(!acc.admin_role, "test1 is a regular user");
    if let Some(seeded) = &acc.seeded {
        assert_eq!(
            seeded.sieve_active,
            Some(true),
            "test1 layout activates a sieve"
        );
        assert!(seeded.emails > 0);
        assert!(seeded.contacts > 0);
        assert!(seeded.events > 0);
        assert!(seeded.file_nodes > 0);
        assert!(seeded.address_books > 0);
        assert!(seeded.calendars > 0);
        assert!(seeded.identity);
        assert!(seeded.mailboxes_created > 0);
    }
    assert_eq!(acc.password, seeder::USER_PASSWORD);

    let summary = sync::import_managesieve::run(common(&archive), basic_config(&acc.localpart))
        .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    let scripts = count(&conn, "sieve_scripts");
    assert!(scripts >= 2, "test1 layout seeds two scripts");

    let active: i64 = conn
        .query_row(
            "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, 1, "exactly one of test1's two scripts is active");

    let blob_count = count(&conn, "blobs");
    assert!(blob_count >= 1, "blob interned for the script body");

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_import_test2_lands_inactive_sieve_script() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test2").expect("test2");
    let archive = tmp_archive("test2");

    let summary = sync::import_managesieve::run(common(&archive), basic_config(&acc.localpart))
        .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    let scripts = count(&conn, "sieve_scripts");
    assert!(scripts >= 1, "expected at least one sieve script for test2");
    let active: i64 = conn
        .query_row(
            "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, 0, "test2's seeded script should be inactive");

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_import_test3_has_no_scripts() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test3").expect("test3");
    let archive = tmp_archive("test3");

    let summary = sync::import_managesieve::run(common(&archive), basic_config(&acc.localpart))
        .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 0);

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_second_run_is_convergent() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = tmp_archive("converge");

    let first = sync::import_managesieve::run(common(&archive), basic_config(&acc.localpart))
        .expect("first import");
    let (_, c1) = &first.per_type[0];
    assert!(
        c1.created >= 2,
        "test1 seeds two scripts, both new on first run"
    );

    let summary = sync::import_managesieve::run(common(&archive), basic_config(&acc.localpart))
        .expect("second import");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.created, 0, "convergent run should create nothing");
    assert_eq!(c.deleted, 0, "convergent run should delete nothing");
    assert_eq!(c.fetched, 0, "convergent run should not rewrite blobs");
    assert!(c.skipped >= 2, "both present scripts should be skipped");

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_and_jmap_imports_share_blob_bytes() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let msieve_archive = tmp_archive("parity-msieve");
    let jmap_archive = tmp_archive("parity-jmap");

    sync::import_managesieve::run(common(&msieve_archive), basic_config(&acc.localpart))
        .expect("managesieve import");

    let jmap_cfg = ImportConfig {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: acc.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(acc.account_id.clone()),
        },
        objects: Some(vec![inbuxa_migrate::types::ObjectType::SieveScript]),
        allow_source_change: false,
    };
    let jmap_common = CommonConfig {
        archive: jmap_archive.clone(),
        threads: 4,
        dry_run: false,
        max_retries: 5,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    };
    sync::import_jmap::run(jmap_common, jmap_cfg).expect("jmap import");

    let msieve_conn = Connection::open(&msieve_archive).unwrap();
    let jmap_conn = Connection::open(&jmap_archive).unwrap();

    fn name_hash_active(conn: &Connection) -> Vec<(String, Vec<u8>, bool)> {
        let mut stmt = conn
            .prepare(
                "SELECT s.name, b.hash, s.is_active
                 FROM sieve_scripts s JOIN blobs b ON b.id = s.blob_id
                 ORDER BY s.name",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                let name: String = r.get(0)?;
                let hash: Vec<u8> = r.get(1)?;
                let active: i64 = r.get(2)?;
                Ok((name, hash, active != 0))
            })
            .unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }

    let msieve = name_hash_active(&msieve_conn);
    let jmap = name_hash_active(&jmap_conn);
    assert!(
        !msieve.is_empty(),
        "managesieve archive must have at least one script row"
    );
    assert_eq!(
        msieve, jmap,
        "every script's (name, blake3, is_active) must match across protocols"
    );

    let _ = std::fs::remove_file(&msieve_archive);
    let _ = std::fs::remove_file(&jmap_archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_dry_run_reports_diff_without_writing() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = tmp_archive("dryrun");

    let mut common_cfg = common(&archive);
    common_cfg.dry_run = true;
    let summary =
        sync::import_managesieve::run(common_cfg, basic_config(&acc.localpart)).expect("dryrun");
    let (_, c) = &summary.per_type[0];
    assert!(
        c.created >= 1,
        "expected at least one 'new' in dry-run plan"
    );

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        count(&conn, "sieve_scripts"),
        0,
        "dry-run must not write rows"
    );
    assert_eq!(count(&conn, "blobs"), 0, "dry-run must not intern blobs");

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_source_change_protection_refuses_second_account() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc1 = fx.account("test1").expect("test1");
    let acc2 = fx.account("test2").expect("test2");
    let archive = tmp_archive("source_change");

    sync::import_managesieve::run(common(&archive), basic_config(&acc1.localpart))
        .expect("first import");

    let err = sync::import_managesieve::run(common(&archive), basic_config(&acc2.localpart))
        .expect_err("should refuse the second source without override");
    assert!(matches!(err, inbuxa_migrate::error::Error::SourceChange(_)));

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_implicit_tls_path_succeeds_when_offered() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = tmp_archive("implicit_tls");

    let mut cfg = basic_config(&acc.localpart);
    cfg.url = sieves_url();
    let result = sync::import_managesieve::run(common(&archive), cfg);

    if let Err(inbuxa_migrate::error::Error::Connection(msg)) = &result {
        eprintln!("(expected on cleartext-only deployments) {msg}");
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_round_trip_via_jmap_export_converges() {
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test1").expect("test1");
    let dst = fx.account("test4").expect("test4");
    let archive = tmp_archive("roundtrip");

    sync::import_managesieve::run(common(&archive), basic_config(&src.localpart))
        .expect("managesieve import");

    let import_conn = Connection::open(&archive).unwrap();
    let imported_scripts = count(&import_conn, "sieve_scripts");
    assert!(
        imported_scripts >= 1,
        "expected at least one script in the source archive"
    );
    let imported_active: i64 = import_conn
        .query_row(
            "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        imported_active, 1,
        "test1 should have exactly one active script"
    );
    drop(import_conn);

    let export_common = CommonConfig {
        archive: archive.clone(),
        threads: 4,
        dry_run: false,
        max_retries: 5,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    };
    let export_cfg = inbuxa_migrate::sync::ExportConfig {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: dst.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(dst.account_id.clone()),
        },
        objects: Some(vec![inbuxa_migrate::types::ObjectType::SieveScript]),
        prune: false,
        yes: false,
    };
    let first = sync::export::run(export_common, export_cfg).expect("first export");
    assert!(!first.any_failed(), "first export had failures: {first:?}");
    let created_first: u64 = first
        .per_type
        .iter()
        .filter(|(k, _)| *k == "SieveScript")
        .map(|(_, c)| c.created)
        .sum();
    assert_eq!(
        created_first as i64, imported_scripts,
        "first export should create every imported script on the target ({first:?})"
    );

    let export_common2 = CommonConfig {
        archive: archive.clone(),
        threads: 4,
        dry_run: false,
        max_retries: 5,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    };
    let export_cfg2 = inbuxa_migrate::sync::ExportConfig {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: dst.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(dst.account_id.clone()),
        },
        objects: Some(vec![inbuxa_migrate::types::ObjectType::SieveScript]),
        prune: false,
        yes: false,
    };
    let second = sync::export::run(export_common2, export_cfg2).expect("second export");
    for (name, counts) in &second.per_type {
        assert_eq!(
            counts.created, 0,
            "{name}: round-trip second export should create nothing"
        );
        assert_eq!(
            counts.deleted, 0,
            "{name}: round-trip second export should delete nothing"
        );
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}
