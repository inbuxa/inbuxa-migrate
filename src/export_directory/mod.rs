/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-directory`: who has an address on a server, without their
//! secrets. Domains, users, groups, mailing lists, aliases, quotas, tenants
//! and roles are read from the registry with an administrator's
//! credentials and written as `directory.json` and CSV files, so the same
//! accounts can be set up on any server. Credentials, OTP seeds, API keys
//! and DKIM keys are never asked for, so they can't be written.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::Error;
use crate::export_files::{check_target, make_private_dir};
use crate::jmap::error::JmapError;
use crate::jmap::http::{Auth, HttpClient, RetryPolicy};
use crate::jmap::request::{Request, URN_INBUXA_REGISTRY, check_method_error};
use crate::jmap::session::{Limits, Session};
use crate::logging::Logger;
use crate::net::CertOverride;
use crate::sync::Summary;

pub mod csv;
pub mod model;

/// The layout this release writes; raised when `directory.json` or a CSV
/// changes in a way a reader has to know about.
pub const FORMAT_VERSION: u32 = 1;

// Every property is named, so nothing the registry adds later, and none of
// `credentials`, `encryptionAtRest` or the DKIM keys, is ever fetched.
const DOMAIN_PROPERTIES: &[&str] = &[
    "name",
    "aliases",
    "isEnabled",
    "description",
    "memberTenantId",
    "catchAllAddress",
    "subAddressing",
];
// `@type` tells a user from a group, and is only sent when asked for.
const ACCOUNT_PROPERTIES: &[&str] = &[
    "@type",
    "name",
    "domainId",
    "description",
    "memberGroupIds",
    "memberTenantId",
    "roles",
    "permissions",
    "quotas",
    "aliases",
    "externalId",
    "locale",
    "timeZone",
];
const LIST_PROPERTIES: &[&str] = &[
    "name",
    "domainId",
    "description",
    "aliases",
    "memberTenantId",
    "recipients",
];
const TENANT_PROPERTIES: &[&str] = &["name", "quotas"];
const ROLE_PROPERTIES: &[&str] = &[
    "description",
    "memberTenantId",
    "roleIds",
    "enabledPermissions",
    "disabledPermissions",
];

pub struct ExportDirectoryConfig {
    pub url: String,
    pub auth: Auth,
    pub dir: PathBuf,
    /// Domains to write, by name; empty for every domain.
    pub domains: Vec<String>,
    pub dry_run: bool,
    pub max_retries: u32,
    pub allow_invalid_certs: bool,
    pub logger: Logger,
}

pub fn run(config: ExportDirectoryConfig) -> Result<Summary, Error> {
    check_target(&config.dir)?;
    let logger = config.logger;
    let client = HttpClient::new(
        config.auth.clone(),
        RetryPolicy::new(config.max_retries),
        CertOverride::for_url(config.allow_invalid_certs, &config.url),
    );
    client.set_logger(logger);
    let session = Session::discover(&client, &config.url)?;
    for mismatch in session.origin_mismatches(&config.url) {
        logger.warn(&mismatch.to_string());
    }
    let limits = session.core_limits()?;
    client.set_limits(&limits);
    let (account_id, urn) = session.registry_account().ok_or_else(|| {
        Error::Account(format!(
            "this login can't read the server's registry ({URN_INBUXA_REGISTRY}); \
             export-directory needs an administrator's credentials"
        ))
    })?;
    let reader = Reader {
        client: &client,
        api_url: &session.api_url,
        account_id,
        urn,
        limits,
    };
    let raw = model::Raw {
        domains: reader.all("x:Domain", DOMAIN_PROPERTIES)?,
        accounts: reader.all("x:Account", ACCOUNT_PROPERTIES)?,
        lists: reader.all("x:MailingList", LIST_PROPERTIES)?,
        tenants: reader.optional("x:Tenant", TENANT_PROPERTIES, logger)?,
        roles: reader.optional("x:Role", ROLE_PROPERTIES, logger)?,
    };
    let directory = model::build(&raw, &config.domains, logger)?;
    let summary = directory.summary();

    let dir = config.dir.as_path();
    if config.dry_run {
        logger.warn(&format!(
            "dry run: nothing was written; {} would hold what is counted below",
            dir.display()
        ));
        return Ok(summary);
    }
    make_private_dir(dir)?;
    write(&directory, dir, &config.url)?;
    Ok(summary)
}

fn write(directory: &model::Directory, dir: &Path, url: &str) -> Result<(), Error> {
    csv::write(&dir.join("domains.csv"), &directory.domains_csv())?;
    csv::write(&dir.join("users.csv"), &directory.users_csv())?;
    csv::write(&dir.join("groups.csv"), &directory.groups_csv())?;
    csv::write(&dir.join("lists.csv"), &directory.lists_csv())?;
    csv::write(&dir.join("aliases.csv"), &directory.aliases_csv())?;
    let document = json!({
        "format": "inbuxa-migrate-directory",
        "formatVersion": FORMAT_VERSION,
        "createdBy": format!("inbuxa-migrate {}", crate::version::shown()),
        "created": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default(),
        "source": { "url": url },
        "domains": directory.domains,
        "tenants": directory.tenants,
        "roles": directory.roles,
        "users": directory.users,
        "groups": directory.groups,
        "lists": directory.lists,
    });
    // Written last, so a directory with a directory.json is a finished one.
    let text = serde_json::to_string_pretty(&document).map_err(std::io::Error::other)?;
    fs::write(dir.join("directory.json"), text + "\n")?;
    Ok(())
}

pub(crate) struct Reader<'a> {
    pub(crate) client: &'a HttpClient,
    pub(crate) api_url: &'a str,
    pub(crate) account_id: &'a str,
    pub(crate) urn: &'static str,
    pub(crate) limits: Limits,
}

impl Reader<'_> {
    /// Every object of one registry type, with only `properties` (and `id`).
    pub(crate) fn all(&self, ty: &str, properties: &[&str]) -> Result<Vec<Value>, JmapError> {
        let ids = self.ids(ty)?;
        let chunk = usize::try_from(self.limits.max_objects_in_get.max(1)).unwrap_or(500);
        let mut out = Vec::with_capacity(ids.len());
        for ids in ids.chunks(chunk) {
            let mut req = Request::new();
            req.registry(Some(self.urn));
            req.call(
                format!("{ty}/get"),
                json!({ "accountId": self.account_id, "ids": ids, "properties": properties }),
                "g",
            );
            let resp = req.send(self.client, self.api_url)?;
            let mr = resp.first()?;
            check_method_error(mr)?;
            let list = mr
                .args
                .get("list")
                .and_then(Value::as_array)
                .ok_or_else(|| JmapError::malformed(format!("{ty}/get response has no list")))?;
            out.extend(list.iter().cloned());
        }
        Ok(out)
    }

    /// As `all`, for a type this login may not be allowed to read, or a
    /// server may not have: that is a warning and an empty list.
    pub(crate) fn optional(
        &self,
        ty: &str,
        properties: &[&str],
        logger: Logger,
    ) -> Result<Vec<Value>, JmapError> {
        match self.all(ty, properties) {
            Ok(list) => Ok(list),
            Err(e @ (JmapError::Method { .. } | JmapError::UnknownMethod)) => {
                logger.warn(&format!("{ty} can't be read here ({e}); none are written"));
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        }
    }

    /// Ids page by page. A server that returns every id at once and ignores
    /// `position` hands back nothing new on the second page, which ends it.
    fn ids(&self, ty: &str) -> Result<Vec<String>, JmapError> {
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        loop {
            let mut req = Request::new();
            req.registry(Some(self.urn));
            req.call(
                format!("{ty}/query"),
                json!({ "accountId": self.account_id, "position": ids.len() }),
                "q",
            );
            let resp = req.send(self.client, self.api_url)?;
            let mr = resp.first()?;
            check_method_error(mr)?;
            let page = mr
                .args
                .get("ids")
                .and_then(Value::as_array)
                .ok_or_else(|| JmapError::malformed(format!("{ty}/query response has no ids")))?;
            let before = ids.len();
            for id in page {
                let id = id.as_str().ok_or_else(|| {
                    JmapError::malformed(format!("{ty}/query id is not a string"))
                })?;
                if seen.insert(id.to_owned()) {
                    ids.push(id.to_owned());
                }
            }
            if ids.len() == before {
                return Ok(ids);
            }
        }
    }
}
