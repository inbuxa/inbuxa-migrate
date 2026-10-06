/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::CommonConfig;
use inbuxa_migrate::sync::import_managesieve::{ManageSieveAuth, ManageSieveImportConfig, run};
use rusqlite::Connection;

type Script = Box<dyn FnOnce(&mut MockConn) -> std::io::Result<()> + Send + 'static>;

struct MockSieveServer {
    addr: String,
    _thread: thread::JoinHandle<()>,
}

struct MockConn {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl MockConn {
    fn write_line(&mut self, s: &str) -> std::io::Result<()> {
        self.writer.write_all(s.as_bytes())?;
        self.writer.write_all(b"\r\n")?;
        self.writer.flush()
    }

    fn write_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    fn read_line(&mut self) -> std::io::Result<String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed",
            ));
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    fn write_capability(&mut self, sasl: &str, starttls: bool) -> std::io::Result<()> {
        self.write_line("\"IMPLEMENTATION\" \"Mock ManageSieve\"")?;
        self.write_line("\"VERSION\" \"1.0\"")?;
        if starttls {
            self.write_line("\"STARTTLS\"")?;
        }
        self.write_line(&format!("\"SASL\" \"{sasl}\""))?;
        self.write_line("\"SIEVE\" \"fileinto vacation\"")?;
        Ok(())
    }
}

impl MockSieveServer {
    fn start<H>(handler: H) -> MockSieveServer
    where
        H: FnOnce(&mut MockConn) -> std::io::Result<()> + Send + 'static,
    {
        Self::start_scripts(vec![Box::new(handler)])
    }

    fn start_scripts(scripts: Vec<Script>) -> MockSieveServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let addr = format!("127.0.0.1:{port}");
        let queue: Mutex<Vec<Script>> = Mutex::new(scripts.into_iter().rev().collect());
        let queue = std::sync::Arc::new(queue);
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let writer = stream.try_clone().expect("clone");
                let reader = BufReader::new(stream);
                let mut conn = MockConn { reader, writer };
                let script = queue.lock().expect("queue").pop();
                if let Some(script) = script {
                    thread::spawn(move || {
                        let _ = script(&mut conn);
                    });
                } else {
                    let _ = conn.write_line("BYE \"no script left\"");
                }
            }
        });
        MockSieveServer {
            addr,
            _thread: thread,
        }
    }

    fn url(&self) -> String {
        format!("sieve://{}", self.addr)
    }
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn tempfile(label: &str) -> PathBuf {
    let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "inbuxa_migrate_mock_managesieve_{label}_{counter}.sqlite"
    ));
    if p.exists() {
        let _ = std::fs::remove_file(&p);
    }
    p
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or(0)
}

fn run_basic(
    server: &MockSieveServer,
    archive: &Path,
) -> Result<inbuxa_migrate::sync::Summary, inbuxa_migrate::error::Error> {
    let common = CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 1,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    };
    let config = ManageSieveImportConfig {
        url: server.url(),
        auth: ManageSieveAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        allow_cleartext: true,
        allow_source_change: false,
    };
    run(common, config)
}

