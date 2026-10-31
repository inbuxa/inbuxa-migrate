/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Paths from the manifest are data: whoever wrote the export chose them.
//! Each is checked to stay inside its part of the tree, and no part of it
//! may be a symbolic link, before anything is opened.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// `rel`, a `/`-separated path from the manifest, under `root`. It must
/// start with `part` (`mail`, `contacts` and so on), climb nowhere, and
/// pass through no symbolic link. What it names must exist.
pub fn resolve(root: &Path, rel: &str, part: &str) -> Result<PathBuf, String> {
    let segments = segments(rel)?;
    if segments.first() != Some(&part) {
        return Err(format!("{rel:?} is outside {part}/"));
    }
    let mut path = root.to_path_buf();
    for segment in &segments {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{rel:?} passes through a symbolic link, which is never followed"
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("{rel:?} is not in the export"));
            }
            Err(e) => return Err(format!("{rel:?}: {e}")),
        }
    }
    Ok(path)
}

/// The segments of a manifest path, refused if any could climb out or
/// mean something other than a plain name.
pub fn segments(rel: &str) -> Result<Vec<&str>, String> {
    if rel.is_empty() {
        return Err("an empty path".to_owned());
    }
    let mut out = Vec::new();
    for segment in rel.split('/') {
        let plain = !segment.is_empty()
            && segment != "."
            && segment != ".."
            && !segment.contains(['\\', '\0'])
            && matches!(
                Path::new(segment)
                    .components()
                    .collect::<Vec<_>>()
                    .as_slice(),
                [Component::Normal(_)]
            );
        if !plain {
            return Err(format!("{rel:?} is not a plain relative path"));
        }
        out.push(segment);
    }
    Ok(out)
}

/// A directory, and not a link to one.
pub fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// A regular file, and not a link to one.
pub fn is_real_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_that_climb_or_jump_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            "mail/../../etc",
            "mail/./x",
            "mail//x",
            "mail/a\\..\\b",
            "..",
        ] {
            assert!(segments(bad).is_err(), "{bad:?}");
        }
        assert_eq!(segments("mail/.INBOX.x").unwrap(), ["mail", ".INBOX.x"]);
    }

    #[test]
    fn a_path_must_stay_in_its_part_and_exist() {
        let td = tempfile::tempdir().unwrap();
        fs::create_dir_all(td.path().join("contacts")).unwrap();
        fs::write(td.path().join("contacts/a.vcf"), "").unwrap();
        fs::create_dir_all(td.path().join("calendars")).unwrap();
        fs::write(td.path().join("calendars/b.ics"), "").unwrap();
        assert!(resolve(td.path(), "contacts/a.vcf", "contacts").is_ok());
        let e = resolve(td.path(), "calendars/b.ics", "contacts").unwrap_err();
        assert!(e.contains("outside contacts/"), "{e}");
        let e = resolve(td.path(), "contacts/missing.vcf", "contacts").unwrap_err();
        assert!(e.contains("not in the export"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_anywhere_on_the_way_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "x").unwrap();
        fs::create_dir_all(td.path().join("sieve")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            td.path().join("sieve/a.sieve"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), td.path().join("contacts")).unwrap();
        let e = resolve(td.path(), "sieve/a.sieve", "sieve").unwrap_err();
        assert!(e.contains("symbolic link"), "{e}");
        let e = resolve(td.path(), "contacts/secret", "contacts").unwrap_err();
        assert!(e.contains("symbolic link"), "{e}");
        assert!(!is_real_file(&td.path().join("sieve/a.sieve")));
        assert!(!is_real_dir(&td.path().join("contacts")));
    }
}
