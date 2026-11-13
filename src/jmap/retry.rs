/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::jmap::http::RetryPolicy;

pub const THROTTLE_BASE: Duration = Duration::from_secs(1);
const THROTTLE_LEVEL_CAP: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Retryable,
    Fatal,
}

pub fn classify_http_status(status: u16) -> Disposition {
    match status {
        429 | 502 | 503 | 504 => Disposition::Retryable,
        _ => Disposition::Fatal,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodCallKind {
    Read,
    SingleObjectWrite,
}

pub fn jmap_method_disposition(error_type: &str, kind: MethodCallKind) -> Disposition {
    match error_type {
        "serverUnavailable" => Disposition::Retryable,
        "serverPartialFail" => match kind {
            MethodCallKind::Read => Disposition::Retryable,
            MethodCallKind::SingleObjectWrite => Disposition::Fatal,
        },
        _ => Disposition::Fatal,
    }
}

pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let trimmed = value.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let imf_fixdate = time::macros::format_description!(
        "[weekday repr:short], [day] [month repr:short] [year] \
         [hour]:[minute]:[second] GMT"
    );
    let parsed = time::PrimitiveDateTime::parse(trimmed, imf_fixdate).ok()?;
    let target: SystemTime = parsed.assume_utc().into();
    target.duration_since(now).ok()
}

thread_local! {
    static RNG_STATE: Cell<u64> = Cell::new(seed());
}

fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let tid = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&std::thread::current().id(), &mut hasher);
        std::hash::Hasher::finish(&hasher)
    };
    (nanos ^ tid).max(1)
}

fn next_u64() -> u64 {
    RNG_STATE.with(|cell| {
        let mut x = cell.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        cell.set(x);
        x
    })
}

fn jitter_uniform(span: Duration) -> Duration {
    let nanos = span.as_nanos();
    if nanos == 0 {
        return Duration::ZERO;
    }
    let pick = (next_u64() as u128) % (nanos + 1);
    Duration::from_nanos(pick.min(u64::MAX as u128) as u64)
}

pub fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(32);
    let scaled = policy
        .base
        .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX));
    let capped = scaled.min(policy.cap);
    jitter_uniform(capped)
}

#[derive(Debug, Default)]
pub struct Cooldown {
    until: Mutex<Option<Instant>>,
}

impl Cooldown {
    pub fn new() -> Cooldown {
        Cooldown {
            until: Mutex::new(None),
        }
    }

    pub fn arm(&self, delay: Duration) {
        let target = Instant::now() + delay;
        if let Ok(mut guard) = self.until.lock() {
            let extend = guard.map(|cur| target > cur).unwrap_or(true);
            if extend {
                *guard = Some(target);
            }
        }
    }

    pub fn remaining(&self) -> Option<Duration> {
        let guard = self.until.lock().ok()?;
        let until = (*guard)?;
        let now = Instant::now();
        if until > now { Some(until - now) } else { None }
    }

    pub fn wait(&self) {
        while let Some(remaining) = self.remaining() {
            std::thread::sleep(remaining);
        }
    }
}

#[derive(Debug, Default)]
pub struct RateLimitState {
    cooldown: Cooldown,
    level: AtomicU32,
}

impl RateLimitState {
    pub fn new() -> RateLimitState {
        RateLimitState {
            cooldown: Cooldown::new(),
            level: AtomicU32::new(0),
        }
    }

    pub fn cooldown(&self) -> &Cooldown {
        &self.cooldown
    }

    pub fn level(&self) -> u32 {
        self.level.load(Ordering::Relaxed)
    }

    pub fn on_success(&self) {
        self.level.store(0, Ordering::Relaxed);
    }

    pub fn on_throttle(&self, policy: &RetryPolicy, retry_after: Option<Duration>) -> Duration {
        let n = self
            .level
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
            .min(THROTTLE_LEVEL_CAP);
        let delay = retry_after.unwrap_or_else(|| throttle_backoff(policy, n));
        self.cooldown.arm(delay);
        delay
    }
}

