/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! An inbuxa server, pinned by digest, set up the way a test needs it:
//! started once in recovery mode to make a domain and an administrator,
//! then again normally on the same data, where the administrator makes an
//! API key. The second start lets HTTP Basic through
//! (`INBUXA_HTTP_BASIC_AUTH=all`) so the test can sign in as the users it
//! checks; the migration itself uses the key.
//!
//! Recovery mode makes no roles, and a server seeds its built-in ones on
//! its first normal start only if it has none, so nothing here creates a
//! role before that start.

use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

use encodify::base64::STANDARD;
use serde_json::{Value, json};
use testcontainers::core::{IntoContainerPort, Mount};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};
use ureq::Agent;

use super::OWNER_LABEL;
use super::error::{ContainerError, ContainerResult};

/// 27.1-rc.4. `GenericImage` joins name and tag with `:`, so the digest is
/// split across the two.
const IMAGE_NAME: &str = "ghcr.io/inbuxa/inbuxa-server@sha256";
const IMAGE_DIGEST: &str = "f43be9a78d3155eaddae7bd95033d5b833eddd11761bab996c1d80913f48b4a6";

const HTTP_PORT: u16 = 8080;
const CONFIG_PATH: &str = "/etc/inbuxa/config.json";
const CONFIG_JSON: &str = r#"{"@type":"RocksDb","path":"/var/lib/inbuxa"}"#;
const RECOVERY_ADMIN: (&str, &str) = ("admin", "admin");

pub const DOMAIN: &str = "example.org";
pub const ADMIN_EMAIL: &str = "boss@example.org";
pub const ADMIN_PASSWORD: &str = "Boss-pass-Xq7v2k9w";

pub struct Inbuxa {
    container: Option<Container<GenericImage>>,
    volume: String,
    pub base_url: String,
    /// The administrator's API key.
    pub token: String,
}

impl Inbuxa {
    pub fn start() -> ContainerResult<Self> {
        let port = free_port()?;
        let base_url = format!("http://127.0.0.1:{port}");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let volume = format!("inbuxa-migrate-itest-{}-{nanos}", std::process::id());

        let recovery = run(&volume, port, &base_url, true)?;
        let admin = basic(RECOVERY_ADMIN.0, RECOVERY_ADMIN.1);
        let session = wait_ready(&base_url, &admin)?;
        let account = registry_account(&session)?;
        let created = jmap(
            &base_url,
            &admin,
            json!([["x:Domain/set", {"accountId": account, "create": {"d": {"name": DOMAIN}}}, "0"]]),
        )?;
        let domain_id = created[0][1]["created"]["d"]["id"]
            .as_str()
            .ok_or_else(|| ContainerError::Seed(format!("domain not created: {created}")))?
            .to_owned();
        let created = jmap(
            &base_url,
            &admin,
            json!([["x:Account/set", {"accountId": account, "create": {"boss": {
                "@type": "User",
                "name": "boss",
                "domainId": domain_id,
                "credentials": {"0": {"@type": "Password", "secret": ADMIN_PASSWORD}},
                "roles": {"@type": "Admin"},
            }}}, "0"]]),
        )?;
        if created[0][1]["created"]["boss"].is_null() {
            return Err(ContainerError::Seed(format!(
                "administrator not created: {created}"
            )));
        }
        drop(recovery);

        let container = run(&volume, port, &base_url, false)?;
        let admin = basic(ADMIN_EMAIL, ADMIN_PASSWORD);
        let session = wait_ready(&base_url, &admin)?;
        let account = registry_account(&session)?;
        let created = jmap(
            &base_url,
            &admin,
            json!([["x:ApiKey/set", {"accountId": account, "create": {"k": {
                "description": "inbuxa-migrate live test",
                "permissions": {"@type": "Inherit"},
            }}}, "0"]]),
        )?;
        let token = created[0][1]["created"]["k"]["secret"]
            .as_str()
            .ok_or_else(|| ContainerError::Seed(format!("no API key: {created}")))?
            .to_owned();
        Ok(Inbuxa {
            container: Some(container),
            volume,
            base_url,
            token,
        })
    }

