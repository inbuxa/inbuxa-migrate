/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `migrate`: many accounts moved by one plan. Each step is a run of this
//! same binary, so it behaves exactly as the command the docs describe, its
//! output goes to that account's log, and one account failing leaves the
//! rest running. The work directory holds an archive and logs per account,
//! the state a second run resumes from, and the report.

pub mod plan;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::logging::Logger;

use plan::{Plan, SECRET_VARS, Step};

pub struct MigrateConfig {
    pub plan: PathBuf,
    pub work: PathBuf,
    /// Accounts migrated at once.
    pub parallel: usize,
    /// Run every account step again, done or not.
    pub again: bool,
    /// Only these accounts; empty for all of them.
    pub only: Vec<String>,
    pub dry_run: bool,
    /// The binary each step runs: this one.
    pub program: PathBuf,
    pub logger: Logger,
}

pub struct Outcome {
    pub done: usize,
    pub unfinished: usize,
    pub report: PathBuf,
}

const STATE_FORMAT: &str = "inbuxa-migrate-state";

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct State {
    format: String,
    format_version: u32,
    setup: BTreeMap<String, Ended>,
    accounts: BTreeMap<String, BTreeMap<String, Ended>>,
}

/// How a step ended. Only `Done` is skipped by the next run.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
enum Ended {
    Done,
    Partial,
    Failed,
}

/// What one step did this run, for the report.
struct StepReport {
    step: String,
    result: &'static str,
    counts: Counts,
    detail: String,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    fetched: u64,
    created: u64,
    updated: u64,
    deleted: u64,
    skipped: u64,
    failed: u64,
}

struct Ran {
    result: Ended,
    counts: Counts,
    detail: String,
}

