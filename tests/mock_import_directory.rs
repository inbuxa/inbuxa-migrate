/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `import-directory` into a registry served by a stateful mock: a
//! directory exported from one server, imported into an empty one and
//! exported again, comes out the same; every new user has the password
//! written for it; a second run creates nothing; a dry run sends no set.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use inbuxa_migrate::export_directory::{self, ExportDirectoryConfig};
use inbuxa_migrate::import_directory::{self, ImportDirectoryConfig};
use inbuxa_migrate::jmap::http::Auth;
use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::{Summary, TypeCounts};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

/// Objects by type, each with its id; and how many /set calls came in.
#[derive(Default)]
struct Store {
    objects: BTreeMap<String, Vec<Value>>,
    next_id: u64,
    sets: u64,
}

impl Store {
    fn get(&self, ty: &str, id: &str) -> Option<&Value> {
        self.objects.get(ty)?.iter().find(|o| o["id"] == id)
    }

    /// The registry's own checks that matter here: what an object points
    /// at must exist, names are unique, and a user's password comes in its
    /// credentials.
    fn check(&self, ty: &str, o: &Value) -> Result<(), String> {
        let list = self.objects.get(ty).map(Vec::as_slice).unwrap_or_default();
        let key = |v: &Value| match ty {
            "x:Role" => v["description"].clone(),
            "x:Account" | "x:MailingList" => json!([v["name"], v["domainId"]]),
            _ => v["name"].clone(),
        };
        if list.iter().any(|v| key(v) == key(o)) {
            return Err("alreadyExists".into());
        }
        if let Some(d) = o.get("domainId")
            && self
                .get("x:Domain", d.as_str().unwrap_or_default())
                .is_none()
        {
            return Err(format!("no domain {d}"));
        }
        if let Some(id) = o.get("memberTenantId").and_then(Value::as_str)
            && self.get("x:Tenant", id).is_none()
        {
            return Err(format!("no tenant {id}"));
        }
        if o["@type"] == "User" {
            let secret = o["credentials"]["0"]["secret"].as_str().unwrap_or_default();
            if o["credentials"]["0"]["@type"] != "Password" || secret.len() < 8 {
                return Err("no usable password".into());
            }
        }
        if o["name"] == "refused" {
            return Err("invalidProperties".into());
        }
        Ok(())
    }
}

