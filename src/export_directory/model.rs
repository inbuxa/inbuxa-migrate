/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Registry objects as a directory any server can be set up from: ids
//! become addresses and names, and only the fields listed here are kept.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::error::Error;
use crate::logging::Logger;
use crate::sync::{Summary, TypeCounts};

/// What the registry returned, one list per type.
#[derive(Default)]
pub struct Raw {
    pub domains: Vec<Value>,
    pub accounts: Vec<Value>,
    pub lists: Vec<Value>,
    pub tenants: Vec<Value>,
    pub roles: Vec<Value>,
}

/// The directory as `directory.json` holds it, each list sorted by name or
/// address.
#[derive(Debug, Default)]
pub struct Directory {
    pub domains: Vec<Value>,
    pub tenants: Vec<Value>,
    pub roles: Vec<Value>,
    pub users: Vec<Value>,
    pub groups: Vec<Value>,
    pub lists: Vec<Value>,
    /// Objects left out because they point at something that isn't there.
    skipped: HashMap<&'static str, u64>,
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// A registry set: `{"a": true, "b": true}` (or a plain array), sorted.
fn set_of(v: &Value, key: &str) -> Vec<String> {
    let set: BTreeSet<String> = match v.get(key) {
        Some(Value::Object(map)) => map
            .iter()
            .filter(|(_, on)| on.as_bool() != Some(false))
            .map(|(k, _)| k.clone())
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => BTreeSet::new(),
    };
    set.into_iter().collect()
}

/// A registry list: `{"0": {...}, "1": {...}}` (or a plain array), in order.
fn list_of<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    match v.get(key) {
        Some(Value::Object(map)) => {
            let mut items: Vec<(u64, &Value)> = map
                .iter()
                .map(|(k, item)| (k.parse().unwrap_or(u64::MAX), item))
                .collect();
            items.sort_by_key(|(k, _)| *k);
            items.into_iter().map(|(_, item)| item).collect()
        }
        Some(Value::Array(items)) => items.iter().collect(),
        _ => Vec::new(),
    }
}

fn opt(v: Option<&str>) -> Value {
    v.map_or(Value::Null, |s| Value::String(s.to_owned()))
}

/// Quotas are counts and byte sizes by name, `maxDiskQuota` and the rest.
fn quotas_of(v: &Value) -> Value {
    let map: Map<String, Value> = v
        .get("quotas")
        .and_then(Value::as_object)
        .map(|q| {
            q.iter()
                .filter(|(_, n)| n.is_u64())
                .map(|(k, n)| (k.clone(), n.clone()))
                .collect()
        })
        .unwrap_or_default();
    Value::Object(map)
}

fn type_of(v: &Value) -> &str {
    str_of(v, "@type").unwrap_or_default()
}

struct Names {
    domains: HashMap<String, String>,
    tenants: HashMap<String, String>,
    roles: HashMap<String, String>,
}

impl Names {
    fn domain(&self, id: Option<&str>) -> Option<&str> {
        id.and_then(|id| self.domains.get(id)).map(String::as_str)
    }

    fn tenant(&self, v: &Value) -> Value {
        opt(str_of(v, "memberTenantId").and_then(|id| self.tenants.get(id).map(String::as_str)))
    }

    fn roles(&self, ids: &[String]) -> Vec<String> {
        ids.iter()
            .filter_map(|id| self.roles.get(id).cloned())
            .collect()
    }

    /// `name@domain`; a name that is already an address is kept.
    fn address(&self, v: &Value) -> Option<String> {
        let name = str_of(v, "name")?;
        if name.contains('@') {
            return Some(name.to_owned());
        }
        Some(format!("{name}@{}", self.domain(str_of(v, "domainId"))?))
    }

    fn aliases(&self, v: &Value) -> Vec<Value> {
        let mut out: Vec<Value> = list_of(v, "aliases")
            .into_iter()
            .filter_map(|a| {
                Some(json!({
                    "email": self.address(a)?,
                    "enabled": a.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                    "description": opt(str_of(a, "description")),
                }))
            })
            .collect();
        out.sort_by(|a, b| str_of(a, "email").cmp(&str_of(b, "email")));
        out
    }

