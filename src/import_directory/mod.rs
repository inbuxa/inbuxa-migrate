/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `import-directory`: the accounts `export-directory` wrote, set up on an
//! inbuxa server through its registry with an administrator's credentials.
//! Tenants, roles, domains, groups, users and mailing lists are created in
//! that order, so each finds what it points at. What the server already
//! has, by name or address, is left as it is, so a second run creates
//! nothing. Each user it creates gets a generated password, written to a
//! new private file for the administrator to hand out.

pub mod passwords;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::error::Error;
use crate::export_directory::{FORMAT_VERSION, Reader};
use crate::jmap::http::{Auth, HttpClient, RetryPolicy};
use crate::jmap::request::{Request, URN_INBUXA_REGISTRY, check_method_error};
use crate::jmap::session::Session;
use crate::logging::{LEVEL_PROGRESS, Logger};
use crate::net::CertOverride;
use crate::sync::{Summary, TypeCounts};

use passwords::PasswordFile;

pub struct ImportDirectoryConfig {
    pub url: String,
    pub auth: Auth,
    /// The directory `export-directory` wrote.
    pub dir: PathBuf,
    /// Domains to set up, by name; empty for every domain in the directory.
    pub domains: Vec<String>,
    /// Where the new users' passwords go; a new file. Needed unless it's a
    /// dry run.
    pub passwords: Option<PathBuf>,
    pub dry_run: bool,
    pub max_retries: u32,
    pub allow_invalid_certs: bool,
    pub logger: Logger,
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A registry set, `{"a": true, ...}`.
fn set(items: impl IntoIterator<Item = String>) -> Value {
    Value::Object(items.into_iter().map(|k| (k, Value::Bool(true))).collect())
}

/// Copies `key` when it holds a value.
fn put(out: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value.filter(|v| !v.is_null()) {
        out.insert(key.to_owned(), v.clone());
    }
}

/// The directory file, checked for its format.
fn read_directory(dir: &Path) -> Result<Value, Error> {
    let path = dir.join("directory.json");
    let text = fs::read_to_string(&path).map_err(|e| {
        Error::Usage(format!(
            "{}: {e}; give the directory export-directory wrote",
            path.display()
        ))
    })?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| Error::Usage(format!("{}: {e}", path.display())))?;
    if str_of(&doc, "format") != Some("inbuxa-migrate-directory") {
        return Err(Error::Usage(format!(
            "{} is not a directory export-directory wrote",
            path.display()
        )));
    }
    let version = doc
        .get("formatVersion")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if version != u64::from(FORMAT_VERSION) {
        return Err(Error::Usage(format!(
            "{} is format version {version}; this release reads version {FORMAT_VERSION}",
            path.display()
        )));
    }
    Ok(doc)
}

/// What the server already has, by name or lower-cased address, and what
/// this run makes, so later objects can point at both.
#[derive(Default)]
struct Known {
    tenants: HashMap<String, String>,
    roles: HashMap<String, String>,
    domains: HashMap<String, String>,
    /// Users and groups by address.
    accounts: HashMap<String, String>,
    lists: HashMap<String, String>,
}

impl Known {
    fn read(reader: &Reader<'_>, logger: Logger) -> Result<Known, Error> {
        let mut known = Known::default();
        let named = |list: Vec<Value>, key: &str| -> HashMap<String, String> {
            list.iter()
                .filter_map(|v| Some((str_of(v, key)?.to_owned(), str_of(v, "id")?.to_owned())))
                .collect()
        };
        known.tenants = named(reader.optional("x:Tenant", &["name"], logger)?, "name");
        known.roles = named(
            reader.optional("x:Role", &["description"], logger)?,
            "description",
        );
        let domains = reader.all("x:Domain", &["name"])?;
        let domain_names: HashMap<String, String> = domains
            .iter()
            .filter_map(|d| Some((str_of(d, "id")?.to_owned(), str_of(d, "name")?.to_owned())))
            .collect();
        for (id, name) in &domain_names {
            known.domains.insert(name.to_ascii_lowercase(), id.clone());
        }
        let address = |v: &Value| -> Option<String> {
            let name = str_of(v, "name")?;
            if name.contains('@') {
                return Some(name.to_ascii_lowercase());
            }
            let domain = domain_names.get(str_of(v, "domainId")?)?;
            Some(format!("{name}@{domain}").to_ascii_lowercase())
        };
        for a in reader.all("x:Account", &["name", "domainId"])? {
            if let (Some(addr), Some(id)) = (address(&a), str_of(&a, "id")) {
                known.accounts.insert(addr, id.to_owned());
            }
        }
        for l in reader.all("x:MailingList", &["name", "domainId"])? {
            if let (Some(addr), Some(id)) = (address(&l), str_of(&l, "id")) {
                known.lists.insert(addr, id.to_owned());
            }
        }
        Ok(known)
    }