#[test]
fn capability_after_auth_is_consumed_when_present() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK \"hi\"")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"PLAIN\""), "got {cmd}");

        conn.write_line("OK")?;

        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("CAPABILITY"), "got {cmd}");
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;

        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("LISTSCRIPTS"), "got {cmd}");
        conn.write_line("OK")?;

        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = tempfile("caps_after_auth");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn sasl_plain_succeeds_when_advertised() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"PLAIN\""), "got {cmd}");
        conn.write_line("OK \"auth ok\"")?;

        let _ = conn.read_line();
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = tempfile("plain");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn sasl_login_fallback_when_plain_rejected() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN LOGIN", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"PLAIN\""), "got {cmd}");

        conn.write_line("NO \"PLAIN not allowed here\"")?;

        let cmd = conn.read_line()?;
        assert!(
            cmd.starts_with("AUTHENTICATE \"LOGIN\""),
            "expected LOGIN fallback, got {cmd}"
        );

        conn.write_line("\"VXNlcm5hbWU6\"")?;
        let _user = conn.read_line()?;

        conn.write_line("\"UGFzc3dvcmQ6\"")?;
        let _pass = conn.read_line()?;
        conn.write_line("OK \"welcome\"")?;
        let _ = conn.read_line();
        conn.write_capability("PLAIN LOGIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = tempfile("login_fallback");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn auth_no_translates_to_connection_error() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("NO \"bad creds\"")?;
        Ok(())
    });
    let archive = tempfile("auth_fail");
    let err = run_basic(&server, &archive).unwrap_err();
    match err {
        inbuxa_migrate::error::Error::Connection(msg) => assert!(
            msg.contains("auth failed") || msg.contains("LOGIN"),
            "got {msg}"
        ),
        other => panic!("expected Connection, got {other:?}"),
    }
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn server_lacking_plain_login_for_basic_is_refused() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("OAUTHBEARER", false)?;
        conn.write_line("OK")?;
        Ok(())
    });
    let archive = tempfile("no_basic_mech");
    let err = run_basic(&server, &archive).unwrap_err();
    assert!(matches!(err, inbuxa_migrate::error::Error::Connection(_)));
    let _ = std::fs::remove_file(&archive);
}

fn auth_then<F>(conn: &mut MockConn, after_auth: F) -> std::io::Result<()>
where
    F: FnOnce(&mut MockConn) -> std::io::Result<()>,
{
    conn.write_capability("PLAIN", false)?;
    conn.write_line("OK")?;
    let _ = conn.read_line()?;
    conn.write_line("OK")?;
    let _ = conn.read_line()?;
    conn.write_capability("PLAIN", false)?;
    conn.write_line("OK")?;
    after_auth(conn)?;
    let _ = conn.read_line();
    conn.write_line("OK \"bye\"")?;
    Ok(())
}

#[test]
fn empty_listscripts_imports_zero_rows() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let cmd = c.read_line()?;
            assert!(cmd.starts_with("LISTSCRIPTS"));
            c.write_line("OK")?;
            Ok(())
        })
    });
    let archive = tempfile("empty_list");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn listscripts_with_one_active_imports_with_active_flag() {
    let script = b"require [\"fileinto\"];\nfileinto \"INBOX\";\n";
    let script_owned = script.to_vec();
    let server = MockSieveServer::start(move |conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"vacation\" ACTIVE")?;
            c.write_line("\"other\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line(&format!("{{{}}}", script_owned.len()))?;
            c.write_raw(&script_owned)?;
            c.write_raw(b"\r\n")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line(&format!("{{{}}}", script_owned.len()))?;
            c.write_raw(&script_owned)?;
            c.write_raw(b"\r\n")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let archive = tempfile("active");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 2);
    let active_name: String = conn
        .query_row(
            "SELECT name FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active_name, "vacation");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn getscript_quoted_body_is_imported_verbatim() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"oneliner\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"stop;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let archive = tempfile("quoted_body");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT b.data FROM blobs b JOIN sieve_scripts s ON s.blob_id=b.id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bytes, b"stop;");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn getscript_nonexistent_is_skipped_and_counted() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"ok-one\"")?;
            c.write_line("\"missing\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("NO (NONEXISTENT) \"vanished\"")?;
            Ok(())
        })
    });
    let archive = tempfile("nonexistent");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(summary.any_failed(), "expected at least one failure");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn referral_aborts_run_with_connection_error() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"x\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("NO (REFERRAL \"sieve://other.example/\") \"go\"")?;
            Ok(())
        })
    });
    let archive = tempfile("referral");
    let err = run_basic(&server, &archive).unwrap_err();
    match err {
        inbuxa_migrate::error::Error::Connection(msg) => assert!(msg.contains("referral"), "{msg}"),
        other => panic!("expected Connection, got {other:?}"),
    }
    let _ = std::fs::remove_file(&archive);
}

fn seed_then_run(seed: Script, second: Script) -> (MockSieveServer, PathBuf) {
    let server = MockSieveServer::start_scripts(vec![seed, second]);
    let archive = tempfile("multi");
    (server, archive)
}

fn run_against(
    server: &MockSieveServer,
    archive: &Path,
) -> Result<inbuxa_migrate::sync::Summary, inbuxa_migrate::error::Error> {
    run_basic(server, archive)
}

