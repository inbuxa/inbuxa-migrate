/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `guide`: the walk-through. A page served from this binary on
//! 127.0.0.1 asks where the mail is, which accounts, and which inbuxa
//! server, then writes a plan into its directory and has `migrate` preview
//! and run it, so it moves exactly what the same plan would from the
//! command line. Only the browser given the address, with its random
//! token, gets an answer, and secrets stay in memory: the runs get them
//! the way a plan's steps do, through variables.

pub mod form;
mod http;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::Error;
use crate::jmap::http::{Auth, HttpClient, RetryPolicy};
use crate::jmap::request::URN_INBUXA;
use crate::jmap::session::Session;
use crate::migrate::plan::SECRET_VARS;
use crate::migrate::private_dir;
use crate::net::CertOverride;
use form::{Form, SourceKind};
use http::{Request, Response};

const PAGE: &str = include_str!("page/index.html");
const SCRIPT: &str = include_str!("page/guide.js");
const STYLE: &str = include_str!("page/guide.css");

/// The non-secret answers, so a guide started again picks up where it was.
const SAVED: &str = "guide.json";
/// The output kept from a run, newest last.
const KEEP_LINES: usize = 400;

pub struct GuideConfig {
    /// Where the plan, the archives and the report go.
    pub dir: PathBuf,
    /// 0 for any free port.
    pub port: u16,
    pub open: bool,
    /// This binary, which runs each step.
    pub program: PathBuf,
}

/// `plan`: the plans the guide would write, from its answers in a file.
pub struct PlanConfig {
    pub answers: PathBuf,
    /// What `export-directory` read from the source, if it was read.
    pub source_directory: Option<PathBuf>,
    pub dir: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Directory,
    Preview,
    Run,
    FinalPass,
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "directory" => Self::Directory,
            "preview" => Self::Preview,
            "run" => Self::Run,
            "final" => Self::FinalPass,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Preview => "preview",
            Self::Run => "run",
            Self::FinalPass => "final",
        }
    }
}

struct Job {
    kind: Kind,
    /// Where a directory run writes.
    out: Option<PathBuf>,
    lines: Vec<String>,
    exit: Option<i32>,
    result: Value,
}

#[derive(Default)]
struct State {
    form: Form,
    /// The source's directory, once read.
    directory: Option<PathBuf>,
    job: Option<Job>,
}

struct Guide {
    dir: PathBuf,
    token: String,
    port: u16,
    program: PathBuf,
    state: Mutex<State>,
}

pub fn run(config: GuideConfig) -> Result<(), Error> {
    let dir = std::path::absolute(&config.dir)?;
    private_dir(&dir)?;
    let listener = TcpListener::bind(("127.0.0.1", config.port))?;
    let port = listener.local_addr()?.port();
    let mut token = [0u8; 16];
    getrandom::fill(&mut token)
        .map_err(|e| Error::Io(std::io::Error::other(format!("no random token: {e}"))))?;
    let token: String = token.iter().map(|b| format!("{b:02x}")).collect();

    let mut state = State::default();
    if let Ok(text) = fs::read_to_string(dir.join(SAVED))
        && let Ok(saved) = serde_json::from_str::<Value>(&text)
    {
        state.form = serde_json::from_value(saved["form"].clone()).unwrap_or_default();
        state.directory = saved["directory"].as_str().map(PathBuf::from);
    }
    let guide = Arc::new(Guide {
        dir,
        token,
        port,
        program: config.program,
        state: Mutex::new(state),
    });

    let url = format!("http://127.0.0.1:{port}/{}/", guide.token);
    // Said, not needed: with stdout closed the guide still serves.
    let say = |line: &str| {
        let _ = writeln!(std::io::stdout(), "{line}");
    };
    say(&format!("The guide is at {url}"));
    say("Keep this running while you use it. Ctrl-C ends it.");
    if config.open && !open_browser(&url) {
        say("Open that address in a browser on this machine.");
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let guide = Arc::clone(&guide);
        std::thread::spawn(move || guide.serve(stream));
    }
    Ok(())
}

