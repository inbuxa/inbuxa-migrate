/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! File names for what the archive holds. Every name comes from the source
//! server, so none is trusted as a path: each becomes one safe component,
//! and two that end up the same are told apart.

use std::collections::HashSet;

/// One path component made from a name the source chose: no separators, no
/// `.` or `..`, nothing Windows refuses, never empty.
pub fn safe_component(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    let trimmed = out.trim_start();
    if trimmed.len() != out.len() {
        out = trimmed.to_owned();
    }
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '_');
    }
    if is_windows_reserved(&out) {
        out.push('_');
    }
    out
}

fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
}

/// Names already used in one directory, compared without case so the tree
/// unpacks the same on a case-insensitive file system.
#[derive(Default)]
pub struct Names {
    used: HashSet<String>,
}

impl Names {
    pub fn reserve(&mut self, name: &str) {
        self.used.insert(name.to_lowercase());
    }

    /// `stem` + `ext`, or `stem (2)` + `ext` and up when that is taken.
    pub fn unique(&mut self, stem: &str, ext: &str) -> String {
        let mut candidate = format!("{stem}{ext}");
        let mut n = 2;
        while !self.used.insert(candidate.to_lowercase()) {
            candidate = format!("{stem} ({n}){ext}");
            n += 1;
        }
        candidate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separators_and_traversal_never_survive() {
        assert_eq!(safe_component("../../etc/passwd"), "_.._.._etc_passwd");
        assert_eq!(safe_component(".."), "_");
        assert_eq!(safe_component("."), "_");
        assert_eq!(safe_component(""), "_");
        assert_eq!(safe_component("a\\b"), "a_b");
        assert_eq!(safe_component("a\0b"), "a_b");
        assert_eq!(safe_component(".hidden"), "_.hidden");
    }

    #[test]
    fn ordinary_names_are_kept() {
        assert_eq!(safe_component("Family & Friends"), "Family & Friends");
        assert_eq!(safe_component("Équipe"), "Équipe");
        assert_eq!(safe_component("v1.0"), "v1.0");
    }

    #[test]
    fn windows_trouble_is_avoided() {
        assert_eq!(safe_component("CON"), "CON_");
        assert_eq!(safe_component("com1.txt"), "com1.txt_");
        assert_eq!(safe_component("Notes. "), "Notes");
        assert_eq!(safe_component("a:b?"), "a_b_");
    }

    #[test]
    fn clashing_names_are_numbered_case_blind() {
        let mut names = Names::default();
        assert_eq!(names.unique("Work", ".ics"), "Work.ics");
        assert_eq!(names.unique("work", ".ics"), "work (2).ics");
        assert_eq!(names.unique("Work", ".ics"), "Work (3).ics");
        names.reserve("INBOX");
        assert_eq!(names.unique("inbox", ""), "inbox (2)");
    }
}
