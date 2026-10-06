/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Batched `Email/import` on export: batches honor the server's limits, one
//! rejected message fails alone, and a batch that ends without a clear answer
//! is settled against the target instead of being sent again.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use inbuxa_migrate::db;
use inbuxa_migrate::jmap::account::AccountSelector;
use inbuxa_migrate::jmap::http::Auth;
use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::{self, CommonConfig, ConnectConfig, ExportConfig, TypeCounts};
use mockito::Matcher;
use serde_json::{Value, json};

const API: &str = "/jmap/api";

fn tmp() -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "inbuxa-migrate-exportbatch-{}-{:?}-{n}.sqlite",
        std::process::id(),
        std::thread::current().id(),
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn session(base: &str, max_objects_in_set: u64, max_concurrent_upload: u64) -> String {
    json!({
        "apiUrl": format!("{base}{API}"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        "capabilities": { "urn:ietf:params:jmap:core": {
            "maxObjectsInGet": 500, "maxObjectsInSet": max_objects_in_set,
            "maxCallsInRequest": 16, "maxConcurrentRequests": 4,
            "maxConcurrentUpload": max_concurrent_upload,
            "maxSizeRequest": 10000000, "maxSizeUpload": 50000000
        } },
        "accounts": { "w": { "name": "alice",
            "accountCapabilities": { "urn:ietf:params:jmap:mail": {} } } }
    })
    .to_string()
}

/// An archive with one Inbox and `n` distinct messages, `<m-1@h>` .. `<m-n@h>`.
fn seed(n: usize) -> PathBuf {
    let archive = tmp();
    let conn = db::init::open(&archive).unwrap();
    conn.execute(
        "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
        [],
    )
    .unwrap();
    for i in 1..=n {
        let raw = format!("From: a@x\r\nSubject: m{i}\r\nMessage-ID: <m-{i}@h>\r\n\r\nbody {i}");
        let blob = db::blobs::intern_blob(&conn, raw.as_bytes()).unwrap();
        let mm = inbuxa_migrate::sync::keys::index_to_json(
            &inbuxa_migrate::sync::emailmeta::email_index_from_blob(raw.as_bytes()),
        );
        conn.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords,message_match)
             VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]',?2)",
            rusqlite::params![blob, mm],
        )
        .unwrap();
    }
    archive
}

/// The session, an Inbox already on the target, and uploads. The target's
/// email list is left to each test.
fn mock_target(server: &mut mockito::ServerGuard, session_body: String) -> Vec<mockito::Mock> {
    vec![
        server.mock("GET", "/").with_status(404).create(),
        server
            .mock("GET", "/.well-known/jmap")
            .with_body(session_body)
            .create(),
        server
            .mock("POST", API)
            .match_body(Matcher::Regex("Mailbox/query".into()))
            .with_body(
                json!({"methodResponses":[["Mailbox/query",
                    {"accountId":"w","ids":["t1"]},"q"]]})
                .to_string(),
            )
            .create(),
        server
            .mock("POST", API)
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex("Mailbox/query".into()),
                Matcher::Regex("anchor".into()),
            ]))
            .with_body(
                json!({"methodResponses":[["Mailbox/query",
                    {"accountId":"w","ids":[]},"q"]]})
                .to_string(),
            )
            .create(),
        server
            .mock("POST", API)
            .match_body(Matcher::Regex("Mailbox/get".into()))
            .with_body(
                json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
                    {"id":"t1","name":"Inbox","role":"inbox","parentId":null,
                     "myRights":{"mayDelete":true}}],"notFound":[]},"g"]]})
                .to_string(),
            )
            .create(),
        server
            .mock("POST", Matcher::Regex("/jmap/upload/".into()))
            .with_body(json!({"blobId":"UP"}).to_string())
            .create(),
    ]
}

fn empty_email_query(server: &mut mockito::ServerGuard) -> mockito::Mock {
    server
        .mock("POST", API)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",{"accountId":"w","ids":[]},"q"]]}).to_string(),
        )
        .create()
}