/// `plan`: writes the plan, the preview's plan and the directory the setup
/// reads, then says on stdout, as JSON, which files to run. The answers
/// carry no secrets; the runs get them through the variables the plans
/// name.
pub fn write_plans(config: PlanConfig) -> Result<(), Error> {
    let usage = |e: String| Error::Usage(format!("{}: {e}", config.answers.display()));
    let text = fs::read_to_string(&config.answers).map_err(|e| usage(e.to_string()))?;
    let f: Form = serde_json::from_str(&text).map_err(|e| usage(e.to_string()))?;
    if !f.source.secret.is_empty() || !f.target.token.is_empty() {
        return Err(usage(format!(
            "the answers hold a secret; leave it out, and give it to migrate in \
             ${} or ${}",
            form::SOURCE_SECRET_VAR,
            form::TARGET_TOKEN_VAR
        )));
    }
    f.check_answers().map_err(usage)?;
    let dir = std::path::absolute(&config.dir)?;
    private_dir(&dir)?;
    let written =
        write_plan_files(&dir, &f, config.source_directory.as_deref()).map_err(Error::Usage)?;
    let _ = writeln!(
        std::io::stdout(),
        "{}",
        json!({
            "plan": written.plan,
            "preview": written.preview,
            "passwords": written.passwords,
            "accounts": f.accounts.len(),
            "secrets": {
                "source": f.source.has_secret().then_some(form::SOURCE_SECRET_VAR),
                "target": form::TARGET_TOKEN_VAR,
            },
        })
    );
    Ok(())
}

/// What [`write_plan_files`] wrote, relative to the directory.
struct Written {
    plan: &'static str,
    preview: &'static str,
    /// Where the move's setup puts new passwords, if it sets up accounts.
    passwords: Option<String>,
}

/// The plan, the preview's plan and, when accounts are to be set up, the
/// directory for it: the source's, if `source` is what `export-directory`
/// read from it.
fn write_plan_files(dir: &Path, f: &Form, source: Option<&Path>) -> Result<Written, String> {
    let write = |name: &str, value: &Value| {
        let path = dir.join(name);
        write_private(&path, &serde_json::to_vec_pretty(value).unwrap_or_default())
            .map_err(|e| format!("{} couldn't be written: {e}", path.display()))
    };
    if f.create {
        let source = match source {
            Some(d) => {
                let text = fs::read_to_string(d.join("directory.json"))
                    .map_err(|e| format!("{}: {e}", d.display()))?;
                Some(serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?)
            }
            None => None,
        };
        private_dir(&dir.join(form::DIRECTORY)).map_err(|e| e.to_string())?;
        write(
            &format!("{}/directory.json", form::DIRECTORY),
            &form::directory(source.as_ref(), &f.accounts),
        )?;
    }
    let passwords = format!("passwords-{}.csv", stamp());
    write("preview-plan.json", &f.preview_plan())?;
    write("plan.json", &f.plan(&passwords))?;
    Ok(Written {
        plan: "plan.json",
        preview: "preview-plan.json",
        passwords: f.create.then_some(passwords),
    })
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut child| std::thread::spawn(move || child.wait()))
        .is_ok()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Equal, in time that doesn't depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

impl Guide {
    fn serve(self: Arc<Self>, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let response = match http::read(&mut stream) {
            Ok(request) => self.answer(&request),
            Err(_) => Response::error(400, "bad request"),
        };
        let _ = response.write(&mut stream);
    }

    fn answer(self: &Arc<Self>, r: &Request) -> Response {
        // A page from anywhere else can't reach this, even by a name that
        // resolves here: the Host has to be this address.
        let hosts = [
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
        ];
        if !r
            .header("host")
            .is_some_and(|h| hosts.iter().any(|x| x == h))
        {
            return Response::error(403, "wrong host");
        }
        let Some(rest) = r
            .path
            .strip_prefix('/')
            .and_then(|p| p.split_once('/'))
            .filter(|(token, _)| same(token, &self.token))
            .map(|(_, rest)| rest)
        else {
            return Response::error(404, "not found");
        };
        if r.method == "POST" {
            if !r
                .header("content-type")
                .is_some_and(|t| t.starts_with("application/json"))
            {
                return Response::error(415, "send JSON");
            }
            if let Some(origin) = r.header("origin")
                && !hosts.iter().any(|h| origin == format!("http://{h}"))
            {
                return Response::error(403, "wrong origin");
            }
        }
        let body = || serde_json::from_slice::<Value>(&r.body).unwrap_or(Value::Null);
        match (r.method.as_str(), rest) {
            ("GET", "") => Response::new(200, "text/html; charset=utf-8", PAGE),
            ("GET", "guide.js") => Response::new(200, "text/javascript; charset=utf-8", SCRIPT),
            ("GET", "guide.css") => Response::new(200, "text/css; charset=utf-8", STYLE),
            ("GET", "api/state") => Response::json(&self.state_json()),
            ("POST", "api/accounts") => {
                let (accounts, rejected) =
                    form::parse_accounts(body()["text"].as_str().unwrap_or_default());
                Response::json(&json!({ "accounts": accounts, "rejected": rejected }))
            }
            ("POST", "api/check-source") => match self.take_form(&body()) {
                Ok(f) => Response::json(&self.check_source(&f)),
                Err(e) => Response::error(400, &e),
            },
            ("POST", "api/check-target") => match self.take_form(&body()) {
                Ok(f) => Response::json(&check_target(&f)),
                Err(e) => Response::error(400, &e),
            },
            ("POST", "api/job") => self.start(&body()),
            (_, "" | "guide.js" | "guide.css" | "api/state") => {
                Response::error(405, "method not allowed")
            }
            _ => Response::error(404, "not found"),
        }
    }

