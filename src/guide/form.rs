/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! What the page sends, and the plan and directory written from it. The
//! plan is an ordinary one: `migrate` runs it, and so can the administrator
//! from the command line. Secrets stay out of it, named by the variables
//! below, which the guide sets for the runs it starts.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::export_directory::FORMAT_VERSION as DIRECTORY_VERSION;
use crate::migrate::plan::{self, FORMAT, FORMAT_VERSION};

/// The source's password or API key.
pub const SOURCE_SECRET_VAR: &str = "MIGRATE_SOURCE_SECRET";
/// The inbuxa administrator's API key.
pub const TARGET_TOKEN_VAR: &str = "MIGRATE_INBUXA_TOKEN";
/// The directory the setup step reads, relative to the guide's directory.
pub const DIRECTORY: &str = "directory";
const GMAIL_IMAP: &str = "imaps://imap.gmail.com";

#[derive(Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Form {
    pub source: Source,
    pub accounts: Vec<String>,
    /// Create on inbuxa the accounts it doesn't have yet.
    pub create: bool,
    pub target: Target,
    pub parallel: usize,
}

#[derive(Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    #[default]
    Imap,
    Jmap,
    Maildir,
    /// Microsoft 365, through Graph, as an app with application permissions.
    Microsoft,
    /// Google Workspace's Gmail, as a service account with domain-wide
    /// delegation.
    Google,
}

impl Source {
    /// Whether the source signs in with a secret typed into the page. A
    /// Google key given as a secret, not a file, is one too.
    pub fn has_secret(&self) -> bool {
        match self.kind {
            SourceKind::Imap | SourceKind::Jmap | SourceKind::Microsoft => true,
            SourceKind::Google => self.key_file.trim().is_empty(),
            SourceKind::Maildir => false,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Source {
    pub kind: SourceKind,
    pub url: String,
    /// IMAP: the login for each account, such as `{email}*master`. JMAP:
    /// the administrator, or empty for an API key.
    pub login: String,
    /// Maildir: where each account's folder is, such as
    /// `/var/vmail/{domain}/{local}/Maildir`.
    pub path: String,
    /// IMAP: credentials on `imap://` without STARTTLS.
    pub cleartext: bool,
    /// Microsoft 365: the Entra tenant and the app's client id.
    pub tenant: String,
    pub client_id: String,
    /// Google: the service account's JSON key file. Empty when the key
    /// itself is the secret, as from a program running the tool.
    pub key_file: String,
    /// The password, API key, client secret or Google key.
    #[serde(skip_serializing)]
    pub secret: String,
}

#[derive(Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Target {
    pub url: String,
    /// `--stay-on-url`: for a server reached at an address other than the
    /// one it advertises, such as its own loopback.
    pub stay_on_url: bool,
    /// `--allow-invalid-certs`, for the same.
    pub allow_invalid_certs: bool,
    #[serde(skip_serializing)]
    pub token: String,
}

impl Target {
    /// The flags every step that writes into inbuxa takes.
    fn flags(&self) -> Vec<&'static str> {
        let mut flags = Vec::new();
        if self.stay_on_url {
            flags.push("--stay-on-url");
        }
        if self.allow_invalid_certs {
            flags.push("--allow-invalid-certs");
        }
        flags
    }
}

impl Form {
    /// The secrets a run needs, by variable.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![(TARGET_TOKEN_VAR, self.target.token.clone())];
        if self.source.has_secret() {
            env.push((SOURCE_SECRET_VAR, self.source.secret.clone()));
        }
        env
    }

    /// Fields left empty keep what an earlier form had: the page doesn't
    /// hold on to secrets across a reload, the guide does.
    pub fn keep_secrets_from(&mut self, earlier: &Form) {
        if self.source.secret.is_empty() {
            self.source.secret.clone_from(&earlier.source.secret);
        }
        if self.target.token.is_empty() {
            self.target.token.clone_from(&earlier.target.token);
        }
    }

    /// Everything a run needs, or what's missing, in the page's words.
    pub fn check(&self) -> Result<(), String> {
        self.check_answers()?;
        let s = &self.source;
        if s.has_secret() && s.secret.is_empty() {
            return Err(match s.kind {
                SourceKind::Microsoft => "Give the app's client secret.".into(),
                SourceKind::Google => "Give the service account's key file.".into(),
                _ => "Give the old server's password or API key.".into(),
            });
        }
        if self.target.token.is_empty() {
            return Err("Give the inbuxa server's address and an administrator's API key.".into());
        }
        Ok(())
    }

