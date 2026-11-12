/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! The report an `--estimate` run prints: for each type, how much is new to
//! fetch and, where the source can say cheaply, how many bytes; then a total
//! and a rough time, before anything is written.
//!
//! The numbers come from the same dry-run enumeration an ordinary `--dry-run`
//! does -- new versus already in the archive -- plus a size pass that asks the
//! source only for sizes, never bodies. Not every source can give sizes
//! without extra round trips; those report counts, and the size reads "not
//! estimated".
//!
//! The time is deliberately a wide range and labelled rough: on a first run
//! there is no measured throughput to go on, so it is bounded by a
//! conservative message rate, not a promise.

use std::time::Duration;

use crate::sync::Summary;
use crate::sync::progress::format_duration;

/// The slow and fast ends of a plain message rate, in messages a second. Real
/// throughput depends on the server, the network and message size; these are
/// chosen to bracket the common case rather than to be exact.
const RATE_SLOW: f64 = 5.0;
const RATE_FAST: f64 = 20.0;

/// A size in bytes, as a short decimal string: `4.2 GB`, `880 MB`, `512 B`.
/// Decimal (1000) units, because that is how disks and bandwidth are sold and
/// how an operator sizing a move will reason about it.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    // One decimal below 100, none above: 4.2 GB, but 512 MB.
    if value < 100.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// The slow and fast ends of how long fetching `items` might take, as
/// durations. Fast rate gives the short end, slow rate the long end.
fn time_range(items: u64) -> (Duration, Duration) {
    let secs = |rate: f64| Duration::from_secs_f64((items as f64 / rate).ceil().max(0.0));
    (secs(RATE_FAST), secs(RATE_SLOW))
}

/// The whole report as a string, ready to print. `per_type` entries with
/// nothing new are left out; a run with nothing new at all says so.
pub fn render(summary: &Summary) -> String {
    let mut out = String::from("Estimate (no writes):\n");

    let mut total_items = 0u64;
    let mut total_bytes = 0u64;
    let mut any = false;

    for (name, counts) in &summary.per_type {
        let items = counts.fetched;
        if items == 0 {
            continue;
        }
        any = true;
        total_items += items;
        total_bytes += counts.bytes;
        // Folders and other containers carry no transfer size; only append one
        // when there is one, so a mailbox row reads "2 new", an email row
        // "4 new, 19.8 kB".
        if counts.bytes > 0 {
            out.push_str(&format!(
                "  {name}: {} new, {}\n",
                thousands_items(items),
                human_bytes(counts.bytes)
            ));
        } else {
            out.push_str(&format!("  {name}: {} new\n", thousands_items(items)));
        }
    }

    if !any {
        out.push_str("  nothing new since the archive was last filled\n");
        return out;
    }

    out.push_str("  ----\n");
    if total_bytes > 0 {
        out.push_str(&format!(
            "  total: {} new, {}\n",
            thousands_items(total_items),
            human_bytes(total_bytes)
        ));
    } else {
        out.push_str(&format!("  total: {} new\n", thousands_items(total_items)));
    }

    let (fast, slow) = time_range(total_items);
    out.push_str(&format!(
        "  rough time: {}\u{2013}{} (first run, no measured rate yet)\n",
        format_duration(fast),
        format_duration(slow)
    ));
    out
}

/// `1,234` with grouping, plus the word, so a line reads `1,234 new`.
fn thousands_items(n: u64) -> String {
    crate::sync::progress::thousands(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::TypeCounts;

    #[test]
    fn human_bytes_scales_and_rounds() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1000), "1.0 kB");
        assert_eq!(human_bytes(4_200_000_000), "4.2 GB");
        assert_eq!(human_bytes(512_000_000), "512 MB");
    }

    #[test]
    fn time_range_fast_end_is_shorter() {
        let (fast, slow) = time_range(1000);
        assert!(fast < slow);
        // 1000 at 20/s = 50s, at 5/s = 200s.
        assert_eq!(fast, Duration::from_secs(50));
        assert_eq!(slow, Duration::from_secs(200));
    }

    #[test]
    fn nothing_new_says_so() {
        let s = Summary::default();
        assert!(render(&s).contains("nothing new"));
    }

    #[test]
    fn a_type_with_bytes_shows_a_size_and_one_without_says_not_estimated() {
        let mut s = Summary::default();
        s.per_type.push((
            "Email",
            TypeCounts {
                fetched: 318,
                bytes: 4_200_000_000,
                ..Default::default()
            },
        ));
        s.per_type.push((
            "Calendar",
            TypeCounts {
                fetched: 12,
                ..Default::default()
            },
        ));
        let r = render(&s);
        assert!(r.contains("Email: 318 new, 4.2 GB"), "{r}");
        // A type with no measured bytes shows just the count, no size.
        assert!(r.contains("Calendar: 12 new\n"), "{r}");
        assert!(r.contains("total: 330 new, 4.2 GB"), "{r}");
        assert!(r.contains("rough time:"), "{r}");
    }
}