    fn state_json(&self) -> Value {
        let state = lock(&self.state);
        let job = state.job.as_ref().map(|j| {
            json!({
                "kind": j.kind.name(), "running": j.exit.is_none(), "exit": j.exit,
                "lines": j.lines, "result": j.result,
            })
        });
        json!({
            "dir": self.dir.display().to_string(),
            "form": state.form,
            "hasSourceSecret": !state.form.source.secret.is_empty(),
            "hasTargetToken": !state.form.target.token.is_empty(),
            "directoryRead": state.directory.is_some(),
            "job": job,
            "command": self.command_line(&state.form),
        })
    }

    /// The same run from a terminal, for the administrator to keep.
    fn command_line(&self, f: &Form) -> String {
        let mut vars: Vec<_> = f.env().into_iter().map(|(var, _)| var).collect();
        vars.sort_unstable();
        let mut text: String = vars.iter().map(|v| format!("export {v}=…\n")).collect();
        text += &format!(
            "inbuxa-migrate migrate --plan {} --parallel {} {}",
            self.dir.join("plan.json").display(),
            f.parallel.max(1),
            self.dir.join("work").display()
        );
        text
    }

    /// The form the page sent, with the secrets it left out kept from the
    /// last one, and saved without them.
    fn take_form(&self, body: &Value) -> Result<Form, String> {
        let mut f: Form = serde_json::from_value(body["form"].clone())
            .map_err(|e| format!("the form didn't read: {e}"))?;
        let mut state = lock(&self.state);
        f.keep_secrets_from(&state.form);
        if f.parallel == 0 {
            f.parallel = 4;
        }
        state.form = f.clone();
        let saved = json!({
            "form": f,
            "directory": state.directory.as_ref().map(|d| d.display().to_string()),
        });
        drop(state);
        write_private(
            &self.dir.join(SAVED),
            &serde_json::to_vec_pretty(&saved).unwrap_or_default(),
        )
        .map_err(|e| format!("{SAVED} couldn't be written: {e}"))?;
        Ok(f)
    }

    fn check_source(&self, f: &Form) -> Value {
        let s = &f.source;
        match s.kind {
            SourceKind::Jmap => {
                let auth = if s.login.trim().is_empty() {
                    Auth::Bearer {
                        token: s.secret.clone(),
                    }
                } else {
                    Auth::Basic {
                        user: s.login.trim().to_owned(),
                        password: s.secret.clone(),
                    }
                };
                match discover(auth, &s.url) {
                    Err(e) => json!({ "ok": false, "message": e }),
                    Ok(session) if session.registry_account().is_none() => json!({
                        "ok": false,
                        "message": "This login reaches only its own account. Sign in as an \
                                    administrator, who can act for every account.",
                    }),
                    Ok(_) => json!({
                        "ok": true, "directory": true,
                        "message": "Signed in as an administrator: the guide can read the \
                                    account list and each account.",
                    }),
                }
            }
            SourceKind::Maildir => {
                let found = f
                    .accounts
                    .iter()
                    .filter(|a| self.dir.join(fill(&s.path, a)).is_dir())
                    .count();
                let total = f.accounts.len();
                json!({
                    "ok": total > 0 && found == total,
                    "message": if total == 0 {
                        "List the accounts, and the guide will look for each one's folder.".to_owned()
                    } else {
                        format!("Found the folder for {found} of {total} accounts.")
                    },
                })
            }
            SourceKind::Imap => json!({
                "ok": true,
                "message": "The preview signs in for each account and shows any that fail.",
            }),
            SourceKind::Microsoft => check_microsoft(f),
            SourceKind::Google => self.check_google(f),
        }
    }