    /// Everything a plan needs, secrets aside: a plan only names the
    /// variables they'll be in.
    pub fn check_answers(&self) -> Result<(), String> {
        let s = &self.source;
        match s.kind {
            SourceKind::Imap | SourceKind::Jmap if s.url.trim().is_empty() => {
                return Err("Give the old server's address.".into());
            }
            SourceKind::Imap if !names_account(&s.login) => {
                return Err(
                    "The login for each account has to name it: {email} or {local}.".into(),
                );
            }
            SourceKind::Maildir if !names_account(&s.path) => {
                return Err("The Maildir path has to name the account: {email} or {local}.".into());
            }
            SourceKind::Microsoft
                if s.tenant.trim().is_empty() || s.client_id.trim().is_empty() =>
            {
                return Err("Give the Microsoft 365 tenant and the app's client id.".into());
            }
            _ => {}
        }
        if self.accounts.is_empty() {
            return Err("List the accounts to move.".into());
        }
        if self.target.url.trim().is_empty() {
            return Err("Give the inbuxa server's address and an administrator's API key.".into());
        }
        Ok(())
    }

    /// The step that reads each account from the source into its archive.
    fn import_step(&self) -> Value {
        let s = &self.source;
        let mut args: Vec<&str> = Vec::new();
        let mut secrets = json!({});
        match s.kind {
            SourceKind::Imap => {
                args.extend(["import", "imap", "--url", s.url.trim()]);
                args.extend(["--auth-basic", s.login.trim()]);
                if s.cleartext {
                    args.push("--allow-cleartext");
                }
                secrets = json!({"password": SOURCE_SECRET_VAR});
            }
            SourceKind::Jmap => {
                args.extend(["import", "jmap", "--url", s.url.trim()]);
                if s.login.trim().is_empty() {
                    args.push("--auth-bearer");
                    secrets = json!({"token": SOURCE_SECRET_VAR});
                } else {
                    args.extend(["--auth-basic", s.login.trim()]);
                    secrets = json!({"password": SOURCE_SECRET_VAR});
                }
                args.extend(["--account-name", "{email}"]);
            }
            SourceKind::Maildir => args.extend(["import", "maildir", s.path.trim()]),
            SourceKind::Microsoft => {
                args.extend(["import", "exchange-graph", "--tenant", s.tenant.trim()]);
                args.extend(["--client-id", s.client_id.trim(), "--app-only"]);
                args.extend(["--user", "{email}"]);
                secrets = json!({"clientSecret": SOURCE_SECRET_VAR});
            }
            SourceKind::Google if s.key_file.trim().is_empty() => {
                args.extend(["import", "imap", "--url", GMAIL_IMAP]);
                args.extend(["--google-key-from-env", "--auth-user", "{email}"]);
                secrets = json!({"googleKey": SOURCE_SECRET_VAR});
            }
            SourceKind::Google => {
                args.extend(["import", "imap", "--url", GMAIL_IMAP]);
                args.extend(["--google-key", s.key_file.trim(), "--auth-user", "{email}"]);
            }
        }
        args.push("{archive}");
        json!({"name": "import", "args": args, "secrets": secrets})
    }

    fn export_step(&self) -> Value {
        let mut args = vec!["export", "--url", self.target.url.trim(), "--auth-bearer"];
        args.extend(self.target.flags());
        args.extend(["--account-name", "{email}", "{archive}"]);
        json!({"name": "export", "args": args, "secrets": {"token": TARGET_TOKEN_VAR}})
    }

    fn setup(&self, passwords: &str) -> Vec<Value> {
        if !self.create {
            return Vec::new();
        }
        let mut args = vec![
            "import-directory",
            "--url",
            self.target.url.trim(),
            "--auth-bearer",
        ];
        args.extend(self.target.flags());
        args.extend(["--passwords", passwords, DIRECTORY]);
        vec![json!({"name": "accounts", "args": args, "secrets": {"token": TARGET_TOKEN_VAR}})]
    }

    /// The plan for the move: set up the accounts if asked, then read each
    /// one and write it into inbuxa. New passwords go to `passwords`.
    pub fn plan(&self, passwords: &str) -> Value {
        json!({
            "format": FORMAT, "formatVersion": FORMAT_VERSION,
            "setup": self.setup(passwords),
            "accounts": self.accounts,
            "steps": [self.import_step(), self.export_step()],
        })
    }

