/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! The plan file: the steps to run once, the steps to run for each account,
//! and the accounts. A step is one inbuxa-migrate command line, written as
//! the docs write it, with `{email}`, `{local}`, `{domain}` and `{archive}`
//! filled in per account. Secrets are never in the plan: a step names the
//! environment variable that holds each one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::error::Error;

pub const FORMAT: &str = "inbuxa-migrate-plan";
pub const FORMAT_VERSION: u32 = 1;

/// The variables a child reads its secrets from. All are cleared before a
/// step runs, so a step only ever sees the secrets it names.
pub const SECRET_VARS: &[&str] = &[
    "INBUXA_MIGRATE_PASSWORD",
    "INBUXA_MIGRATE_API_KEY",
    "INBUXA_MIGRATE_TOKEN",
    "INBUXA_MIGRATE_EWS_CLIENT_SECRET",
    "INBUXA_MIGRATE_GRAPH_TOKEN",
];

const ACCOUNT_COMMANDS: &[&str] = &[
    "import",
    "export",
    "export-imap",
    "export-managesieve",
    "export-caldav",
    "export-carddav",
    "export-files",
];
const SETUP_COMMANDS: &[&str] = &["import-directory", "export-directory"];
const PLACEHOLDERS: &[&str] = &["{email}", "{local}", "{domain}", "{archive}"];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawPlan {
    format: String,
    format_version: u32,
    #[serde(default)]
    setup: Vec<RawStep>,
    accounts: RawAccounts,
    steps: Vec<RawStep>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawAccounts {
    List(Vec<String>),
    Directory(FromDirectory),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FromDirectory {
    directory: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    name: String,
    args: Vec<String>,
    #[serde(default)]
    secrets: Secrets,
}

/// Each secret by the variable that holds it.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Secrets {
    password: Option<String>,
    token: Option<String>,
    client_secret: Option<String>,
    graph_token: Option<String>,
}

pub struct Plan {
    /// The plan's own directory: steps run there, so relative paths in a
    /// step mean what they say in the plan.
    pub base: PathBuf,
    pub setup: Vec<Step>,
    pub accounts: Vec<String>,
    pub steps: Vec<Step>,
}

/// No `Debug`: `env` holds secrets.
pub struct Step {
    pub name: String,
    pub args: Vec<String>,
    /// The child's secret variables, with their values.
    pub env: Vec<(&'static str, String)>,
}

impl Step {
    /// The command line for one account, or for setup.
    pub fn args_for(&self, account: Option<(&str, &Path)>) -> Vec<String> {
        let Some((email, archive)) = account else {
            return self.args.clone();
        };
        let (local, domain) = email.rsplit_once('@').unwrap_or((email, ""));
        let archive = archive.to_string_lossy();
        self.args
            .iter()
            .map(|a| {
                a.replace("{email}", email)
                    .replace("{local}", local)
                    .replace("{domain}", domain)
                    .replace("{archive}", &archive)
            })
            .collect()
    }
}

pub fn load(path: &Path) -> Result<Plan, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Usage(format!("can't read plan {}: {e}", path.display())))?;
    let raw: RawPlan = serde_json::from_str(&text)
        .map_err(|e| Error::Usage(format!("plan {}: {e}", path.display())))?;
    if raw.format != FORMAT {
        return Err(Error::Usage(format!(
            "plan {}: format is \"{}\", not \"{FORMAT}\"",
            path.display(),
            raw.format
        )));
    }
    if raw.format_version != FORMAT_VERSION {
        return Err(Error::Usage(format!(
            "plan {}: formatVersion {} is not one this release reads ({FORMAT_VERSION})",
            path.display(),
            raw.format_version
        )));
    }
    let base = std::path::absolute(path)?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();

    if raw.steps.is_empty() {
        return Err(Error::Usage("the plan has no steps".to_owned()));
    }
    let mut names = HashSet::new();
    let mut setup = Vec::new();
    for step in raw.setup {
        setup.push(step_of(step, false, &mut names)?);
    }
    let mut steps = Vec::new();
    for step in raw.steps {
        steps.push(step_of(step, true, &mut names)?);
    }

    let listed = match raw.accounts {
        RawAccounts::List(list) => list,
        RawAccounts::Directory(from) => directory_users(&base.join(from.directory))?,
    };
    let mut accounts = Vec::new();
    let mut seen = HashSet::new();
    for email in listed {
        let email = account_email(&email)?;
        if !seen.insert(email.clone()) {
            return Err(Error::Usage(format!("account {email} is listed twice")));
        }
        accounts.push(email);
    }
    if accounts.is_empty() {
        return Err(Error::Usage("the plan lists no accounts".to_owned()));
    }
    Ok(Plan {
        base,
        setup,
        accounts,
        steps,
    })
}

fn step_of(raw: RawStep, per_account: bool, names: &mut HashSet<String>) -> Result<Step, Error> {
    let name = raw.name;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::Usage(format!(
            "step name \"{name}\": use letters, digits, - and _"
        )));
    }
    if !names.insert(name.clone()) {
        return Err(Error::Usage(format!("step name \"{name}\" is used twice")));
    }
    check_args(&name, &raw.args, per_account)?;

    let mut env = Vec::new();
    let secrets = raw.secrets;
    for (child_var, from) in [
        ("INBUXA_MIGRATE_PASSWORD", secrets.password),
        ("INBUXA_MIGRATE_API_KEY", secrets.token),
        ("INBUXA_MIGRATE_EWS_CLIENT_SECRET", secrets.client_secret),
        ("INBUXA_MIGRATE_GRAPH_TOKEN", secrets.graph_token),
    ] {
        let Some(from) = from else { continue };
        match std::env::var(&from) {
            Ok(value) if !value.is_empty() => env.push((child_var, value)),
            _ => {
                return Err(Error::Usage(format!(
                    "step {name}: ${from} is not set; it holds a secret the step names"
                )));
            }
        }
    }
    Ok(Step {
        name,
        args: raw.args,
        env,
    })
}