fn session(base: &str) -> String {
    json!({
        "apiUrl": format!("{base}/jmap/api"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        "capabilities": {
            "urn:ietf:params:jmap:core": {
                "maxObjectsInGet": 500, "maxObjectsInSet": 2, "maxCallsInRequest": 16,
                "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
                "maxSizeRequest": 10000000, "maxSizeUpload": 50000000
            }
        },
        "accounts": {
            "adm": { "name": "admin", "accountCapabilities": { "urn:inbuxa:jmap:registry": {} } }
        }
    })
    .to_string()
}

struct Registry {
    server: mockito::ServerGuard,
    store: Arc<Mutex<Store>>,
}

impl Registry {
    fn url(&self) -> String {
        format!("{}/jmap/session", self.server.url())
    }
}

fn registry(store: Store) -> Registry {
    let mut server = mockito::Server::new();
    let base = server.url();
    server
        .mock("GET", "/jmap/session")
        .with_status(200)
        .with_body(session(&base))
        .create();
    let store = Arc::new(Mutex::new(store));
    let shared = store.clone();
    server
        .mock("POST", "/jmap/api")
        .with_status(200)
        .with_body_from_request(move |req| {
            let body: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            let call = &body["methodCalls"][0];
            let (method, args) = (call[0].as_str().unwrap(), &call[1]);
            assert_eq!(args["accountId"], "adm");
            let (ty, verb) = method.split_once('/').unwrap();
            let mut s = shared.lock().unwrap();
            let response = match verb {
                "query" => {
                    let from = args["position"].as_u64().unwrap_or(0) as usize;
                    let ids: Vec<Value> = s
                        .objects
                        .get(ty)
                        .into_iter()
                        .flatten()
                        .skip(from)
                        .map(|o| o["id"].clone())
                        .collect();
                    json!({"ids": ids, "position": from})
                }
                "get" => {
                    let props: Vec<&str> = args["properties"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| p.as_str().unwrap())
                        .collect();
                    let ids = args["ids"].as_array().unwrap();
                    let list: Vec<Value> = s
                        .objects
                        .get(ty)
                        .into_iter()
                        .flatten()
                        .filter(|o| ids.contains(&o["id"]))
                        .map(|o| {
                            let kept: Map<String, Value> = o
                                .as_object()
                                .unwrap()
                                .iter()
                                .filter(|(k, _)| *k == "id" || props.contains(&k.as_str()))
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect();
                            Value::Object(kept)
                        })
                        .collect();
                    json!({"list": list, "notFound": []})
                }
                "set" => {
                    s.sets += 1;
                    let mut created = Map::new();
                    let mut not_created = Map::new();
                    let create = args["create"].as_object().cloned().unwrap_or_default();
                    assert!(create.len() <= 2, "batches keep to maxObjectsInSet");
                    for (cid, o) in create {
                        match s.check(ty, &o) {
                            Ok(()) => {
                                s.next_id += 1;
                                let id = format!("n{}", s.next_id);
                                let mut o = o.as_object().unwrap().clone();
                                o.insert("id".into(), json!(id));
                                s.objects
                                    .entry(ty.to_owned())
                                    .or_default()
                                    .push(Value::Object(o));
                                created.insert(cid, json!({"id": id}));
                            }
                            Err(why) => {
                                not_created.insert(
                                    cid,
                                    json!({"type": "invalidProperties", "description": why}),
                                );
                            }
                        }
                    }
                    let update = args["update"].as_object().cloned().unwrap_or_default();
                    for (id, patch) in update {
                        let list = s.objects.get_mut(ty).unwrap();
                        let o = list.iter_mut().find(|o| o["id"] == id.as_str()).unwrap();
                        for (k, v) in patch.as_object().unwrap() {
                            o[k] = v.clone();
                        }
                    }
                    json!({"created": created, "notCreated": not_created})
                }
                other => panic!("unexpected {other}"),
            };
            json!({"methodResponses": [[method, response, call[2]]]})
                .to_string()
                .into_bytes()
        })
        .create();
    Registry { server, store }
}

/// A source server with one of everything, linked up.
fn source() -> Store {
    let mut s = Store::default();
    let mut add = |ty: &str, v: Value| s.objects.entry(ty.to_owned()).or_default().push(v);
    add(
        "x:Tenant",
        json!({"id": "t", "name": "Acme", "quotas": {"maxAccounts": 50}}),
    );
    add(
        "x:Role",
        json!({"id": "r1", "description": "Helpdesk", "memberTenantId": "t",
        "roleIds": {}, "enabledPermissions": {"impersonate": true}, "disabledPermissions": {}}),
    );
    add(
        "x:Role",
        json!({"id": "r2", "description": "Helpdesk lead", "roleIds": {"r1": true},
        "enabledPermissions": {}, "disabledPermissions": {"deleteAccount": true}}),
    );
    add(
        "x:Domain",
        json!({"id": "d", "name": "example.org", "aliases": {"example.net": true},
        "isEnabled": true, "subAddressing": {"@type": "Enabled"},
        "catchAllAddress": "ann@example.org", "memberTenantId": "t"}),
    );
    add(
        "x:Domain",
        json!({"id": "e", "name": "other.test", "isEnabled": false,
        "description": "parked", "subAddressing": {"@type": "Custom", "customRule": "r"}}),
    );
    add(
        "x:Account",
        json!({"id": "g", "@type": "Group", "name": "sales", "domainId": "d",
        "roles": {"@type": "Default"}, "quotas": {}, "aliases": {}}),
    );
    add(
        "x:Account",
        json!({"id": "a", "@type": "User", "name": "ann", "domainId": "d",
        "description": "Ann", "memberGroupIds": {"g": true}, "memberTenantId": "t",
        "roles": {"@type": "Custom", "roleIds": {"r2": true}},
        "permissions": {"@type": "Merge", "enabledPermissions": {"x": true}, "disabledPermissions": {}},
        "quotas": {"maxDiskQuota": 5000},
        "aliases": {"0": {"enabled": true, "name": "a", "domainId": "d", "description": "short"},
                    "1": {"enabled": false, "name": "ann", "domainId": "e"}},
        "locale": "en_US", "timeZone": "America/Phoenix", "externalId": "emp-1"}),
    );
    add(
        "x:Account",
        json!({"id": "b", "@type": "User", "name": "bob", "domainId": "e",
        "roles": {"@type": "Admin"}, "quotas": {}, "aliases": {}}),
    );
    add(
        "x:MailingList",
        json!({"id": "l", "name": "all", "domainId": "d", "description": "Everyone",
        "recipients": {"ann@example.org": true, "outside@else.where": true},
        "aliases": {"0": {"enabled": true, "name": "everyone", "domainId": "d"}}}),
    );
    s
}

fn auth() -> Auth {
    Auth::Basic {
        user: "admin".into(),
        password: "p".into(),
    }
}

fn export(registry: &Registry, dir: &Path) {
    let summary = export_directory::run(ExportDirectoryConfig {
        url: registry.url(),
        auth: auth(),
        dir: dir.to_path_buf(),
        domains: Vec::new(),
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    })
    .expect("export");
    assert!(!summary.any_failed(), "{summary:?}");
}

fn import(registry: &Registry, dir: &Path, passwords: Option<PathBuf>, dry_run: bool) -> Summary {
    import_directory::run(ImportDirectoryConfig {
        url: registry.url(),
        auth: auth(),
        dir: dir.to_path_buf(),
        domains: Vec::new(),
        passwords,
        dry_run,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    })
    .expect("import")
}

fn counts<'a>(summary: &'a Summary, ty: &str) -> &'a TypeCounts {
    &summary.per_type.iter().find(|(t, _)| *t == ty).unwrap().1
}