    /// The plan the preview dry-runs. An export reads the archive, which a
    /// dry run leaves empty, so it would show nothing; the import is where
    /// a wrong login or a missing folder shows up.
    pub fn preview_plan(&self) -> Value {
        json!({
            "format": FORMAT, "formatVersion": FORMAT_VERSION,
            "setup": self.setup("passwords.csv"),
            "accounts": self.accounts,
            "steps": [self.import_step()],
        })
    }
}

fn names_account(template: &str) -> bool {
    template.contains("{email}") || template.contains("{local}")
}

/// The addresses in pasted text or a CSV: the first field on each line
/// that is one. Blank lines, `#` comments and a header are skipped; a line
/// with no usable address is handed back, so the page can show it.
pub fn parse_accounts(text: &str) -> (Vec<String>, Vec<String>) {
    let mut accounts = Vec::new();
    let mut rejected = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let address = line
            .split([',', ';', '\t', ' '])
            .map(|f| f.trim().trim_matches('"'))
            .find(|f| f.contains('@'));
        match address.map(plan::account_email) {
            Some(Ok(email)) => {
                if !accounts.contains(&email) {
                    accounts.push(email);
                }
            }
            None if n == 0 => {}
            _ => rejected.push(line.to_owned()),
        }
    }
    (accounts, rejected)
}

/// The users in a directory `export-directory` wrote.
pub fn directory_accounts(doc: &Value) -> Vec<String> {
    doc.get("users")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|u| u.get("email").and_then(Value::as_str))
        .filter_map(|e| plan::account_email(e).ok())
        .collect()
}