    fn domain_of(&self, email: &str) -> Option<&str> {
        let domain = email.rsplit_once('@')?.1.to_ascii_lowercase();
        self.domains.get(&domain).map(String::as_str)
    }

    fn tenant(&self, v: &Value) -> Option<Value> {
        str_of(v, "tenant")
            .and_then(|t| self.tenants.get(t))
            .map(|id| json!(id))
    }
}

/// One object to create, with what to call it in the log and summary.
struct Create {
    label: String,
    object: Value,
    /// A user's password, written once the user exists.
    password: Option<String>,
}

/// Placeholder id for what a dry run would create.
const PLANNED: &str = "(new)";

struct Writer<'a> {
    client: &'a HttpClient,
    api_url: &'a str,
    account_id: &'a str,
    urn: &'static str,
    chunk: usize,
    dry_run: bool,
    logger: Logger,
}

impl Writer<'_> {
    /// Creates each object, in batches the server takes; the id of each
    /// one made, by label. A refused object is counted and the rest go on.
    fn create(
        &self,
        ty: &str,
        items: Vec<Create>,
        counts: &mut TypeCounts,
        passwords: &mut Option<PasswordFile>,
    ) -> Result<HashMap<String, String>, Error> {
        let mut made = HashMap::new();
        if self.dry_run {
            for item in items {
                counts.created += 1;
                made.insert(item.label, PLANNED.to_owned());
            }
            return Ok(made);
        }
        let mut items = items.into_iter().peekable();
        while items.peek().is_some() {
            let batch: Vec<Create> = items.by_ref().take(self.chunk).collect();
            let create: Map<String, Value> = batch
                .iter()
                .enumerate()
                .map(|(i, c)| (format!("c{i}"), c.object.clone()))
                .collect();
            let mut req = Request::new();
            req.registry(Some(self.urn));
            req.call(
                format!("{ty}/set"),
                json!({ "accountId": self.account_id, "create": create }),
                "s",
            );
            // Sent once: a create repeated after a lost reply would make
            // the object twice, where running again finds it and moves on.
            let resp = req.send_once(self.client, self.api_url)?;
            let mr = resp.first()?;
            check_method_error(mr)?;
            let created = mr.args.get("created").and_then(Value::as_object);
            let refused = mr.args.get("notCreated").and_then(Value::as_object);
            for (i, item) in batch.into_iter().enumerate() {
                let cid = format!("c{i}");
                if let Some(id) = created
                    .and_then(|c| c.get(&cid))
                    .and_then(|v| str_of(v, "id"))
                {
                    counts.created += 1;
                    if let (Some(file), Some(pw)) = (passwords.as_mut(), &item.password) {
                        file.add(&item.label, pw)?;
                    }
                    if self.logger.enabled(LEVEL_PROGRESS) {
                        eprintln!("{ty} {}: created", item.label);
                    }
                    made.insert(item.label, id.to_owned());
                } else {
                    let why = refused
                        .and_then(|n| n.get(&cid))
                        .map(describe_set_error)
                        .unwrap_or_else(|| "the server didn't say".to_owned());
                    self.logger
                        .warn(&format!("{ty} {} was not created: {why}", item.label));
                    counts.failed += 1;
                }
            }
            if let Some(file) = passwords.as_mut() {
                file.flush()?;
            }
        }
        Ok(made)
    }

    /// Sets properties on objects that exist, by id; failures are warnings.
    fn update(
        &self,
        ty: &str,
        updates: Map<String, Value>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        if self.dry_run || updates.is_empty() {
            return Ok(());
        }
        let mut req = Request::new();
        req.registry(Some(self.urn));
        req.call(
            format!("{ty}/set"),
            json!({ "accountId": self.account_id, "update": updates }),
            "u",
        );
        let resp = req.send(self.client, self.api_url)?;
        let mr = resp.first()?;
        check_method_error(mr)?;
        if let Some(refused) = mr.args.get("notUpdated").and_then(Value::as_object) {
            for (id, err) in refused {
                self.logger.warn(&format!(
                    "{ty} {id} was made but not linked: {}",
                    describe_set_error(err)
                ));
                counts.failed += 1;
            }
        }
        Ok(())
    }
}

