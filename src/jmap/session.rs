/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::http::HttpClient;
use crate::jmap::request::{URN_INBUXA_REGISTRY, URN_STALWART_REGISTRY};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub api_url: String,
    pub upload_url: String,
    pub download_url: String,
    pub capabilities: IndexMap<String, Value>,
    pub accounts: IndexMap<String, Account>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub name: String,
    #[serde(default)]
    pub account_capabilities: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_objects_in_get: u64,
    pub max_objects_in_set: u64,
    pub max_calls_in_request: u64,
    pub max_concurrent_requests: u64,
    pub max_concurrent_upload: u64,
    pub max_size_request: u64,
    pub max_size_upload: u64,
}

fn parse_session(body: &str) -> Option<Session> {
    let value: Value = serde_json::from_str(body).ok()?;
    let has_shape = value.get("apiUrl").is_some()
        && value.get("accounts").is_some()
        && value.get("capabilities").is_some();
    if !has_shape {
        return None;
    }
    serde_json::from_value(value).ok()
}

fn well_known_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    format!("{trimmed}/.well-known/jmap")
}

fn scheme_end(url: &str) -> Option<usize> {
    let bytes = url.as_bytes();
    if !bytes.first()?.is_ascii_alphabetic() {
        return None;
    }
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte == b':' {
            return Some(index);
        }
        if !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')) {
            return None;
        }
    }
    None
}

pub fn resolve_reference(base: &str, reference: &str) -> String {
    if reference.is_empty() {
        return base.to_owned();
    }
    if scheme_end(reference).is_some() {
        return reference.to_owned();
    }
    let Some(base_scheme_end) = scheme_end(base) else {
        return reference.to_owned();
    };
    let scheme = &base[..=base_scheme_end];
    let Some(authority_and_path) = base[base_scheme_end + 1..].strip_prefix("//") else {
        return reference.to_owned();
    };
    let mut out = String::with_capacity(scheme.len() + 2 + base.len() + reference.len());
    out.push_str(scheme);
    out.push_str("//");
    if let Some(rest) = reference.strip_prefix("//") {
        out.push_str(rest);
        return out;
    }
    let authority_len = authority_and_path
        .find(['/', '?', '#'])
        .unwrap_or(authority_and_path.len());
    out.push_str(&authority_and_path[..authority_len]);
    if reference.starts_with('/') {
        out.push_str(reference);
        return out;
    }
    let path = &authority_and_path[authority_len..];
    let path = path.split(['?', '#']).next().unwrap_or("");
    match path.rfind('/') {
        Some(last) => out.push_str(&path[..=last]),
        None => out.push('/'),
    }
    out.push_str(reference);
    out
}

pub fn origin_of(url: &str) -> Option<String> {
    let origin = url::Url::parse(url).ok()?.origin();
    if !origin.is_tuple() {
        return None;
    }
    Some(origin.ascii_serialization())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginMismatch {
    pub fields: Vec<&'static str>,
    pub session_origin: String,
    pub connect_origin: String,
}

impl std::fmt::Display for OriginMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session advertises ")?;
        for (index, field) in self.fields.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(field)?;
        }
        write!(
            f,
            " on {} (connected to {}); inbuxa-migrate must use the advertised URL. \
             If this is wrong, fix the server's advertised HTTP URL setting.",
            self.session_origin, self.connect_origin
        )
    }
}

impl Session {
    pub fn discover(client: &HttpClient, url: &str) -> Result<Session, JmapError> {
        match client.get(url) {
            Ok(direct) => {
                if let Some(session) = parse_session(&direct) {
                    return session.rebased_on(url).ensure_authenticated();
                }
            }
            Err(JmapError::Auth(m)) => return Err(JmapError::Auth(m)),
            Err(_) => {}
        }
        let well_known = well_known_url(url);
        match client.get(&well_known) {
            Ok(body) => {
                if let Some(session) = parse_session(&body) {
                    return session.rebased_on(&well_known).ensure_authenticated();
                }
                Err(JmapError::Connect(format!(
                    "no JMAP Session object at {url} or {well_known}"
                )))
            }
            Err(JmapError::Auth(m)) => Err(JmapError::Auth(m)),
            Err(e) => Err(JmapError::Connect(format!(
                "session discovery failed: no Session at {url}; {well_known}: {e}"
            ))),
        }
    }

