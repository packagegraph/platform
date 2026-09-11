//! One shared HTTP transport: retry, backoff, `Retry-After`, and per-host
//! rate limiting. Collectors get a `send`, not just a `build`.

use once_cell::sync::Lazy;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many times to retry, and how long to wait between attempts.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts including the first. 5 means 1 try + 4 retries.
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Spread retries over [delay/2, delay] so concurrent workers that
    /// failed together do not retry in lockstep. Set false in tests.
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// Exponential backoff for a zero-based attempt number, capped at
    /// `max_delay`. Saturating throughout, so a large `attempt` clamps to
    /// the cap rather than overflowing.
    pub fn backoff_delay(&self, attempt: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempt.min(31));
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }

    /// Scale `d` into [d/2, d]. Identity when `jitter` is false.
    pub fn jittered(&self, d: Duration) -> Duration {
        if !self.jitter {
            return d;
        }
        let nanos = d.as_nanos().min(u64::MAX as u128) as u64;
        let half = nanos / 2;
        let span = nanos - half;
        Duration::from_nanos(half + next_random() % span.saturating_add(1))
    }
}

/// Process-wide xorshift64. The crate has no `rand` dependency and the
/// design forbids adding one, so seed from `RandomState`, which the std
/// library seeds from the OS.
static JITTER_STATE: Lazy<AtomicU64> = Lazy::new(|| {
    let mut h = RandomState::new().build_hasher();
    h.write_u64(0x9E37_79B9_7F4A_7C15);
    // A xorshift seed of zero is a fixed point; force it non-zero.
    AtomicU64::new(h.finish() | 1)
});

fn next_random() -> u64 {
    let mut x = JITTER_STATE.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    JITTER_STATE.store(x, Ordering::Relaxed);
    x
}

/// Parse a `Retry-After` header: either whole seconds or an HTTP date.
///
/// A date already in the past returns `None` rather than an error or a
/// zero wait -- "no wait needed" and "no header" are the same instruction
/// to the caller, so there is no error case to handle.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    (when.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .to_std()
        .ok()
}

#[derive(Debug, Clone, Copy)]
struct HostState {
    interval: Duration,
    /// Earliest instant at which the next request to this host may start.
    next_allowed: Instant,
}

/// Paces requests per host. One instance is shared by every collector, so
/// two collectors hitting the same registry cannot jointly exceed its
/// limit the way independent `rate_limit()` sleeps could.
#[derive(Debug)]
pub struct HostLimiter {
    default_interval: Duration,
    state: Mutex<HashMap<String, HostState>>,
}

