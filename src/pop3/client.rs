/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! A small POP3 client (RFC 1939, STLS from RFC 2595), enough to read a
//! mailbox once for migration: sign in, list the messages with their stable
//! unique-ids and sizes, fetch each, and quit. It does not delete anything on
//! the server. The TLS transport is shared with the IMAP client -- the same
//! `Connector` and stream -- so STLS is an `upgrade_tls` on the stream.

use std::io::Write;

use crate::imap::transport::{Connector, ImapStream};

use super::error::Pop3Error;

/// How to reach the server: a TLS port from the first byte, a plain port that
/// upgrades with STLS, or (only when the caller allows it) plain throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pop3Mode {
    Plain,
    StartTls,
    ImplicitTls,
}

/// One message as the server lists it: its number for this session, its stable
/// unique-id (UIDL), and its size in octets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageListing {
    pub number: u32,
    pub uid: String,
    pub octets: u64,
}

const READ_CHUNK: usize = 16 * 1024;

pub struct Pop3Client {
    stream: Box<dyn ImapStream>,
    rx: Vec<u8>,
    rx_pos: usize,
}

impl Pop3Client {
    /// Connect, read the greeting, and (for `StartTls`) upgrade before anything
    /// secret is sent. The returned client is connected but not signed in.
    pub fn connect(
        connector: &Connector,
        host: &str,
        port: u16,
        mode: Pop3Mode,
    ) -> Result<Pop3Client, Pop3Error> {
        let stream = match mode {
            Pop3Mode::ImplicitTls => connector.connect_tls(host, port)?,
            Pop3Mode::Plain | Pop3Mode::StartTls => connector.connect_plain(host, port)?,
        };
        let mut client = Pop3Client {
            stream,
            rx: Vec::new(),
            rx_pos: 0,
        };
        // The greeting is one status line.
        client.read_status()?;
        if mode == Pop3Mode::StartTls {
            client.start_tls(connector, host)?;
        }
        Ok(client)
    }

    /// Build a client over a stream that is already connected and past its
    /// greeting -- used by the tests to drive the protocol without a socket.
    #[cfg(test)]
    pub fn from_stream(stream: Box<dyn ImapStream>) -> Pop3Client {
        Pop3Client {
            stream,
            rx: Vec::new(),
            rx_pos: 0,
        }
    }

    fn start_tls(&mut self, connector: &Connector, host: &str) -> Result<(), Pop3Error> {
        self.send("STLS")?;
        self.read_status()?;
        // The buffer must be empty before the handshake: a conformant server
        // sends nothing between the "+OK" and the TLS ClientHello, so anything
        // buffered would be a prefix of the handshake we must not swallow.
        if self.rx_pos < self.rx.len() {
            return Err(Pop3Error::Protocol(
                "server sent data after STLS +OK but before the TLS handshake".to_owned(),
            ));
        }
        self.rx.clear();
        self.rx_pos = 0;
        let stream = std::mem::replace(&mut self.stream, Box::new(Dead));
        self.stream = stream.upgrade_tls(connector, host)?;
        Ok(())
    }

    /// Sign in with a username and password (USER/PASS, RFC 1939). Over a plain
    /// connection this sends the password in the clear, which the caller gates.
    pub fn login(&mut self, user: &str, password: &str) -> Result<(), Pop3Error> {
        self.send(&format!("USER {user}"))?;
        self.read_status()
            .map_err(|e| Pop3Error::Auth(format!("USER refused: {e}")))?;
        self.send(&format!("PASS {password}"))?;
        self.read_status()
            .map_err(|e| Pop3Error::Auth(format!("PASS refused: {e}")))?;
        Ok(())
    }