    /// The account a user's own session names, signed in as that user.
    pub fn account_of(&self, email: &str, password: &str) -> ContainerResult<String> {
        let session = session(&self.base_url, &basic(email, password))?;
        session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ContainerError::Protocol(format!("no mail account: {session}")))
    }

    /// A blob's bytes, signed in as `email`.
    pub fn download(
        &self,
        email: &str,
        password: &str,
        account: &str,
        blob: &str,
    ) -> ContainerResult<Vec<u8>> {
        let auth = basic(email, password);
        let template = session(&self.base_url, &auth)?["downloadUrl"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let url = template
            .replace("{accountId}", account)
            .replace("{blobId}", blob)
            .replace("{name}", "blob")
            .replace("{type}", "application/octet-stream");
        let mut resp = agent().get(&url).header("Authorization", &auth).call()?;
        resp.body_mut()
            .read_to_vec()
            .map_err(|e| ContainerError::Protocol(format!("download: {e}")))
    }

    /// Method calls, signed in as `email`.
    pub fn call(&self, email: &str, password: &str, calls: Value) -> ContainerResult<Value> {
        jmap(&self.base_url, &basic(email, password), calls)
    }
}

impl Drop for Inbuxa {
    fn drop(&mut self) {
        drop(self.container.take());
        let _ = Command::new("docker")
            .args(["volume", "rm", "-f", &self.volume])
            .output();
    }
}

fn run(
    volume: &str,
    port: u16,
    base_url: &str,
    recovery: bool,
) -> ContainerResult<Container<GenericImage>> {
    let mut request = GenericImage::new(IMAGE_NAME, IMAGE_DIGEST)
        .with_exposed_port(HTTP_PORT.tcp())
        .with_env_var("INBUXA_PUBLIC_URL", base_url)
        .with_labels([(OWNER_LABEL, "1")])
        .with_copy_to(CONFIG_PATH, CONFIG_JSON.as_bytes().to_vec())
        .with_mount(Mount::volume_mount(volume, "/var/lib/inbuxa"))
        .with_mapped_port(port, HTTP_PORT.tcp())
        .with_startup_timeout(Duration::from_secs(180));
    request = if recovery {
        request
            .with_env_var("INBUXA_RECOVERY_MODE", "1")
            .with_env_var(
                "INBUXA_RECOVERY_ADMIN",
                format!("{}:{}", RECOVERY_ADMIN.0, RECOVERY_ADMIN.1),
            )
    } else {
        request.with_env_var("INBUXA_HTTP_BASIC_AUTH", "all")
    };
    Ok(request.start()?)
}

fn wait_ready(base_url: &str, auth: &str) -> ContainerResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match session(base_url, auth) {
            Ok(s) if s.get("apiUrl").is_some() => return Ok(s),
            Ok(s) if Instant::now() > deadline => {
                return Err(ContainerError::Protocol(format!("no session: {s}")));
            }
            Err(e) if Instant::now() > deadline => return Err(e),
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

fn session(base_url: &str, auth: &str) -> ContainerResult<Value> {
    let mut resp = agent()
        .get(format!("{base_url}/jmap/session"))
        .header("Authorization", auth)
        .call()?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| ContainerError::Protocol(format!("body read: {e}")))?;
    if status != 200 {
        return Err(ContainerError::Protocol(format!(
            "session status {status}: {text}"
        )));
    }
    serde_json::from_str(&text).map_err(|e| ContainerError::Protocol(format!("session: {e}")))
}

/// The account that carries the registry: an administrator's own.
fn registry_account(session: &Value) -> ContainerResult<String> {
    session["accounts"]
        .as_object()
        .and_then(|accounts| {
            accounts.iter().find(|(_, a)| {
                a["accountCapabilities"]
                    .get("urn:inbuxa:jmap:registry")
                    .is_some()
            })
        })
        .map(|(id, _)| id.clone())
        .ok_or_else(|| ContainerError::Protocol(format!("no registry account: {session}")))
}

fn jmap(base_url: &str, auth: &str, calls: Value) -> ContainerResult<Value> {
    let body = json!({
        "using": [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:mail",
            "urn:ietf:params:jmap:sieve",
            "urn:inbuxa:jmap:registry",
        ],
        "methodCalls": calls,
    });
    let mut resp = agent()
        .post(format!("{base_url}/jmap/"))
        .header("Authorization", auth)
        .header("Content-Type", "application/json")
        .send(body.to_string())?;
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| ContainerError::Protocol(format!("body read: {e}")))?;
    let reply: Value = serde_json::from_str(&text)
        .map_err(|e| ContainerError::Protocol(format!("reply {text}: {e}")))?;
    Ok(reply["methodResponses"].clone())
}

fn free_port() -> ContainerResult<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent()
}

pub fn basic(user: &str, password: &str) -> String {
    let mut header = String::from("Basic ");
    STANDARD.encode_append(format!("{user}:{password}"), &mut header);
    header
}