impl HostLimiter {
    pub fn new(default_interval: Duration) -> Self {
        Self {
            default_interval,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Builder-style per-host override, for known rate-sensitive hosts.
    pub fn with_host(self, host: &str, interval: Duration) -> Self {
        {
            let mut guard = self.lock();
            guard.insert(
                host.to_string(),
                HostState {
                    interval,
                    next_allowed: Instant::now(),
                },
            );
        }
        self
    }

    /// A poisoned lock means another thread panicked mid-update. The map is
    /// still structurally valid and the worst case is one mistimed request,
    /// so recover rather than propagate: there is no error for callers to
    /// handle.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, HostState>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Block until this host's next slot, reserving it for this caller.
    pub fn wait_turn(&self, host: &str) {
        let slot = {
            let mut guard = self.lock();
            let now = Instant::now();
            let default_interval = self.default_interval;
            let entry = guard.entry(host.to_string()).or_insert(HostState {
                interval: default_interval,
                next_allowed: now,
            });
            let slot = entry.next_allowed.max(now);
            entry.next_allowed = slot + entry.interval;
            slot
        }; // lock released before sleeping

        let now = Instant::now();
        if slot > now {
            std::thread::sleep(slot - now);
        }
    }

    /// Widen a host's interval, e.g. after it sent `Retry-After`. Never
    /// shrinks an existing interval.
    pub fn widen(&self, host: &str, at_least: Duration) {
        let mut guard = self.lock();
        let now = Instant::now();
        let default_interval = self.default_interval;
        let entry = guard.entry(host.to_string()).or_insert(HostState {
            interval: default_interval,
            next_allowed: now,
        });
        if at_least > entry.interval {
            entry.interval = at_least;
        }
    }
}

impl Default for HostLimiter {
    fn default() -> Self {
        Self::new(crate::enricher::DEFAULT_RATE_LIMIT)
    }
}

/// Counters for one transport's lifetime. Collectors print a snapshot at
/// end of run so a pathological cache-miss or failure rate is visible in
/// one line instead of invisible for months.
#[derive(Debug, Default)]
pub struct TransportStats {
    attempts: AtomicU64,
    successes: AtomicU64,
    retries: AtomicU64,
    rate_limited: AtomicU64,
    not_found: AtomicU64,
    failures: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub attempts: u64,
    pub successes: u64,
    pub retries: u64,
    pub rate_limited: u64,
    pub not_found: u64,
    pub failures: u64,
}

impl TransportStats {
    pub fn record_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_success(&self) {
        self.successes.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_rate_limited(&self) {
        self.rate_limited.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_not_found(&self) {
        self.not_found.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            attempts: self.attempts.load(Ordering::Relaxed),
            successes: self.successes.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            rate_limited: self.rate_limited.load(Ordering::Relaxed),
            not_found: self.not_found.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
        }
    }
}

impl std::fmt::Display for StatsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "http: attempts={} ok={} retries={} 429={} 404={} failed={}",
            self.attempts,
            self.successes,
            self.retries,
            self.rate_limited,
            self.not_found,
            self.failures
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_each_attempt() {
        let p = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            jitter: false,
        };
        assert_eq!(p.backoff_delay(0), Duration::from_millis(100));
        assert_eq!(p.backoff_delay(1), Duration::from_millis(200));
        assert_eq!(p.backoff_delay(2), Duration::from_millis(400));
    }

    #[test]
    fn backoff_is_capped_at_max_delay() {
        let p = RetryPolicy {
            max_attempts: 20,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            jitter: false,
        };
        assert_eq!(p.backoff_delay(10), Duration::from_secs(1));
        // Must not overflow at large attempt counts.
        assert_eq!(p.backoff_delay(31), Duration::from_secs(1));
    }

    #[test]
    fn jitter_disabled_returns_the_delay_unchanged() {
        let p = RetryPolicy {
            jitter: false,
            ..RetryPolicy::default()
        };
        let d = Duration::from_millis(800);
        assert_eq!(p.jittered(d), d);
    }

    #[test]
    fn jitter_stays_within_half_to_full_range() {
        let p = RetryPolicy {
            jitter: true,
            ..RetryPolicy::default()
        };
        let d = Duration::from_millis(1000);
        for _ in 0..200 {
            let j = p.jittered(d);
            assert!(j >= Duration::from_millis(500), "too short: {j:?}");
            assert!(j <= d, "too long: {j:?}");
        }
    }

    #[test]
    fn retry_after_parses_whole_seconds() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("  7 "), Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_parses_an_http_date_in_the_future() {
        let future = chrono::Utc::now() + chrono::Duration::seconds(120);
        let header = future.to_rfc2822();
        let parsed = parse_retry_after(&header).expect("future date should parse");
        // Allow scheduling slack; the point is it lands near two minutes.
        assert!(parsed <= Duration::from_secs(120), "got {parsed:?}");
        assert!(parsed >= Duration::from_secs(100), "got {parsed:?}");
    }

    #[test]
    fn retry_after_in_the_past_yields_no_wait() {
        let past = chrono::Utc::now() - chrono::Duration::seconds(60);
        assert_eq!(parse_retry_after(&past.to_rfc2822()), None);
    }

    #[test]
    fn retry_after_garbage_yields_none() {
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    // ── HostLimiter ─────────────────────────────────────────────────────

    #[test]
    fn same_host_calls_are_spaced_by_the_interval() {
        let limiter = HostLimiter::new(Duration::from_millis(120));
        let start = std::time::Instant::now();
        limiter.wait_turn("example.com");
        limiter.wait_turn("example.com");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(120),
            "second call should have waited, elapsed {elapsed:?}"
        );
    }

    #[test]
    fn different_hosts_do_not_block_each_other() {
        let limiter = HostLimiter::new(Duration::from_millis(300));
        let start = std::time::Instant::now();
        limiter.wait_turn("a.example.com");
        limiter.wait_turn("b.example.com");
        limiter.wait_turn("c.example.com");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(300),
            "distinct hosts must not serialise, elapsed {elapsed:?}"
        );
    }

