//! One shared HTTP transport: retry, backoff, `Retry-After`, and per-host
//! rate limiting. Collectors get a `send`, not just a `build`.

use once_cell::sync::Lazy;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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
}
