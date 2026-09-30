/*
 * SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Settings every HTTP agent shares: timeouts and TLS.

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

#[cfg(test)]
mod tests {
    use super::*;

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
