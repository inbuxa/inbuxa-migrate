/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Signing in to Google Workspace as a service account with domain-wide
//! delegation. A short JWT, signed with the account's key, names the user
//! to act for, and Google's token endpoint answers with an access token for
//! that user (RFC 7523). One key reaches every mailbox in the domain, so no
//! one hands over a password.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use encodify::base64::URL_SAFE_NO_PAD;
use rustls::SignatureScheme;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject;
use rustls::sign::SigningKey;
use serde_json::{Value, json};
use ureq::config::Config;

use crate::error::Error;
use crate::net::{tls, with_timeouts};

/// Full IMAP access, the scope Gmail's XOAUTH2 needs.
pub const MAIL_SCOPE: &str = "https://mail.google.com/";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// The longest a Google assertion may ask for.
const LIFETIME_SECS: u64 = 3600;
/// A cached token is renewed when it has less than this left.
const MARGIN: Duration = Duration::from_secs(300);

/// A service account's JSON key, as the Google Cloud console hands it out.
pub struct ServiceAccount {
    client_email: String,
    token_uri: String,
    key: Arc<dyn SigningKey>,
}

impl fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("client_email", &self.client_email)
            .finish_non_exhaustive()
    }
}

impl ServiceAccount {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Usage(format!("{}: {e}", path.display())))?;
        Self::parse(&text).map_err(|e| Error::Usage(format!("{}: {e}", path.display())))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let doc: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let field = |name: &str| doc.get(name).and_then(Value::as_str);
        if field("type") != Some("service_account") {
            return Err("not a service account's JSON key".to_owned());
        }
        let client_email = field("client_email")
            .ok_or("the key has no client_email")?
            .to_owned();
        let pem = field("private_key").ok_or("the key has no private_key")?;
        let der = PrivateKeyDer::from_pem_slice(pem.as_bytes())
            .map_err(|e| format!("private_key: {e}"))?;
        let key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&der)
            .map_err(|e| format!("private_key: {e}"))?;
        Ok(Self {
            client_email,
            token_uri: field("token_uri").unwrap_or(TOKEN_URI).to_owned(),
            key,
        })
    }

    pub fn client_email(&self) -> &str {
        &self.client_email
    }

    /// The signed JWT that asks for `scope` as `user`.
    pub fn assertion(&self, user: &str, scope: &str, now: u64) -> Result<String, String> {
        let header = json!({"alg": "RS256", "typ": "JWT"});
        let claims = json!({
            "iss": self.client_email, "sub": user, "scope": scope,
            "aud": self.token_uri, "iat": now, "exp": now + LIFETIME_SECS,
        });
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string().as_bytes()),
            URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes())
        );
        let signer = self
            .key
            .choose_scheme(&[SignatureScheme::RSA_PKCS1_SHA256])
            .ok_or("the key can't sign RS256: Google's keys are RSA")?;
        let signature = signer
            .sign(input.as_bytes())
            .map_err(|e| format!("signing: {e}"))?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(&signature)))
    }

    /// An access token for `user`, and how long it lasts.
    pub fn token_for(&self, user: &str, scope: &str) -> Result<(String, Duration), Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let assertion = self.assertion(user, scope, now).map_err(Error::Usage)?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
            .append_pair("assertion", &assertion)
            .finish();
        let config: Config = with_timeouts!(
            Config::builder()
                .http_status_as_error(false)
                .tls_config(tls(false))
        )
        .build();
        let mut resp = config
            .new_agent()
            .post(&self.token_uri)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body.as_bytes())
            .map_err(|e| Error::Connection(format!("Google token endpoint: {e}")))?;
        let status = resp.status().as_u16();
        let reply: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        if let Some(token) = reply.get("access_token").and_then(Value::as_str)
            && status == 200
        {
            let lasts = reply
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(LIFETIME_SECS);
            return Ok((token.to_owned(), Duration::from_secs(lasts)));
        }
        let error = reply.get("error").and_then(Value::as_str).unwrap_or("");
        let detail = reply
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let hint = if error == "unauthorized_client" {
            format!(
                "; in the Admin console, allow this service account's client id \
                 domain-wide delegation for {scope}"
            )
        } else {
            String::new()
        };
        Err(Error::Connection(format!(
            "Google refused a token for {user} as {} (http {status}): {error} {detail}{hint}",
            self.client_email
        )))
    }
}

/// A service account acting for one user, with the token it last got,
/// renewed when it's close to running out. Each connection signs in with
/// it, so a run longer than a token's hour takes a new one on reconnect.
pub struct Delegated {
    account: Arc<ServiceAccount>,
    user: String,
    scope: &'static str,
    cached: Mutex<Option<(String, Instant)>>,
}

impl fmt::Debug for Delegated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Delegated")
            .field("account", &self.account.client_email)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl Delegated {
    pub fn new(account: Arc<ServiceAccount>, user: &str, scope: &'static str) -> Self {
        Self {
            account,
            user: user.to_owned(),
            scope,
            cached: Mutex::new(None),
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn token(&self) -> Result<String, Error> {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((token, until)) = cached.as_ref()
            && Instant::now() + MARGIN < *until
        {
            return Ok(token.clone());
        }
        let (token, lasts) = self.account.token_for(&self.user, self.scope)?;
        *cached = Some((token.clone(), Instant::now() + lasts));
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_that_isnt_a_service_accounts_is_refused() {
        let e = ServiceAccount::parse(r#"{"type":"authorized_user"}"#).unwrap_err();
        assert!(e.contains("service account"), "{e}");
        let e = ServiceAccount::parse(
            r#"{"type":"service_account","client_email":"a@p.iam.gserviceaccount.com","private_key":"nope"}"#,
        )
        .unwrap_err();
        assert!(e.contains("private_key"), "{e}");
    }
}
