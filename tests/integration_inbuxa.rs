/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Exports into inbuxa itself, an image pinned by digest. Run before each
//! release: `cargo test --test integration_inbuxa -- --ignored`.

mod integration;

use std::fs;
use std::path::Path;
use std::process::Command;

use inbuxa_migrate::db;
use integration::inbuxa::{ADMIN_EMAIL, ADMIN_PASSWORD, Inbuxa};
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_inbuxa-migrate");

fn message(folder: &Path, file: &str, id: &str, subject: &str) {
    for sub in ["cur", "new", "tmp"] {
        fs::create_dir_all(folder.join(sub)).unwrap();
    }
    fs::write(
        folder.join("cur").join(file),
        format!(
            "From: x@example.org\r\nTo: y@example.org\r\nSubject: {subject}\r\n\
             Message-ID: <{id}@example.org>\r\nDate: Mon, 4 Mar 2024 09:00:00 +0000\r\n\r\nHi\r\n"
        ),
    )
    .unwrap();
}

fn user(email: &str) -> Value {
    let (name, domain) = email.split_once('@').unwrap();
    json!({
        "email": email, "name": name, "domain": domain, "description": null,
        "roles": "user", "permissions": null, "aliases": [], "quotas": {},
        "tenant": null, "locale": null, "timeZone": null, "externalId": null, "groups": [],
    })
}

/// Each email in the account as (subject, number of mailboxes, keywords).
fn emails(server: &Inbuxa, email: &str, password: &str) -> Vec<(String, usize, Vec<String>)> {
    let account = server.account_of(email, password).unwrap();
    let r = server
        .call(
            email,
            password,
            json!([
                ["Email/query", {"accountId": account}, "q"],
                ["Email/get", {"accountId": account,
                    "#ids": {"resultOf": "q", "name": "Email/query", "path": "/ids"},
                    "properties": ["subject", "mailboxIds", "keywords"]}, "g"],
            ]),
        )
        .unwrap();
    let mut out: Vec<_> = r[1][1]["list"]
        .as_array()
        .unwrap_or_else(|| panic!("{r}"))
        .iter()
        .map(|e| {
            let mut keywords: Vec<String> = e["keywords"]
                .as_object()
                .map(|k| k.keys().cloned().collect())
                .unwrap_or_default();
            keywords.sort();
            (
                e["subject"].as_str().unwrap_or_default().to_owned(),
                e["mailboxIds"].as_object().map_or(0, |m| m.len()),
                keywords,
            )
        })
        .collect();
    out.sort();
    out
}

fn report(work: &Path) -> Vec<String> {
    fs::read_to_string(work.join("report.csv"))
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| l.split(',').take(9).collect::<Vec<_>>().join(","))
        .collect()
}

#[test]
#[ignore = "requires Docker"]
fn a_plan_sets_up_the_accounts_and_moves_each_with_one_admin_token() {
    let server = Inbuxa::start().expect("start inbuxa");
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("directory")).unwrap();
    let directory = json!({
        "format": "inbuxa-migrate-directory", "formatVersion": 1,
        "domains": [{"name": "other.test", "enabled": true, "description": null, "aliases": [],
                     "catchAllAddress": null, "subAddressing": "enabled", "tenant": null}],
        "tenants": [], "roles": [], "groups": [], "lists": [],
        "users": [user("ann@example.org"), user("bob@other.test")],
    });
    fs::write(root.join("directory/directory.json"), directory.to_string()).unwrap();
    // Ann's message is in her inbox and in Archive: one email, two mailboxes.
    message(&root.join("mail/ann"), "1:2,S", "ann-1", "For Ann");
    message(&root.join("mail/ann/.Archive"), "1:2,S", "ann-1", "For Ann");
    message(&root.join("mail/bob"), "1:2,", "bob-1", "For Bob");

    let url = server.base_url.as_str();
    let plan = json!({
        "format": "inbuxa-migrate-plan", "formatVersion": 1,
        "setup": [{"name": "directory",
            "args": ["import-directory", "--url", url, "--auth-bearer",
                     "--passwords", "passwords.csv", "directory"],
            "secrets": {"token": "ITEST_INBUXA_TOKEN"}}],
        "accounts": {"directory": "directory/directory.json"},
        "steps": [
            {"name": "mail", "args": ["import", "maildir", "mail/{local}", "{archive}"]},
            {"name": "inbuxa",
             "args": ["export", "--url", url, "--auth-bearer", "--account-name", "{email}",
                      "--objects", "mailbox,email", "{archive}"],
             "secrets": {"token": "ITEST_INBUXA_TOKEN"}},
        ],
    });
    fs::write(root.join("plan.json"), plan.to_string()).unwrap();
    let work = root.join("work");
    let migrate = |again: bool| {
        let mut command = Command::new(BIN);
        command
            .args(["migrate", "-q", "--plan"])
            .arg(root.join("plan.json"))
            .arg(&work)
            .env("ITEST_INBUXA_TOKEN", &server.token);
        if again {
            command.arg("--again");
        }
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stderr),
            fs::read_to_string(work.join("report.csv")).unwrap_or_default()
        );
    };

    migrate(false);
    let rows = report(&work);
    assert_eq!(rows[0], "(setup),directory,done,0,3,0,0,0,0", "{rows:?}");
    let passwords = fs::read_to_string(root.join("passwords.csv")).unwrap();
    let password = |email: &str| {
        passwords
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{email},")))
            .unwrap_or_else(|| panic!("{email} not in {passwords}"))
            .to_owned()
    };
    let (ann, bob) = (password("ann@example.org"), password("bob@other.test"));
    let seen = vec!["$seen".to_owned()];
    assert_eq!(
        emails(&server, "ann@example.org", &ann),
        [("For Ann".to_owned(), 2, seen)]
    );
    assert_eq!(
        emails(&server, "bob@other.test", &bob),
        [("For Bob".to_owned(), 1, Vec::new())]
    );
    // The administrator's own mailbox got nothing.
    assert_eq!(emails(&server, ADMIN_EMAIL, ADMIN_PASSWORD), []);

    // The final pass finds nothing new and sends nothing.
    migrate(true);
    let rows = report(&work);
    assert!(
        rows.iter()
            .filter(|r| r.contains(",inbuxa,"))
            .all(|r| r.contains(",inbuxa,done,0,0,0,0,")),
        "{rows:?}"
    );
    assert_eq!(emails(&server, "bob@other.test", &bob).len(), 1);
}