    /// `"user"`, `"admin"` or `"default"`, or the custom roles by name.
    fn role_of(&self, v: &Value) -> Value {
        let roles = v.get("roles").unwrap_or(&Value::Null);
        match type_of(roles) {
            "Custom" => json!({ "custom": self.roles(&set_of(roles, "roleIds")) }),
            "" => Value::Null,
            other => Value::String(other.to_ascii_lowercase()),
        }
    }
}

/// What an account may do beyond its roles: nothing for `Inherit`, else
/// the permissions it adds (`merge`) or has instead (`replace`).
fn permissions_of(v: &Value) -> Value {
    let Some(p) = v.get("permissions") else {
        return Value::Null;
    };
    match type_of(p) {
        "Merge" | "Replace" => json!({
            "mode": type_of(p).to_ascii_lowercase(),
            "enabled": set_of(p, "enabledPermissions"),
            "disabled": set_of(p, "disabledPermissions"),
        }),
        _ => Value::Null,
    }
}

fn sub_addressing_of(v: &Value) -> Value {
    let Some(s) = v.get("subAddressing") else {
        return Value::Null;
    };
    match type_of(s) {
        "Custom" => json!({ "customRule": s.get("customRule").cloned().unwrap_or(Value::Null) }),
        "" => Value::Null,
        other => Value::String(other.to_ascii_lowercase()),
    }
}

fn id_of(v: &Value) -> Option<&str> {
    str_of(v, "id")
}

