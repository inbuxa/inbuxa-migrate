/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! A progress line for long runs: how many of how many, how fast, and about
//! how long is left. Printed to stderr at the default log level, at most once
//! per interval, so a large export shows it is moving without flooding the
//! terminal.

use std::time::{Duration, Instant};

use crate::logging::{LEVEL_DEFAULT, Logger};

/// How often a progress line is printed while work continues.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

pub struct Progress {
    label: String,
    total: u64,
    done: u64,
    started: Instant,
    last: Instant,
    interval: Duration,
    enabled: bool,
}

impl Progress {
    /// `label` names the work, e.g. "export: Email". Nothing is printed when
    /// the logger is quiet or there is nothing to do.
    pub fn new(label: impl Into<String>, total: u64, logger: &Logger) -> Progress {
        let now = Instant::now();
        Progress {
            label: label.into(),
            total,
            done: 0,
            started: now,
            last: now,
            interval: PROGRESS_INTERVAL,
            enabled: logger.enabled(LEVEL_DEFAULT) && total > 0,
        }
    }

    /// Records `n` more items done, and prints a line once the interval has
    /// passed since the last one.
    pub fn add(&mut self, n: u64) {
        self.done = (self.done + n).min(self.total);
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last) >= self.interval {
            self.last = now;
            eprintln!("{}", self.line(now.duration_since(self.started)));
        }
    }

    fn line(&self, elapsed: Duration) -> String {
        progress_line(&self.label, self.done, self.total, elapsed)
    }
}

/// `export: Email 1,200/5,000 (24%), 40/s, about 1m35s left`. The rate and
/// the time left are left out until there is enough to estimate them from.
pub fn progress_line(label: &str, done: u64, total: u64, elapsed: Duration) -> String {
    let pct = (done * 100).checked_div(total).unwrap_or(100);
    let mut out = format!("{label} {}/{} ({pct}%)", thousands(done), thousands(total));
    let secs = elapsed.as_secs_f64();
    if done > 0 && secs >= 1.0 {
        let rate = done as f64 / secs;
        out.push_str(&format!(", {}/s", format_rate(rate)));
        let left = (total - done) as f64 / rate;
        if total > done && left.is_finite() {
            out.push_str(&format!(
                ", about {} left",
                format_duration(Duration::from_secs_f64(left))
            ));
        }
    }
    out
}

/// A duration as `45s`, `1m35s` or `2h03m`.
pub fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m{:02}s", total / 60, total % 60)
    } else {
        format!("{}h{:02}m", total / 3600, (total % 3600) / 60)
    }
}

/// The line printed when a type is finished: `export: Email done: 120
/// created, 3 updated, 5,000 unchanged, 0 failed (2m03s)`.
pub fn done_line(label: &str, counts: &crate::sync::TypeCounts, elapsed: Duration) -> String {
    format!(
        "{label} done: {} created, {} updated, {} unchanged, {} failed ({})",
        thousands(counts.created),
        thousands(counts.updated),
        thousands(counts.skipped),
        thousands(counts.failed),
        format_duration(elapsed)
    )
}

fn format_rate(rate: f64) -> String {
    if rate >= 10.0 {
        format!("{:.0}", rate)
    } else {
        format!("{:.1}", rate)
    }
}

pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_has_count_rate_and_time_left() {
        let line = progress_line("export: Email", 1200, 5000, Duration::from_secs(30));
        assert!(
            line.starts_with("export: Email 1,200/5,000 (24%)"),
            "{line}"
        );
        assert!(line.contains("40/s"), "{line}");
        assert!(line.contains("about 1m35s left"), "{line}");
    }

    #[test]
    fn no_estimate_before_there_is_something_to_estimate_from() {
        assert_eq!(
            progress_line("export: Email", 0, 10, Duration::from_secs(5)),
            "export: Email 0/10 (0%)"
        );
        assert_eq!(
            progress_line("export: Email", 3, 10, Duration::from_millis(200)),
            "export: Email 3/10 (30%)"
        );
    }

    #[test]
    fn a_finished_run_shows_no_time_left() {
        let line = progress_line("export: Email", 10, 10, Duration::from_secs(4));
        assert!(!line.contains("left"), "{line}");
        assert!(line.contains("(100%)"), "{line}");
    }

    #[test]
    fn durations_and_counts_read_naturally() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(95)), "1m35s");
        assert_eq!(format_duration(Duration::from_secs(7380)), "2h03m");
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn the_done_line_names_every_count() {
        let counts = crate::sync::TypeCounts {
            created: 120,
            updated: 3,
            skipped: 5000,
            failed: 1,
            ..Default::default()
        };
        assert_eq!(
            done_line("export: Email", &counts, Duration::from_secs(123)),
            "export: Email done: 120 created, 3 updated, 5,000 unchanged, 1 failed (2m03s)"
        );
    }

    #[test]
    fn a_quiet_logger_or_empty_job_prints_nothing() {
        let p = Progress::new("x", 10, &Logger::from_flags(true, 0));
        assert!(!p.enabled);
        let p = Progress::new("x", 0, &Logger::from_flags(false, 0));
        assert!(!p.enabled);
    }
}