    fn rebased_on(mut self, session_resource_url: &str) -> Session {
        self.api_url = resolve_reference(session_resource_url, &self.api_url);
        self.upload_url = resolve_reference(session_resource_url, &self.upload_url);
        self.download_url = resolve_reference(session_resource_url, &self.download_url);
        self
    }

    /// The session with every URL moved onto `connect_url`'s origin: for
    /// `--stay-on-url`, when the address that reaches the server isn't the
    /// one it advertises, such as its own loopback.
    pub fn kept_on(mut self, connect_url: &str) -> Session {
        let Some(origin) = origin_of(connect_url) else {
            return self;
        };
        for url in [
            &mut self.api_url,
            &mut self.upload_url,
            &mut self.download_url,
        ] {
            if let Some(after) = url.find("://").map(|at| at + 3) {
                let path = url[after..]
                    .find('/')
                    .map_or(url.len(), |slash| after + slash);
                *url = format!("{origin}{}", &url[path..]);
            }
        }
        self
    }

    pub fn origin_mismatches(&self, connect_url: &str) -> Vec<OriginMismatch> {
        let Some(connect_origin) = origin_of(connect_url) else {
            return Vec::new();
        };
        let mut mismatches: Vec<OriginMismatch> = Vec::new();
        for (field, value) in [
            ("apiUrl", &self.api_url),
            ("uploadUrl", &self.upload_url),
            ("downloadUrl", &self.download_url),
        ] {
            let Some(session_origin) = origin_of(value) else {
                continue;
            };
            if session_origin == connect_origin {
                continue;
            }
            match mismatches
                .iter_mut()
                .find(|m| m.session_origin == session_origin)
            {
                Some(existing) => existing.fields.push(field),
                None => mismatches.push(OriginMismatch {
                    fields: vec![field],
                    session_origin,
                    connect_origin: connect_origin.clone(),
                }),
            }
        }
        mismatches
    }

    fn ensure_authenticated(self) -> Result<Session, JmapError> {
        if self.accounts.is_empty() {
            return Err(JmapError::Auth(
                "session enumerates no accounts (anonymous session: authentication failed)"
                    .to_owned(),
            ));
        }
        Ok(self)
    }

    pub fn core_limits(&self) -> Result<Limits, Error> {
        let core = self
            .capabilities
            .get("urn:ietf:params:jmap:core")
            .ok_or_else(|| {
                Error::Connection("session has no urn:ietf:params:jmap:core capability".to_owned())
            })?;
        serde_json::from_value(core.clone())
            .map_err(|e| Error::Connection(format!("session core capability is malformed: {e}")))
    }

    /// The capability to use for registry (`x:`) calls on this server:
    /// inbuxa's name when advertised, else Stalwart's, else none.
    pub fn registry_urn(&self) -> Option<&'static str> {
        [URN_INBUXA_REGISTRY, URN_STALWART_REGISTRY]
            .into_iter()
            .find(|urn| self.capabilities.contains_key(*urn))
    }

    /// The account to send registry (`x:`) calls to, with the capability
    /// to name for them. Servers advertise the registry per account, on the
    /// accounts whose credentials may administer it; a session-wide one is
    /// taken with the first account when no account lists it.
    pub fn registry_account(&self) -> Option<(&str, &'static str)> {
        for urn in [URN_INBUXA_REGISTRY, URN_STALWART_REGISTRY] {
            if let Some((id, _)) = self
                .accounts
                .iter()
                .find(|(_, a)| a.account_capabilities.contains_key(urn))
            {
                return Some((id.as_str(), urn));
            }
        }
        let urn = self.registry_urn()?;
        let (id, _) = self.accounts.first()?;
        Some((id.as_str(), urn))
    }

    pub fn account(&self, account_id: &str) -> Option<&Account> {
        self.accounts.get(account_id)
    }

    pub fn account_capabilities(&self, account_id: &str) -> Option<&IndexMap<String, Value>> {
        self.accounts
            .get(account_id)
            .map(|a| &a.account_capabilities)
    }

    pub fn supports(&self, account_id: &str, capability_urn: &str) -> bool {
        self.account_capabilities(account_id)
            .map(|caps| caps.contains_key(capability_urn))
            .unwrap_or(false)
    }

    pub fn may_support(&self, account_id: &str, capability_urn: &str) -> bool {
        match self.account_capabilities(account_id) {
            Some(caps) => caps.contains_key(capability_urn),
            None => self.capabilities.contains_key(capability_urn),
        }
    }

    pub fn upload_url_for(&self, account_id: &str) -> String {
        self.upload_url
            .replace("{accountId}", &encode_segment(account_id))
    }

    pub fn download_url_for(
        &self,
        account_id: &str,
        blob_id: &str,
        type_hint: &str,
        name: &str,
    ) -> String {
        self.download_url
            .replace("{accountId}", &encode_segment(account_id))
            .replace("{blobId}", &encode_segment(blob_id))
            .replace("{type}", &encode_segment(type_hint))
            .replace("{name}", &encode_segment(name))
    }
}

