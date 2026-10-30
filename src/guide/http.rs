/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Just enough HTTP/1.1 for one page and its calls, from one browser on
//! this machine: a request per connection, then the connection closes.

use std::io::{self, BufRead, BufReader, Read, Write};

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 1024 * 1024;

pub struct Request {
    pub method: String,
    pub path: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

pub fn read(stream: impl Read) -> io::Result<Request> {
    let mut reader = BufReader::new(stream.take((MAX_HEAD + MAX_BODY) as u64));
    let mut head = 0;
    let mut line = String::new();
    let mut next_line = |reader: &mut BufReader<_>, line: &mut String| -> io::Result<()> {
        line.clear();
        head += reader.read_line(line)?;
        if head > MAX_HEAD {
            return Err(invalid("request head too large"));
        }
        Ok(())
    };
    next_line(&mut reader, &mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path), Some(_version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("bad request line"));
    };
    let (method, path) = (method.to_owned(), path.to_owned());
    let mut headers = Vec::new();
    loop {
        next_line(&mut reader, &mut line)?;
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            break;
        }
        let (name, value) = l.split_once(':').ok_or_else(|| invalid("bad header"))?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let mut request = Request {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    let length = match request.header("content-length") {
        Some(v) => v.parse::<usize>().map_err(|_| invalid("bad length"))?,
        None => 0,
    };
    if length > MAX_BODY {
        return Err(invalid("request body too large"));
    }
    request.body = vec![0; length];
    reader.read_exact(&mut request.body)?;
    Ok(request)
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
        }
    }

    pub fn json(value: &serde_json::Value) -> Self {
        Self::new(200, "application/json", value.to_string())
    }

    pub fn error(status: u16, message: &str) -> Self {
        Self::new(
            status,
            "application/json",
            serde_json::json!({ "error": message }).to_string(),
        )
    }

    /// The token is in the page's address, so nothing is cached, nothing
    /// is sent on as a referrer, and the page loads only from here.
    pub fn write(&self, mut stream: impl Write) -> io::Result<()> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            415 => "Unsupported Media Type",
            _ => "Error",
        };
        write!(
            stream,
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
             X-Content-Type-Options: nosniff\r\n\
             Content-Security-Policy: default-src 'self'; frame-ancestors 'none'; \
             form-action 'none'; base-uri 'none'\r\nConnection: close\r\n\r\n",
            self.status,
            self.content_type,
            self.body.len()
        )?;
        stream.write_all(&self.body)?;
        stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_read_with_its_headers_and_body() {
        let raw = b"POST /t/api/job HTTP/1.1\r\nHost: 127.0.0.1:1\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let r = read(&raw[..]).unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/t/api/job"));
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(r.body, b"{}");
    }

    #[test]
    fn oversized_or_broken_requests_are_refused() {
        let big = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(read(big.as_bytes()).is_err());
        assert!(read(&b"nonsense\r\n\r\n"[..]).is_err());
        let long = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert!(read(long.as_bytes()).is_err());
    }
}
