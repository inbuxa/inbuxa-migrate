/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Settings every HTTP agent shares: timeouts, TLS, and which hosts, if any,
//! may present a certificate that does not verify.

use std::time::Duration;

use ureq::tls::{RootCerts, TlsConfig};

/// Opening the socket and completing any TLS handshake.
pub const CONNECT: Duration = Duration::from_secs(30);

/// Writing the request line and headers.
pub const SEND_REQUEST: Duration = Duration::from_secs(60);

/// Waiting for the response headers once the request is sent. This is the
/// server's thinking time: a large `Email/import`, an EWS `FindItem` over a big
/// folder or a CalDAV REPORT can legitimately take a while before the first
/// byte comes back.
pub const RECV_RESPONSE: Duration = Duration::from_secs(5 * 60);

/// Reading the whole response body. ureq counts this as one budget for the
/// entire body, not per read, so it has to cover the largest body a client
/// accepts (512 MiB) on a slow link: 30 minutes is about 300 KB/s. A stalled
/// transfer is abandoned and retried after at most this long.
pub const RECV_BODY: Duration = Duration::from_secs(30 * 60);

/// Sending a request body when its size is not known in advance. Uploads know
/// their size and get [`send_body_budget`] instead.
pub const SEND_BODY: Duration = Duration::from_secs(30 * 60);

/// The slowest upload rate a send budget allows for, in bytes per second.
const MIN_UPLOAD_RATE: u64 = 64 * 1024;

/// The floor under every send budget, so small bodies still get a sensible
/// allowance on a slow or busy connection.
const SEND_BODY_FLOOR: Duration = Duration::from_secs(2 * 60);

/// How long sending a body of `len` bytes may take: the floor plus the time it
/// takes at [`MIN_UPLOAD_RATE`].
pub fn send_body_budget(len: usize) -> Duration {
    SEND_BODY_FLOOR + Duration::from_secs(len as u64 / MIN_UPLOAD_RATE)
}

/// Applies the shared timeouts to a ureq `ConfigBuilder`. A macro rather than
/// a function because ureq keeps the builder's scope types private, so a
/// function could not name them.
macro_rules! with_timeouts {
    ($builder:expr) => {
        $builder
            .timeout_connect(Some($crate::net::CONNECT))
            .timeout_send_request(Some($crate::net::SEND_REQUEST))
            .timeout_send_body(Some($crate::net::SEND_BODY))
            .timeout_recv_response(Some($crate::net::RECV_RESPONSE))
            .timeout_recv_body(Some($crate::net::RECV_BODY))
    };
}
pub(crate) use with_timeouts;

/// TLS settings for an agent: the platform's roots, and certificate checks off
/// only when `accept_invalid` is set.
pub fn tls(accept_invalid: bool) -> TlsConfig {
    TlsConfig::builder()
        .unversioned_rustls_crypto_provider(std::sync::Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .root_certs(RootCerts::PlatformVerifier)
        .disable_verification(accept_invalid)
        .build()
}

/// Hosts that are always verified, whatever `--allow-invalid-certs` says:
/// the Microsoft and Google sign-in and cloud endpoints. A certificate that
/// fails there is an attack or a broken network, never a self-signed server
/// the user meant to trust. Matched as a suffix on a label boundary.
const ALWAYS_VERIFY: &[&str] = &[
    "microsoftonline.com",
    "microsoftonline.us",
    "microsoft.com",
    "microsoft.us",
    "office365.com",
    "office.com",
    "outlook.com",
    "chinacloudapi.cn",
    "partner.outlook.cn",
    "google.com",
    "googleapis.com",
    "gmail.com",
];

/// Where `--allow-invalid-certs` applies: the host the user named, or, for
/// Exchange Autodiscover without a `--url`, the mailbox's own domain and its
/// subdomains. Everything else, including any host a server redirects or
/// points to, is verified as usual.
#[derive(Debug, Clone, Default)]
pub struct CertOverride {
    hosts: Vec<String>,
    domains: Vec<String>,
}

impl CertOverride {
    /// Verify everything.
    pub fn none() -> Self {
        Self::default()
    }

    /// When `enabled`, accept invalid certificates from the host of `url`.
    pub fn for_url(enabled: bool, url: &str) -> Self {
        match (enabled, host_of(url)) {
            (true, Some(host)) if !always_verified(&host) => CertOverride {
                hosts: vec![host],
                domains: Vec::new(),
            },
            _ => Self::none(),
        }
    }

    /// When `enabled`, accept invalid certificates from `domain` and every
    /// host under it.
    pub fn for_domain(enabled: bool, domain: &str) -> Self {
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        if enabled && !domain.is_empty() && !always_verified(&domain) {
            CertOverride {
                hosts: Vec::new(),
                domains: vec![domain],
            }
        } else {
            Self::none()
        }
    }

    /// The same override, narrowed to the host of `url`, if `url` is one this
    /// override already covers. Used once Autodiscover has found the real
    /// endpoint.
    pub fn narrowed_to(&self, url: &str) -> Self {
        match host_of(url) {
            Some(host) if self.allows_host(&host) => CertOverride {
                hosts: vec![host],
                domains: Vec::new(),
            },
            _ => Self::none(),
        }
    }

    /// Whether this override covers anything at all.
    pub fn is_active(&self) -> bool {
        !self.hosts.is_empty() || !self.domains.is_empty()
    }

    /// Whether a certificate that does not verify is accepted for `url`.
    pub fn allows(&self, url: &str) -> bool {
        host_of(url).is_some_and(|host| self.allows_host(&host))
    }

    fn allows_host(&self, host: &str) -> bool {
        if always_verified(host) {
            return false;
        }
        self.hosts.iter().any(|h| h == host)
            || self
                .domains
                .iter()
                .any(|d| host == d || host.ends_with(&format!(".{d}")))
    }
}

fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    Some(
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned(),
    )
}