/// A proactive request-rate cap, shared across worker threads. It spaces the
/// *start* of successive requests at least one interval apart, so a known
/// provider limit (Microsoft Graph, Google) is kept to in the first place,
/// rather than reactively, after the server returns 429 and the [`Cooldown`]
/// engages. The interval is set once, after the client is built (like a
/// logger), and a zero interval disables it so there is no cost on the hot
/// path when no cap is asked for. Requests in flight are not limited — only
/// how often a new one may begin — so it composes with the connection count.
#[derive(Debug, Default)]
pub struct Pacer {
    /// Minimum spacing between request starts, in nanoseconds; 0 disables.
    min_interval_nanos: AtomicU64,
    next: Mutex<Option<Instant>>,
}

impl Pacer {
    pub fn new() -> Pacer {
        Pacer {
            min_interval_nanos: AtomicU64::new(0),
            next: Mutex::new(None),
        }
    }

    /// Set the cap from a requests-per-second figure. `None`, zero, or a
    /// non-finite rate disables pacing. A later call replaces the cap.
    pub fn set_per_second(&self, max_rps: Option<f64>) {
        let nanos = match max_rps {
            Some(rps) if rps.is_finite() && rps > 0.0 => {
                (1.0e9 / rps).round().clamp(1.0, u64::MAX as f64) as u64
            }
            _ => 0,
        };
        self.min_interval_nanos.store(nanos, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.min_interval_nanos.load(Ordering::Relaxed) != 0
    }

    /// Block until this thread may start its request, then reserve its slot.
    /// Holding the lock across the sleep is deliberate: it serializes the
    /// reservations, so N waiters leave one interval apart instead of all at
    /// once. An idle gap does not bank credit — the next request starts now,
    /// never a burst.
    pub fn wait(&self) {
        let nanos = self.min_interval_nanos.load(Ordering::Relaxed);
        if nanos == 0 {
            return;
        }
        let interval = Duration::from_nanos(nanos);
        let mut next = self.next.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let start = match *next {
            Some(t) if t > now => {
                std::thread::sleep(t - now);
                t
            }
            _ => now,
        };
        *next = Some(start + interval);
    }
}

pub fn throttle_backoff(policy: &RetryPolicy, level: u32) -> Duration {
    let shift = level.saturating_sub(1).min(20);
    let scaled = THROTTLE_BASE
        .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
        .min(policy.cap);
    let half = scaled / 2;
    half + jitter_uniform(scaled - half)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy::new(5)
    }

    #[test]
    fn backoff_is_bounded_by_cap_with_full_jitter() {
        let p = policy();
        for attempt in 1..=12 {
            let d = backoff_delay(&p, attempt);
            assert!(d <= p.cap, "attempt {attempt} delay {d:?} exceeds cap");
        }
    }

    #[test]
    fn backoff_grows_until_cap() {
        let p = policy();
        let big = backoff_delay(&p, 30);
        assert!(big <= p.cap);
    }

    #[test]
    fn retry_after_seconds() {
        assert_eq!(
            parse_retry_after("120", SystemTime::now()),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn retry_after_http_date_in_future() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777);
        let d = parse_retry_after("Sun, 06 Nov 1994 08:49:47 GMT", now);
        assert_eq!(d, Some(Duration::from_secs(10)));
    }

    #[test]
    fn retry_after_garbage_is_none() {
        assert_eq!(parse_retry_after("soon", SystemTime::now()), None);
    }

    #[test]
    fn status_classification() {
        assert_eq!(classify_http_status(429), Disposition::Retryable);
        assert_eq!(classify_http_status(503), Disposition::Retryable);
        assert_eq!(classify_http_status(400), Disposition::Fatal);
        assert_eq!(classify_http_status(401), Disposition::Fatal);
    }

