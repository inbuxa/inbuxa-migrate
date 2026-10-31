/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `manifest.json`, as <https://coffeylabs.org/specs/account-export/>
//! defines it. A part that is missing wasn't exported; a part that is there
//! and empty says the account had none. Members this release doesn't know
//! are ignored, so a later writer can add them.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::error::Error;
use crate::export_files::{FORMAT, FORMAT_VERSION};

/// What inbuxa-migrate 27.1.0-rc.2 wrote as the format before it was named
/// by the specification's address. The specification lets readers accept it.
pub const LEGACY_FORMAT: &str = "inbuxa-migrate-files";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub sources: Vec<Source>,
    #[serde(default)]
    pub mail: Option<Mail>,
    #[serde(default)]
    pub contacts: Option<Vec<Collection>>,
    #[serde(default)]
    pub calendars: Option<Vec<Collection>>,
    #[serde(default)]
    pub sieve: Option<Vec<Script>>,
    #[serde(default)]
    pub files: Option<Files>,
    #[serde(default)]
    pub identities: Option<Vec<Identity>>,
    #[serde(default)]
    pub participant_identities: Option<Vec<ParticipantIdentity>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub account_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mail {
    pub layout: String,
    pub path: String,
    #[serde(default)]
    pub folders: Vec<Folder>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub name: String,
    #[serde(default)]
    pub path: Vec<String>,
    pub dir: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default = "yes")]
    pub subscribed: bool,
    #[serde(default)]
    pub messages: Option<u64>,
    #[serde(default)]
    pub keywords_dropped: Option<u64>,
}