    /// Asks Google for a token for the first account, as each run will.
    fn check_google(&self, f: &Form) -> Value {
        use crate::google::{MAIL_SCOPE, ServiceAccount};
        let path = self.dir.join(f.source.key_file.trim());
        let account = match ServiceAccount::load(&path) {
            Ok(a) => a,
            Err(e) => return json!({ "ok": false, "message": e.to_string() }),
        };
        let Some(first) = f.accounts.first() else {
            return json!({ "ok": false, "message": format!(
                "The key is {}'s. List the accounts, and the guide will try the first.",
                account.client_email()) });
        };
        match account.token_for(first, MAIL_SCOPE) {
            Ok(_) => json!({ "ok": true, "message": format!(
                "Google lets {} act for {first}.", account.client_email()) }),
            Err(e) => json!({ "ok": false, "message": e.to_string() }),
        }
    }

    fn start(self: &Arc<Self>, body: &Value) -> Response {
        let Some(kind) = body["kind"].as_str().and_then(Kind::parse) else {
            return Response::error(400, "no such run");
        };
        if lock(&self.state)
            .job
            .as_ref()
            .is_some_and(|j| j.exit.is_none())
        {
            return Response::error(409, "Something is already running; wait for it to end.");
        }
        let f = match self.take_form(body) {
            Ok(f) => f,
            Err(e) => return Response::error(400, &e),
        };
        let prepared = match kind {
            Kind::Directory => self.directory_run(&f),
            _ => self.plan_run(&f, kind),
        };
        let (args, env, out) = match prepared {
            Ok(p) => p,
            Err(e) => return Response::error(400, &e),
        };
        let mut command = Command::new(&self.program);
        command
            .args(&args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in SECRET_VARS {
            command.env_remove(var);
        }
        command.envs(env);
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => return Response::error(400, &format!("couldn't start: {e}")),
        };
        lock(&self.state).job = Some(Job {
            kind,
            out,
            lines: vec![format!("inbuxa-migrate {}", args.join(" "))],
            exit: None,
            result: Value::Null,
        });
        let readers: Vec<_> = [
            child
                .stdout
                .take()
                .map(|o| Box::new(o) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|e| Box::new(e) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        .map(|out| {
            let me = Arc::clone(self);
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if let Some(job) = lock(&me.state).job.as_mut() {
                        job.lines.push(line);
                        let over = job.lines.len().saturating_sub(KEEP_LINES);
                        job.lines.drain(..over);
                    }
                }
            })
        })
        .collect();
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            for r in readers {
                let _ = r.join();
            }
            let exit = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
            let result = me.result_of(kind, exit);
            if let Some(job) = lock(&me.state).job.as_mut() {
                job.result = result;
                job.exit = Some(exit);
            }
        });
        Response::json(&json!({ "started": kind.name() }))
    }

    /// `export-directory` into a new directory beside the plan.
    fn directory_run(&self, f: &Form) -> Result<Prepared, String> {
        let s = &f.source;
        if s.kind != SourceKind::Jmap || s.url.trim().is_empty() || s.secret.is_empty() {
            return Err(
                "Only a JMAP server, signed in as an administrator, has a list to read.".into(),
            );
        }
        let out = format!("source-directory-{}", stamp());
        let mut args = vec![
            "export-directory".to_owned(),
            "--url".into(),
            s.url.trim().into(),
        ];
        let var = if s.login.trim().is_empty() {
            args.push("--auth-bearer".into());
            "INBUXA_MIGRATE_API_KEY"
        } else {
            args.extend(["--auth-basic".into(), s.login.trim().into()]);
            "INBUXA_MIGRATE_PASSWORD"
        };
        args.push(out.clone());
        Ok((
            args,
            vec![(var, s.secret.clone())],
            Some(self.dir.join(out)),
        ))
    }

    /// Writes the plan, and the directory it sets up, for `migrate`.
    fn plan_run(&self, f: &Form, kind: Kind) -> Result<Prepared, String> {
        f.check()?;
        let source = lock(&self.state).directory.clone();
        let written = write_plan_files(&self.dir, f, source.as_deref())?;
        let plan = if kind == Kind::Preview {
            written.preview
        } else {
            written.plan
        };
        let mut args: Vec<String> = ["migrate", "--plan", plan, "--parallel"]
            .map(str::to_owned)
            .into();
        args.push(f.parallel.max(1).to_string());
        match kind {
            Kind::Preview => args.push("--dry-run".into()),
            Kind::FinalPass => args.push("--again".into()),
            _ => {}
        }
        args.push("work".into());
        Ok((args, f.env(), None))
    }

    /// What the page shows once a run ends.
    fn result_of(&self, kind: Kind, exit: i32) -> Value {
        if kind == Kind::Directory {
            let dir = lock(&self.state).job.as_ref().and_then(|j| j.out.clone());
            let doc = dir.as_ref().and_then(|d| {
                let text = fs::read_to_string(d.join("directory.json")).ok()?;
                serde_json::from_str::<Value>(&text).ok()
            });
            return match (exit, dir, doc) {
                (0, Some(dir), Some(doc)) => {
                    lock(&self.state).directory = Some(dir);
                    json!({ "accounts": form::directory_accounts(&doc) })
                }
                _ => Value::Null,
            };
        }
        let report = if kind == Kind::Preview {
            "dry-run-report.csv"
        } else {
            "report.csv"
        };
        let passwords = if kind == Kind::Preview {
            None
        } else {
            newest_passwords(&self.dir)
        };
        json!({
            "report": fs::read_to_string(self.dir.join("work").join(report)).ok(),
            "reportPath": self.dir.join("work").join(report).display().to_string(),
            "passwords": passwords.map(|p| p.display().to_string()),
        })
    }
}

