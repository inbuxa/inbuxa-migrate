/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! The shared `~/.config/inbuxa` profile. A `config.toml` names an inbuxa
//! server once, so this and `inbuxa-cli` read the same server address and API
//! key instead of each being told again. For `inbuxa-migrate` it supplies the
//! inbuxa server the `import` and `import-directory` commands write to:
//!
//! ```toml
//! [profile.default]
//! url = "https://mail.example.org"
//! api-key = "inbuxa_pat_…"
//!
//! [profile.staging]
//! url = "https://staging.example.org"
//! api-key-file = "~/.secrets/staging.key"
//! ```
//!
//! `INBUXA_PROFILE` or `--profile` picks a section; without one the `default`
//! section is used if it is there. A flag or an `INBUXA_MIGRATE_*` variable
//! always wins, so the file is only a fallback. The format is the same one
//! `inbuxa-cli` reads.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// What a profile can supply: the pieces a flag or variable didn't.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub url: Option<String>,
    pub api_key: Option<String>,
}

impl Profile {
    /// The active profile. `flag` is `--profile` if it was given; otherwise
    /// `$INBUXA_PROFILE` is consulted. When either names a profile, a missing
    /// file or section is an error; with neither, the `default` section is
    /// read if the file has one, and otherwise an empty profile is returned.
    pub fn load(flag: Option<&str>) -> Result<Profile, Error> {
        let from_env = std::env::var("INBUXA_PROFILE")
            .ok()
            .filter(|v| !v.is_empty());
        let explicit = flag.is_some() || from_env.is_some();
        let name = flag.or(from_env.as_deref()).unwrap_or("default");
        let Some(path) = config_path() else {
            return Ok(Profile::default());
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return if explicit {
                    Err(Error::Usage(format!(
                        "no profile file at {}",
                        path.display()
                    )))
                } else {
                    Ok(Profile::default())
                };
            }
            Err(e) => return Err(Error::Usage(format!("{}: {e}", path.display()))),
        };
        from_toml(&text, name, explicit, &path, home_dir().as_deref())
    }
}

/// Parses one section out of the file's text. Split out from [`Profile::load`]
/// so it can be tested without a real config directory; `home` resolves a
/// leading `~/` in an `api-key-file`.
fn from_toml(
    text: &str,
    name: &str,
    explicit: bool,
    path: &Path,
    home: Option<&Path>,
) -> Result<Profile, Error> {
    let at = |e: String| Error::Usage(format!("{}: {e}", path.display()));
    let doc: toml::Table = toml::from_str(text).map_err(|e| at(e.to_string()))?;
    let section = doc
        .get("profile")
        .and_then(|p| p.as_table())
        .and_then(|t| t.get(name));
    let Some(section) = section else {
        return if explicit {
            Err(at(format!("no [profile.{name}]")))
        } else {
            Ok(Profile::default())
        };
    };
    let field = |key: &str| -> Result<Option<String>, Error> {
        match section.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| Some(s.to_owned()))
                .ok_or_else(|| at(format!("profile.{name}.{key} must be a non-empty string"))),
        }
    };
    let url = field("url")?;
    let api_key = match (field("api-key")?, field("api-key-file")?) {
        (Some(_), Some(_)) => {
            return Err(at(format!(
                "profile.{name} sets both api-key and api-key-file; use one"
            )));
        }
        (Some(key), None) => Some(key),
        (None, Some(file)) => Some(read_key_file(&file, home, path, name)?),
        (None, None) => None,
    };
    Ok(Profile { url, api_key })
}

/// Reads an `api-key-file`: a `~/` is expanded against `home`, and one
/// trailing newline is dropped so an edited file still works.
fn read_key_file(
    spec: &str,
    home: Option<&Path>,
    path: &Path,
    name: &str,
) -> Result<String, Error> {
    let file = expand_tilde(spec, home);
    let raw = fs::read_to_string(&file).map_err(|e| {
        Error::Usage(format!(
            "{}: profile.{name}.api-key-file {}: {e}",
            path.display(),
            file.display()
        ))
    })?;
    let key = raw
        .strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(&raw);
    if key.is_empty() {
        return Err(Error::Usage(format!(
            "{}: profile.{name}.api-key-file {} is empty",
            path.display(),
            file.display()
        )));
    }
    Ok(key.to_owned())
}

fn expand_tilde(spec: &str, home: Option<&Path>) -> PathBuf {
    match (spec.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(spec),
    }
}

/// `~/.config/inbuxa/config.toml`, honoring `XDG_CONFIG_HOME`.
fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => home_dir()?.join(".config"),
    };
    Some(base.join("inbuxa").join("config.toml"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("/home/me/.config/inbuxa/config.toml")
    }

    #[test]
    fn reads_url_and_inline_key_from_the_named_section() {
        let text = "\
[profile.default]
url = \"https://mail.example.org\"
api-key = \"secret-default\"

[profile.staging]
url = \"https://staging.example.org\"
api-key = \"secret-staging\"
";
        let default = from_toml(text, "default", false, &path(), None).unwrap();
        assert_eq!(default.url.as_deref(), Some("https://mail.example.org"));
        assert_eq!(default.api_key.as_deref(), Some("secret-default"));
        let staging = from_toml(text, "staging", true, &path(), None).unwrap();
        assert_eq!(staging.api_key.as_deref(), Some("secret-staging"));
    }

    #[test]
    fn a_missing_default_is_empty_but_a_missing_named_one_is_an_error() {
        let text = "[profile.work]\nurl = \"https://work.example.org\"\n";
        assert_eq!(
            from_toml(text, "default", false, &path(), None).unwrap(),
            Profile::default()
        );
        let err = from_toml(text, "other", true, &path(), None).unwrap_err();
        assert!(err.to_string().contains("no [profile.other]"), "{err}");
    }

    #[test]
    fn both_api_key_forms_set_is_an_error() {
        let text = "[profile.default]\napi-key = \"inline\"\napi-key-file = \"/tmp/x\"\n";
        let err = from_toml(text, "default", false, &path(), None).unwrap_err();
        assert!(
            err.to_string().contains("both api-key and api-key-file"),
            "{err}"
        );
    }

    #[test]
    fn a_non_string_field_is_an_error() {
        let text = "[profile.default]\nurl = 42\n";
        let err = from_toml(text, "default", false, &path(), None).unwrap_err();
        assert!(
            err.to_string().contains("url must be a non-empty string"),
            "{err}"
        );
    }

    #[test]
    fn api_key_file_is_read_tilde_expanded_with_one_trailing_newline_dropped() {
        let dir =
            std::env::temp_dir().join(format!("inbuxa-migrate-profile-{}", std::process::id()));
        fs::create_dir_all(dir.join(".secrets")).unwrap();
        fs::write(dir.join(".secrets/k.key"), "file-key\n").unwrap();
        let text = "[profile.default]\napi-key-file = \"~/.secrets/k.key\"\n";
        let p = from_toml(text, "default", false, &path(), Some(&dir)).unwrap();
        assert_eq!(p.api_key.as_deref(), Some("file-key"));

        fs::write(dir.join(".secrets/empty.key"), "\n").unwrap();
        let empty = "[profile.default]\napi-key-file = \"~/.secrets/empty.key\"\n";
        let err = from_toml(empty, "default", false, &path(), Some(&dir)).unwrap_err();
        assert!(err.to_string().contains("is empty"), "{err}");
        fs::remove_dir_all(&dir).ok();
    }
}