fn check_args(name: &str, args: &[String], per_account: bool) -> Result<(), Error> {
    let usage = |m: String| Err(Error::Usage(format!("step {name}: {m}")));
    let (allowed, kind) = if per_account {
        (ACCOUNT_COMMANDS, "an account step")
    } else {
        (SETUP_COMMANDS, "a setup step")
    };
    match args.first() {
        Some(command) if allowed.contains(&command.as_str()) => {}
        Some(command) => {
            return usage(format!(
                "{kind} runs one of {}, not \"{command}\"",
                allowed.join(", ")
            ));
        }
        None => return usage("args is empty".to_owned()),
    }
    for (i, arg) in args.iter().enumerate() {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(f, _)| f);
        match flag {
            "--auth-password" | "--ews-client-secret" => {
                return usage(format!(
                    "{flag} would put a secret in the plan; name its variable in \"secrets\""
                ));
            }
            "--auth-bearer" | "--access-token" => {
                let next = args.get(i + 1);
                if arg.contains('=') || next.is_some_and(|n| !n.starts_with('-')) {
                    return usage(format!(
                        "{flag} takes its token from \"secrets\"; write it with no value, \
                         before another flag"
                    ));
                }
            }
            "--dry-run" => {
                return usage("leave out --dry-run; migrate --dry-run adds it".to_owned());
            }
            _ => {}
        }
    }
    let placeholders = args
        .iter()
        .any(|a| PLACEHOLDERS.iter().any(|p| a.contains(p)));
    if per_account && !args.iter().any(|a| a.contains("{archive}")) {
        return usage("an account step has to name {archive}".to_owned());
    }
    if !per_account && placeholders {
        return usage(
            "a setup step runs once, so it can't use {email}, {archive} and the rest".to_owned(),
        );
    }
    Ok(())
}

/// The users in a `directory.json` that `export-directory` wrote.
fn directory_users(path: &Path) -> Result<Vec<String>, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Usage(format!("can't read {}: {e}", path.display())))?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| Error::Usage(format!("{}: {e}", path.display())))?;
    if doc.get("format").and_then(Value::as_str) != Some("inbuxa-migrate-directory") {
        return Err(Error::Usage(format!(
            "{} is not a directory.json from export-directory",
            path.display()
        )));
    }
    Ok(doc
        .get("users")
        .and_then(Value::as_array)
        .map(|users| {
            users
                .iter()
                .filter_map(|u| u.get("email").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

/// An address that is safe as a directory name, lowercased.
pub(crate) fn account_email(email: &str) -> Result<String, Error> {
    let email = email.trim().to_ascii_lowercase();
    let ok = match email.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && !domain.contains('@')
                && !email.starts_with('.')
                && !email
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\'))
        }
        None => false,
    };
    if !ok {
        return Err(Error::Usage(format!(
            "account \"{email}\" is not an address the plan can use"
        )));
    }
    Ok(email)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn err(list: &[&str], per_account: bool) -> String {
        check_args("s", &args(list), per_account)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn secrets_and_dry_run_stay_out_of_the_args() {
        let e = err(
            &["import", "imap", "--auth-password", "x", "{archive}"],
            true,
        );
        assert!(e.contains("secrets"), "{e}");
        let e = err(&["import", "imap", "--auth-password=x", "{archive}"], true);
        assert!(e.contains("secrets"), "{e}");
        // A bare --auth-bearer before the archive would take it as the token.
        let e = err(&["export", "--auth-bearer", "{archive}"], true);
        assert!(e.contains("before another flag"), "{e}");
        let e = err(&["export", "--dry-run", "{archive}"], true);
        assert!(e.contains("--dry-run"), "{e}");
        check_args(
            "s",
            &args(&[
                "export",
                "--auth-bearer",
                "--account-name",
                "{email}",
                "{archive}",
            ]),
            true,
        )
        .unwrap();
    }

    #[test]
    fn steps_run_the_commands_they_are_for() {
        let e = err(&["import-directory", "dir"], true);
        assert!(e.contains("an account step runs one of"), "{e}");
        let e = err(&["import", "imap", "--url", "x"], true);
        assert!(e.contains("{archive}"), "{e}");
        let e = err(&["import-directory", "--url", "{domain}", "dir"], false);
        assert!(e.contains("runs once"), "{e}");
        check_args("s", &args(&["import-directory", "dir"]), false).unwrap();
    }

    #[test]
    fn placeholders_are_filled_per_account() {
        let step = Step {
            name: "mail".into(),
            args: args(&[
                "import",
                "imap",
                "--auth-basic",
                "{local}*master@{domain}",
                "{archive}",
                "^A\\d{4}$",
            ]),
            env: Vec::new(),
        };
        assert_eq!(
            step.args_for(Some((
                "ann@example.org",
                Path::new("/w/ann/archive.sqlite")
            ))),
            args(&[
                "import",
                "imap",
                "--auth-basic",
                "ann*master@example.org",
                "/w/ann/archive.sqlite",
                "^A\\d{4}$"
            ])
        );
    }

    #[test]
    fn accounts_are_plain_addresses() {
        assert_eq!(
            account_email(" Ann@Example.org ").unwrap(),
            "ann@example.org"
        );
        for bad in [
            "ann",
            "@example.org",
            "ann@",
            "a/b@example.org",
            "../x@y",
            "a b@c",
        ] {
            assert!(account_email(bad).is_err(), "{bad}");
        }
    }
}