pub fn run(config: MigrateConfig) -> Result<Outcome, Error> {
    let plan = plan::load(&config.plan)?;
    let accounts = chosen(&plan, &config.only)?;
    if config.parallel == 0 {
        return Err(Error::Usage("--parallel must be at least 1".to_owned()));
    }
    let work = std::path::absolute(&config.work)?;
    private_dir(&work)?;

    let state_path = work.join("state.json");
    let state = Mutex::new(load_state(&state_path)?);
    let save = |state: &State| -> Result<(), Error> {
        if config.dry_run {
            return Ok(());
        }
        let tmp = work.join("state.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state).map_err(io_err)?)?;
        fs::rename(&tmp, &state_path)?;
        Ok(())
    };

    // Setup runs once, before any account, and a failure stops the run:
    // the accounts need what it makes.
    let mut setup_reports = Vec::new();
    if !plan.setup.is_empty() {
        private_dir(&work.join("setup"))?;
    }
    for step in &plan.setup {
        if lock(&state).setup.get(&step.name) == Some(&Ended::Done) {
            setup_reports.push(earlier(&step.name));
            continue;
        }
        let log = work.join("setup").join(format!("{}.log", step.name));
        let ran = run_step(&config, &plan, step, None, &log);
        setup_reports.push(report_of(&step.name, &ran));
        if !config.dry_run {
            lock(&state).setup.insert(step.name.clone(), ran.result);
        }
        save(&lock(&state))?;
        if ran.result == Ended::Failed {
            write_report(&work, &config, &setup_reports, &[])?;
            return Err(Error::Partial(format!(
                "setup step {} failed: {}; see {}",
                step.name,
                ran.detail,
                log.display()
            )));
        }
    }

    let next = AtomicUsize::new(0);
    let finished = AtomicUsize::new(0);
    let reports: Mutex<Vec<(String, Vec<StepReport>)>> = Mutex::new(Vec::new());
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let total = accounts.len();
    std::thread::scope(|scope| {
        for _ in 0..config.parallel.min(total) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(email) = accounts.get(i) else { break };
                    match migrate_account(&config, &plan, &work, email, &state, &save) {
                        Ok(steps) => {
                            let n = finished.fetch_add(1, Ordering::SeqCst) + 1;
                            if config.logger.enabled(crate::logging::LEVEL_DEFAULT) {
                                eprintln!("[{n}/{total}] {email}: {}", line_of(&steps));
                            }
                            lock(&reports).push((email.clone(), steps));
                        }
                        Err(e) => {
                            // The state can't be saved: stop handing out
                            // accounts, so nothing runs unrecorded.
                            next.store(total, Ordering::SeqCst);
                            lock(&failure).get_or_insert(e);
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = lock(&failure).take() {
        return Err(e);
    }

    let mut reports = reports.into_inner().unwrap_or_else(|e| e.into_inner());
    reports.sort_by_key(|(email, _)| accounts.iter().position(|a| a == email));
    let unfinished = reports
        .iter()
        .filter(|(_, steps)| {
            steps
                .iter()
                .any(|s| s.result != "done" && s.result != "done earlier")
        })
        .count();
    let report = write_report(&work, &config, &setup_reports, &reports)?;
    Ok(Outcome {
        done: reports.len() - unfinished,
        unfinished,
        report,
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn io_err(e: serde_json::Error) -> Error {
    Error::Io(std::io::Error::other(e))
}

fn chosen(plan: &Plan, only: &[String]) -> Result<Vec<String>, Error> {
    if only.is_empty() {
        return Ok(plan.accounts.clone());
    }
    let mut out = Vec::new();
    for email in only {
        let email = email.trim().to_ascii_lowercase();
        if !plan.accounts.contains(&email) {
            return Err(Error::Usage(format!(
                "--account {email} is not in the plan"
            )));
        }
        if !out.contains(&email) {
            out.push(email);
        }
    }
    Ok(out)
}

/// The work directory holds whole mailboxes: readable by its owner only.
pub(crate) fn private_dir(path: &Path) -> Result<(), Error> {
    if path.is_dir() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}

fn load_state(path: &Path) -> Result<State, Error> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(State {
                format: STATE_FORMAT.to_owned(),
                format_version: 1,
                ..State::default()
            });
        }
        Err(e) => return Err(e.into()),
    };
    let state: State = serde_json::from_str(&text)
        .map_err(|e| Error::Usage(format!("{}: {e}", path.display())))?;
    if state.format != STATE_FORMAT || state.format_version != 1 {
        return Err(Error::Usage(format!(
            "{} is not a migrate state file this release reads",
            path.display()
        )));
    }
    Ok(state)
}

/// One account's steps. Done steps at the front are skipped; from the first
/// that isn't done, every step runs, so an import that picked up more is
/// always followed by the export that delivers it. A step that fails stops
/// the account; a partial one doesn't.
fn migrate_account(
    config: &MigrateConfig,
    plan: &Plan,
    work: &Path,
    email: &str,
    state: &Mutex<State>,
    save: &(dyn Fn(&State) -> Result<(), Error> + Sync),
) -> Result<Vec<StepReport>, Error> {
    let dir = work.join(email);
    private_dir(&dir)?;
    let archive = dir.join("archive.sqlite");
    let mut reports = Vec::new();
    let mut running = config.again;
    let mut stopped = false;
    for step in &plan.steps {
        if stopped {
            reports.push(StepReport {
                step: step.name.clone(),
                result: "not run",
                counts: Counts::default(),
                detail: "an earlier step failed".to_owned(),
            });
            continue;
        }
        let was = lock(state)
            .accounts
            .get(email)
            .and_then(|steps| steps.get(&step.name))
            .copied();
        if !running && was == Some(Ended::Done) {
            reports.push(earlier(&step.name));
            continue;
        }
        running = true;
        let log = dir.join(format!("{}.log", step.name));
        let ran = run_step(config, plan, step, Some((email, &archive)), &log);
        reports.push(report_of(&step.name, &ran));
        if !config.dry_run {
            let mut s = lock(state);
            s.accounts
                .entry(email.to_owned())
                .or_default()
                .insert(step.name.clone(), ran.result);
            save(&s)?;
        }
        stopped = ran.result == Ended::Failed;
    }
    Ok(reports)
}

fn earlier(step: &str) -> StepReport {
    StepReport {
        step: step.to_owned(),
        result: "done earlier",
        counts: Counts::default(),
        detail: String::new(),
    }
}

fn report_of(step: &str, ran: &Ran) -> StepReport {
    StepReport {
        step: step.to_owned(),
        result: match ran.result {
            Ended::Done => "done",
            Ended::Partial => "partial",
            Ended::Failed => "failed",
        },
        counts: ran.counts,
        detail: ran.detail.clone(),
    }
}

fn line_of(steps: &[StepReport]) -> String {
    match steps
        .iter()
        .find(|s| s.result == "failed" || s.result == "partial")
    {
        Some(s) if s.detail.is_empty() => format!("{} at {}", s.result, s.step),
        Some(s) => format!("{} at {}: {}", s.result, s.step, s.detail),
        None => "done".to_owned(),
    }
}

/// Runs one step as a child: no terminal, so a missing secret fails
/// rather than waits; only the secrets the step names; stderr to the log,
/// appended so a resumed run keeps the history.
fn run_step(
    config: &MigrateConfig,
    plan: &Plan,
    step: &Step,
    account: Option<(&str, &Path)>,
    log: &Path,
) -> Ran {
    let failed = |detail: String| Ran {
        result: Ended::Failed,
        counts: Counts::default(),
        detail,
    };
    let mut args = step.args_for(account);
    if config.dry_run {
        args.push("--dry-run".to_owned());
    }
    let (mut file, start) = match open_log(log) {
        Ok(opened) => opened,
        Err(e) => return failed(format!("can't write {}: {e}", log.display())),
    };
    // The args hold no secrets; the plan refuses them.
    let _ = writeln!(file, "== inbuxa-migrate {}", args.join(" "));
    let stderr = match file.try_clone() {
        Ok(f) => f,
        Err(e) => return failed(format!("can't write {}: {e}", log.display())),
    };
    let mut command = Command::new(&config.program);
    command
        .args(&args)
        .current_dir(&plan.base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    for var in SECRET_VARS {
        command.env_remove(var);
    }
    for (var, value) in &step.env {
        command.env(var, value);
    }
    let output = match command.output() {
        Ok(output) => output,
        Err(e) => return failed(format!("can't run {}: {e}", config.program.display())),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let _ = file.write_all(stdout.as_bytes());
    let counts = counts_of(&stdout);
    let result = match output.status.code() {
        Some(0) => Ended::Done,
        Some(5) => Ended::Partial,
        _ => Ended::Failed,
    };
    let detail = match result {
        Ended::Done => String::new(),
        _ => last_error(log, start).unwrap_or_else(|| match output.status.code() {
            Some(code) => format!("exit code {code}"),
            None => "stopped by a signal".to_owned(),
        }),
    };
    Ran {
        result,
        counts,
        detail,
    }
}

fn open_log(path: &Path) -> std::io::Result<(File, u64)> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    let start = file.metadata()?.len();
    Ok((file, start))
}

/// The last `error:` line this run wrote to the log.
fn last_error(log: &Path, start: u64) -> Option<String> {
    let mut file = File::open(log).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.lines()
        .rev()
        .find_map(|l| l.strip_prefix("error: "))
        .map(str::to_owned)
}

/// The per-type lines a step prints, summed:
/// `email: created=1 fetched=2 updated=0 deleted=0 skipped=0 failed=0`.
fn counts_of(stdout: &str) -> Counts {
    let mut c = Counts::default();
    for line in stdout.lines() {
        let Some((_, rest)) = line.split_once(": ") else {
            continue;
        };
        for field in rest.split_whitespace() {
            let Some((key, n)) = field.split_once('=') else {
                continue;
            };
            let Ok(n) = n.parse::<u64>() else { continue };
            match key {
                "fetched" => c.fetched += n,
                "created" => c.created += n,
                "updated" => c.updated += n,
                "deleted" => c.deleted += n,
                "skipped" => c.skipped += n,
                "failed" => c.failed += n,
                _ => {}
            }
        }
    }
    c
}

/// `report.csv` in the work directory (`dry-run-report.csv` for a dry
/// run): one row per step per account, setup first.
fn write_report(
    work: &Path,
    config: &MigrateConfig,
    setup: &[StepReport],
    accounts: &[(String, Vec<StepReport>)],
) -> Result<PathBuf, Error> {
    let path = work.join(if config.dry_run {
        "dry-run-report.csv"
    } else {
        "report.csv"
    });
    let mut out =
        String::from("account,step,result,fetched,created,updated,deleted,skipped,failed,detail\n");
    let rows = setup.iter().map(|s| ("(setup)", s)).chain(
        accounts
            .iter()
            .flat_map(|(email, steps)| steps.iter().map(move |s| (email.as_str(), s))),
    );
    for (account, s) in rows {
        let c = s.counts;
        out.push_str(&format!(
            "{account},{},{},{},{},{},{},{},{},{}\n",
            s.step,
            s.result,
            c.fetched,
            c.created,
            c.updated,
            c.deleted,
            c.skipped,
            c.failed,
            csv_field(&s.detail)
        ));
    }
    fs::write(&path, out)?;
    Ok(path)
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_summed_over_the_types() {
        let c = counts_of(
            "email: created=3 fetched=4 updated=1 deleted=0 skipped=2 failed=1\n\
             mailbox: created=2 fetched=2 updated=0 deleted=0 skipped=0 failed=0\n",
        );
        assert_eq!((c.fetched, c.created, c.updated), (6, 5, 1));
        assert_eq!((c.skipped, c.failed), (2, 1));
    }

    #[test]
    fn details_are_quoted_when_they_need_it() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a, \"b\""), "\"a, \"\"b\"\"\"");
    }
}