pub fn build(raw: &Raw, only: &[String], logger: Logger) -> Result<Directory, Error> {
    let by_id = |list: &[Value], key: &str| -> HashMap<String, String> {
        list.iter()
            .filter_map(|v| Some((id_of(v)?.to_owned(), str_of(v, key)?.to_owned())))
            .collect()
    };
    let names = Names {
        domains: by_id(&raw.domains, "name"),
        tenants: by_id(&raw.tenants, "name"),
        roles: by_id(&raw.roles, "description"),
    };

    let only: HashSet<String> = only.iter().map(|d| d.to_ascii_lowercase()).collect();
    for wanted in &only {
        if !names
            .domains
            .values()
            .any(|d| d.eq_ignore_ascii_case(wanted))
        {
            return Err(Error::Usage(format!("the server has no domain {wanted}")));
        }
    }
    let selected = |v: &Value| -> bool {
        only.is_empty()
            || names
                .domain(str_of(v, "domainId"))
                .is_some_and(|d| only.contains(&d.to_ascii_lowercase()))
    };

    let mut dir = Directory::default();
    let mut tenant_ids: HashSet<String> = HashSet::new();
    let mut role_ids: Vec<String> = Vec::new();
    let mut note_refs = |v: &Value| {
        if let Some(t) = str_of(v, "memberTenantId") {
            tenant_ids.insert(t.to_owned());
        }
        if let Some(r) = v.get("roles") {
            role_ids.extend(set_of(r, "roleIds"));
        }
    };

    for d in &raw.domains {
        let Some(name) = str_of(d, "name") else {
            continue;
        };
        if !only.is_empty() && !only.contains(&name.to_ascii_lowercase()) {
            continue;
        }
        note_refs(d);
        dir.domains.push(json!({
            "name": name,
            "enabled": d.get("isEnabled").and_then(Value::as_bool).unwrap_or(true),
            "description": opt(str_of(d, "description")),
            "aliases": set_of(d, "aliases"),
            "catchAllAddress": opt(str_of(d, "catchAllAddress")),
            "subAddressing": sub_addressing_of(d),
            "tenant": names.tenant(d),
        }));
    }

    // Groups are named by address everywhere, so every account's address is
    // known before any is written, whichever domains are selected.
    let mut address_of: HashMap<String, String> = HashMap::new();
    for a in &raw.accounts {
        match (id_of(a), names.address(a)) {
            (Some(id), Some(email)) => {
                address_of.insert(id.to_owned(), email);
            }
            _ => {
                logger.warn(&format!(
                    "account {} has no domain this login can see; left out",
                    str_of(a, "name").unwrap_or("(unnamed)")
                ));
                *dir.skipped.entry(account_kind(a)).or_default() += 1;
            }
        }
    }
    let mut members: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for a in raw.accounts.iter().filter(|a| type_of(a) == "User") {
        let Some(email) = id_of(a).and_then(|id| address_of.get(id)) else {
            continue;
        };
        for group in set_of(a, "memberGroupIds") {
            if let Some(group_email) = address_of.get(&group) {
                members
                    .entry(group_email.as_str())
                    .or_default()
                    .insert(email.as_str());
            }
        }
    }

    for a in &raw.accounts {
        let Some(email) = id_of(a).and_then(|id| address_of.get(id)) else {
            continue;
        };
        if !selected(a) {
            continue;
        }
        note_refs(a);
        let mut entry = json!({
            "email": email,
            "name": str_of(a, "name"),
            "domain": names.domain(str_of(a, "domainId")),
            "description": opt(str_of(a, "description")),
            "roles": names.role_of(a),
            "permissions": permissions_of(a),
            "aliases": names.aliases(a),
            "quotas": quotas_of(a),
            "tenant": names.tenant(a),
            "locale": opt(str_of(a, "locale")),
            "timeZone": opt(str_of(a, "timeZone")),
            "externalId": opt(str_of(a, "externalId")),
        });
        match type_of(a) {
            "Group" => {
                let list: Vec<&str> = members
                    .get(email.as_str())
                    .map(|m| m.iter().copied().collect())
                    .unwrap_or_default();
                entry["members"] = json!(list);
                dir.groups.push(entry);
            }
            _ => {
                let mut groups: Vec<&String> = set_of(a, "memberGroupIds")
                    .iter()
                    .filter_map(|g| address_of.get(g))
                    .collect();
                groups.sort();
                entry["groups"] = json!(groups);
                dir.users.push(entry);
            }
        }
    }

    for l in &raw.lists {
        if !selected(l) {
            continue;
        }
        let Some(email) = names.address(l) else {
            *dir.skipped.entry("mailinglist").or_default() += 1;
            continue;
        };
        note_refs(l);
        dir.lists.push(json!({
            "email": email,
            "name": str_of(l, "name"),
            "domain": names.domain(str_of(l, "domainId")),
            "description": opt(str_of(l, "description")),
            "recipients": set_of(l, "recipients"),
            "aliases": names.aliases(l),
            "tenant": names.tenant(l),
        }));
    }

    // Roles the written accounts hold, and the roles those build on.
    let roles_by_id: HashMap<&str, &Value> = raw
        .roles
        .iter()
        .filter_map(|r| Some((id_of(r)?, r)))
        .collect();
    let mut kept_roles: HashSet<String> = HashSet::new();
    while let Some(id) = role_ids.pop() {
        if !kept_roles.insert(id.clone()) {
            continue;
        }
        if let Some(r) = roles_by_id.get(id.as_str()) {
            role_ids.extend(set_of(r, "roleIds"));
        }
    }
    for r in &raw.roles {
        let Some(id) = id_of(r) else {
            continue;
        };
        if !only.is_empty() && !kept_roles.contains(id) {
            continue;
        }
        if let Some(t) = str_of(r, "memberTenantId") {
            tenant_ids.insert(t.to_owned());
        }
        dir.roles.push(json!({
            "name": str_of(r, "description"),
            "tenant": names.tenant(r),
            "roles": names.roles(&set_of(r, "roleIds")),
            "enabledPermissions": set_of(r, "enabledPermissions"),
            "disabledPermissions": set_of(r, "disabledPermissions"),
        }));
    }
    for t in &raw.tenants {
        let Some(id) = id_of(t) else {
            continue;
        };
        if !only.is_empty() && !tenant_ids.contains(id) {
            continue;
        }
        dir.tenants.push(json!({
            "name": str_of(t, "name"),
            "quotas": quotas_of(t),
        }));
    }

    let by = |key: &'static str| {
        move |a: &Value, b: &Value| {
            let k = |v: &Value| str_of(v, key).map(str::to_lowercase);
            k(a).cmp(&k(b))
        }
    };
    dir.domains.sort_by(by("name"));
    dir.tenants.sort_by(by("name"));
    dir.roles.sort_by(by("name"));
    dir.users.sort_by(by("email"));
    dir.groups.sort_by(by("email"));
    dir.lists.sort_by(by("email"));
    Ok(dir)
}