    /// `STAT`: the message count and total size of the maildrop.
    pub fn stat(&mut self) -> Result<(u64, u64), Pop3Error> {
        self.send("STAT")?;
        let line = self.read_status()?;
        let mut parts = line.split_whitespace();
        let count = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Pop3Error::Protocol(format!("STAT: no count in {line:?}")))?;
        let octets = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Pop3Error::Protocol(format!("STAT: no size in {line:?}")))?;
        Ok((count, octets))
    }

    /// Every message's number, UIDL and size, by pairing `UIDL` and `LIST`.
    /// A message `UIDL` lists but `LIST` does not (or the reverse) is dropped
    /// with no size rather than guessed at.
    pub fn listing(&mut self) -> Result<Vec<MessageListing>, Pop3Error> {
        let uids = self.uidl()?;
        let sizes = self.list()?;
        let mut out = Vec::with_capacity(uids.len());
        for (number, uid) in uids {
            if let Some(&octets) = sizes.get(&number) {
                out.push(MessageListing {
                    number,
                    uid,
                    octets,
                });
            }
        }
        Ok(out)
    }

    /// `UIDL`: each message's number and its unique-id, stable across sessions.
    fn uidl(&mut self) -> Result<Vec<(u32, String)>, Pop3Error> {
        self.send("UIDL")?;
        self.read_status()?;
        let lines = self.read_multiline()?;
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            let text = String::from_utf8_lossy(&line);
            let mut parts = text.split_whitespace();
            let number = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Pop3Error::Protocol(format!("UIDL: no number in {text:?}")))?;
            let uid = parts
                .next()
                .ok_or_else(|| Pop3Error::Protocol(format!("UIDL: no unique-id in {text:?}")))?;
            out.push((number, uid.to_owned()));
        }
        Ok(out)
    }

    /// `LIST`: each message's number and size in octets.
    fn list(&mut self) -> Result<std::collections::HashMap<u32, u64>, Pop3Error> {
        self.send("LIST")?;
        self.read_status()?;
        let lines = self.read_multiline()?;
        let mut out = std::collections::HashMap::with_capacity(lines.len());
        for line in lines {
            let text = String::from_utf8_lossy(&line);
            let mut parts = text.split_whitespace();
            let number = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Pop3Error::Protocol(format!("LIST: no number in {text:?}")))?;
            let octets = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Pop3Error::Protocol(format!("LIST: no size in {text:?}")))?;
            out.insert(number, octets);
        }
        Ok(out)
    }

    /// `RETR`: the whole message, as RFC 822 bytes with CRLF line endings,
    /// dot-unstuffed and with the terminating dot removed.
    pub fn retr(&mut self, number: u32) -> Result<Vec<u8>, Pop3Error> {
        self.send(&format!("RETR {number}"))?;
        self.read_status()?;
        let lines = self.read_multiline()?;
        let mut body = Vec::new();
        for line in lines {
            body.extend_from_slice(&line);
            body.extend_from_slice(b"\r\n");
        }
        Ok(body)
    }

    /// `QUIT`, best effort: the caller is done, so a failure to read the final
    /// `+OK` does not matter.
    pub fn quit(&mut self) {
        if self.send("QUIT").is_ok() {
            let _ = self.read_status();
        }
    }

    fn send(&mut self, command: &str) -> Result<(), Pop3Error> {
        self.stream.write_all(command.as_bytes())?;
        self.stream.write_all(b"\r\n")?;
        self.stream.flush()?;
        Ok(())
    }

    /// Read one status line and return the text after `+OK`, or map `-ERR` to
    /// an error carrying the server's message.
    fn read_status(&mut self) -> Result<String, Pop3Error> {
        let line = self.next_line()?;
        let text = String::from_utf8_lossy(&line);
        if let Some(rest) = text.strip_prefix("+OK") {
            Ok(rest.trim().to_owned())
        } else if let Some(rest) = text.strip_prefix("-ERR") {
            Err(Pop3Error::Err(rest.trim().to_owned()))
        } else {
            Err(Pop3Error::Protocol(format!(
                "expected +OK or -ERR, got {text:?}"
            )))
        }
    }

    /// Read a dot-terminated multiline response: the data lines, each with any
    /// leading dot unstuffed and the trailing CRLF removed, up to the lone `.`.
    fn read_multiline(&mut self) -> Result<Vec<Vec<u8>>, Pop3Error> {
        let mut lines = Vec::new();
        loop {
            let mut line = self.next_line()?;
            if line == b"." {
                return Ok(lines);
            }
            // Dot-stuffing: a data line that begins with '.' had one prepended.
            if line.first() == Some(&b'.') {
                line.remove(0);
            }
            lines.push(line);
        }
    }

    /// One line, without its CRLF. A `\n` ends it; a bare `\r` does not. EOF
    /// before a newline is a protocol error -- a line, or the terminating dot,
    /// never arrived.
    fn next_line(&mut self) -> Result<Vec<u8>, Pop3Error> {
        loop {
            if let Some(rel) = self.rx[self.rx_pos..].iter().position(|&b| b == b'\n') {
                let end = self.rx_pos + rel;
                let mut line = self.rx[self.rx_pos..end].to_vec();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.rx_pos = end + 1;
                return Ok(line);
            }
            if !self.fill()? {
                return Err(Pop3Error::Protocol("connection closed mid-line".to_owned()));
            }
        }
    }

    /// Pull more bytes into the buffer. Returns false at end of stream. Drops
    /// the already-consumed prefix first, so the buffer does not grow without
    /// bound over a large RETR.
    fn fill(&mut self) -> Result<bool, Pop3Error> {
        if self.rx_pos > 0 {
            self.rx.drain(..self.rx_pos);
            self.rx_pos = 0;
        }
        let start = self.rx.len();
        self.rx.resize(start + READ_CHUNK, 0);
        let n = self.stream.read(&mut self.rx[start..])?;
        self.rx.truncate(start + n);
        Ok(n > 0)
    }
}

