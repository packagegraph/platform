//! One shared HTTP transport: retry, backoff, `Retry-After`, and per-host
//! rate limiting. Collectors get a `send`, not just a `build`.

use crate::cached_fetch::HttpResponse;
use crate::enricher::default_http_client;
use crate::fetch_error::FetchError;
use once_cell::sync::Lazy;
use reqwest::blocking::Client;
use reqwest::header::{ETAG, IF_NONE_MATCH, LAST_MODIFIED, RETRY_AFTER};
use reqwest::StatusCode;
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

    /// The interval this limiter enforces for `host`. Exposed so the pacing
    /// policy can be asserted on directly instead of by timing a request.
    pub fn interval_for(&self, host: &str) -> Duration {
        self.lock()
            .get(host)
            .map(|s| s.interval)
            .unwrap_or(self.default_interval)
    }

    /// The instant the next caller to `wait_turn` would be granted, without
    /// consuming the slot.
    #[cfg(test)]
    pub(crate) fn reserved_slot_for_test(&self, host: &str) -> Instant {
        let guard = self.lock();
        let now = Instant::now();
        guard
            .get(host)
            .map(|s| s.next_allowed.max(now))
            .unwrap_or(now)
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

    /// Block until this host is free, then claim it.
    ///
    /// The cursor is advanced by the caller that actually proceeds, never by
    /// one that is about to sleep. An earlier version handed each waiter a
    /// timestamp up front, which meant a `Retry-After` arriving mid-wait
    /// could not reach the slots already given out: every waiter inside the
    /// embargo woke on the same instant and sent together, handing the server
    /// exactly the burst that got us rate limited. Re-reading the cursor on
    /// each wake costs a few spurious wakeups and removes that whole class of
    /// bug -- a waiter cannot hold a stale slot if it never holds one.
    ///
    /// Ordering between waiters is therefore not FIFO. That is deliberate:
    /// what this guarantees is the spacing between requests, and no caller
    /// depends on which worker goes first.
    pub fn wait_turn(&self, host: &str) {
        loop {
            let wait = {
                let mut guard = self.lock();
                let now = Instant::now();
                let default_interval = self.default_interval;
                let entry = guard.entry(host.to_string()).or_insert(HostState {
                    interval: default_interval,
                    next_allowed: now,
                });
                if entry.next_allowed <= now {
                    entry.next_allowed = now + entry.interval;
                    return;
                }
                entry.next_allowed - now
            }; // lock released before sleeping

            std::thread::sleep(wait);
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
        // Push the cursor out too, or the next caller would be cleared to
        // send immediately despite the server having just told us to wait.
        // Waiters re-read this on each wake, so they re-space themselves at
        // the widened interval rather than all resuming on the deadline.
        let floor = now + at_least;
        if floor > entry.next_allowed {
            entry.next_allowed = floor;
        }
    }
}

/// Shorten `s` to at most `ERROR_BODY_LIMIT` bytes, cutting at a character
/// boundary.
///
/// `String::truncate` panics when the index lands inside a multibyte
/// character, so a localized error page could take the whole collector down
/// in place of returning a `FetchError`.
fn truncate_on_boundary(mut s: String) -> String {
    if s.len() <= ERROR_BODY_LIMIT {
        return s;
    }
    let mut end = ERROR_BODY_LIMIT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("... (truncated)");
    s
}

/// How much of a failing response body to keep for the error message.
const ERROR_BODY_LIMIT: usize = 512;

/// Default pacing between two requests to the same host.
pub const DEFAULT_RATE_LIMIT: Duration = Duration::from_millis(200);

/// Pacing for hosts that need a gentler hand than the default.
pub const SLOW_RATE_LIMIT: Duration = Duration::from_secs(1);

/// Hosts that need to be paced slower than `DEFAULT_RATE_LIMIT`.
///
/// This is the one place the project records how hard a given host may be
/// hit. Before the shared transport each collector slept for itself, so the
/// same host could be paced three different ways depending on which
/// collector reached it, and a new collector hitting a known-touchy host
/// got no pacing at all unless its author remembered. Now pacing comes with
/// the transport: there is nothing to remember and nothing to forget.
///
/// Intervals are the slowest of what the collectors previously used, since
/// widening is always the safe direction.
const HOST_INTERVALS: &[(&str, Duration)] = &[
    // Small community-run infrastructure, previously SLOW_RATE_LIMIT.
    ("repology.org", SLOW_RATE_LIMIT),
    ("bodhi.fedoraproject.org", SLOW_RATE_LIMIT),
    ("security.gentoo.org", SLOW_RATE_LIMIT),
    ("aur.archlinux.org", SLOW_RATE_LIMIT),
    // Quota'd APIs, previously a 500ms sleep per loop iteration.
    ("api.github.com", Duration::from_millis(500)),
    ("api.osv.dev", Duration::from_millis(500)),
    ("access.redhat.com", Duration::from_millis(500)),
    ("koji.fedoraproject.org", Duration::from_millis(500)),
];

impl Default for HostLimiter {
    fn default() -> Self {
        HOST_INTERVALS
            .iter()
            .fold(Self::new(DEFAULT_RATE_LIMIT), |l, &(h, i)| {
                l.with_host(h, i)
            })
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

/// HTTP verbs the transport supports. Deliberately minimal: collectors
/// read upstream data and, for query APIs like OSV and SPARQL, post a
/// request document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
}

/// One attempt's outcome, plus any `Retry-After` the server sent. The
/// header has to travel separately because `FetchError` carries no
/// headers.
struct Attempt {
    result: Result<HttpResponse, FetchError>,
    retry_after: Option<Duration>,
}

/// The crate's single HTTP send path: connection reuse, retry, backoff,
/// `Retry-After`, and per-host pacing in one place.
///
/// `enricher::default_http_client()` gives callers a configured client and
/// leaves every hard part to them, which is why 20 collectors hand-roll 58
/// `thread::sleep` retry loops. This owns the send instead.
#[derive(Debug)]
pub struct HttpTransport {
    client: Client,
    limiter: HostLimiter,
    policy: RetryPolicy,
    stats: TransportStats,
}

impl HttpTransport {
    pub fn new() -> Self {
        Self::with_client(default_http_client())
    }

    /// Use a caller-supplied client -- needed wherever the default will not
    /// do, e.g. RpmCollector's TLS client-cert auth against the RHEL CDN.
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            limiter: HostLimiter::default(),
            policy: RetryPolicy::default(),
            stats: TransportStats::default(),
        }
    }

    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_limiter(mut self, limiter: HostLimiter) -> Self {
        self.limiter = limiter;
        self
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// Fetch a URL, retrying transient failures.
    ///
    /// The signature matches `CachedFetcher::fetch`'s `http_get` parameter
    /// exactly, so it can be passed straight through as
    /// `|u, e| transport.get(u, e)` without changing `cached_fetch.rs`.
    pub fn get(&self, url: &str, if_none_match: Option<&str>) -> Result<HttpResponse, FetchError> {
        self.get_with(url, &[], if_none_match)
    }

    /// Like [`get`](Self::get), with extra request headers applied to every
    /// attempt -- e.g. hackage's `Accept: application/json` or the snap
    /// store's `Snap-Device-Series`.
    pub fn get_with(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        if_none_match: Option<&str>,
    ) -> Result<HttpResponse, FetchError> {
        self.execute(Method::Get, url, headers, if_none_match, None)
    }

    /// POST a body, with the same retry, backoff and pacing as `get`.
    ///
    /// The body is cloned per attempt rather than consumed, so a retry
    /// resends it intact.
    pub fn post(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<HttpResponse, FetchError> {
        self.execute(Method::Post, url, headers, None, Some(body))
    }

    fn execute(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        if_none_match: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, FetchError> {
        let host = host_of(url);
        let mut attempt: u32 = 0;

        loop {
            self.limiter.wait_turn(&host);
            self.stats.record_attempt();

            let Attempt {
                result,
                retry_after,
            } = self.send_once(method, url, headers, if_none_match, body.clone());

            let err = match result {
                Ok(response) => {
                    self.stats.record_success();
                    return Ok(response);
                }
                Err(e) => e,
            };

            match &err {
                FetchError::NotFound { .. } => self.stats.record_not_found(),
                FetchError::HttpStatus { status: 429, .. } => self.stats.record_rate_limited(),
                _ => {}
            }

            attempt += 1;
            if !err.is_retryable() || attempt >= self.policy.max_attempts {
                self.stats.record_failure();
                return Err(err);
            }

            // A server-supplied Retry-After is an instruction: obey it
            // verbatim and slow this host down for the rest of the run.
            // Only self-chosen backoff gets jittered.
            let delay = match retry_after {
                Some(wait) => {
                    self.limiter.widen(&host, wait);
                    wait
                }
                None => self.policy.jittered(self.policy.backoff_delay(attempt - 1)),
            };

            self.stats.record_retry();
            std::thread::sleep(delay);
        }
    }

    fn send_once(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        if_none_match: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> Attempt {
        let mut request = match method {
            Method::Get => self.client.get(url),
            Method::Post => self.client.post(url),
        };
        if let Some(body) = body {
            request = request.body(body);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(etag) = if_none_match {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = match request.send() {
            Ok(r) => r,
            Err(source) => {
                return Attempt {
                    result: Err(FetchError::Transport {
                        url: url.to_string(),
                        source,
                    }),
                    retry_after: None,
                }
            }
        };

        let status = response.status();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|h| h.to_str().ok())
            .and_then(parse_retry_after);

        if status == StatusCode::NOT_FOUND {
            return Attempt {
                result: Err(FetchError::NotFound {
                    url: url.to_string(),
                }),
                retry_after,
            };
        }

        if !status.is_success() && status != StatusCode::NOT_MODIFIED {
            // The response is still in hand here, so capture whatever
            // explanation the server sent. Truncated, because an error page
            // can be megabytes of HTML and this ends up in a log line.
            let body = response.text().ok().map(truncate_on_boundary);
            return Attempt {
                result: Err(FetchError::HttpStatus {
                    url: url.to_string(),
                    status: status.as_u16(),
                    body,
                }),
                retry_after,
            };
        }

        // Take owned validators before `bytes()` consumes the response.
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|h| h.to_str().ok())
            .map(String::from);
        let last_modified = response
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|h| h.to_str().ok())
            .map(String::from);

        match response.bytes() {
            Ok(body) => Attempt {
                result: Ok(HttpResponse {
                    status: status.as_u16(),
                    bytes: body.to_vec(),
                    etag,
                    last_modified,
                }),
                retry_after,
            },
            Err(source) => Attempt {
                result: Err(FetchError::Transport {
                    url: url.to_string(),
                    source,
                }),
                retry_after,
            },
        }
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// Host key for rate limiting. An unparseable URL groups under a single
/// bucket rather than failing -- pacing an odd URL slightly wrong beats
/// returning an error the caller cannot act on.
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "<unparseable>".to_string())
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

    // ── HttpTransport::get ──────────────────────────────────────────────

    /// Millisecond delays and no jitter, so retry tests stay fast and
    /// deterministic.
    fn fast_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(4),
            jitter: false,
        }
    }

    fn test_transport(policy: RetryPolicy) -> HttpTransport {
        HttpTransport::new()
            .with_policy(policy)
            .with_limiter(HostLimiter::new(Duration::from_millis(0)))
    }

    #[test]
    fn get_returns_body_and_etag_on_200() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/ok")
            .with_status(200)
            .with_header("etag", "\"v1\"")
            .with_body("hello")
            .expect(1)
            .create();

        let t = test_transport(fast_policy(5));
        let resp = t.get(&format!("{}/ok", server.url()), None).unwrap();

        mock.assert();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.bytes, b"hello");
        assert_eq!(resp.etag.as_deref(), Some("\"v1\""));
    }

    #[test]
    fn get_sends_if_none_match_when_given_an_etag() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/cond")
            .match_header("if-none-match", "\"v1\"")
            .with_status(304)
            .expect(1)
            .create();

        let t = test_transport(fast_policy(5));
        let resp = t
            .get(&format!("{}/cond", server.url()), Some("\"v1\""))
            .unwrap();

        mock.assert();
        assert_eq!(resp.status, 304, "304 must surface as Ok, not an error");
    }

    #[test]
    fn get_maps_404_to_not_found_without_retrying() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/missing")
            .with_status(404)
            .expect(1)
            .create();

        let t = test_transport(fast_policy(5));
        let err = t
            .get(&format!("{}/missing", server.url()), None)
            .unwrap_err();

        mock.assert();
        assert!(matches!(err, FetchError::NotFound { .. }), "got {err:?}");
    }

    #[test]
    fn get_retries_5xx_and_succeeds() {
        let mut server = mockito::Server::new();
        let boom = server
            .mock("GET", "/flaky")
            .with_status(503)
            .expect(1)
            .create();
        let ok = server
            .mock("GET", "/flaky")
            .with_status(200)
            .with_body("recovered")
            .expect(1)
            .create();

        let t = test_transport(fast_policy(5));
        let resp = t.get(&format!("{}/flaky", server.url()), None).unwrap();

        boom.assert();
        ok.assert();
        assert_eq!(resp.bytes, b"recovered");
        assert_eq!(t.stats().retries, 1);
    }

    #[test]
    fn get_stops_after_max_attempts_and_returns_the_last_error() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/down")
            .with_status(500)
            .expect(3) // max_attempts = 3 means exactly three requests
            .create();

        let t = test_transport(fast_policy(3));
        let err = t.get(&format!("{}/down", server.url()), None).unwrap_err();

        mock.assert();
        assert!(
            matches!(err, FetchError::HttpStatus { status: 500, .. }),
            "got {err:?}"
        );
        assert_eq!(t.stats().failures, 1);
    }

    #[test]
    fn get_does_not_retry_a_400() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/bad")
            .with_status(400)
            .expect(1) // is_retryable() excludes 4xx other than 429
            .create();

        let t = test_transport(fast_policy(5));
        let err = t.get(&format!("{}/bad", server.url()), None).unwrap_err();

        mock.assert();
        assert!(
            matches!(err, FetchError::HttpStatus { status: 400, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn get_honours_retry_after_on_429() {
        let mut server = mockito::Server::new();
        let limited = server
            .mock("GET", "/limited")
            .with_status(429)
            .with_header("retry-after", "1")
            .expect(1)
            .create();
        let ok = server
            .mock("GET", "/limited")
            .with_status(200)
            .with_body("fine")
            .expect(1)
            .create();

        let t = test_transport(fast_policy(5));
        let start = std::time::Instant::now();
        let resp = t.get(&format!("{}/limited", server.url()), None).unwrap();

        limited.assert();
        ok.assert();
        assert_eq!(resp.bytes, b"fine");
        assert!(
            start.elapsed() >= Duration::from_secs(1),
            "Retry-After must be obeyed verbatim, not jittered down"
        );
        assert_eq!(t.stats().rate_limited, 1);
    }

    #[test]
    fn get_captures_last_modified_for_conditional_reuse() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/archive.gz")
            .with_status(200)
            .with_header("last-modified", "Wed, 10 Sep 2026 12:00:00 GMT")
            .with_header("etag", "\"abc\"")
            .with_body("payload")
            .expect(1)
            .create();

        let t = test_transport(fast_policy(5));
        let resp = t
            .get(&format!("{}/archive.gz", server.url()), None)
            .unwrap();

        mock.assert();
        assert_eq!(
            resp.last_modified.as_deref(),
            Some("Wed, 10 Sep 2026 12:00:00 GMT")
        );
        assert_eq!(resp.etag.as_deref(), Some("\"abc\""));
    }

    #[test]
    fn get_with_sends_caller_supplied_headers() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/hdr")
            .match_header("accept", "application/json")
            .match_header("snap-device-series", "16")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create();

        let t = test_transport(fast_policy(5));
        let resp = t
            .get_with(
                &format!("{}/hdr", server.url()),
                &[("Accept", "application/json"), ("Snap-Device-Series", "16")],
                None,
            )
            .unwrap();

        mock.assert();
        assert_eq!(resp.bytes, b"ok");
    }

    #[test]
    fn get_with_retries_carry_the_headers_on_every_attempt() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/hdr-retry")
            .match_header("accept", "application/json")
            .with_status(503)
            .expect(3)
            .create();

        let t = test_transport(fast_policy(3));
        let err = t
            .get_with(
                &format!("{}/hdr-retry", server.url()),
                &[("Accept", "application/json")],
                None,
            )
            .unwrap_err();

        // All three attempts matched the header matcher, so headers are not
        // lost on retry.
        mock.assert();
        assert!(matches!(err, FetchError::HttpStatus { status: 503, .. }));
    }

    #[test]
    fn post_sends_the_body_and_returns_the_response() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/query")
            .match_header("content-type", "application/json")
            .match_body(r#"{"package":"curl"}"#)
            .with_status(200)
            .with_body(r#"{"vulns":[]}"#)
            .expect(1)
            .create();

        let t = test_transport(fast_policy(5));
        let resp = t
            .post(
                &format!("{}/query", server.url()),
                &[("Content-Type", "application/json")],
                br#"{"package":"curl"}"#.to_vec(),
            )
            .unwrap();

        mock.assert();
        assert_eq!(resp.bytes, br#"{"vulns":[]}"#);
    }

    #[test]
    fn post_resends_the_body_on_every_retry() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/flaky")
            .match_body("payload")
            .with_status(503)
            .expect(3)
            .create();

        let t = test_transport(fast_policy(3));
        let err = t
            .post(&format!("{}/flaky", server.url()), &[], b"payload".to_vec())
            .unwrap_err();

        // All three attempts matched the body matcher, so the body is not
        // consumed by the first attempt.
        mock.assert();
        assert!(matches!(err, FetchError::HttpStatus { status: 503, .. }));
    }

    #[test]
    fn stats_track_a_successful_fetch() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/ok")
            .with_status(200)
            .with_body("x")
            .create();

        let t = test_transport(fast_policy(5));
        t.get(&format!("{}/ok", server.url()), None).unwrap();

        let snap = t.stats();
        assert_eq!(snap.attempts, 1);
        assert_eq!(snap.successes, 1);
        assert_eq!(snap.failures, 0);
    }

    #[test]
    fn with_client_actually_uses_the_supplied_client() {
        // The collectors that download whole-ecosystem archives build a
        // client with a long timeout and hand it over. If `with_client` were
        // ignored they would silently run on the 60s default and a large
        // transfer could never finish, restarting on every retry.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/slow")
            .with_status(200)
            .with_chunked_body(|_| {
                std::thread::sleep(Duration::from_millis(300));
                Ok(())
            })
            .create();

        let impatient = crate::enricher::http_client_builder()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        let t = HttpTransport::with_client(impatient)
            .with_policy(fast_policy(1))
            .with_limiter(HostLimiter::new(Duration::ZERO));

        let err = t.get(&format!("{}/slow", server.url()), None).unwrap_err();
        assert!(
            matches!(err, FetchError::Transport { .. }),
            "a 20ms client must time out on a 300ms body; got {err:?}"
        );
    }

    #[test]
    fn error_body_truncation_respects_utf8_boundaries() {
        // A localized error page can put a multibyte character across the
        // truncation point. String::truncate panics there, which would take
        // the whole collector down instead of returning a FetchError.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/boom")
            .with_status(400)
            // 511 ASCII bytes, then a 3-byte character straddling byte 512.
            .with_body(format!("{}\u{4e16}{}", "x".repeat(511), "y".repeat(200)))
            .create();

        let t = test_transport(fast_policy(1));
        let err = t.get(&format!("{}/boom", server.url()), None).unwrap_err();

        match &err {
            FetchError::HttpStatus { body, .. } => {
                let b = body.as_deref().unwrap_or("");
                assert!(
                    b.len() <= 600 && b.starts_with("xxx"),
                    "got {} bytes",
                    b.len()
                );
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[test]
    fn queued_waiters_do_not_burst_when_retry_after_expires() {
        use std::sync::Arc;
        // Three workers queue behind a host, then a `Retry-After` arrives.
        // They must come out the far side spaced by the widened interval.
        // Releasing them all on the deadline would hand the server the same
        // burst that got us rate limited in the first place.
        let limiter = Arc::new(HostLimiter::new(Duration::from_millis(100)));
        limiter.wait_turn("example.org"); // occupy the host

        let start = Instant::now();
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let l = Arc::clone(&limiter);
                std::thread::spawn(move || {
                    l.wait_turn("example.org");
                    Instant::now()
                })
            })
            .collect();

        // Let all three park in wait_turn behind the occupied host, then
        // land the embargo while they are still queued.
        std::thread::sleep(Duration::from_millis(20));
        limiter.widen("example.org", Duration::from_millis(400));

        let mut times: Vec<Instant> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        times.sort();

        assert!(
            times[0].duration_since(start) >= Duration::from_millis(350),
            "first waiter escaped the embargo after {:?}",
            times[0].duration_since(start)
        );
        for pair in times.windows(2) {
            let gap = pair[1].duration_since(pair[0]);
            assert!(
                gap >= Duration::from_millis(350),
                "waiters burst together: only {:?} apart",
                gap
            );
        }
    }

    #[test]
    fn retry_after_delays_the_next_reservation() {
        // widen() used to raise only the interval, leaving next_allowed
        // untouched -- so right after a `Retry-After: 60` the very next
        // caller still got a slot computed under the old 200ms pacing.
        let limiter = HostLimiter::new(Duration::from_millis(1));
        limiter.wait_turn("example.org");

        limiter.widen("example.org", Duration::from_secs(60));

        let start = Instant::now();
        let next = limiter.reserved_slot_for_test("example.org");
        assert!(
            next.duration_since(start) > Duration::from_secs(50),
            "next slot must fall inside the Retry-After window, was {:?} away",
            next.duration_since(start)
        );
    }

    #[test]
    fn failing_status_carries_the_server_explanation() {
        // Fuseki puts the SPARQL parse error in the body of its 400. Without
        // it the caller can only report "HTTP 400" and the operator has to
        // reproduce the query by hand to find out what was wrong with it.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/update")
            .with_status(400)
            .with_body("Parse error: line 1, column 8: Unresolved prefixed name: pkg:foo")
            .create();

        let t = test_transport(fast_policy(1));
        let err = t
            .post(&format!("{}/update", server.url()), &[], b"bad".to_vec())
            .unwrap_err();

        match &err {
            FetchError::HttpStatus { body, .. } => {
                let body = body.as_deref().unwrap_or("");
                assert!(
                    body.contains("Unresolved prefixed name"),
                    "body not captured, got {body:?}"
                );
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
        assert!(
            err.to_string().contains("Unresolved prefixed name"),
            "Display should surface it too: {err}"
        );
    }

    #[test]
    fn failing_status_body_is_truncated() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/update")
            .with_status(500)
            .with_body("x".repeat(10_000))
            .create();

        let t = test_transport(fast_policy(1));
        let err = t
            .post(&format!("{}/update", server.url()), &[], b"q".to_vec())
            .unwrap_err();

        match &err {
            FetchError::HttpStatus { body, .. } => {
                let body = body.as_deref().unwrap_or("");
                assert!(
                    body.len() <= 600,
                    "a 10k HTML error page should not reach the log whole, got {}",
                    body.len()
                );
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_default_limiter_paces_rate_sensitive_hosts() {
        let limiter = HostLimiter::default();

        // Hosts that used to be paced by a hand-rolled `rate_limit(SLOW_RATE_LIMIT)`
        // in their collector.
        for host in [
            "repology.org",
            "bodhi.fedoraproject.org",
            "security.gentoo.org",
            "aur.archlinux.org",
        ] {
            assert_eq!(
                limiter.interval_for(host),
                Duration::from_secs(1),
                "{host} should be paced at 1s"
            );
        }

        // Hosts whose collectors slept 500ms per iteration.
        for host in [
            "api.github.com",
            "api.osv.dev",
            "access.redhat.com",
            "koji.fedoraproject.org",
        ] {
            assert_eq!(
                limiter.interval_for(host),
                Duration::from_millis(500),
                "{host} should be paced at 500ms"
            );
        }

        // Anything unlisted falls back to the default.
        assert_eq!(
            limiter.interval_for("registry.npmjs.org"),
            DEFAULT_RATE_LIMIT
        );
    }

    #[test]
    fn test_widen_still_overrides_a_table_entry() {
        let limiter = HostLimiter::default();
        limiter.widen("api.github.com", Duration::from_secs(30));
        assert_eq!(
            limiter.interval_for("api.github.com"),
            Duration::from_secs(30),
            "a Retry-After should still be able to slow a table-listed host"
        );
        // ...but never speed one up.
        limiter.widen("repology.org", Duration::from_millis(1));
        assert_eq!(limiter.interval_for("repology.org"), Duration::from_secs(1));
    }
}