fn account_kind(v: &Value) -> &'static str {
    if type_of(v) == "Group" {
        "group"
    } else {
        "user"
    }
}

/// Cells holding more than one value join them with `;`.
fn joined<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values.into_iter().collect::<Vec<_>>().join(";")
}

fn cell(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Array(items)) => joined(items.iter().filter_map(Value::as_str)),
        _ => String::new(),
    }
}

fn roles_cell(v: &Value) -> String {
    match v.get("roles") {
        Some(Value::String(s)) => s.clone(),
        Some(r) => cell(r, "custom"),
        None => String::new(),
    }
}

fn alias_emails(v: &Value) -> String {
    joined(
        v.get("aliases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|a| str_of(a, "email")),
    )
}

fn disk_quota(v: &Value) -> String {
    v.get("quotas")
        .and_then(|q| q.get("maxDiskQuota"))
        .map(Value::to_string)
        .unwrap_or_default()
}

/// One CSV file: its header, then its rows.
pub type Table = (Vec<&'static str>, Vec<Vec<String>>);

impl Directory {
    pub fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for (ty, list) in [
            ("domain", &self.domains),
            ("tenant", &self.tenants),
            ("role", &self.roles),
            ("user", &self.users),
            ("group", &self.groups),
            ("mailinglist", &self.lists),
        ] {
            summary.per_type.push((
                ty,
                TypeCounts {
                    created: list.len() as u64,
                    skipped: self.skipped.get(ty).copied().unwrap_or(0),
                    ..TypeCounts::default()
                },
            ));
        }
        summary
    }

    pub fn domains_csv(&self) -> Table {
        let header = vec![
            "name",
            "enabled",
            "description",
            "aliases",
            "catch_all",
            "sub_addressing",
            "tenant",
        ];
        let rows = self
            .domains
            .iter()
            .map(|d| {
                let sub = match d.get("subAddressing") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Object(_)) => "custom".to_owned(),
                    _ => String::new(),
                };
                vec![
                    cell(d, "name"),
                    cell(d, "enabled"),
                    cell(d, "description"),
                    cell(d, "aliases"),
                    cell(d, "catchAllAddress"),
                    sub,
                    cell(d, "tenant"),
                ]
            })
            .collect();
        (header, rows)
    }

    pub fn users_csv(&self) -> Table {
        let header = vec![
            "email",
            "name",
            "domain",
            "description",
            "roles",
            "groups",
            "aliases",
            "disk_quota",
            "tenant",
            "locale",
            "time_zone",
            "external_id",
        ];
        let rows = self
            .users
            .iter()
            .map(|u| {
                vec![
                    cell(u, "email"),
                    cell(u, "name"),
                    cell(u, "domain"),
                    cell(u, "description"),
                    roles_cell(u),
                    cell(u, "groups"),
                    alias_emails(u),
                    disk_quota(u),
                    cell(u, "tenant"),
                    cell(u, "locale"),
                    cell(u, "timeZone"),
                    cell(u, "externalId"),
                ]
            })
            .collect();
        (header, rows)
    }

    pub fn groups_csv(&self) -> Table {
        let header = vec![
            "email",
            "name",
            "domain",
            "description",
            "members",
            "aliases",
            "disk_quota",
            "tenant",
        ];
        let rows = self
            .groups
            .iter()
            .map(|g| {
                vec![
                    cell(g, "email"),
                    cell(g, "name"),
                    cell(g, "domain"),
                    cell(g, "description"),
                    cell(g, "members"),
                    alias_emails(g),
                    disk_quota(g),
                    cell(g, "tenant"),
                ]
            })
            .collect();
        (header, rows)
    }

    pub fn lists_csv(&self) -> Table {
        let header = vec![
            "email",
            "name",
            "domain",
            "description",
            "recipients",
            "aliases",
            "tenant",
        ];
        let rows = self
            .lists
            .iter()
            .map(|l| {
                vec![
                    cell(l, "email"),
                    cell(l, "name"),
                    cell(l, "domain"),
                    cell(l, "description"),
                    cell(l, "recipients"),
                    alias_emails(l),
                    cell(l, "tenant"),
                ]
            })
            .collect();
        (header, rows)
    }

    /// Every address alias with what it delivers to, one row each.
    pub fn aliases_csv(&self) -> Table {
        let header = vec!["alias", "target", "kind", "enabled"];
        let mut rows = Vec::new();
        for (kind, list) in [
            ("user", &self.users),
            ("group", &self.groups),
            ("list", &self.lists),
        ] {
            for owner in list {
                for alias in owner
                    .get("aliases")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    rows.push(vec![
                        cell(alias, "email"),
                        cell(owner, "email"),
                        kind.to_owned(),
                        cell(alias, "enabled"),
                    ]);
                }
            }
        }
        rows.sort();
        (header, rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> Raw {
        Raw {
            domains: vec![
                json!({"id": "b", "name": "example.org", "aliases": {"example.net": true},
                       "isEnabled": true, "catchAllAddress": "info@example.org",
                       "subAddressing": {"@type": "Enabled"}, "memberTenantId": "t"}),
                json!({"id": "c", "name": "other.test", "aliases": {}, "isEnabled": false,
                       "subAddressing": {"@type": "Disabled"}}),
            ],
            accounts: vec![
                json!({"id": "b", "@type": "Group", "name": "sales", "domainId": "b",
                       "roles": {"@type": "Default"}, "permissions": {"@type": "Inherit"},
                       "quotas": {}, "aliases": {}}),
                json!({"id": "c", "@type": "User", "name": "alice", "domainId": "b",
                       "memberGroupIds": {"b": true}, "memberTenantId": "t",
                       "roles": {"@type": "Custom", "roleIds": {"r": true}},
                       "permissions": {"@type": "Merge", "enabledPermissions": {"emailSend": true},
                                       "disabledPermissions": {}},
                       "quotas": {"maxDiskQuota": 1073741824},
                       "aliases": {"0": {"enabled": true, "name": "a.example", "domainId": "b"},
                                   "1": {"enabled": false, "name": "alice", "domainId": "c"}},
                       "locale": "en-US", "timeZone": "Europe/Paris"}),
                json!({"id": "d", "@type": "User", "name": "dave", "domainId": "c",
                       "memberGroupIds": {"b": true}, "roles": {"@type": "Admin"},
                       "permissions": {"@type": "Inherit"}, "quotas": {}, "aliases": {}}),
                json!({"id": "e", "@type": "User", "name": "ghost", "domainId": "zz"}),
            ],
            lists: vec![json!({"id": "b", "name": "all", "domainId": "b",
                               "recipients": {"carol@elsewhere.test": true, "alice@example.org": true},
                               "aliases": {}})],
            tenants: vec![
                json!({"id": "t", "name": "acme", "quotas": {"maxAccounts": 50}}),
                json!({"id": "u", "name": "unused", "quotas": {}}),
            ],
            roles: vec![
                json!({"id": "r", "description": "Helpdesk", "roleIds": {"s": true},
                       "enabledPermissions": {"impersonate": true}, "disabledPermissions": {}}),
                json!({"id": "s", "description": "Reader", "roleIds": {"r": true},
                       "enabledPermissions": {}, "disabledPermissions": {}}),
                json!({"id": "x", "description": "Unused", "roleIds": {}}),
            ],
        }
    }

    fn emails(list: &[Value]) -> Vec<&str> {
        list.iter().filter_map(|v| str_of(v, "email")).collect()
    }

    #[test]
    fn ids_become_addresses_and_names() {
        let dir = build(&raw(), &[], Logger::new(0)).unwrap();
        assert_eq!(emails(&dir.users), ["alice@example.org", "dave@other.test"]);
        let alice = &dir.users[0];
        assert_eq!(alice["groups"], json!(["sales@example.org"]));
        assert_eq!(alice["roles"], json!({"custom": ["Helpdesk"]}));
        assert_eq!(alice["tenant"], "acme");
        assert_eq!(alice["quotas"], json!({"maxDiskQuota": 1073741824}));
        assert_eq!(
            alice["permissions"],
            json!({"mode": "merge", "enabled": ["emailSend"], "disabled": []})
        );
        assert_eq!(
            alice["aliases"],
            json!([
                {"email": "a.example@example.org", "enabled": true, "description": null},
                {"email": "alice@other.test", "enabled": false, "description": null},
            ])
        );
        assert_eq!(dir.users[1]["roles"], "admin");
        assert_eq!(
            dir.groups[0]["members"],
            json!(["alice@example.org", "dave@other.test"])
        );
        assert_eq!(
            dir.lists[0]["recipients"],
            json!(["alice@example.org", "carol@elsewhere.test"])
        );
        assert_eq!(dir.domains[0]["aliases"], json!(["example.net"]));
        assert_eq!(dir.domains[0]["subAddressing"], "enabled");
        assert_eq!(dir.domains[1]["enabled"], false);
        assert_eq!(dir.tenants.len(), 2);
        assert_eq!(dir.roles.len(), 3);
    }

    #[test]
    fn an_account_on_an_unknown_domain_is_counted_and_left_out() {
        let dir = build(&raw(), &[], Logger::new(0)).unwrap();
        let summary = dir.summary();
        let users = &summary
            .per_type
            .iter()
            .find(|(t, _)| *t == "user")
            .unwrap()
            .1;
        assert_eq!((users.created, users.skipped), (2, 1));
    }

    #[test]
    fn one_domain_keeps_only_what_it_uses() {
        let dir = build(&raw(), &["Example.org".to_owned()], Logger::new(0)).unwrap();
        assert_eq!(dir.domains.len(), 1);
        assert_eq!(emails(&dir.users), ["alice@example.org"]);
        // Members elsewhere are still named by address.
        assert_eq!(
            dir.groups[0]["members"],
            json!(["alice@example.org", "dave@other.test"])
        );
        let roles: Vec<&str> = dir.roles.iter().filter_map(|r| str_of(r, "name")).collect();
        assert_eq!(roles, ["Helpdesk", "Reader"]);
        let tenants: Vec<&str> = dir
            .tenants
            .iter()
            .filter_map(|t| str_of(t, "name"))
            .collect();
        assert_eq!(tenants, ["acme"]);
    }

    #[test]
    fn an_unknown_domain_is_a_usage_error() {
        let err = build(&raw(), &["nope.test".to_owned()], Logger::new(0)).unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
    }

    #[test]
    fn csv_rows_follow_their_headers() {
        let dir = build(&raw(), &[], Logger::new(0)).unwrap();
        let (header, rows) = dir.users_csv();
        assert_eq!(rows[0].len(), header.len());
        assert_eq!(rows[0][4], "Helpdesk");
        assert_eq!(rows[0][6], "a.example@example.org;alice@other.test");
        assert_eq!(rows[0][7], "1073741824");
        let (_, aliases) = dir.aliases_csv();
        assert_eq!(
            aliases[0],
            ["a.example@example.org", "alice@example.org", "user", "true"]
        );
        let (_, domains) = dir.domains_csv();
        assert_eq!(domains[0][3], "example.net");
        assert_eq!(domains[0][4], "info@example.org");
    }
}
