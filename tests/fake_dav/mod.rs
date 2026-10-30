/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! A small in-memory CalDAV and CardDAV server for tests: discovery, depth-1
//! listings, multiget, GET, conditional PUT, MKCALENDAR, MKCOL and DELETE,
//! one request per connection. The home sets are `/cal/` and `/card/`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

/// What the server holds, by path: a collection's name, or an item's body
/// and etag.
#[derive(Clone)]
pub enum Resource {
    Collection { name: String, calendar: bool },
    Item { body: String, etag: u32 },
}

#[derive(Default)]
pub struct Store {
    pub resources: BTreeMap<String, Resource>,
    pub next_etag: u32,
    /// Each write as (method, path, condition header).
    pub writes: Vec<(String, String, String)>,
    /// Paths a PUT to is refused with 415.
    pub refuse: Vec<String>,
}

impl Store {
    pub fn collection(&mut self, path: &str, name: &str, calendar: bool) {
        self.resources.insert(
            path.to_owned(),
            Resource::Collection {
                name: name.to_owned(),
                calendar,
            },
        );
    }

    pub fn put_item(&mut self, path: &str, body: &str) {
        self.next_etag += 1;
        self.resources.insert(
            path.to_owned(),
            Resource::Item {
                body: body.to_owned(),
                etag: self.next_etag,
            },
        );
    }

    /// A path and everything under it.
    pub fn remove(&mut self, path: &str) {
        self.resources.retain(|p, _| !p.starts_with(path));
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn props(path: &str, resource: Option<&Resource>, with_data: bool) -> String {
    let inner = match resource {
        None => "<d:resourcetype><d:collection/></d:resourcetype>".to_owned(),
        Some(Resource::Collection { name, calendar }) => format!(
            "<d:resourcetype><d:collection/>{}</d:resourcetype><d:displayname>{name}</d:displayname>",
            if *calendar {
                "<c:calendar/>"
            } else {
                "<card:addressbook/>"
            }
        ),
        Some(Resource::Item { etag, body }) => {
            let data = match (with_data, path.starts_with("/cal/")) {
                (false, _) => String::new(),
                (true, true) => format!("<c:calendar-data>{}</c:calendar-data>", escape(body)),
                (true, false) => format!("<card:address-data>{}</card:address-data>", escape(body)),
            };
            format!("<d:resourcetype/><d:getetag>\"{etag}\"</d:getetag>{data}")
        }
    };
    format!(
        "<d:response><d:href>{path}</d:href><d:propstat><d:prop>{inner}</d:prop>\
         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
    )
}

fn multistatus(responses: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\" \
         xmlns:c=\"urn:ietf:params:xml:ns:caldav\" \
         xmlns:card=\"urn:ietf:params:xml:ns:carddav\">{responses}</d:multistatus>"
    )
}

fn displayname(body: &str) -> String {
    let start = body.find("displayname>").map(|i| i + "displayname>".len());
    start
        .and_then(|s| body[s..].find('<').map(|e| body[s..s + e].to_owned()))
        .unwrap_or_default()
}

/// Every `<…href>` in a request body.
fn hrefs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("href>") {
        rest = &rest[i + "href>".len()..];
        if let Some(end) = rest.find('<') {
            if !rest[..end].trim().is_empty() {
                out.push(rest[..end].trim().to_owned());
            }
            rest = &rest[end..];
        }
    }
    out
}

/// One request against the store, as (status, etag, body).
fn handle(
    store: &mut Store,
    method: &str,
    path: &str,
    headers: &BTreeMap<String, String>,
    body: &str,
) -> (u16, Option<u32>, String) {
    match method {
        "PROPFIND" if headers.get("depth").map(String::as_str) == Some("0") => {
            let home = "<d:current-user-principal><d:href>/p/</d:href></d:current-user-principal>\
                <c:calendar-home-set><d:href>/cal/</d:href></c:calendar-home-set>\
                <card:addressbook-home-set><d:href>/card/</d:href></card:addressbook-home-set>";
            let response = format!(
                "<d:response><d:href>{path}</d:href><d:propstat><d:prop>{home}</d:prop>\
                 <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
            );
            (207, None, multistatus(&response))
        }
        "PROPFIND" => {
            let mut out = props(path, store.resources.get(path), false);
            for (child, resource) in store.resources.range(path.to_owned()..) {
                let Some(rest) = child.strip_prefix(path) else {
                    break;
                };
                let direct = !rest.is_empty() && !rest.trim_end_matches('/').contains('/');
                if direct {
                    out.push_str(&props(child, Some(resource), false));
                }
            }
            (207, None, multistatus(&out))
        }
        "REPORT" => {
            let mut out = String::new();
            for href in hrefs(body) {
                match store.resources.get(&href) {
                    Some(item @ Resource::Item { .. }) => {
                        out.push_str(&props(&href, Some(item), true))
                    }
                    _ => out.push_str(&format!(
                        "<d:response><d:href>{href}</d:href>\
                         <d:status>HTTP/1.1 404 Not Found</d:status></d:response>"
                    )),
                }
            }
            (207, None, multistatus(&out))
        }
        "MKCALENDAR" | "MKCOL" => {
            store
                .writes
                .push((method.to_owned(), path.to_owned(), String::new()));
            if store.resources.contains_key(path) {
                return (405, None, String::new());
            }
            store.collection(path, &displayname(body), method == "MKCALENDAR");
            (201, None, String::new())
        }
        "GET" => match store.resources.get(path) {
            Some(Resource::Item { body, etag }) => (200, Some(*etag), body.clone()),
            _ => (404, None, String::new()),
        },
        "DELETE" => {
            store.remove(path);
            (204, None, String::new())
        }
        "PUT" => {
            let condition = headers
                .get("if-none-match")
                .map(|v| format!("If-None-Match: {v}"))
                .or_else(|| headers.get("if-match").map(|v| format!("If-Match: {v}")))
                .unwrap_or_default();
            store
                .writes
                .push(("PUT".to_owned(), path.to_owned(), condition));
            if store.refuse.iter().any(|r| r == path) {
                return (415, None, String::new());
            }
            let current = match store.resources.get(path) {
                Some(Resource::Item { etag, .. }) => Some(format!("\"{etag}\"")),
                _ => None,
            };
            let allowed = match (headers.get("if-none-match"), headers.get("if-match")) {
                (Some(_), _) => current.is_none(),
                (_, Some(want)) => current.as_deref() == Some(want.as_str()),
                _ => false,
            };
            if !allowed {
                return (412, None, String::new());
            }
            let status = if current.is_some() { 204 } else { 201 };
            store.put_item(path, body);
            (status, Some(store.next_etag), String::new())
        }
        _ => (405, None, String::new()),
    }
}

/// Serves `store` on a local port; the URL of its root.
pub fn serve(store: Arc<Mutex<Store>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_owned();
            let path = parts.next().unwrap_or_default().to_owned();
            let mut headers = BTreeMap::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
                }
            }
            let length: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8_lossy(&body);
            let (status, etag, out) =
                handle(&mut store.lock().unwrap(), &method, &path, &headers, &body);
            let etag = etag
                .map(|e| format!("ETag: \"{e}\"\r\n"))
                .unwrap_or_default();
            let kind = if status == 207 {
                "Content-Type: application/xml; charset=utf-8\r\n"
            } else {
                ""
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\n{etag}{kind}Content-Length: {}\r\nConnection: close\r\n\r\n{out}",
                out.len()
            );
        }
    });
    url
}