    #[test]
    fn method_classification() {
        for kind in [MethodCallKind::Read, MethodCallKind::SingleObjectWrite] {
            assert_eq!(
                jmap_method_disposition("serverUnavailable", kind),
                Disposition::Retryable,
                "{kind:?}"
            );
            assert_eq!(
                jmap_method_disposition("invalidArguments", kind),
                Disposition::Fatal,
                "{kind:?}"
            );
            assert_eq!(
                jmap_method_disposition("forbidden", kind),
                Disposition::Fatal,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn partial_fail_is_never_retried_for_a_write() {
        assert_eq!(
            jmap_method_disposition("serverPartialFail", MethodCallKind::Read),
            Disposition::Retryable
        );
        assert_eq!(
            jmap_method_disposition("serverPartialFail", MethodCallKind::SingleObjectWrite),
            Disposition::Fatal
        );
    }

    #[test]
    fn cooldown_arms_and_clears() {
        let c = Cooldown::new();
        assert!(c.remaining().is_none());
        c.arm(Duration::from_millis(40));
        assert!(c.remaining().is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(c.remaining().is_none());
    }

    #[test]
    fn throttle_backoff_doubles_then_clamps_to_cap() {
        let p = RetryPolicy::new(8);
        let mut prev = throttle_backoff(&p, 1);
        for level in 2..=4 {
            let d = throttle_backoff(&p, level);
            assert!(
                d >= prev / 2,
                "level {level} ({d:?}) collapsed below half of prev ({prev:?})"
            );
            prev = d;
        }
        assert!(throttle_backoff(&p, 20) <= p.cap);
    }

    #[test]
    fn rate_limit_state_escalates_across_concurrent_callers() {
        let p = RetryPolicy::new(8);
        let s = RateLimitState::new();
        let first = s.on_throttle(&p, None);
        let second = s.on_throttle(&p, None);
        let third = s.on_throttle(&p, None);
        assert!(
            third >= first,
            "level 3 delay ({third:?}) should not be smaller than level 1 ({first:?})"
        );
        assert!(
            second >= first,
            "level 2 delay ({second:?}) should not be smaller than level 1 ({first:?})"
        );
        assert!(s.level() >= 3);
    }

    #[test]
    fn rate_limit_state_resets_on_success() {
        let p = RetryPolicy::new(8);
        let s = RateLimitState::new();
        let _ = s.on_throttle(&p, None);
        let _ = s.on_throttle(&p, None);
        let _ = s.on_throttle(&p, None);
        assert!(s.level() >= 3);
        s.on_success();
        assert_eq!(s.level(), 0);
        let next = s.on_throttle(&p, None);
        assert!(
            next <= THROTTLE_BASE,
            "first throttle after reset should be small ({next:?})"
        );
    }

    #[test]
    fn rate_limit_state_honours_retry_after_over_schedule() {
        let p = RetryPolicy::new(8);
        let s = RateLimitState::new();
        let d = s.on_throttle(&p, Some(Duration::from_millis(750)));
        assert_eq!(d, Duration::from_millis(750));
        assert_eq!(s.level(), 1);
    }

    #[test]
    fn pacer_disabled_by_default_and_by_zero_or_none() {
        let p = Pacer::new();
        assert!(!p.is_enabled());
        p.set_per_second(Some(10.0));
        assert!(p.is_enabled());
        p.set_per_second(None);
        assert!(!p.is_enabled());
        p.set_per_second(Some(0.0));
        assert!(!p.is_enabled());
        p.set_per_second(Some(-5.0));
        assert!(!p.is_enabled());
        p.set_per_second(Some(f64::NAN));
        assert!(!p.is_enabled());
    }

    #[test]
    fn pacer_spaces_successive_waits_by_the_interval() {
        // 50 rps -> 20ms apart. The first wait returns at once (no prior
        // slot); the next few are each spaced ~one interval on.
        let p = Pacer::new();
        p.set_per_second(Some(50.0));
        let start = Instant::now();
        for _ in 0..4 {
            p.wait();
        }
        // Three gaps of 20ms = 60ms minimum after the immediate first.
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(55),
            "four 50rps waits took {elapsed:?}, expected >= ~60ms"
        );
    }

    #[test]
    fn pacer_wait_is_immediate_when_disabled() {
        let p = Pacer::new();
        let start = Instant::now();
        for _ in 0..1000 {
            p.wait();
        }
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn pacer_does_not_bank_credit_over_an_idle_gap() {
        // After a pause longer than the interval, the next request still
        // starts immediately (one slot), not a burst of pre-earned slots.
        let p = Pacer::new();
        p.set_per_second(Some(100.0)); // 10ms
        p.wait(); // takes a slot now
        std::thread::sleep(Duration::from_millis(40));
        let start = Instant::now();
        p.wait(); // next is in the past -> immediate
        assert!(start.elapsed() < Duration::from_millis(5));
    }
}