fn describe_set_error(err: &Value) -> String {
    let ty = str_of(err, "type").unwrap_or("error");
    match str_of(err, "description") {
        Some(d) => format!("{ty}: {d}"),
        None => ty.to_owned(),
    }
}

/// `{"email": ...}` aliases as the registry's list, each on a domain the
/// server has; one on a domain it lacks is left off with a warning.
fn aliases(v: &Value, known: &Known, owner: &str, logger: Logger) -> Value {
    let mut out = Map::new();
    for a in v
        .get("aliases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(email) = str_of(a, "email") else {
            continue;
        };
        let (Some((local, _)), Some(domain_id)) = (email.rsplit_once('@'), known.domain_of(email))
        else {
            logger.warn(&format!(
                "{owner}: alias {email} is on a domain this server doesn't have; left off"
            ));
            continue;
        };
        if domain_id == PLANNED {
            continue;
        }
        let mut alias = Map::new();
        alias.insert("name".into(), json!(local));
        alias.insert("domainId".into(), json!(domain_id));
        alias.insert(
            "enabled".into(),
            json!(a.get("enabled").and_then(Value::as_bool).unwrap_or(true)),
        );
        put(&mut alias, "description", a.get("description"));
        out.insert(out.len().to_string(), Value::Object(alias));
    }
    Value::Object(out)
}

fn permissions(v: &Value) -> Option<Value> {
    let p = v.get("permissions").filter(|p| !p.is_null())?;
    let mode = match str_of(p, "mode")? {
        "merge" => "Merge",
        "replace" => "Replace",
        _ => return None,
    };
    Some(json!({
        "@type": mode,
        "enabledPermissions": set(strings(p, "enabled")),
        "disabledPermissions": set(strings(p, "disabled")),
    }))
}

/// A user's (`user`, `admin`) or group's (`default`) roles, or custom
/// roles by name; a custom role the server lacks is left off.
fn roles(v: &Value, known: &Known, owner: &str, logger: Logger) -> Option<Value> {
    let r = v.get("roles").filter(|r| !r.is_null())?;
    if let Some(name) = r.as_str() {
        let tag = match name {
            "user" => "User",
            "admin" => "Admin",
            "default" => "Default",
            other => {
                logger.warn(&format!(
                    "{owner}: unknown role kind {other}; left to the default"
                ));
                return None;
            }
        };
        return Some(json!({ "@type": tag }));
    }
    let ids: Vec<String> = strings(r, "custom")
        .into_iter()
        .filter_map(|name| match known.roles.get(&name) {
            Some(id) if id != PLANNED => Some(id.clone()),
            Some(_) => None,
            None => {
                logger.warn(&format!(
                    "{owner}: role {name} isn't on this server; left off"
                ));
                None
            }
        })
        .collect();
    Some(json!({ "@type": "Custom", "roleIds": set(ids) }))
}

fn sub_addressing(v: &Value) -> Option<Value> {
    match v.get("subAddressing")? {
        Value::String(s) if s == "enabled" => Some(json!({ "@type": "Enabled" })),
        Value::String(s) if s == "disabled" => Some(json!({ "@type": "Disabled" })),
        Value::Object(o) => Some(json!({
            "@type": "Custom",
            "customRule": o.get("customRule").cloned().unwrap_or(Value::Null),
        })),
        _ => None,
    }
}