fn seed_one_then(seed_name: &str, seed_bytes: &str, second: Script) -> (MockSieveServer, PathBuf) {
    let seed_name_owned = seed_name.to_owned();
    let seed_bytes_owned = seed_bytes.to_owned();
    let seed: Script = Box::new(move |conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line(&format!("\"{seed_name_owned}\""))?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line(&format!("\"{seed_bytes_owned}\""))?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    seed_then_run(seed, second)
}

#[test]
fn present_unchanged_script_does_not_rewrite_blob() {
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"stop;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_one_then("a", "stop;", second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("reconcile");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.created, 0);
    assert_eq!(c.deleted, 0);
    assert_eq!(c.skipped, 1, "present unchanged should count as skipped");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn present_content_updated_swaps_blob_pointer() {
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"v2\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_one_then("a", "v1", second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("reconcile");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.fetched, 1, "content update should count as fetched");
    let conn = Connection::open(&archive).unwrap();
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT b.data FROM blobs b JOIN sieve_scripts s ON s.blob_id=b.id WHERE s.name='a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bytes, b"v2");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn vanished_script_is_deleted_locally() {
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_one_then("a", "x", second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("reconcile");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.deleted, 1, "vanished script should be counted deleted");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn active_flag_flip_does_not_violate_partial_unique_index() {
    let seed: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\" ACTIVE")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"x\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("\"b\" ACTIVE")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("\"x\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("\"y\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_then_run(seed, second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("reconcile");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    let active: String = conn
        .query_row(
            "SELECT name FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, "b");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn resume_after_partial_run_completes_remainder() {
    let seed: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"first\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("\"b\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"first\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"second\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_then_run(seed, second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("second");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.created, 1);
    assert_eq!(c.skipped, 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn second_run_is_convergent() {
    let s1: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"only\" ACTIVE")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let s2: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"only\" ACTIVE")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_then_run(s1, s2);
    let _ = run_against(&server, &archive).expect("first");
    let summary = run_against(&server, &archive).expect("second");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.created, 0);
    assert_eq!(c.fetched, 0);
    assert_eq!(c.deleted, 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn transient_no_on_getscript_retries_then_succeeds() {
    let archive = tempfile("transient");
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("NO \"try again later\"")?;

            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let common = CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 3,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    };
    let config = ManageSieveImportConfig {
        url: server.url(),
        auth: ManageSieveAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        allow_cleartext: true,
        allow_source_change: false,
    };
    let summary = run(common, config).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn bye_mid_listscripts_reconnects_and_succeeds() {
    let first: Script = Box::new(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("BYE \"server restarting\"")?;
        Ok(())
    });
    let second: Script = Box::new(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let server = MockSieveServer::start_scripts(vec![first, second]);
    let archive = tempfile("bye_listscripts");
    let common = CommonConfig {
        archive: archive.clone(),
        threads: 1,
        dry_run: false,
        max_retries: 3,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    };
    let config = ManageSieveImportConfig {
        url: server.url(),
        auth: ManageSieveAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        allow_cleartext: true,
        allow_source_change: false,
    };
    let summary = run(common, config).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn bye_mid_getscript_reconnects_and_completes_remaining_scripts() {
    let first: Script = Box::new(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("\"a\"")?;
        conn.write_line("\"b\"")?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("\"keep;\"")?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("BYE \"server restarting\"")?;
        Ok(())
    });
    let second: Script = Box::new(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("\"keep;\"")?;
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let server = MockSieveServer::start_scripts(vec![first, second]);
    let archive = tempfile("bye_getscript");
    let common = CommonConfig {
        archive: archive.clone(),
        threads: 1,
        dry_run: false,
        max_retries: 3,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    };
    let config = ManageSieveImportConfig {
        url: server.url(),
        auth: ManageSieveAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        allow_cleartext: true,
        allow_source_change: false,
    };
    let summary = run(common, config).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 2);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn post_auth_unsolicited_capability_is_consumed() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(
            cmd.starts_with("LISTSCRIPTS"),
            "expected LISTSCRIPTS directly (no CAPABILITY refresh), got {cmd}"
        );
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = tempfile("post_auth_caps");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn dry_run_does_not_mutate_archive() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let archive = tempfile("dryrun");
    let common = CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: true,
        max_retries: 1,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    };
    let config = ManageSieveImportConfig {
        url: server.url(),
        auth: ManageSieveAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        allow_cleartext: true,
        allow_source_change: false,
    };
    let summary = run(common, config).expect("dryrun");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.created, 1);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 0, "dry-run must not write");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn oauthbearer_authenticates_end_to_end_when_advertised() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("OAUTHBEARER", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(
            cmd.starts_with("AUTHENTICATE \"OAUTHBEARER\""),
            "expected OAUTHBEARER, got {cmd}"
        );
        conn.write_line("OK \"hi\"")?;
        let _ = conn.read_line();
        conn.write_capability("OAUTHBEARER", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("LISTSCRIPTS"), "got {cmd}");
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = tempfile("oauthbearer");
    let common = CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 1,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    };
    let config = ManageSieveImportConfig {
        url: server.url(),
        auth: ManageSieveAuth::Bearer {
            user: "alice@example.com".to_owned(),
            token: "tok-xyz".to_owned(),
        },
        allow_cleartext: true,
        allow_source_change: false,
    };
    let summary = run(common, config).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn plain_then_login_both_refused_aborts_run_with_combined_message() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN LOGIN", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"PLAIN\""), "{cmd}");
        conn.write_line("NO \"plain refused\"")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"LOGIN\""), "{cmd}");

        conn.write_line("\"VXNlcm5hbWU6\"")?;
        let _ = conn.read_line()?;

        conn.write_line("\"UGFzc3dvcmQ6\"")?;
        let _ = conn.read_line()?;
        conn.write_line("NO \"login refused too\"")?;
        Ok(())
    });
    let archive = tempfile("plain_and_login_refused");
    let err = run_basic(&server, &archive).unwrap_err();
    match err {
        inbuxa_migrate::error::Error::Connection(msg) => {
            assert!(
                msg.contains("LOGIN") && msg.contains("PLAIN"),
                "expected combined LOGIN/PLAIN error text, got {msg}"
            );
        }
        other => panic!("expected Connection, got {other:?}"),
    }
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn three_scripts_with_middle_active_lands_the_middle_active() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"first\"")?;
            c.write_line("\"middle\" ACTIVE")?;
            c.write_line("\"third\"")?;
            c.write_line("OK")?;
            for body in &["one", "two", "three"] {
                let _ = c.read_line()?;
                c.write_line(&format!("\"{body}\""))?;
                c.write_line("OK")?;
            }
            Ok(())
        })
    });
    let archive = tempfile("middle_active");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 3);
    let active: String = conn
        .query_row(
            "SELECT name FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, "middle");
    let total_active: i64 = conn
        .query_row(
            "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total_active, 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn transient_no_exhausting_max_retries_skips_script_and_followup_picks_it_up() {
    let archive = tempfile("retry_exhaustion");
    let first = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"flaky\"")?;
            c.write_line("OK")?;

            for _ in 0..3 {
                let _ = c.read_line()?;
                c.write_line("NO \"try again later\"")?;
            }
            Ok(())
        })
    });
    let common = |archive: &Path| CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 2,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    };
    let cfg = |url: String| ManageSieveImportConfig {
        url,
        auth: ManageSieveAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        allow_cleartext: true,
        allow_source_change: false,
    };
    let summary = run(common(&archive), cfg(first.url())).expect("first run");
    assert!(
        summary.any_failed(),
        "expected per-script failure to count toward exit 5: {summary:?}"
    );
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.failed, 1, "exactly one failure recorded");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        count(&conn, "sieve_scripts"),
        0,
        "the script must NOT have landed locally"
    );
    drop(conn);

    let second = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"flaky\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let mut cfg2 = cfg(second.url());
    cfg2.allow_source_change = true;
    let summary2 = run(common(&archive), cfg2).expect("second run");
    assert!(!summary2.any_failed(), "{summary2:?}");
    let (_, c2) = &summary2.per_type[0];
    assert_eq!(c2.created, 1);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "sieve_scripts"), 1);
    let _ = std::fs::remove_file(&archive);
}