/// The directory `import-directory` sets up: the source's, when it was
/// read, with only the chosen users; and a plain user, and its domain, for
/// each chosen account it doesn't have.
pub fn directory(source: Option<&Value>, accounts: &[String]) -> Value {
    let mut doc = source.cloned().unwrap_or_else(|| {
        json!({
            "format": "inbuxa-migrate-directory", "formatVersion": DIRECTORY_VERSION,
            "domains": [], "tenants": [], "roles": [], "groups": [], "lists": [], "users": [],
        })
    });
    let email_of = |v: &Value| {
        v.get("email")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
    };
    let mut users: Vec<Value> = doc
        .get("users")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|u| email_of(u).is_some_and(|e| accounts.contains(&e)))
        .cloned()
        .collect();
    let mut domains: Vec<Value> = doc
        .get("domains")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for email in accounts {
        let Some((local, domain)) = email.split_once('@') else {
            continue;
        };
        if !users.iter().any(|u| email_of(u).as_ref() == Some(email)) {
            users.push(json!({
                "email": email, "name": local, "domain": domain, "description": null,
                "roles": "user", "permissions": null, "aliases": [], "quotas": {},
                "tenant": null, "locale": null, "timeZone": null, "externalId": null,
                "groups": [],
            }));
        }
        let known = domains.iter().any(|d| {
            d.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.eq_ignore_ascii_case(domain))
        });
        if !known {
            domains.push(json!({
                "name": domain, "enabled": true, "description": null, "aliases": [],
                "catchAllAddress": null, "subAddressing": "enabled", "tenant": null,
            }));
        }
    }
    doc["users"] = Value::Array(users);
    doc["domains"] = Value::Array(domains);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(kind: SourceKind) -> Form {
        Form {
            source: Source {
                kind,
                url: "imaps://old.example.org".into(),
                login: "{email}*master".into(),
                path: "/var/vmail/{domain}/{local}/Maildir".into(),
                cleartext: false,
                tenant: String::new(),
                client_id: String::new(),
                key_file: String::new(),
                secret: "s3cret".into(),
            },
            accounts: vec!["ann@example.org".into()],
            create: true,
            target: Target {
                url: "https://mail.example.org".into(),
                token: "t0ken".into(),
                ..Target::default()
            },
            parallel: 4,
        }
    }

    #[test]
    fn an_imap_plan_sets_up_reads_with_the_master_login_and_exports() {
        let plan = form(SourceKind::Imap).plan("passwords-1.csv");
        let text = plan.to_string();
        assert!(
            !text.contains("s3cret") && !text.contains("t0ken"),
            "{text}"
        );
        assert_eq!(
            plan["setup"][0]["args"],
            json!([
                "import-directory",
                "--url",
                "https://mail.example.org",
                "--auth-bearer",
                "--passwords",
                "passwords-1.csv",
                "directory"
            ])
        );
        assert_eq!(
            plan["steps"][0]["args"],
            json!([
                "import",
                "imap",
                "--url",
                "imaps://old.example.org",
                "--auth-basic",
                "{email}*master",
                "{archive}"
            ])
        );
        assert_eq!(plan["steps"][0]["secrets"]["password"], SOURCE_SECRET_VAR);
        assert_eq!(plan["steps"][1]["args"][5], "{email}");
        assert_eq!(plan["steps"][1]["secrets"]["token"], TARGET_TOKEN_VAR);
    }

    #[test]
    fn a_server_on_its_own_loopback_stays_there_in_every_inbuxa_step() {
        let mut f = form(SourceKind::Imap);
        f.target.url = "https://127.0.0.1:443".into();
        f.target.stay_on_url = true;
        f.target.allow_invalid_certs = true;
        let plan = f.plan("p.csv");
        for args in [&plan["setup"][0]["args"], &plan["steps"][1]["args"]] {
            assert_eq!(args[4], "--stay-on-url", "{args}");
            assert_eq!(args[5], "--allow-invalid-certs", "{args}");
        }
        let import = plan["steps"][0]["args"].to_string();
        assert!(!import.contains("--stay-on-url"), "{import}");
    }

    #[test]
    fn a_google_key_is_a_file_or_the_secret() {
        let mut f = form(SourceKind::Google);
        f.source.key_file = "sa.json".into();
        f.source.secret.clear();
        assert_eq!(f.check(), Ok(()));
        let step = &f.plan("p.csv")["steps"][0];
        assert_eq!(step["args"][4], "--google-key");
        assert_eq!(step["secrets"], json!({}));
        assert_eq!(f.env().len(), 1, "only the target's token");

        f.source.key_file.clear();
        assert_eq!(f.check_answers(), Ok(()));
        assert_eq!(
            f.check(),
            Err("Give the service account's key file.".into())
        );
        f.source.secret = "{\"type\":\"service_account\"}".into();
        assert_eq!(f.check(), Ok(()));
        let step = &f.plan("p.csv")["steps"][0];
        assert_eq!(step["args"][4], "--google-key-from-env");
        assert_eq!(step["secrets"]["googleKey"], SOURCE_SECRET_VAR);
        assert!(f.env().iter().any(|(var, _)| *var == SOURCE_SECRET_VAR));
    }

    #[test]
    fn answers_are_checked_without_their_secrets() {
        let mut f = form(SourceKind::Imap);
        f.source.secret.clear();
        f.target.token.clear();
        assert_eq!(f.check_answers(), Ok(()));
        assert!(f.check().is_err());
        f.target.url.clear();
        assert!(f.check_answers().is_err());
    }

    #[test]
    fn a_jmap_source_acts_for_each_account_by_basic_or_by_key() {
        let mut f = form(SourceKind::Jmap);
        f.source.login = "admin".into();
        let step = &f.plan("p.csv")["steps"][0];
        assert_eq!(step["args"][4], "--auth-basic");
        assert_eq!(step["args"][6], "--account-name");
        f.source.login.clear();
        let step = &f.plan("p.csv")["steps"][0];
        assert_eq!(step["args"][4], "--auth-bearer");
        assert_eq!(step["secrets"]["token"], SOURCE_SECRET_VAR);
    }

    #[test]
    fn the_preview_has_no_export_and_maildir_needs_no_secret() {
        let mut f = form(SourceKind::Maildir);
        f.create = false;
        f.source.secret.clear();
        assert_eq!(f.check(), Ok(()));
        let plan = f.preview_plan();
        assert_eq!(plan["setup"], json!([]));
        assert_eq!(plan["steps"].as_array().unwrap().len(), 1);
        assert_eq!(
            plan["steps"][0]["args"],
            json!([
                "import",
                "maildir",
                "/var/vmail/{domain}/{local}/Maildir",
                "{archive}"
            ])
        );
        assert_eq!(f.env(), [(TARGET_TOKEN_VAR, "t0ken".to_owned())]);
    }

    #[test]
    fn microsoft_365_reads_as_the_app_and_google_as_the_service_account() {
        let mut f = form(SourceKind::Microsoft);
        f.source.tenant = "contoso.onmicrosoft.com".into();
        f.source.client_id = "app-1".into();
        let step = &f.plan("p.csv")["steps"][0];
        assert_eq!(
            step["args"],
            json!([
                "import",
                "exchange-graph",
                "--tenant",
                "contoso.onmicrosoft.com",
                "--client-id",
                "app-1",
                "--app-only",
                "--user",
                "{email}",
                "{archive}"
            ])
        );
        assert_eq!(step["secrets"]["clientSecret"], SOURCE_SECRET_VAR);
        f.source.tenant.clear();
        assert!(f.check().unwrap_err().contains("tenant"));

        let mut f = form(SourceKind::Google);
        f.source.secret.clear();
        assert!(f.check().unwrap_err().contains("key file"));
        f.source.key_file = "/keys/sa.json".into();
        assert_eq!(f.check(), Ok(()));
        let step = &f.plan("p.csv")["steps"][0];
        assert_eq!(
            step["args"],
            json!([
                "import",
                "imap",
                "--url",
                "imaps://imap.gmail.com",
                "--google-key",
                "/keys/sa.json",
                "--auth-user",
                "{email}",
                "{archive}"
            ])
        );
        assert_eq!(step["secrets"], json!({}));
        assert_eq!(f.env(), [(TARGET_TOKEN_VAR, "t0ken".to_owned())]);
    }

    #[test]
    fn a_form_that_cant_make_a_plan_says_why() {
        let mut f = form(SourceKind::Imap);
        f.source.login = "master".into();
        assert!(f.check().unwrap_err().contains("{email}"));
        let mut f = form(SourceKind::Imap);
        f.accounts.clear();
        assert!(f.check().unwrap_err().contains("accounts"));
        let mut f = form(SourceKind::Jmap);
        f.source.secret.clear();
        assert!(f.check().unwrap_err().contains("password"));
    }

    #[test]
    fn secrets_are_never_saved_and_an_empty_one_keeps_the_last() {
        let f = form(SourceKind::Imap);
        let saved = serde_json::to_string(&f).unwrap();
        assert!(
            !saved.contains("s3cret") && !saved.contains("t0ken"),
            "{saved}"
        );
        let mut again: Form = serde_json::from_str(&saved).unwrap();
        again.keep_secrets_from(&f);
        assert_eq!(
            (again.source.secret.as_str(), again.target.token.as_str()),
            ("s3cret", "t0ken")
        );
    }

    #[test]
    fn accounts_come_from_pasted_lines_or_a_csv() {
        let (ok, bad) = parse_accounts(
            "email,name\nAnn@Example.org,Ann\n\n# left out\n\"bob@example.org\";Bob\nann@example.org\nno address here\n/x@y\n",
        );
        assert_eq!(ok, ["ann@example.org", "bob@example.org"]);
        assert_eq!(bad, ["no address here", "/x@y"]);
    }

    #[test]
    fn the_directory_keeps_the_chosen_users_and_adds_the_rest() {
        let source = json!({
            "format": "inbuxa-migrate-directory", "formatVersion": 1,
            "domains": [{"name": "example.org"}],
            "groups": [{"email": "team@example.org"}],
            "users": [{"email": "Ann@example.org", "name": "ann", "domain": "example.org",
                       "roles": "admin"},
                      {"email": "cat@example.org"}],
        });
        let accounts = ["ann@example.org".to_owned(), "bob@other.test".to_owned()];
        let doc = directory(Some(&source), &accounts);
        let emails: Vec<_> = doc["users"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["email"].as_str().unwrap())
            .collect();
        assert_eq!(emails, ["Ann@example.org", "bob@other.test"]);
        assert_eq!(doc["users"][0]["roles"], "admin");
        assert_eq!(doc["users"][1]["roles"], "user");
        assert_eq!(doc["domains"][1]["name"], "other.test");
        assert_eq!(doc["domains"].as_array().unwrap().len(), 2);
        assert_eq!(doc["groups"], source["groups"]);
        assert_eq!(directory_accounts(&doc), accounts);

        let fresh = directory(None, &accounts[..1]);
        assert_eq!(fresh["format"], "inbuxa-migrate-directory");
        assert_eq!(fresh["domains"][0]["name"], "example.org");
    }
}