fn always_verified(host: &str) -> bool {
    ALWAYS_VERIFY
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_flag_covers_nothing() {
        let o = CertOverride::for_url(false, "https://mail.example.test/jmap");
        assert!(!o.is_active());
        assert!(!o.allows("https://mail.example.test/jmap"));
    }

    #[test]
    fn covers_only_the_named_host() {
        let o = CertOverride::for_url(true, "https://Mail.Example.test:8443/.well-known/jmap");
        assert!(o.is_active());
        assert!(o.allows("https://mail.example.test/api"));
        assert!(o.allows("https://MAIL.example.test:9000/upload"));
        assert!(!o.allows("https://files.example.test/download"));
        assert!(!o.allows("https://example.test/"));
        assert!(!o.allows("https://mail.example.test.evil.test/"));
    }

    #[test]
    fn sign_in_and_cloud_hosts_are_always_verified() {
        for url in [
            "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            "https://graph.microsoft.com/v1.0/me",
            "https://outlook.office365.com/EWS/Exchange.asmx",
            "https://autodiscover-s.outlook.com/autodiscover/autodiscover.xml",
            "https://oauth2.googleapis.com/token",
            "https://accounts.google.com/o/oauth2/device/code",
        ] {
            let o = CertOverride::for_url(true, url);
            assert!(!o.is_active(), "{url}");
            assert!(!o.allows(url), "{url}");
        }
    }

    #[test]
    fn a_domain_covers_its_subdomains_but_not_look_alikes() {
        let o = CertOverride::for_domain(true, "Corp.Example.");
        assert!(o.allows("https://autodiscover.corp.example/autodiscover/autodiscover.xml"));
        assert!(o.allows("https://corp.example/autodiscover/autodiscover.xml"));
        assert!(!o.allows("https://notcorp.example/"));
        assert!(!o.allows("https://corp.example.evil.test/"));
    }

    #[test]
    fn a_domain_override_never_reaches_microsoft() {
        let o = CertOverride::for_domain(true, "office365.com");
        assert!(!o.is_active());
        let corp = CertOverride::for_domain(true, "corp.example");
        assert!(!corp.allows("https://outlook.office365.com/EWS/Exchange.asmx"));
    }

    #[test]
    fn narrowing_keeps_only_a_covered_endpoint() {
        let o = CertOverride::for_domain(true, "corp.example");
        let inside = o.narrowed_to("https://mail.corp.example/EWS/Exchange.asmx");
        assert!(inside.allows("https://mail.corp.example/EWS/Exchange.asmx"));
        assert!(!inside.allows("https://autodiscover.corp.example/"));
        let outside = o.narrowed_to("https://outlook.office365.com/EWS/Exchange.asmx");
        assert!(!outside.is_active());
    }

    #[test]
    fn ip_literals_are_matched() {
        let o = CertOverride::for_url(true, "https://[::1]:8443/jmap");
        assert!(o.allows("https://[::1]:9000/other"));
        let v4 = CertOverride::for_url(true, "https://192.0.2.10/jmap");
        assert!(v4.allows("https://192.0.2.10:8443/"));
        assert!(!v4.allows("https://192.0.2.11/"));
    }

    #[test]
    fn send_budget_grows_with_size() {
        assert_eq!(send_body_budget(0), Duration::from_secs(120));
        assert_eq!(
            send_body_budget(64 * 1024 * 600),
            Duration::from_secs(120 + 600)
        );
        assert!(send_body_budget(512 * 1024 * 1024) > Duration::from_secs(2 * 60 * 60));
    }

    #[test]
    fn timeouts_are_applied_to_a_config() {
        let config: ureq::config::Config = with_timeouts!(ureq::config::Config::builder()).build();
        let t = config.timeouts();
        assert_eq!(t.connect, Some(CONNECT));
        assert_eq!(t.send_request, Some(SEND_REQUEST));
        assert_eq!(t.send_body, Some(SEND_BODY));
        assert_eq!(t.recv_response, Some(RECV_RESPONSE));
        assert_eq!(t.recv_body, Some(RECV_BODY));
    }
}
