/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! A `guide` started from the real binary, and requests sent to it the way
//! a browser would, raw, so a test can also send what a browser wouldn't.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub struct Guide {
    child: Child,
    /// Kept open: the guide may still be printing.
    _out: Option<BufReader<ChildStdout>>,
    pub host: String,
    pub token: String,
}

impl Drop for Guide {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `bin` is the test's `CARGO_BIN_EXE_inbuxa-migrate`.
pub fn start(bin: &str, dir: &Path) -> Guide {
    let mut child = Command::new(bin)
        .args(["guide", "--no-open"])
        .arg(dir)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    out.read_line(&mut line).unwrap();
    let url = line
        .trim()
        .strip_prefix("The guide is at http://")
        .unwrap_or_else(|| panic!("{line}"));
    let (host, token) = url.trim_end_matches('/').split_once('/').unwrap();
    Guide {
        host: host.to_owned(),
        token: token.to_owned(),
        child,
        _out: Some(out),
    }
}

/// A guide whose stdout is closed before it prints anything, on `port`;
/// the token is read from nowhere, so only its refusals can be checked.
pub fn start_unheard(bin: &str, dir: &Path, port: u16) -> Guide {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let child = Command::new(bin)
        .args(["guide", "--no-open", "--port", &port.to_string()])
        .arg(dir)
        .stdout(writer)
        .spawn()
        .unwrap();
    Guide {
        host: format!("127.0.0.1:{port}"),
        token: String::new(),
        child,
        _out: None,
    }
}

/// Status, headers and body of one request, sent as given.
pub fn send(guide: &Guide, head: &str, body: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(&guide.host).unwrap();
    write!(s, "{head}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let (headers, body) = out.split_once("\r\n\r\n").unwrap();
    let status = headers[9..12].parse().unwrap();
    (status, headers.to_owned(), body.to_owned())
}

pub fn get(guide: &Guide, path: &str) -> (u16, String, String) {
    send(
        guide,
        &format!(
            "GET /{}/{path} HTTP/1.1\r\nHost: {}",
            guide.token, guide.host
        ),
        "",
    )
}

pub fn post(guide: &Guide, path: &str, body: &Value) -> (u16, Value) {
    let (status, _, text) = send(
        guide,
        &format!(
            "POST /{}/{path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nOrigin: http://{}",
            guide.token, guide.host, guide.host
        ),
        &body.to_string(),
    );
    (status, serde_json::from_str(&text).unwrap())
}

/// The state once the running job, if any, has ended.
pub fn settled(guide: &Guide) -> Value {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let state: Value = serde_json::from_str(&get(guide, "api/state").2).unwrap();
        if !state["job"]["running"].as_bool().unwrap_or(false) {
            return state;
        }
        assert!(Instant::now() < deadline, "{state}");
        std::thread::sleep(Duration::from_millis(100));
    }
}