#[test]
#[ignore = "requires Docker"]
fn a_second_export_updates_flags_and_sieve_names_are_renamed() {
    let server = Inbuxa::start().expect("start inbuxa");
    let dir = TempDir::new().unwrap();
    let maildir = dir.path().join("mail");
    let archive = dir.path().join("archive.sqlite");
    message(&maildir, "1:2,S", "m-1", "Flag me");

    let run = |args: &[&str]| {
        let out = Command::new(BIN)
            .args(args)
            .env("INBUXA_MIGRATE_API_KEY", &server.token)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{args:?}: {stderr}");
        (String::from_utf8_lossy(&out.stdout).into_owned(), stderr)
    };
    let import = || {
        run(&[
            "import",
            "maildir",
            maildir.to_str().unwrap(),
            archive.to_str().unwrap(),
        ])
    };
    let export = || {
        run(&[
            "export",
            "--url",
            &server.base_url,
            "--auth-bearer",
            "--account-name",
            ADMIN_EMAIL,
            archive.to_str().unwrap(),
        ])
    };

    import();
    {
        let conn = db::init::open(&archive).unwrap();
        let blob = db::blobs::intern_blob(
            &conn,
            b"require [\"fileinto\", \"vnd.stalwart.while\"];\nkeep;\n",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (name, is_active, blob_id) VALUES ('main', 1, ?1)",
            rusqlite::params![blob],
        )
        .unwrap();
    }
    let (_, stderr) = export();
    assert!(
        stderr.contains("renamed vnd.stalwart.while to vnd.inbuxa.while"),
        "{stderr}"
    );
    assert_eq!(
        emails(&server, ADMIN_EMAIL, ADMIN_PASSWORD),
        [("Flag me".to_owned(), 1, vec!["$seen".to_owned()])]
    );
    let account = server.account_of(ADMIN_EMAIL, ADMIN_PASSWORD).unwrap();
    let r = server
        .call(
            ADMIN_EMAIL,
            ADMIN_PASSWORD,
            json!([["SieveScript/get", {"accountId": account}, "s"]]),
        )
        .unwrap();
    let script = r[0][1]["list"]
        .as_array()
        .and_then(|l| l.iter().find(|s| s["name"] == "main"))
        .unwrap_or_else(|| panic!("no script: {r}"))
        .clone();
    assert_eq!(script["isActive"], true, "{script}");
    let body = server
        .download(
            ADMIN_EMAIL,
            ADMIN_PASSWORD,
            &account,
            script["blobId"].as_str().unwrap(),
        )
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("vnd.inbuxa.while") && !body.contains("vnd.stalwart"),
        "{body}"
    );

    // Flagged at the source: the next import and export carry it over.
    fs::rename(maildir.join("cur/1:2,S"), maildir.join("cur/1:2,FS")).unwrap();
    import();
    let (stdout, _) = export();
    assert!(
        stdout.contains("Email: created=0 fetched=0 updated=1"),
        "{stdout}"
    );
    assert_eq!(
        emails(&server, ADMIN_EMAIL, ADMIN_PASSWORD),
        [(
            "Flag me".to_owned(),
            1,
            vec!["$flagged".to_owned(), "$seen".to_owned()]
        )]
    );
}