fn encode_segment(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let b = *byte;
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_session() -> &'static str {
        r#"{
            "apiUrl": "https://example.org/jmap/api",
            "uploadUrl": "https://example.org/jmap/upload/{accountId}/",
            "downloadUrl": "https://example.org/jmap/download/{accountId}/{blobId}/{type}/{name}",
            "capabilities": {
                "urn:ietf:params:jmap:core": {
                    "maxObjectsInGet": 500,
                    "maxObjectsInSet": 500,
                    "maxCallsInRequest": 16,
                    "maxConcurrentRequests": 4,
                    "maxConcurrentUpload": 4,
                    "maxSizeRequest": 10000000,
                    "maxSizeUpload": 50000000
                }
            },
            "accounts": {
                "w": {
                    "name": "vspec-user@example.org",
                    "accountCapabilities": { "urn:ietf:params:jmap:mail": {} }
                }
            }
        }"#
    }

    fn session_with(extra_caps: &[&str]) -> Session {
        let mut v: serde_json::Value = serde_json::from_str(raw_session()).unwrap();
        for urn in extra_caps {
            v["capabilities"][*urn] = serde_json::json!({});
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn registry_urn_prefers_inbuxa() {
        assert_eq!(
            session_with(&[URN_INBUXA_REGISTRY]).registry_urn(),
            Some(URN_INBUXA_REGISTRY)
        );
        assert_eq!(
            session_with(&[URN_STALWART_REGISTRY, URN_INBUXA_REGISTRY]).registry_urn(),
            Some(URN_INBUXA_REGISTRY)
        );
    }

    #[test]
    fn registry_urn_falls_back_to_stalwart() {
        assert_eq!(
            session_with(&[URN_STALWART_REGISTRY]).registry_urn(),
            Some(URN_STALWART_REGISTRY)
        );
    }

    #[test]
    fn registry_account_is_the_account_that_lists_it() {
        let mut v: serde_json::Value = serde_json::from_str(raw_session()).unwrap();
        v["accounts"]["a"] = serde_json::json!({
            "name": "admin",
            "accountCapabilities": { URN_INBUXA_REGISTRY: {} }
        });
        let session: Session = serde_json::from_value(v).unwrap();
        assert_eq!(session.registry_account(), Some(("a", URN_INBUXA_REGISTRY)));
    }

    #[test]
    fn registry_account_falls_back_to_a_session_capability() {
        assert_eq!(
            session_with(&[URN_STALWART_REGISTRY]).registry_account(),
            Some(("w", URN_STALWART_REGISTRY))
        );
        assert_eq!(session_with(&[]).registry_account(), None);
    }

    #[test]
    fn registry_urn_is_none_without_either() {
        assert_eq!(session_with(&[]).registry_urn(), None);
    }

    #[test]
    fn parses_a_session_and_reads_limits() {
        let session = parse_session(raw_session()).unwrap();
        let limits = session.core_limits().unwrap();
        assert_eq!(limits.max_objects_in_get, 500);
        assert_eq!(limits.max_size_upload, 50000000);
        assert_eq!(session.accounts["w"].name, "vspec-user@example.org");
    }

    #[test]
    fn capability_gate_reads_account_capabilities() {
        let session = parse_session(raw_session()).unwrap();
        assert!(session.supports("w", "urn:ietf:params:jmap:mail"));
        assert!(!session.supports("w", "urn:ietf:params:jmap:filenode"));
        assert!(!session.supports("missing", "urn:ietf:params:jmap:mail"));
    }

    // Listed accounts use their own capabilities; unlisted ones fall back to the session's.
    #[test]
    fn may_support_falls_back_to_session_capabilities_for_unlisted_account() {
        let session = parse_session(raw_session()).unwrap();
        assert!(session.may_support("w", "urn:ietf:params:jmap:mail"));
        assert!(!session.may_support("w", "urn:ietf:params:jmap:core"));
        assert!(session.may_support("missing", "urn:ietf:params:jmap:core"));
        assert!(!session.may_support("missing", "urn:ietf:params:jmap:mail"));
    }

    #[test]
    fn templates_upload_and_download_urls_with_encoding() {
        let session = parse_session(raw_session()).unwrap();
        assert_eq!(
            session.upload_url_for("a c"),
            "https://example.org/jmap/upload/a%20c/"
        );
        assert_eq!(
            session.download_url_for("w", "G1/2", "application/sieve", "my script"),
            "https://example.org/jmap/download/w/G1%2F2/application%2Fsieve/my%20script"
        );
    }

    #[test]
    fn rejects_body_that_is_not_a_session() {
        assert!(parse_session("{\"hello\":true}").is_none());
        assert!(parse_session("not json").is_none());
    }

    #[test]
    fn anonymous_session_is_auth_failure() {
        let raw =
            r#"{"apiUrl":"x","uploadUrl":"u","downloadUrl":"d","capabilities":{},"accounts":{}}"#;
        let session = parse_session(raw).unwrap();
        assert!(matches!(
            session.ensure_authenticated(),
            Err(JmapError::Auth(_))
        ));
    }

    #[test]
    fn origin_treats_the_default_port_as_equivalent() {
        assert_eq!(
            origin_of("https://mail.example/jmap"),
            origin_of("https://mail.example:443/jmap")
        );
        assert_eq!(
            origin_of("http://mail.example/jmap"),
            origin_of("http://mail.example:80/jmap")
        );
    }

    #[test]
    fn origin_distinguishes_port_host_and_scheme() {
        let base = origin_of("https://mail.example:10443/jmap");
        assert_ne!(base, origin_of("https://mail.example/jmap"));
        assert_ne!(base, origin_of("https://other.example:10443/jmap"));
        assert_ne!(base, origin_of("http://mail.example:10443/jmap"));
    }

    fn session_on(base: &str) -> Session {
        let raw = format!(
            r#"{{"apiUrl":"{base}/jmap/","uploadUrl":"{base}/jmap/upload/{{accountId}}/",
                "downloadUrl":"{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}",
                "capabilities":{{}},"accounts":{{}}}}"#
        );
        parse_session(&raw).expect("session")
    }

    #[test]
    fn matching_origin_reports_no_mismatch() {
        let session = session_on("https://mail.example:10443");
        assert!(
            session
                .origin_mismatches("https://mail.example:10443")
                .is_empty()
        );
        assert!(
            session_on("https://mail.example")
                .origin_mismatches("https://mail.example:443/")
                .is_empty()
        );
    }

    #[test]
    fn dropped_port_is_reported_once_naming_every_field() {
        let session = session_on("https://mail.example");
        let mismatches = session.origin_mismatches("https://mail.example:10443");
        assert_eq!(mismatches.len(), 1, "one warning per distinct origin");
        assert_eq!(
            mismatches[0].fields,
            vec!["apiUrl", "uploadUrl", "downloadUrl"]
        );
        assert_eq!(mismatches[0].session_origin, "https://mail.example");
        assert_eq!(mismatches[0].connect_origin, "https://mail.example:10443");
        let warning = mismatches[0].to_string();
        assert!(warning.contains("session advertises apiUrl"), "{warning}");
        assert!(
            warning.contains("on https://mail.example (connected to https://mail.example:10443)"),
            "{warning}"
        );
        assert!(warning.contains("advertised HTTP URL setting"), "{warning}");
    }

    #[test]
    fn a_split_upload_host_is_reported_separately_from_the_api_host() {
        let raw = r#"{"apiUrl":"https://mail.example:10443/jmap/",
            "uploadUrl":"https://blobs.example/upload/{accountId}/",
            "downloadUrl":"https://blobs.example/dl/{accountId}/{blobId}/{type}/{name}",
            "capabilities":{},"accounts":{}}"#;
        let session = parse_session(raw).expect("session");
        let mismatches = session.origin_mismatches("https://mail.example:10443");
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].fields, vec!["uploadUrl", "downloadUrl"]);
        assert_eq!(mismatches[0].session_origin, "https://blobs.example");
    }

    #[test]
    fn resolve_reference_keeps_absolute_urls_verbatim() {
        assert_eq!(
            resolve_reference(
                "https://h.example/.well-known/jmap",
                "https://other.example:8443/jmap/api"
            ),
            "https://other.example:8443/jmap/api"
        );
    }

    #[test]
    fn resolve_reference_handles_scheme_relative_absolute_and_relative_paths() {
        let base = "https://h.example:10443/.well-known/jmap";
        assert_eq!(
            resolve_reference(base, "//other.example/jmap/api"),
            "https://other.example/jmap/api"
        );
        assert_eq!(
            resolve_reference(base, "/jmap/api"),
            "https://h.example:10443/jmap/api"
        );
        assert_eq!(
            resolve_reference(base, "api"),
            "https://h.example:10443/.well-known/api"
        );
    }

    #[test]
    fn resolve_reference_preserves_uri_template_braces() {
        assert_eq!(
            resolve_reference(
                "https://h.example/.well-known/jmap",
                "/jmap/upload/{accountId}/"
            ),
            "https://h.example/jmap/upload/{accountId}/"
        );
    }

    #[test]
    fn relative_session_urls_are_resolved_against_the_session_resource() {
        let raw = r#"{"apiUrl":"/jmap/","uploadUrl":"/jmap/upload/{accountId}/",
            "downloadUrl":"/jmap/download/{accountId}/{blobId}/{type}/{name}",
            "capabilities":{},"accounts":{}}"#;
        let session = parse_session(raw)
            .expect("session")
            .rebased_on("https://h.example:10443/.well-known/jmap");
        assert_eq!(session.api_url, "https://h.example:10443/jmap/");
        assert_eq!(
            session.upload_url_for("w"),
            "https://h.example:10443/jmap/upload/w/"
        );
        assert!(
            session
                .origin_mismatches("https://h.example:10443")
                .is_empty(),
            "a resolved relative URL shares the connect origin"
        );
    }

    #[test]
    fn well_known_appends_correctly() {
        assert_eq!(
            well_known_url("https://h.example/"),
            "https://h.example/.well-known/jmap"
        );
        assert_eq!(
            well_known_url("https://h.example"),
            "https://h.example/.well-known/jmap"
        );
    }

    #[test]
    fn kept_on_moves_every_url_to_the_connect_origin() {
        let raw = r#"{"apiUrl":"https://mail.example/jmap/",
            "uploadUrl":"https://mail.example/jmap/upload/{accountId}/",
            "downloadUrl":"https://mail.example:443/jmap/download/{accountId}/{blobId}/{name}?accept={type}",
            "capabilities":{},"accounts":{}}"#;
        let session = parse_session(raw)
            .unwrap()
            .kept_on("http://127.0.0.1:8080/");
        assert_eq!(session.api_url, "http://127.0.0.1:8080/jmap/");
        assert_eq!(
            session.upload_url,
            "http://127.0.0.1:8080/jmap/upload/{accountId}/"
        );
        assert_eq!(
            session.download_url,
            "http://127.0.0.1:8080/jmap/download/{accountId}/{blobId}/{name}?accept={type}"
        );
        assert!(
            session
                .origin_mismatches("http://127.0.0.1:8080")
                .is_empty()
        );
    }
}