/// directory.json without what differs between any two runs.
fn directory(dir: &Path) -> Value {
    let mut v: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("directory.json")).unwrap()).unwrap();
    for k in ["created", "createdBy", "source"] {
        v.as_object_mut().unwrap().remove(k);
    }
    v
}

#[test]
fn a_directory_imported_and_exported_again_comes_out_the_same() {
    let tmp = TempDir::new().unwrap();
    let from = registry(source());
    let exported = tmp.path().join("from");
    export(&from, &exported);

    let to = registry(Store::default());
    let dry = import(&to, &exported, None, true);
    assert_eq!(counts(&dry, "user").created, 2, "{dry:?}");
    assert_eq!(to.store.lock().unwrap().sets, 0, "a dry run sends no set");

    let passwords = tmp.path().join("passwords.csv");
    let first = import(&to, &exported, Some(passwords.clone()), false);
    assert!(!first.any_failed(), "{first:?}");
    for (ty, n) in [
        ("tenant", 1),
        ("role", 2),
        ("domain", 2),
        ("group", 1),
        ("user", 2),
        ("mailinglist", 1),
    ] {
        assert_eq!(counts(&first, ty).created, n, "{ty}: {first:?}");
    }

    // Each user's stored password is the one written for it, privately.
    let text = fs::read_to_string(&passwords).unwrap();
    let rows: BTreeMap<&str, &str> = text
        .lines()
        .skip(1)
        .map(|l| l.split_once(',').unwrap())
        .collect();
    assert_eq!(
        rows.keys().copied().collect::<Vec<_>>(),
        ["ann@example.org", "bob@other.test"]
    );
    {
        let s = to.store.lock().unwrap();
        for user in s.objects["x:Account"]
            .iter()
            .filter(|a| a["@type"] == "User")
        {
            let domain = s
                .get("x:Domain", user["domainId"].as_str().unwrap())
                .unwrap();
            let email = format!(
                "{}@{}",
                user["name"].as_str().unwrap(),
                domain["name"].as_str().unwrap()
            );
            assert_eq!(
                user["credentials"]["0"]["secret"],
                rows[email.as_str()],
                "{email}"
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&passwords).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let again_dir = tmp.path().join("again");
    export(&to, &again_dir);
    assert_eq!(directory(&again_dir), directory(&exported));

    // Running again creates nothing and wants a new password file.
    let err = import_directory::run(ImportDirectoryConfig {
        url: to.url(),
        auth: auth(),
        dir: exported.clone(),
        domains: Vec::new(),
        passwords: Some(passwords.clone()),
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    })
    .expect_err("an existing password file is refused");
    assert!(err.to_string().contains("already exists"), "{err}");
    let second = import(&to, &exported, Some(tmp.path().join("second.csv")), false);
    for (ty, _) in &second.per_type {
        let c = counts(&second, ty);
        assert_eq!((c.created, c.failed), (0, 0), "{ty}: {second:?}");
    }
    assert_eq!(
        fs::read_to_string(tmp.path().join("second.csv")).unwrap(),
        "email,password\n"
    );
}

#[test]
fn a_refused_account_is_counted_and_the_rest_are_made() {
    let tmp = TempDir::new().unwrap();
    let mut src = source();
    src.objects.get_mut("x:Account").unwrap().push(
        json!({"id": "z", "@type": "User", "name": "refused", "domainId": "d",
               "roles": {"@type": "User"}, "quotas": {}, "aliases": {}}),
    );
    let from = registry(src);
    let exported = tmp.path().join("from");
    export(&from, &exported);
    let to = registry(Store::default());
    let passwords = tmp.path().join("p.csv");
    let summary = import(&to, &exported, Some(passwords.clone()), false);
    let users = counts(&summary, "user");
    assert_eq!((users.created, users.failed), (2, 1), "{summary:?}");
    let text = fs::read_to_string(&passwords).unwrap();
    assert!(
        !text.contains("refused"),
        "no password for an account not made: {text}"
    );
}