/// The creation ids of the emails in an `Email/import` request body.
fn import_cids(body: &[u8]) -> Vec<String> {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    v["methodCalls"][0][1]["emails"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// An `Email/import` answer: each id in `cids` created, except those in
/// `rejected`, which come back as `invalidEmail`.
fn import_answer(cids: &[String], rejected: &[&str]) -> Vec<u8> {
    let mut created = serde_json::Map::new();
    let mut not_created = serde_json::Map::new();
    for cid in cids {
        if rejected.contains(&cid.as_str()) {
            not_created.insert(cid.clone(), json!({"type":"invalidEmail"}));
        } else {
            created.insert(
                cid.clone(),
                json!({"id": format!("T{cid}"), "blobId":"b","threadId":"t","size":10}),
            );
        }
    }
    json!({"methodResponses":[["Email/import",
        {"accountId":"w","created":created,"notCreated":not_created},"i"]]})
    .to_string()
    .into_bytes()
}

fn run_export(archive: &Path, base: &str, threads: usize) -> TypeCounts {
    let summary = sync::export::run(
        CommonConfig {
            archive: archive.to_path_buf(),
            threads,
            dry_run: false,
            max_retries: 1,
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
    .expect("export run");
    summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts")
}

#[test]
fn imports_are_batched_up_to_max_objects_in_set() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = seed(5);
    let _base_mocks = mock_target(&mut server, session(&base, 2, 4));
    let _eq = empty_email_query(&mut server);

    let sizes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = sizes.clone();
    let imports = server
        .mock("POST", API)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body_from_request(move |req| {
            let cids = import_cids(req.body().unwrap());
            seen.lock().unwrap().push(cids.len());
            import_answer(&cids, &[])
        })
        .expect(3)
        .create();

    let email = run_export(&archive, &base, 4);
    assert_eq!(email.created, 5);
    assert_eq!(email.failed, 0);
    imports.assert();
    let mut sizes = sizes.lock().unwrap().clone();
    sizes.sort_unstable();
    assert_eq!(
        sizes,
        vec![1, 2, 2],
        "no call carries more than maxObjectsInSet"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn a_rejected_message_in_a_batch_fails_alone() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = seed(3);
    let _base_mocks = mock_target(&mut server, session(&base, 50, 4));
    let _eq = empty_email_query(&mut server);

    let rejected = Arc::new(std::sync::Mutex::new(String::new()));
    let pick = rejected.clone();
    let imports = server
        .mock("POST", API)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body_from_request(move |req| {
            let cids = import_cids(req.body().unwrap());
            let mut sorted = cids.clone();
            sorted.sort();
            let middle = sorted[1].clone();
            *pick.lock().unwrap() = middle.clone();
            import_answer(&cids, &[middle.as_str()])
        })
        .expect(1)
        .create();

    let email = run_export(&archive, &base, 1);
    assert_eq!(email.created, 2, "the other two land");
    assert_eq!(email.failed, 1, "only {} fails", rejected.lock().unwrap());
    imports.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn an_unclear_batch_is_settled_against_the_target_not_resent() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = seed(2);
    let _base_mocks = mock_target(&mut server, session(&base, 50, 4));

    // First look: the target is empty. After the unclear batch: m-1 arrived.
    let queries = Arc::new(AtomicUsize::new(0));
    let q = queries.clone();
    let _eq = server
        .mock("POST", API)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body_from_request(move |req| {
            // A page after the first (it carries an anchor) is empty.
            let paged = String::from_utf8_lossy(req.body().unwrap()).contains("anchor");
            let ids = if paged || q.fetch_add(1, Ordering::SeqCst) == 0 {
                json!([])
            } else {
                json!(["arrived"])
            };
            json!({"methodResponses":[["Email/query",{"accountId":"w","ids":ids},"q"]]})
                .to_string()
                .into_bytes()
        })
        .create();
    let _eg = server
        .mock("POST", API)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","list":[
                {"id":"arrived","messageId":["m-1@h"],"mailboxIds":{"t1":true},"keywords":{}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .create();

    // The batch carries both messages and gets a gateway timeout, so it may
    // or may not have been applied. mockito answers with the first matching
    // mock still owed hits, so this answers the first import only; every
    // later import is created by the next mock, which records what it sees.
    let gateway = server
        .mock("POST", API)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_status(504)
        .expect(1)
        .create();
    let later = Arc::new(std::sync::Mutex::new(Vec::new()));
    let later_seen = later.clone();
    let _created = server
        .mock("POST", API)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body_from_request(move |req| {
            let cids = import_cids(req.body().unwrap());
            later_seen.lock().unwrap().push(cids.clone());
            import_answer(&cids, &[])
        })
        .create();

    let email = run_export(&archive, &base, 1);
    gateway.assert();
    assert!(
        queries.load(Ordering::SeqCst) >= 2,
        "the target is read again before anything is resent"
    );
    assert_eq!(
        *later.lock().unwrap(),
        vec![vec!["e2".to_owned()]],
        "only the message that did not arrive is sent again, on its own"
    );
    assert_eq!(
        email.created, 2,
        "one found on the target, one imported again"
    );
    assert_eq!(email.failed, 0);
    let _ = std::fs::remove_file(&archive);
}
