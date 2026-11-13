/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::imap::error::ImapError;

/// What can go wrong talking POP3. The transport is shared with the IMAP
/// client (the same TLS `Connector` and stream), so a connect or TLS failure
/// arrives as an `ImapError` and is wrapped here.
#[derive(Debug, thiserror::Error)]
pub enum Pop3Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("transport: {0}")]
    Transport(#[from] ImapError),

    /// The server answered `-ERR`, with its message.
    #[error("server refused: {0}")]
    Err(String),

    /// The server's answer did not fit the protocol (no `+OK`/`-ERR`, a
    /// malformed list line, a response that ended early).
    #[error("protocol: {0}")]
    Protocol(String),

    #[error("sign-in failed: {0}")]
    Auth(String),
}
