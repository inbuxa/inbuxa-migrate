/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `guide` as the browser sees it: the real binary serving its page on
//! 127.0.0.1, answering only its own address and token, and running
//! `migrate` on the plan it writes. Maildir in, offline; the inbuxa side
//! is a port nothing listens on, so the export fails as it should.

mod integration;

use std::fs;

use integration::guide::{get, post, send, settled, start, start_unheard};
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_inbuxa-migrate");

#[test]
fn the_page_answers_only_its_own_address_and_token() {
    let dir = TempDir::new().unwrap();
    let guide = start(BIN, dir.path());

    let (status, headers, page) = get(&guide, "");
    assert_eq!(status, 200);
    assert!(page.contains("Move to inbuxa"), "{page}");
    assert!(headers.contains("Content-Security-Policy: default-src 'self'"));
    assert!(headers.contains("Referrer-Policy: no-referrer"));
    assert_eq!(get(&guide, "guide.js").0, 200);

    let wrong_token = format!("GET /{}x/ HTTP/1.1\r\nHost: {}", guide.token, guide.host);
    assert_eq!(send(&guide, &wrong_token, "").0, 404);
    let port = guide.host.rsplit(':').next().unwrap();
    let rebound = format!(
        "GET /{}/ HTTP/1.1\r\nHost: attacker.example:{port}",
        guide.token
    );
    assert_eq!(send(&guide, &rebound, "").0, 403);
    let form_post = format!(
        "POST /{}/api/job HTTP/1.1\r\nHost: {}\r\nContent-Type: text/plain",
        guide.token, guide.host
    );
    assert_eq!(send(&guide, &form_post, "{}").0, 415);
    let elsewhere = format!(
        "POST /{}/api/job HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nOrigin: https://attacker.example",
        guide.token, guide.host
    );
    assert_eq!(send(&guide, &elsewhere, "{}").0, 403);
}

#[test]
fn a_maildir_move_previews_runs_and_reports() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mail = root.join("mail/ann");
    for sub in ["cur", "new", "tmp"] {
        fs::create_dir_all(mail.join(sub)).unwrap();
    }
    fs::write(
        mail.join("cur/1:2,S"),
        "From: a@example.org\r\nSubject: Hi\r\nMessage-ID: <m1@example.org>\r\n\r\nHi\r\n",
    )
    .unwrap();
    let guide = start(BIN, root);

    let (_, parsed) = post(
        &guide,
        "api/accounts",
        &json!({"text": "email\nAnn@example.org\nnot one\n"}),
    );
    assert_eq!(parsed["accounts"], json!(["ann@example.org"]));
    assert_eq!(parsed["rejected"], json!(["not one"]));

    let form = json!({
        "source": {"kind": "maildir", "path": "mail/{local}"},
        "accounts": ["ann@example.org"],
        "create": false,
        "target": {"url": "http://127.0.0.1:9", "token": "t0ken-for-the-test"},
        "parallel": 2,
    });
    let (_, found) = post(&guide, "api/check-source", &json!({"form": form}));
    assert_eq!(found["ok"], true, "{found}");

    let (status, _) = post(&guide, "api/job", &json!({"kind": "preview", "form": form}));
    assert_eq!(status, 200);
    let state = settled(&guide);
    let report = state["job"]["result"]["report"].as_str().unwrap();
    assert!(
        report.contains("ann@example.org,import,done,2,2"),
        "{report}"
    );
    assert_eq!(state["job"]["exit"], 0, "{state}");

    // The token came once; an empty one keeps it.
    let mut again = form.clone();
    again["target"]["token"] = json!("");
    let (status, _) = post(&guide, "api/job", &json!({"kind": "run", "form": again}));
    assert_eq!(status, 200);
    let state = settled(&guide);
    assert_eq!(state["job"]["exit"], 5, "{state}");
    let report = state["job"]["result"]["report"].as_str().unwrap();
    assert!(
        report.contains("ann@example.org,import,done,2,2"),
        "{report}"
    );
    assert!(report.contains("ann@example.org,export,failed"), "{report}");
    assert!(
        state["command"]
            .as_str()
            .unwrap()
            .contains("export MIGRATE_INBUXA_TOKEN=…"),
        "{state}"
    );

    // The plan is an ordinary one, and nothing written holds the token.
    let plan: Value =
        serde_json::from_str(&fs::read_to_string(root.join("plan.json")).unwrap()).unwrap();
    assert_eq!(
        plan["steps"][0]["args"],
        json!(["import", "maildir", "mail/{local}", "{archive}"])
    );
    for name in [
        "plan.json",
        "preview-plan.json",
        "guide.json",
        "work/report.csv",
    ] {
        let text = fs::read_to_string(root.join(name)).unwrap();
        assert!(!text.contains("t0ken-for-the-test"), "{name}: {text}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.join("guide.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let saved: Value =
        serde_json::from_str(&fs::read_to_string(root.join("guide.json")).unwrap()).unwrap();
    assert_eq!(saved["form"]["target"]["url"], "http://127.0.0.1:9");
}

#[test]
fn a_closed_stdout_doesnt_end_the_guide() {
    let dir = TempDir::new().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let guide = start_unheard(BIN, dir.path(), port);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::net::TcpStream::connect(&guide.host).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the guide never listened"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let head = format!("GET /x/ HTTP/1.1\r\nHost: {}", guide.host);
    assert_eq!(send(&guide, &head, "").0, 404);
}