    #[test]
    fn per_host_override_beats_the_default() {
        let limiter = HostLimiter::new(Duration::from_millis(1))
            .with_host("slow.example.com", Duration::from_millis(150));
        let start = std::time::Instant::now();
        limiter.wait_turn("slow.example.com");
        limiter.wait_turn("slow.example.com");
        assert!(start.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn widen_increases_a_hosts_interval_for_the_rest_of_the_run() {
        let limiter = HostLimiter::new(Duration::from_millis(1));
        limiter.wait_turn("example.com");
        limiter.widen("example.com", Duration::from_millis(150));
        let start = std::time::Instant::now();
        limiter.wait_turn("example.com");
        limiter.wait_turn("example.com");
        assert!(start.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn widen_never_shrinks_an_interval() {
        let limiter = HostLimiter::new(Duration::from_millis(200));
        limiter.widen("example.com", Duration::from_millis(1));
        let start = std::time::Instant::now();
        limiter.wait_turn("example.com");
        limiter.wait_turn("example.com");
        assert!(start.elapsed() >= Duration::from_millis(200));
    }

    #[test]
    fn limiter_is_sync_and_usable_across_rayon_threads() {
        use rayon::prelude::*;
        let limiter = HostLimiter::new(Duration::from_millis(20));
        let start = std::time::Instant::now();
        // Four requests to one host must serialise to at least 3 intervals.
        (0..4).into_par_iter().for_each(|_| {
            limiter.wait_turn("example.com");
        });
        assert!(
            start.elapsed() >= Duration::from_millis(60),
            "concurrent callers must not all fire at once, elapsed {:?}",
            start.elapsed()
        );
    }

    // ── TransportStats ──────────────────────────────────────────────────

    #[test]
    fn stats_start_at_zero() {
        let s = TransportStats::default();
        let snap = s.snapshot();
        assert_eq!(snap.attempts, 0);
        assert_eq!(snap.successes, 0);
        assert_eq!(snap.failures, 0);
    }

    #[test]
    fn stats_count_each_category_independently() {
        let s = TransportStats::default();
        s.record_attempt();
        s.record_attempt();
        s.record_success();
        s.record_retry();
        s.record_rate_limited();
        s.record_not_found();
        s.record_failure();

        let snap = s.snapshot();
        assert_eq!(snap.attempts, 2);
        assert_eq!(snap.successes, 1);
        assert_eq!(snap.retries, 1);
        assert_eq!(snap.rate_limited, 1);
        assert_eq!(snap.not_found, 1);
        assert_eq!(snap.failures, 1);
    }

    #[test]
    fn stats_summary_line_names_every_nonzero_category() {
        let s = TransportStats::default();
        s.record_attempt();
        s.record_success();
        s.record_rate_limited();
        let line = format!("{}", s.snapshot());
        assert!(line.contains("attempts=1"), "got: {line}");
        assert!(line.contains("ok=1"), "got: {line}");
        assert!(line.contains("429=1"), "got: {line}");
    }

    #[test]
    fn stats_are_sync_and_total_correctly_under_concurrency() {
        use rayon::prelude::*;
        let s = TransportStats::default();
        (0..500).into_par_iter().for_each(|_| {
            s.record_attempt();
            s.record_success();
        });
        let snap = s.snapshot();
        assert_eq!(snap.attempts, 500);
        assert_eq!(snap.successes, 500);
    }
}