/// A run's arguments, the secrets it gets, and where a directory run writes.
type Prepared = (Vec<String>, Vec<(&'static str, String)>, Option<PathBuf>);

/// A template with the account's parts filled in.
fn fill(template: &str, email: &str) -> String {
    let (local, domain) = email.split_once('@').unwrap_or((email, ""));
    template
        .replace("{email}", email)
        .replace("{local}", local)
        .replace("{domain}", domain)
}

fn discover(auth: Auth, url: &str) -> Result<Session, String> {
    if url.trim().is_empty() {
        return Err("Give the server's address.".into());
    }
    let client = HttpClient::new(auth, RetryPolicy::new(1), CertOverride::none());
    Session::discover(&client, url.trim()).map_err(|e| format!("Couldn't sign in: {e}"))
}

fn check_target(f: &Form) -> Value {
    let t = &f.target;
    if t.token.is_empty() {
        return json!({ "ok": false, "message": "Give an administrator's API key." });
    }
    let auth = Auth::Bearer {
        token: t.token.clone(),
    };
    match discover(auth, &t.url) {
        Err(e) => json!({ "ok": false, "message": e }),
        Ok(s) if !s.capabilities.contains_key(URN_INBUXA) => json!({
            "ok": false, "message": "That server isn't inbuxa.",
        }),
        Ok(s) => match s.registry_account() {
            None => json!({
                "ok": false,
                "message": "This key can't act for other accounts. Use an administrator's API key.",
            }),
            Some((id, _)) => json!({
                "ok": true,
                "message": format!(
                    "Signed in to inbuxa as {}, an administrator.",
                    s.accounts.get(id).map_or("?", |a| a.name.as_str())
                ),
            }),
        },
    }
}

/// Signs the app in: what it can read shows in its token's roles.
fn check_microsoft(f: &Form) -> Value {
    use crate::exchange_graph::oauth;
    let s = &f.source;
    if s.tenant.trim().is_empty() || s.client_id.trim().is_empty() || s.secret.is_empty() {
        return json!({ "ok": false,
            "message": "Give the tenant, the app's client id and its client secret." });
    }
    let authority = oauth::default_authority(s.tenant.trim());
    match oauth::client_credentials(&authority, s.client_id.trim(), &s.secret) {
        Err(e) => json!({ "ok": false, "message": format!("The app couldn't sign in: {e}") }),
        Ok(token) => {
            let roles = oauth::decode_jwt_claims(&token.access_token)
                .map(|c| c.roles)
                .unwrap_or_default();
            // Each need, and the permissions that cover it.
            let needs: [(&str, &[&str]); 2] = [
                (
                    "User.Read.All",
                    &[
                        "User.Read.All",
                        "User.ReadWrite.All",
                        "Directory.Read.All",
                        "Directory.ReadWrite.All",
                    ],
                ),
                ("Mail.Read", &["Mail.Read", "Mail.ReadWrite"]),
            ];
            let missing: Vec<&str> = needs
                .iter()
                .filter(|(_, any)| !roles.iter().any(|have| any.contains(&have.as_str())))
                .map(|(need, _)| *need)
                .collect();
            if missing.is_empty() {
                json!({ "ok": true, "message": format!(
                    "The app signed in to {} and may read: {}.", s.tenant.trim(), roles.join(", ")) })
            } else {
                json!({ "ok": false, "message": format!(
                    "The app signed in, but it hasn't been granted {}. Add them as application \
                     permissions and grant admin consent.", missing.join(" and ")) })
            }
        }
    }
}

/// The passwords file the last setup wrote, if it wrote one.
fn newest_passwords(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("passwords-") && n.ends_with(".csv"))
        })
        .max()
}

fn stamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    std::io::Write::write_all(&mut options.open(path)?, bytes)
}
