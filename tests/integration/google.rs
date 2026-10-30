/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! A Google service account's key, made for the test, so no real key is
//! ever in the repository.

/// A service account's JSON key for a new RSA key, with the token
/// endpoint at `token_uri`, and the key's public half.
pub fn service_account_key(token_uri: &str) -> (String, Vec<u8>) {
    use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
    use aws_lc_rs::signature::KeyPair as _;
    let pair = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
    let der: Pkcs8V1Der<'static> = pair.as_der().unwrap();
    let body = encodify::base64::STANDARD.encode(der.as_ref());
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        lines.join("\n")
    );
    let key = serde_json::json!({
        "type": "service_account", "client_email": "migrate@proj.iam.gserviceaccount.com",
        "private_key": pem, "token_uri": token_uri,
    });
    (key.to_string(), pair.public_key().as_ref().to_vec())
}
