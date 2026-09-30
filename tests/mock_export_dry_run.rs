/*
 * SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export --dry-run` predicts what a real run would fail on: a message
//! larger than the target accepts, an object too big for one request, and a
//! Sieve script the target rejects. It keeps the counts, so the run exits
//! non-zero just as the real one would, and it writes nothing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use inbuxa_migrate::db;
use inbuxa_migrate::jmap::account::AccountSelector;
use inbuxa_migrate::jmap::http::Auth;
use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::{self, CommonConfig, ConnectConfig, ExportConfig, Summary, TypeCounts};
use mockito::Matcher;
use serde_json::json;

const API: &str = "/jmap/api";

fn tmp() -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "inbuxa-migrate-exportdry-{}-{:?}-{n}.sqlite",
        std::process::id(),
        std::thread::current().id(),
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn session(base: &str, max_size_upload: u64, max_size_request: u64, sieve: &[&str]) -> String {
    json!({
        "apiUrl": format!("{base}{API}"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        "capabilities": { "urn:ietf:params:jmap:core": {
            "maxObjectsInGet": 500, "maxObjectsInSet": 500, "maxCallsInRequest": 16,
            "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
            "maxSizeRequest": max_size_request, "maxSizeUpload": max_size_upload
        } },
        "accounts": { "w": { "name": "alice",
            "accountCapabilities": {
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:contacts": {},
                "urn:ietf:params:jmap:sieve": { "sieveExtensions": sieve }
            } } }
    })
    .to_string()
}

fn dry_run(archive: &Path, base: &str) -> Summary {
    sync::export::run(
        CommonConfig {
            archive: archive.to_path_buf(),
            threads: 2,
            dry_run: true,
            max_retries: 0,
            allow_invalid_certs: false,
            logger: Logger::from_flags(true, 0),
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base.to_owned(),
                auth: Auth::Basic {
                    user: "u".into(),
                    password: "p".into(),
                },
                account: AccountSelector::Id("w".into()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("a dry run returns its plan")
}

fn counts(summary: &Summary, ty: &str) -> TypeCounts {
    summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ty)
        .map(|(_, c)| c.clone())
        .unwrap_or_else(|| panic!("no counts for {ty}: {summary:?}"))
}

fn root_and_session(server: &mut mockito::ServerGuard, body: String) -> Vec<mockito::Mock> {
    vec![
        server.mock("GET", "/").with_status(404).create(),
        server
            .mock("GET", "/.well-known/jmap")
            .with_body(body)
            .create(),
    ]
}

/// Nothing that writes to the account: no `/set`, no import. Returns the
/// mocks, each expecting no calls.
fn no_writes(server: &mut mockito::ServerGuard) -> Vec<mockito::Mock> {
    ["/set\"", "Email/import"]
        .into_iter()
        .map(|m| {
            server
                .mock("POST", API)
                .match_body(Matcher::Regex(m.into()))
                .expect(0)
                .create()
        })
        .collect()
}

fn empty(
    server: &mut mockito::ServerGuard,
    method: &str,
    reply: serde_json::Value,
) -> mockito::Mock {
    server
        .mock("POST", API)
        .match_body(Matcher::Regex(method.into()))
        .with_body(json!({ "methodResponses": [[method, reply, "x"]] }).to_string())
        .create()
}

#[test]
fn a_message_too_large_to_upload_is_predicted_to_fail() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        for body in ["short".to_owned(), "x".repeat(4000)] {
            let raw = format!(
                "From: a@x\r\nSubject: s\r\nMessage-ID: <{}@h>\r\n\r\n{body}",
                body.len()
            );
            let blob = db::blobs::intern_blob(&conn, raw.as_bytes()).unwrap();
            conn.execute(
                "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
                 VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]')",
                rusqlite::params![blob],
            )
            .unwrap();
        }
    }
    let _s = root_and_session(&mut server, session(&base, 1000, 10_000_000, &[]));
    let _mq = empty(
        &mut server,
        "Mailbox/query",
        json!({"accountId":"w","ids":[]}),
    );
    let _eq = empty(
        &mut server,
        "Email/query",
        json!({"accountId":"w","ids":[]}),
    );
    let no_upload = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .expect(0)
        .create();
    let writes = no_writes(&mut server);

    let summary = dry_run(&archive, &base);
    let email = counts(&summary, "Email");
    assert_eq!(email.created, 1, "the short one would be created");
    assert_eq!(email.failed, 1, "the long one is over maxSizeUpload");
    assert!(
        summary.any_failed(),
        "so the dry run exits non-zero, like a real run"
    );
    no_upload.assert();
    for w in writes {
        w.assert();
    }
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn a_contact_too_large_for_one_request_is_predicted_to_fail() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,is_default) VALUES (1,'Personal',1)",
            [],
        )
        .unwrap();
        let photo = db::blobs::intern_blob(&conn, &vec![b'P'; 8000]).unwrap();
        let huge = json!({ "@type": "Card", "name": { "full": "Photo Person" },
            "media": { "photo": { "@type": "Media", "kind": "photo",
                "@blob": photo, "mediaType": "image/png" } } })
        .to_string();
        let small = json!({ "@type": "Card", "name": { "full": "Small Person" } }).to_string();
        for (id, uid, data) in [(1, "huge-card", &huge), (2, "small-card", &small)] {
            conn.execute(
                "INSERT INTO contact_cards (id,uid,address_book_ids,data) VALUES (?1,?2,'[1]',?3)",
                rusqlite::params![id, uid, data],
            )
            .unwrap();
        }
    }
    let _s = root_and_session(&mut server, session(&base, 50_000_000, 4000, &[]));
    let _ab = empty(
        &mut server,
        "AddressBook/get",
        json!({"accountId":"w","list":[],"notFound":[]}),
    );
    let _cq = empty(
        &mut server,
        "ContactCard/query",
        json!({"accountId":"w","ids":[]}),
    );
    let writes = no_writes(&mut server);

    let summary = dry_run(&archive, &base);
    let cards = counts(&summary, "ContactCard");
    assert_eq!(cards.created, 1, "the small card would be created");
    assert_eq!(
        cards.failed, 1,
        "the card with the photo inlined is over maxSizeRequest"
    );
    assert!(summary.any_failed());
    for w in writes {
        w.assert();
    }
    let _ = std::fs::remove_file(&archive);
}

/// One active script with Stalwart's `vnd.stalwart.while`, against a target
/// that names it `vnd.inbuxa.while`; `validate` is the answer to
/// `SieveScript/validate`. Checks that the script sent for validation is the
/// renamed one, and returns the run's counts.
fn dry_run_one_script(validate: serde_json::Value) -> Summary {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        let blob = db::blobs::intern_blob(
            &conn,
            b"require [\"fileinto\", \"vnd.stalwart.while\"];\nkeep;\n",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (id,name,is_active,blob_id) VALUES (1,'main',1,?1)",
            rusqlite::params![blob],
        )
        .unwrap();
    }
    let _s = root_and_session(
        &mut server,
        session(
            &base,
            50_000_000,
            10_000_000,
            &["fileinto", "vnd.inbuxa.while"],
        ),
    );
    let _get = empty(
        &mut server,
        "SieveScript/get",
        json!({"accountId":"w","list":[],"notFound":[]}),
    );
    let upload = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .match_body(Matcher::Regex("vnd\\.inbuxa\\.while".into()))
        .with_body(json!({"blobId":"TMP"}).to_string())
        .expect(1)
        .create();
    let _validate = server
        .mock("POST", API)
        .match_body(Matcher::Regex("SieveScript/validate".into()))
        .with_body(json!({"methodResponses":[validate]}).to_string())
        .create();
    let writes = no_writes(&mut server);
    let summary = dry_run(&archive, &base);
    for w in writes {
        w.assert();
    }
    upload.assert();
    let _ = std::fs::remove_file(&archive);
    summary
}

#[test]
fn a_sieve_script_the_target_rejects_is_predicted_to_fail() {
    let summary = dry_run_one_script(json!(["SieveScript/validate",
        {"accountId":"w","error":{"type":"invalidScript","description":"unknown test"}},"v"]));
    let sieve = counts(&summary, "SieveScript");
    assert_eq!(sieve.failed, 1);
    assert_eq!(sieve.created, 0);
    assert!(summary.any_failed());
}

#[test]
fn a_valid_sieve_script_is_validated_after_renaming_and_nothing_is_written() {
    let summary = dry_run_one_script(json!(["SieveScript/validate",
        {"accountId":"w","error":null},"v"]));
    let sieve = counts(&summary, "SieveScript");
    assert_eq!(sieve.created, 1, "it would be created");
    assert_eq!(sieve.failed, 0);
    assert!(!summary.any_failed());
}

#[test]
fn a_target_without_validate_still_gets_a_plan() {
    let summary = dry_run_one_script(json!(["error",
        {"type":"unknownMethod"},"v"]));
    let sieve = counts(&summary, "SieveScript");
    assert_eq!(sieve.created, 1, "not checked, so planned as written");
    assert_eq!(sieve.failed, 0);
}
