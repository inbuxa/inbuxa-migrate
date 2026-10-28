/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! The version as tags name it: Cargo's `27.1.0` is `27.1`, `27.1.3` stays
//! `27.1.3`, `27.1.0-rc.1` is `27.1-rc.1`, the same rule as the rest of the
//! inbuxa suite. `--version` and the User-Agent show it; the release workflow
//! checks the tag against it.

use std::sync::OnceLock;

/// This build's version, as tags name it.
pub fn shown() -> &'static str {
    static SHOWN: OnceLock<String> = OnceLock::new();
    SHOWN.get_or_init(|| display(env!("CARGO_PKG_VERSION")))
}

fn display(cargo: &str) -> String {
    let (core, pre) = match cargo.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (cargo, None),
    };
    match core.strip_suffix(".0") {
        Some(series) if series.matches('.').count() == 1 => match pre {
            Some(pre) => format!("{series}-{pre}"),
            None => series.to_string(),
        },
        _ => cargo.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{display, shown};

    #[test]
    fn shown_as_tags_name_it() {
        assert_eq!(display("27.1.0"), "27.1");
        assert_eq!(display("27.1.3"), "27.1.3");
        assert_eq!(display("27.1.0-rc.1"), "27.1-rc.1");
        assert_eq!(display("28.7.10"), "28.7.10");
        assert_eq!(display("2026.10.26"), "2026.10.26");
        assert!(!shown().is_empty());
    }
}