/// An account (`User` or `Group`) as the registry creates it.
fn account(
    a: &Value,
    ty: &str,
    known: &Known,
    logger: Logger,
) -> Option<(String, Map<String, Value>)> {
    let email = str_of(a, "email")?.to_owned();
    let domain_id = known.domain_of(&email)?.to_owned();
    let name = str_of(a, "name")
        .map(str::to_owned)
        .or_else(|| email.rsplit_once('@').map(|(l, _)| l.to_owned()))?;
    let mut o = Map::new();
    o.insert("@type".into(), json!(ty));
    o.insert("name".into(), json!(name));
    o.insert("domainId".into(), json!(domain_id));
    put(&mut o, "description", a.get("description"));
    put(&mut o, "memberTenantId", known.tenant(a).as_ref());
    put(&mut o, "roles", roles(a, known, &email, logger).as_ref());
    put(&mut o, "permissions", permissions(a).as_ref());
    put(&mut o, "quotas", a.get("quotas"));
    o.insert("aliases".into(), aliases(a, known, &email, logger));
    put(&mut o, "locale", a.get("locale"));
    put(&mut o, "timeZone", a.get("timeZone"));
    Some((email, o))
}

pub fn run(config: ImportDirectoryConfig) -> Result<Summary, Error> {
    let logger = config.logger;
    let doc = read_directory(&config.dir)?;
    let only: Vec<String> = config
        .domains
        .iter()
        .map(|d| d.to_ascii_lowercase())
        .collect();
    let in_scope = |domain: Option<&str>| -> bool {
        only.is_empty() || domain.is_some_and(|d| only.contains(&d.to_ascii_lowercase()))
    };
    let list = |key: &str| -> Vec<Value> {
        doc.get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let domains: Vec<Value> = list("domains")
        .into_iter()
        .filter(|d| in_scope(str_of(d, "name")))
        .collect();
    for wanted in &only {
        if !domains
            .iter()
            .any(|d| str_of(d, "name").is_some_and(|n| n.eq_ignore_ascii_case(wanted)))
        {
            return Err(Error::Usage(format!(
                "the directory has no domain {wanted}"
            )));
        }
    }
    let of_domain = |v: &Value| in_scope(str_of(v, "domain"));
    let users: Vec<Value> = list("users").into_iter().filter(of_domain).collect();
    let groups: Vec<Value> = list("groups").into_iter().filter(of_domain).collect();
    let lists: Vec<Value> = list("lists").into_iter().filter(of_domain).collect();

    let mut passwords = match (&config.passwords, config.dry_run) {
        (_, true) => None,
        (Some(path), false) => Some(PasswordFile::create(path)?),
        (None, false) => {
            return Err(Error::Usage(
                "--passwords FILE is needed: each user created gets a generated password, \
                 written there"
                    .to_owned(),
            ));
        }
    };

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
            "this login can't change the server's registry ({URN_INBUXA_REGISTRY}); \
             import-directory needs an administrator's credentials"
        ))
    })?;
    let chunk = usize::try_from(limits.max_objects_in_set.clamp(1, 500)).unwrap_or(500);
    let reader = Reader {
        client: &client,
        api_url: &session.api_url,
        account_id,
        urn,
        limits,
    };
    let mut known = Known::read(&reader, logger)?;
    let writer = Writer {
        client: &client,
        api_url: &session.api_url,
        account_id,
        urn,
        chunk,
        dry_run: config.dry_run,
        logger,
    };
    let mut counts: HashMap<&'static str, TypeCounts> = HashMap::new();

    // Tenants.
    let mut todo = Vec::new();
    let c = counts.entry("tenant").or_default();
    for t in list("tenants") {
        let Some(name) = str_of(&t, "name") else {
            continue;
        };
        if known.tenants.contains_key(name) {
            c.skipped += 1;
            continue;
        }
        let mut o = Map::new();
        o.insert("name".into(), json!(name));
        put(&mut o, "quotas", t.get("quotas"));
        todo.push(Create {
            label: name.to_owned(),
            object: Value::Object(o),
            password: None,
        });
    }
    known
        .tenants
        .extend(writer.create("x:Tenant", todo, c, &mut passwords)?);

    // Roles: made first, then pointed at the roles they build on, which
    // may be made in the same pass.
    let roles_in = list("roles");
    let mut todo = Vec::new();
    let c = counts.entry("role").or_default();
    for r in &roles_in {
        let Some(name) = str_of(r, "name") else {
            continue;
        };
        if known.roles.contains_key(name) {
            c.skipped += 1;
            continue;
        }
        let mut o = Map::new();
        o.insert("description".into(), json!(name));
        put(&mut o, "memberTenantId", known.tenant(r).as_ref());
        o.insert(
            "enabledPermissions".into(),
            set(strings(r, "enabledPermissions")),
        );
        o.insert(
            "disabledPermissions".into(),
            set(strings(r, "disabledPermissions")),
        );
        todo.push(Create {
            label: name.to_owned(),
            object: Value::Object(o),
            password: None,
        });
    }
    let made_roles = writer.create("x:Role", todo, c, &mut passwords)?;
    known.roles.extend(made_roles.clone());
    let mut links = Map::new();
    for r in &roles_in {
        let Some(id) = str_of(r, "name").and_then(|n| made_roles.get(n)) else {
            continue;
        };
        let parents: Vec<String> = strings(r, "roles")
            .iter()
            .filter_map(|n| known.roles.get(n).cloned())
            .collect();
        if !parents.is_empty() {
            links.insert(id.clone(), json!({ "roleIds": set(parents) }));
        }
    }
    writer.update("x:Role", links, c)?;

    // Domains.
    let mut todo = Vec::new();
    let c = counts.entry("domain").or_default();
    for d in &domains {
        let Some(name) = str_of(d, "name") else {
            continue;
        };
        if known.domains.contains_key(&name.to_ascii_lowercase()) {
            c.skipped += 1;
            continue;
        }
        let mut o = Map::new();
        o.insert("name".into(), json!(name));
        o.insert(
            "isEnabled".into(),
            json!(d.get("enabled").and_then(Value::as_bool).unwrap_or(true)),
        );
        put(&mut o, "description", d.get("description"));
        o.insert("aliases".into(), set(strings(d, "aliases")));
        put(&mut o, "catchAllAddress", d.get("catchAllAddress"));
        put(&mut o, "subAddressing", sub_addressing(d).as_ref());
        put(&mut o, "memberTenantId", known.tenant(d).as_ref());
        todo.push(Create {
            label: name.to_owned(),
            object: Value::Object(o),
            password: None,
        });
    }
    for (name, id) in writer.create("x:Domain", todo, c, &mut passwords)? {
        known.domains.insert(name.to_ascii_lowercase(), id);
    }

    // Groups, then the users in them.
    for (kind, ty, entries) in [("group", "Group", &groups), ("user", "User", &users)] {
        let mut todo = Vec::new();
        let c = counts.entry(kind).or_default();
        for a in entries {
            let Some(email) = str_of(a, "email") else {
                continue;
            };
            if known.accounts.contains_key(&email.to_ascii_lowercase()) {
                c.skipped += 1;
                continue;
            }
            let Some((email, mut o)) = account(a, ty, &known, logger) else {
                logger.warn(&format!(
                    "{kind} {email}: its domain isn't on this server; left out"
                ));
                c.failed += 1;
                continue;
            };
            if config.dry_run && known.domain_of(&email) == Some(PLANNED) {
                c.created += 1;
                continue;
            }
            let mut password = None;
            if ty == "User" {
                let groups: Vec<String> = strings(a, "groups")
                    .iter()
                    .filter_map(|g| known.accounts.get(&g.to_ascii_lowercase()).cloned())
                    .filter(|id| id != PLANNED)
                    .collect();
                o.insert("memberGroupIds".into(), set(groups));
                put(&mut o, "externalId", a.get("externalId"));
                let pw = passwords::generate()?;
                o.insert(
                    "credentials".into(),
                    json!({ "0": { "@type": "Password", "secret": pw } }),
                );
                password = Some(pw);
            }
            todo.push(Create {
                label: email,
                object: Value::Object(o),
                password,
            });
        }
        for (email, id) in writer.create("x:Account", todo, c, &mut passwords)? {
            known.accounts.insert(email.to_ascii_lowercase(), id);
        }
    }

    // Mailing lists.
    let mut todo = Vec::new();
    let c = counts.entry("mailinglist").or_default();
    for l in &lists {
        let Some(email) = str_of(l, "email") else {
            continue;
        };
        if known.lists.contains_key(&email.to_ascii_lowercase()) {
            c.skipped += 1;
            continue;
        }
        let Some(domain_id) = known.domain_of(email).map(str::to_owned) else {
            logger.warn(&format!(
                "mailing list {email}: its domain isn't on this server; left out"
            ));
            c.failed += 1;
            continue;
        };
        if domain_id == PLANNED {
            c.created += 1;
            continue;
        }
        let mut o = Map::new();
        let name = str_of(l, "name")
            .map(str::to_owned)
            .or_else(|| email.rsplit_once('@').map(|(n, _)| n.to_owned()))
            .unwrap_or_default();
        o.insert("name".into(), json!(name));
        o.insert("domainId".into(), json!(domain_id));
        put(&mut o, "description", l.get("description"));
        o.insert("recipients".into(), set(strings(l, "recipients")));
        o.insert("aliases".into(), aliases(l, &known, email, logger));
        put(&mut o, "memberTenantId", known.tenant(l).as_ref());
        todo.push(Create {
            label: email.to_owned(),
            object: Value::Object(o),
            password: None,
        });
    }
    writer.create("x:MailingList", todo, c, &mut passwords)?;

    if config.dry_run {
        logger.warn("dry run: nothing was created and no passwords were written");
    } else if let Some(path) = &config.passwords {
        let made = counts.get("user").map_or(0, |c| c.created);
        logger.warn(&format!(
            "{made} new user password(s) are in {}; hand them out, then delete the file",
            path.display()
        ));
    }
    let mut summary = Summary::default();
    for ty in ["tenant", "role", "domain", "group", "user", "mailinglist"] {
        summary
            .per_type
            .push((ty, counts.remove(ty).unwrap_or_default()));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Known {
        let mut k = Known::default();
        k.domains.insert("example.org".into(), "d1".into());
        k.roles.insert("Helpdesk".into(), "r1".into());
        k
    }

    #[test]
    fn aliases_go_on_domains_the_server_has() {
        let v = json!({"aliases": [
            {"email": "info@example.org", "enabled": false, "description": "front desk"},
            {"email": "x@elsewhere.net"}
        ]});
        assert_eq!(
            aliases(&v, &known(), "ann@example.org", Logger::from_flags(true, 0)),
            json!({"0": {"name": "info", "domainId": "d1", "enabled": false,
                          "description": "front desk"}})
        );
    }

    #[test]
    fn roles_and_permissions_take_the_registry_shape() {
        let log = Logger::from_flags(true, 0);
        let k = known();
        assert_eq!(
            roles(&json!({"roles": "admin"}), &k, "a", log),
            Some(json!({"@type": "Admin"}))
        );
        assert_eq!(
            roles(&json!({"roles": "default"}), &k, "a", log),
            Some(json!({"@type": "Default"}))
        );
        assert_eq!(
            roles(
                &json!({"roles": {"custom": ["Helpdesk", "Gone"]}}),
                &k,
                "a",
                log
            ),
            Some(json!({"@type": "Custom", "roleIds": {"r1": true}}))
        );
        assert_eq!(roles(&json!({"roles": null}), &k, "a", log), None);
        assert_eq!(
            permissions(
                &json!({"permissions": {"mode": "merge", "enabled": ["a"], "disabled": []}})
            ),
            Some(json!({"@type": "Merge", "enabledPermissions": {"a": true},
                         "disabledPermissions": {}}))
        );
        assert_eq!(
            sub_addressing(&json!({"subAddressing": {"customRule": "r"}})),
            Some(json!({"@type": "Custom", "customRule": "r"}))
        );
    }

    #[test]
    fn only_a_directory_export_directory_wrote_is_read() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_directory(dir.path()).is_err());
        fs::write(dir.path().join("directory.json"), r#"{"format":"other"}"#).unwrap();
        let err = read_directory(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not a directory"), "{err}");
        fs::write(
            dir.path().join("directory.json"),
            r#"{"format":"inbuxa-migrate-directory","formatVersion":99}"#,
        )
        .unwrap();
        let err = read_directory(dir.path()).unwrap_err().to_string();
        assert!(err.contains("version 99"), "{err}");
    }
}
