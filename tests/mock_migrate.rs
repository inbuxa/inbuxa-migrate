/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `migrate` runs a plan's steps for each account as runs of the real
//! binary: Maildir in, files out, offline. One account fails and the rest
//! go on; a second run resumes only what didn't finish; `--again` runs it
//! all once more. Counts in the report are summed over the types: a
//! mailbox and an email are two.

use std::fs;
use std::path::{Path, PathBuf};

use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::migrate::{self, MigrateConfig, Outcome};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

fn message(dir: &Path, name: &str, subject: &str) {
    for sub in ["cur", "new", "tmp"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    fs::write(
        dir.join("cur").join(format!("{name}:2,S")),
        format!(
            "From: a@example.org\r\nTo: b@example.org\r\nSubject: {subject}\r\n\
             Message-ID: <{name}@example.org>\r\nDate: Mon, 4 Mar 2024 09:00:00 +0000\r\n\r\nHi\r\n"
        ),
    )
    .unwrap();
}

fn config(plan: &Path, work: &Path) -> MigrateConfig {
    MigrateConfig {
        plan: plan.to_path_buf(),
        work: work.to_path_buf(),
        parallel: 2,
        again: false,
        only: Vec::new(),
        dry_run: false,
        program: PathBuf::from(env!("CARGO_BIN_EXE_inbuxa-migrate")),
        logger: Logger::from_flags(true, 0),
    }
}

fn emails(archive: &Path) -> i64 {
    Connection::open(archive)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM emails", [], |r| r.get(0))
        .unwrap()
}

fn report(outcome: &Outcome) -> Vec<String> {
    fs::read_to_string(&outcome.report)
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| l.split(',').take(5).collect::<Vec<_>>().join(","))
        .collect()
}

fn write_plan(path: &Path, steps: serde_json::Value) {
    let plan = json!({
        "format": "inbuxa-migrate-plan",
        "formatVersion": 1,
        "accounts": ["ann@example.org", "Bob@example.org"],
        "steps": steps,
    });
    fs::write(path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
}

#[test]
fn accounts_run_their_steps_resume_and_run_again() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    message(&root.join("mail/ann"), "m1", "First");
    let plan = root.join("plan.json");
    // Relative paths are the plan's own: the steps run in its directory.
    write_plan(
        &plan,
        json!([
            {"name": "mail", "args": ["import", "maildir", "mail/{local}", "{archive}"]},
            {"name": "files", "args": ["export-files", "{archive}", "out/{email}"]},
        ]),
    );
    let work = root.join("work");

    // Bob has no Maildir yet: his import fails, his export never runs.
    let first = migrate::run(config(&plan, &work)).unwrap();
    assert_eq!((first.done, first.unfinished), (1, 1));
    assert_eq!(
        report(&first),
        [
            "ann@example.org,mail,done,2,2",
            "ann@example.org,files,done,0,2",
            "bob@example.org,mail,failed,0,0",
            "bob@example.org,files,not run,0,0",
        ]
    );
    let detail = fs::read_to_string(&first.report).unwrap();
    assert!(
        detail.contains("mail/bob"),
        "the reason is reported: {detail}"
    );
    assert_eq!(emails(&work.join("ann@example.org/archive.sqlite")), 1);
    assert!(root.join("out/ann@example.org/manifest.json").exists());
    let log = fs::read_to_string(work.join("ann@example.org/mail.log")).unwrap();
    assert!(
        log.starts_with("== inbuxa-migrate import maildir mail/ann "),
        "{log}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&work), 0o700);
        assert_eq!(mode(&work.join("ann@example.org/mail.log")), 0o600);
    }

    // The second run leaves Ann alone (her export-files would refuse a
    // directory that isn't empty) and finishes Bob.
    message(&root.join("mail/bob"), "m2", "Second");
    let second = migrate::run(config(&plan, &work)).unwrap();
    assert_eq!((second.done, second.unfinished), (2, 0));
    assert_eq!(
        report(&second),
        [
            "ann@example.org,mail,done earlier,0,0",
            "ann@example.org,files,done earlier,0,0",
            "bob@example.org,mail,done,2,2",
            "bob@example.org,files,done,0,2",
        ]
    );

    // --again: every step runs once more and picks up what's new.
    write_plan(
        &plan,
        json!([{"name": "mail", "args": ["import", "maildir", "mail/{local}", "{archive}"]}]),
    );
    message(&root.join("mail/ann"), "m3", "Third");
    let mut again = config(&plan, &work);
    again.again = true;
    again.only = vec!["ann@example.org".into()];
    let third = migrate::run(again).unwrap();
    assert_eq!(report(&third), ["ann@example.org,mail,done,1,1"]);
    assert_eq!(emails(&work.join("ann@example.org/archive.sqlite")), 2);
}

#[test]
fn a_step_sees_only_the_secrets_it_names() {
    let dir = TempDir::new().unwrap();
    let plan = dir.path().join("plan.json");
    let step = |secrets: serde_json::Value| {
        json!([{
            "name": "mail",
            "args": ["import", "imap", "--url", "imap://127.0.0.1:9", "--allow-cleartext",
                     "--auth-basic", "{email}", "{archive}"],
            "secrets": secrets,
        }])
    };

    write_plan(&plan, step(json!({"password": "MIGRATE_TEST_UNSET_VAR"})));
    let err = migrate::run(config(&plan, &dir.path().join("w1")))
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("$MIGRATE_TEST_UNSET_VAR is not set"), "{err}");

    // No secret named: the password this test run may have in its own
    // environment is not passed on, and with no terminal the step fails
    // at once rather than waiting for one.
    write_plan(&plan, step(json!({})));
    let out = migrate::run(config(&plan, &dir.path().join("w2"))).unwrap();
    assert_eq!(out.unfinished, 2);
    let text = fs::read_to_string(&out.report).unwrap();
    assert!(text.contains("no password provided"), "{text}");
}