/// An address book or a calendar: the members both have, and those only a
/// calendar has.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub file: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub cards: Option<u64>,
    #[serde(default)]
    pub events: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Script {
    pub name: String,
    pub file: String,
    #[serde(default)]
    pub active: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Files {
    pub path: String,
    #[serde(default)]
    pub media_types: BTreeMap<String, String>,
    #[serde(default)]
    pub links: Vec<Link>,
}

#[derive(Debug, Deserialize)]
pub struct Link {
    pub path: String,
    #[serde(default)]
    pub target: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    #[serde(default)]
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub reply_to: Value,
    #[serde(default)]
    pub bcc: Value,
    #[serde(default)]
    pub text_signature: String,
    #[serde(default)]
    pub html_signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParticipantIdentity {
    #[serde(default)]
    pub name: String,
    pub calendar_address: String,
    #[serde(default)]
    pub default: bool,
}

fn yes() -> bool {
    true
}

/// The manifest of the export at `root`. A tree without one is unfinished
/// and is refused, and so is a format or version this release doesn't read.
pub fn read(root: &Path) -> Result<Manifest, Error> {
    let path = root.join("manifest.json");
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(Error::Usage(format!(
                "{}: manifest.json is a symbolic link; an export's files are never links",
                root.display()
            )));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(Error::Usage(format!(
                "{}: manifest.json is not a file",
                root.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Usage(format!(
                "{} has no manifest.json: either the export was interrupted (a writer \
                 adds the manifest last) or this is not an account export",
                root.display()
            )));
        }
        Err(e) => return Err(Error::Io(e)),
    }
    let text = fs::read_to_string(&path)?;
    parse(&text)
}

pub fn parse(text: &str) -> Result<Manifest, Error> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| Error::Usage(format!("manifest.json is not valid JSON: {e}")))?;
    let Value::Object(map) = &value else {
        return Err(Error::Usage(
            "manifest.json is not a JSON object".to_owned(),
        ));
    };
    match map.get("format").and_then(Value::as_str) {
        Some(f) if f == FORMAT || f == LEGACY_FORMAT => {}
        Some(other) => {
            return Err(Error::Usage(format!(
                "manifest.json names the format {other:?}, not the account export layout \
                 ({FORMAT})"
            )));
        }
        None => {
            return Err(Error::Usage(
                "manifest.json has no format; this is not an account export".to_owned(),
            ));
        }
    }
    match map.get("formatVersion").and_then(Value::as_u64) {
        Some(v) if v == u64::from(FORMAT_VERSION) => {}
        Some(v) if v > u64::from(FORMAT_VERSION) => {
            return Err(Error::Usage(format!(
                "this export follows version {v} of the layout, and this release reads \
                 version {FORMAT_VERSION}; use a newer inbuxa-migrate"
            )));
        }
        _ => {
            return Err(Error::Usage(
                "manifest.json has no usable formatVersion".to_owned(),
            ));
        }
    }
    let manifest: Manifest =
        serde_json::from_value(value).map_err(|e| Error::Usage(format!("manifest.json: {e}")))?;
    if let Some(mail) = &manifest.mail
        && !mail.layout.eq_ignore_ascii_case("maildir++")
    {
        return Err(Error::Usage(format!(
            "manifest.json: mail layout {:?} is not one this release reads (maildir++)",
            mail.layout
        )));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str) -> String {
        parse(text).unwrap_err().to_string()
    }

    #[test]
    fn the_format_is_the_spec_address_or_the_old_name() {
        let m = parse(&format!(r#"{{"format":"{FORMAT}","formatVersion":1}}"#)).unwrap();
        assert!(m.mail.is_none() && m.contacts.is_none(), "nothing exported");
        parse(r#"{"format":"inbuxa-migrate-files","formatVersion":1}"#).unwrap();
        assert!(
            err(r#"{"format":"something-else","formatVersion":1}"#).contains("not the account")
        );
        assert!(err(r#"{"formatVersion":1}"#).contains("no format"));
    }

    #[test]
    fn a_later_version_is_refused_rather_than_guessed_at() {
        let e = err(&format!(r#"{{"format":"{FORMAT}","formatVersion":2}}"#));
        assert!(e.contains("version 2") && e.contains("newer"), "{e}");
        assert!(
            err(&format!(r#"{{"format":"{FORMAT}","formatVersion":"1"}}"#))
                .contains("formatVersion")
        );
        assert!(err(&format!(r#"{{"format":"{FORMAT}"}}"#)).contains("formatVersion"));
    }

    #[test]
    fn unknown_members_are_ignored_and_empty_parts_kept_apart_from_missing_ones() {
        let m = parse(&format!(
            r#"{{"format":"{FORMAT}","formatVersion":1,"futureThing":{{"x":1}},
                "contacts":[],"mail":{{"layout":"maildir++","path":"mail","folders":[
                  {{"name":"Inbox","path":["Inbox"],"dir":"mail","role":"inbox",
                    "subscribed":true,"messages":3,"newMember":true}}]}}}}"#
        ))
        .unwrap();
        assert_eq!(m.contacts.as_deref().map(<[Collection]>::len), Some(0));
        assert!(m.calendars.is_none());
        let mail = m.mail.unwrap();
        assert_eq!(mail.folders[0].role.as_deref(), Some("inbox"));
    }

    #[test]
    fn another_mail_layout_is_refused() {
        let e = err(&format!(
            r#"{{"format":"{FORMAT}","formatVersion":1,"mail":{{"layout":"mbox","path":"mail"}}}}"#
        ));
        assert!(e.contains("mbox"), "{e}");
    }

    #[test]
    fn a_missing_or_linked_manifest_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let e = read(td.path()).unwrap_err().to_string();
        assert!(e.contains("interrupted"), "{e}");
        #[cfg(unix)]
        {
            let real = td.path().join("real.json");
            fs::write(
                &real,
                format!(r#"{{"format":"{FORMAT}","formatVersion":1}}"#),
            )
            .unwrap();
            std::os::unix::fs::symlink(&real, td.path().join("manifest.json")).unwrap();
            let e = read(td.path()).unwrap_err().to_string();
            assert!(e.contains("symbolic link"), "{e}");
        }
    }
}