/// A placeholder stream, held for the instant between taking the real stream
/// out to upgrade it and putting the TLS stream back. It is never used.
struct Dead;

impl std::io::Read for Dead {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("stream taken for TLS upgrade"))
    }
}
impl Write for Dead {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("stream taken for TLS upgrade"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl ImapStream for Dead {
    fn upgrade_tls(
        self: Box<Self>,
        _: &Connector,
        _: &str,
    ) -> Result<Box<dyn ImapStream>, crate::imap::error::ImapError> {
        Err(crate::imap::error::ImapError::Protocol(
            "dead stream".into(),
        ))
    }
    fn is_tls(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read};
    use std::sync::{Arc, Mutex};

    /// A scripted in-memory peer: it serves `inbound` bytes to reads and
    /// records everything written, so a test can assert the commands sent.
    #[derive(Clone)]
    struct Script {
        inbound: Arc<Mutex<Vec<u8>>>,
        inbound_pos: Arc<Mutex<usize>>,
        outbound: Arc<Mutex<Vec<u8>>>,
    }

    impl Script {
        fn new(inbound: &[u8]) -> Self {
            Script {
                inbound: Arc::new(Mutex::new(inbound.to_vec())),
                inbound_pos: Arc::new(Mutex::new(0)),
                outbound: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn sent(&self) -> String {
            String::from_utf8_lossy(&self.outbound.lock().unwrap()).into_owned()
        }
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let inbound = self.inbound.lock().unwrap();
            let mut pos = self.inbound_pos.lock().unwrap();
            if *pos >= inbound.len() {
                return Ok(0);
            }
            let avail = &inbound[*pos..];
            let n = avail.len().min(buf.len());
            buf[..n].copy_from_slice(&avail[..n]);
            *pos += n;
            Ok(n)
        }
    }
    impl Write for Script {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.outbound.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl ImapStream for Script {
        fn upgrade_tls(
            self: Box<Self>,
            _: &Connector,
            _: &str,
        ) -> Result<Box<dyn ImapStream>, crate::imap::error::ImapError> {
            Ok(self)
        }
        fn is_tls(&self) -> bool {
            false
        }
    }

    fn client_for(inbound: &[u8]) -> (Pop3Client, Script) {
        let script = Script::new(inbound);
        let client = Pop3Client::from_stream(Box::new(script.clone()));
        (client, script)
    }

    #[test]
    fn login_sends_user_then_pass() {
        let (mut c, script) = client_for(b"+OK user ok\r\n+OK signed in\r\n");
        c.login("alice", "secret").unwrap();
        assert_eq!(script.sent(), "USER alice\r\nPASS secret\r\n");
    }

    #[test]
    fn login_maps_err_to_auth_failure() {
        let (mut c, _) = client_for(b"+OK user ok\r\n-ERR bad password\r\n");
        let err = c.login("alice", "wrong").unwrap_err();
        assert!(matches!(err, Pop3Error::Auth(_)), "{err}");
        assert!(err.to_string().contains("bad password"));
    }

    #[test]
    fn stat_parses_count_and_size() {
        let (mut c, _) = client_for(b"+OK 3 10240\r\n");
        assert_eq!(c.stat().unwrap(), (3, 10240));
    }

    #[test]
    fn listing_pairs_uidl_with_sizes() {
        // UIDL response, then LIST response.
        let (mut c, script) =
            client_for(b"+OK\r\n1 abc\r\n2 def\r\n.\r\n+OK\r\n1 100\r\n2 250\r\n.\r\n");
        let got = c.listing().unwrap();
        assert_eq!(
            got,
            vec![
                MessageListing {
                    number: 1,
                    uid: "abc".into(),
                    octets: 100
                },
                MessageListing {
                    number: 2,
                    uid: "def".into(),
                    octets: 250
                },
            ]
        );
        assert_eq!(script.sent(), "UIDL\r\nLIST\r\n");
    }

    #[test]
    fn listing_drops_a_message_with_a_uid_but_no_size() {
        let (mut c, _) = client_for(b"+OK\r\n1 abc\r\n2 def\r\n.\r\n+OK\r\n1 100\r\n.\r\n");
        let got = c.listing().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, "abc");
    }

    #[test]
    fn retr_reconstructs_crlf_and_unstuffs_dots() {
        // Body has a line starting with a dot (stuffed to "..") and a line that
        // is just "." in the content would itself be stuffed, so ".." decodes
        // to "." and "..foo" to ".foo".
        let (mut c, script) = client_for(
            b"+OK 20 octets\r\nFrom: a@x\r\nSubject: hi\r\n\r\n..stuffed\r\nplain\r\n.\r\n",
        );
        let body = c.retr(7).unwrap();
        assert_eq!(
            body,
            b"From: a@x\r\nSubject: hi\r\n\r\n.stuffed\r\nplain\r\n".to_vec()
        );
        assert_eq!(script.sent(), "RETR 7\r\n");
    }

    #[test]
    fn retr_handles_an_empty_body() {
        let (mut c, _) = client_for(b"+OK 0\r\n.\r\n");
        assert_eq!(c.retr(1).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn a_connection_that_closes_mid_line_is_a_protocol_error() {
        let (mut c, _) = client_for(b"+OK no newline");
        let err = c.stat().unwrap_err();
        assert!(matches!(err, Pop3Error::Protocol(_)), "{err}");
    }

    #[test]
    fn eight_bit_bytes_in_a_message_survive_retr() {
        let mut inbound = b"+OK\r\n".to_vec();
        inbound.extend_from_slice(b"Subject: \xc3\xa9\r\nbody \xff\xfe\r\n.\r\n");
        let (mut c, _) = client_for(&inbound);
        let body = c.retr(1).unwrap();
        assert_eq!(body, b"Subject: \xc3\xa9\r\nbody \xff\xfe\r\n".to_vec());
    }

    #[test]
    fn a_long_body_spanning_many_read_chunks_is_read_whole() {
        // More than READ_CHUNK bytes, to exercise fill() and prefix-draining.
        let mut inbound = b"+OK\r\n".to_vec();
        let line = vec![b'x'; 1000];
        let count = (READ_CHUNK / 1002) + 20;
        for _ in 0..count {
            inbound.extend_from_slice(&line);
            inbound.extend_from_slice(b"\r\n");
        }
        inbound.extend_from_slice(b".\r\n");
        let (mut c, _) = client_for(&inbound);
        let body = c.retr(1).unwrap();
        assert_eq!(body.len(), count * 1002);
    }
}
