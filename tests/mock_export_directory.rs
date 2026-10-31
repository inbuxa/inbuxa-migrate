/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-directory` against a registry served by a mock: what is asked
//! for, what is written, and that no secret reaches the files even from a
//! server that sends one unasked.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use inbuxa_migrate::export_directory::{self, ExportDirectoryConfig};
use inbuxa_migrate::jmap::http::Auth;
use inbuxa_migrate::logging::Logger;
use serde_json::{Value, json};

const SECRET: &str = "hunter2-never-written";

fn objects(ty: &str) -> Vec<Value> {
    match ty {
        "x:Domain" => vec![
            json!({"id": "b", "name": "example.org", "aliases": {"example.net": true},
            "isEnabled": true, "subAddressing": {"@type": "Enabled"},
            "dkimManagement": {"@type": "Automatic"}, "dnsZoneFile": "example.org. IN MX 10 mx."}),
        ],
        "x:Account" => vec![
            json!({"id": "b", "@type": "Group", "name": "sales", "domainId": "b",
                   "roles": {"@type": "Default"}, "quotas": {}, "aliases": {}}),
            json!({"id": "c", "@type": "User", "name": "alice", "domainId": "b",
                   "description": "Smith, Alice", "memberGroupIds": {"b": true},
                   "roles": {"@type": "User"}, "quotas": {"maxDiskQuota": 5},
                   "aliases": {"0": {"enabled": true, "name": "a", "domainId": "b"}},
                   "credentials": {"0": {"@type": "Password", "secret": SECRET}},
                   "encryptionAtRest": {"@type": "Aes256", "publicKey": SECRET}}),
        ],
        "x:MailingList" => vec![json!({"id": "b", "name": "all", "domainId": "b",
            "description": "=cmd|' /C calc'!A0", "recipients": {"alice@example.org": true}})],
        _ => Vec::new(),
    }
}

fn session(base: &str) -> String {
    json!({
        "apiUrl": format!("{base}/jmap/api"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        "capabilities": {
            "urn:ietf:params:jmap:core": {
                "maxObjectsInGet": 1, "maxObjectsInSet": 500, "maxCallsInRequest": 16,
                "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
                "maxSizeRequest": 10000000, "maxSizeUpload": 50000000
            }
        },
        "accounts": {
            "self": { "name": "admin", "accountCapabilities": {} },
            "adm": { "name": "admin", "accountCapabilities": { "urn:inbuxa:jmap:registry": {} } }
        }
    })
    .to_string()
}

/// Each get as (type, the properties it asked for).
type Asked = Arc<Mutex<Vec<(String, Vec<String>)>>>;

/// A registry that answers `/query` and `/get` from `objects`, records the
/// properties each get asked for, and with `honor_properties` off sends
/// every field whatever was asked.
struct Registry {
    server: mockito::ServerGuard,
    asked: Asked,
}

fn registry(honor_properties: bool) -> Registry {
    let mut server = mockito::Server::new();
    let base = server.url();
    server
        .mock("GET", "/jmap/session")
        .with_status(200)
        .with_body(session(&base))
        .create();
    let asked: Asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    server
        .mock("POST", "/jmap/api")
        .with_status(200)
        .with_body_from_request(move |req| {
            let body: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            assert!(
                body["using"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("urn:inbuxa:jmap:registry")),
                "{body}"
            );
            let call = &body["methodCalls"][0];
            let (method, args) = (call[0].as_str().unwrap(), &call[1]);
            assert_eq!(args["accountId"], "adm");
            let (ty, verb) = method.split_once('/').unwrap();
            let all = objects(ty);
            let response = match verb {
                "query" => {
                    let from = args["position"].as_u64().unwrap_or(0) as usize;
                    let ids: Vec<&Value> = all.iter().skip(from).map(|o| &o["id"]).collect();
                    json!({"accountId": "adm", "ids": ids, "position": from})
                }
                "get" => {
                    let props: Vec<String> = args["properties"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| p.as_str().unwrap().to_owned())
                        .collect();
                    log.lock().unwrap().push((ty.to_owned(), props.clone()));
                    let ids = args["ids"].as_array().unwrap();
                    let list: Vec<Value> = all
                        .into_iter()
                        .filter(|o| ids.contains(&o["id"]))
                        .map(|o| {
                            if !honor_properties {
                                return o;
                            }
                            let mut kept = serde_json::Map::new();
                            for (k, v) in o.as_object().unwrap() {
                                if k == "id" || props.contains(k) {
                                    kept.insert(k.clone(), v.clone());
                                }
                            }
                            Value::Object(kept)
                        })
                        .collect();
                    json!({"accountId": "adm", "list": list, "notFound": []})
                }
                other => panic!("unexpected {other}"),
            };
            json!({"methodResponses": [[method, response, call[2]]]})
                .to_string()
                .into_bytes()
        })
        .create();
    Registry { server, asked }
}

fn out_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("inbuxa-migrate-dir-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn config(registry: &Registry, dir: &std::path::Path) -> ExportDirectoryConfig {
    ExportDirectoryConfig {
        stay_on_url: false,
        url: format!("{}/jmap/session", registry.server.url()),
        auth: Auth::Basic {
            user: "admin".into(),
            password: "p".into(),
        },
        dir: dir.to_path_buf(),
        domains: Vec::new(),
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: Logger::new(0),
    }
}

fn every_file(dir: &std::path::Path) -> String {
    let mut text = String::new();
    for entry in fs::read_dir(dir).unwrap() {
        text.push_str(&fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    text
}

#[test]
fn the_directory_is_written_without_asking_for_secrets() {
    let registry = registry(true);
    let dir = out_dir("asked");
    export_directory::run(config(&registry, &dir)).expect("export");

    let asked = registry.asked.lock().unwrap();
    let types: BTreeSet<&str> = asked.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        types,
        BTreeSet::from(["x:Account", "x:Domain", "x:MailingList"])
    );
    // maxObjectsInGet is 1, so the two accounts take two gets.
    assert_eq!(asked.iter().filter(|(t, _)| t == "x:Account").count(), 2);
    for (_, props) in asked.iter() {
        for never in [
            "credentials",
            "encryptionAtRest",
            "dkimManagement",
            "dnsZoneFile",
        ] {
            assert!(!props.iter().any(|p| p == never), "{props:?}");
        }
    }

    let names: BTreeSet<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(
            [
                "aliases.csv",
                "directory.json",
                "domains.csv",
                "groups.csv",
                "lists.csv",
                "users.csv",
            ]
            .map(str::to_owned)
        )
    );
    let doc: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("directory.json")).unwrap()).unwrap();
    assert_eq!(doc["format"], "inbuxa-migrate-directory");
    assert_eq!(doc["users"][0]["email"], "alice@example.org");
    assert_eq!(doc["users"][0]["groups"], json!(["sales@example.org"]));
    assert_eq!(doc["groups"][0]["members"], json!(["alice@example.org"]));
    assert_eq!(doc["lists"][0]["description"], "=cmd|' /C calc'!A0");

    let users = fs::read_to_string(dir.join("users.csv")).unwrap();
    assert!(
        users.contains("alice@example.org,alice,example.org,\"Smith, Alice\",user,sales@example.org,a@example.org,5,"),
        "{users}"
    );
    let lists = fs::read_to_string(dir.join("lists.csv")).unwrap();
    assert!(lists.contains(",'=cmd|' /C calc'!A0,"), "{lists}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_secret_sent_unasked_is_still_not_written() {
    let registry = registry(false);
    let dir = out_dir("unasked");
    export_directory::run(config(&registry, &dir)).expect("export");
    let text = every_file(&dir);
    assert!(text.contains("alice@example.org"));
    assert!(!text.contains(SECRET));
    assert!(!text.contains("credentials"));
    assert!(!text.contains("IN MX"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_dry_run_writes_nothing() {
    let registry = registry(true);
    let dir = out_dir("dry");
    let mut config = config(&registry, &dir);
    config.dry_run = true;
    let summary = export_directory::run(config).expect("export");
    let users = &summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "user")
        .unwrap()
        .1;
    assert_eq!(users.created, 1);
    assert!(!dir.exists());
}
