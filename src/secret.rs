/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{BufRead, IsTerminal, Write};
use std::process::Command;

use crate::error::Error;

/// The variables an API key or bearer token is read from, first match wins.
/// `INBUXA_MIGRATE_TOKEN` is the older name, still read.
pub const API_KEY_VARS: &[&str] = &["INBUXA_MIGRATE_API_KEY", "INBUXA_MIGRATE_TOKEN"];

pub fn resolve(
    flag_value: Option<&str>,
    env_var: &str,
    prompt_label: &str,
) -> Result<String, Error> {
    resolve_any(flag_value, &[env_var], prompt_label)
}

pub fn resolve_any(
    flag_value: Option<&str>,
    env_vars: &[&str],
    prompt_label: &str,
) -> Result<String, Error> {
    if let Some(value) = flag_value {
        return Ok(value.to_owned());
    }
    if let Some(value) = from_env(env_vars)? {
        return Ok(value);
    }
    if std::io::stdin().is_terminal() {
        return prompt_no_echo(prompt_label);
    }
    Err(Error::Usage(format!(
        "no {prompt_label} provided: pass the flag, set ${0} or ${0}_FILE, or run on a TTY for an interactive prompt",
        env_vars[0]
    )))
}

/// The first secret found among `env_vars`, in order. For each name `V` the
/// secret is read from `$V` directly, or from the file named by `${V}_FILE`:
/// the Docker, Compose and systemd convention, so a secret can be kept in a
/// file (a mounted secret, a tmpfs path) instead of the environment or the
/// command line. Setting both `$V` and `${V}_FILE` for the same name is a
/// mistake and is rejected. A file's single trailing newline is stripped;
/// any other whitespace is kept, as a secret may contain it.
pub fn from_env(env_vars: &[&str]) -> Result<Option<String>, Error> {
    for var in env_vars {
        let direct = std::env::var(var).ok().filter(|v| !v.is_empty());
        let file_var = format!("{var}_FILE");
        let file = std::env::var(&file_var).ok().filter(|v| !v.is_empty());
        match (direct, file) {
            (Some(_), Some(_)) => {
                return Err(Error::Usage(format!(
                    "set either ${var} or ${file_var}, not both"
                )));
            }
            (Some(value), None) => return Ok(Some(value)),
            (None, Some(path)) => return Ok(Some(read_secret_file(&path, &file_var)?)),
            (None, None) => {}
        }
    }
    Ok(None)
}

/// Read a secret from the file `${var}` points at, stripping one trailing
/// newline (CRLF or LF). An unreadable or empty file is a usage error.
fn read_secret_file(path: &str, var: &str) -> Result<String, Error> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Usage(format!("${var}: can't read secret file {path}: {e}")))?;
    let secret = raw
        .strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(&raw);
    if secret.is_empty() {
        return Err(Error::Usage(format!("${var}: secret file {path} is empty")));
    }
    Ok(secret.to_owned())
}

fn prompt_no_echo(label: &str) -> Result<String, Error> {
    let mut stderr = std::io::stderr();
    write!(stderr, "{label}: ")?;
    stderr.flush()?;

    let echo_off = set_echo(false);
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if echo_off {
        let _ = set_echo(true);
    }
    writeln!(stderr)?;
    stderr.flush()?;
    read?;

    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Err(Error::Usage(format!("empty {label}")));
    }
    Ok(trimmed.to_owned())
}

fn set_echo(on: bool) -> bool {
    let arg = if on { "echo" } else { "-echo" };
    Command::new("stty")
        .arg(arg)
        .stdin(std::process::Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(contents: &[u8]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "inbuxa-migrate-secret-{}-{n}.tmp",
            std::process::id()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        path
    }

    #[test]
    fn secret_file_strips_one_trailing_newline() {
        let p = temp_file(b"s3cret\n");
        assert_eq!(
            read_secret_file(p.to_str().unwrap(), "X_FILE").unwrap(),
            "s3cret"
        );
        std::fs::remove_file(&p).ok();

        let p = temp_file(b"s3cret\r\n");
        assert_eq!(
            read_secret_file(p.to_str().unwrap(), "X_FILE").unwrap(),
            "s3cret"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn secret_file_keeps_inner_and_leading_whitespace_and_no_trailing_newline() {
        // No trailing newline: taken verbatim.
        let p = temp_file(b"a b c");
        assert_eq!(
            read_secret_file(p.to_str().unwrap(), "X_FILE").unwrap(),
            "a b c"
        );
        std::fs::remove_file(&p).ok();

        // Only the final newline goes; a blank line before it stays.
        let p = temp_file(b" lead\n\n");
        assert_eq!(
            read_secret_file(p.to_str().unwrap(), "X_FILE").unwrap(),
            " lead\n"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn secret_file_empty_or_just_a_newline_is_an_error() {
        let p = temp_file(b"");
        assert!(read_secret_file(p.to_str().unwrap(), "X_FILE").is_err());
        std::fs::remove_file(&p).ok();

        let p = temp_file(b"\n");
        assert!(read_secret_file(p.to_str().unwrap(), "X_FILE").is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn secret_file_missing_is_an_error_naming_the_var() {
        let err = read_secret_file("/no/such/secret/file", "MY_SECRET_FILE").unwrap_err();
        assert!(format!("{err}").contains("MY_SECRET_FILE"));
    }

    #[test]
    fn from_env_reads_direct_then_file_then_both_is_rejected() {
        // Unique names so this test does not race other env-touching tests.
        let direct = "INBUXA_MIGRATE_TEST_SECRET_DIRECT";
        let base = "INBUXA_MIGRATE_TEST_SECRET";
        let file_var = "INBUXA_MIGRATE_TEST_SECRET_FILE";
        unsafe {
            std::env::remove_var(direct);
            std::env::remove_var(base);
            std::env::remove_var(file_var);
        }

        // Nothing set.
        assert_eq!(from_env(&[base]).unwrap(), None);

        // Direct value.
        unsafe { std::env::set_var(base, "plain") };
        assert_eq!(from_env(&[base]).unwrap().as_deref(), Some("plain"));

        // File value (direct cleared).
        unsafe { std::env::remove_var(base) };
        let p = temp_file(b"from-file\n");
        unsafe { std::env::set_var(file_var, p.to_str().unwrap()) };
        assert_eq!(from_env(&[base]).unwrap().as_deref(), Some("from-file"));

        // Both set: rejected.
        unsafe { std::env::set_var(base, "plain") };
        assert!(from_env(&[base]).is_err());

        unsafe {
            std::env::remove_var(base);
            std::env::remove_var(file_var);
        }
        std::fs::remove_file(&p).ok();
    }
}
