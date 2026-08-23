//! HTTP response cache with hash-sharded storage and atomic writes.
//!
//! Designed for collector-level caching of per-package HTTP responses
//! (PyPI JSON, Maven metadata, etc.). Uses SHA-256 URL hashing with
//! single-level prefix sharding for O(1) lookups that scale to hundreds
//! of thousands of entries.
//!
//! Cache I/O errors are returned as `io::Result` and are intended to be
//! handled (logged, ignored) by the caller — they never propagate as
//! [`FetchError`](crate::fetch_error::FetchError).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ── Clock trait (public, not behind #[cfg(test)]) ───────────────────────

/// Abstraction over system time for deterministic testing.
pub trait Clock: Send + Sync {
    /// Returns the current time.
    fn now(&self) -> SystemTime;
}

/// Real system clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Deterministic clock for testing. Advances via `advance()`.
pub struct MockClock(AtomicU64);

impl MockClock {
    /// Create a mock clock set to the given number of seconds since UNIX epoch.
    pub fn new(secs_since_epoch: u64) -> Self {
        Self(AtomicU64::new(secs_since_epoch))
    }

    /// Advance the clock by the given number of seconds.
    pub fn advance(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::Relaxed);
    }
}

impl Clock for MockClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.0.load(Ordering::Relaxed))
    }
}

// ── Entry metadata ──────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct EntryMeta {
    url: String,
    fetched_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    status_code: u16,
    body_sha256: String,
    body_length: u64,
}

// ── CacheEntry (public) ─────────────────────────────────────────────────

/// A cached HTTP response.
#[derive(Debug)]
pub struct CacheEntry {
    pub url: String,
    pub body: Vec<u8>,
    pub fetched_at: String,
    pub expires_at: Option<String>,
    pub ttl_seconds: Option<u64>,
    pub etag: Option<String>,
    pub status_code: u16,
}

// ── HttpCache ───────────────────────────────────────────────────────────

/// HTTP response cache with hash-sharded file storage.
///
/// Storage layout: `{base_dir}/{collector}/{hash[0..2]}/{sha256}.entry`
#[derive(Clone)]
pub struct HttpCache {
    base_dir: PathBuf,
    #[allow(dead_code)]
    collector_name: String,
    clock: Arc<dyn Clock>,
}

/// Global counter for unique temp file names within a process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl HttpCache {
    /// Create a new cache using the real system clock.
    pub fn new(cache_dir: &str, collector: &str) -> io::Result<Self> {
        Self::with_clock(cache_dir, collector, Arc::new(SystemClock))
    }

    /// Create a new cache with a custom clock (for testing).
    pub fn with_clock(cache_dir: &str, collector: &str, clock: Arc<dyn Clock>) -> io::Result<Self> {
        let base_dir = PathBuf::from(cache_dir).join(collector);
        fs::create_dir_all(&base_dir)?;
        Ok(Self {
            base_dir,
            collector_name: collector.to_string(),
            clock,
        })
    }

    /// Return the root cache directory (parent of the collector-specific dir).
    ///
    /// Useful when a collector needs to create sibling cache instances
    /// for different content types (e.g. `maven-search` and `maven-pom`).
    pub fn base_dir_str(&self) -> &str {
        self.base_dir
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("")
    }

    /// Create a sibling cache sharing the same root dir and clock.
    ///
    /// Useful when a collector needs separate namespaces for different
    /// content types (e.g. `maven-search` vs `maven-pom`) but must
    /// share the clock for deterministic testing.
    pub fn sibling(&self, collector: &str) -> io::Result<Self> {
        let parent = self.base_dir.parent().unwrap_or(&self.base_dir);
        Self::with_clock(
            parent.to_str().unwrap_or(""),
            collector,
            self.clock.clone(),
        )
    }

    /// Return a fresh (non-expired) cached entry, or None.
    ///
    /// Entries that fail integrity checks are evicted automatically.
    /// Filesystem errors are propagated as `io::Error` so callers can
    /// log them before deciding to treat the read as a miss.
    pub fn get_fresh(&self, url: &str) -> io::Result<Option<CacheEntry>> {
        let path = self.entry_path(url);
        let (meta, body) = match self.read_envelope(&path) {
            Ok(Some(pair)) => pair,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        };

        // Integrity checks (evict on failure)
        if !self.check_integrity(url, &meta, &body, &path) {
            return Ok(None);
        }

        // Expiry check
        if let Some(ref expires_str) = meta.expires_at {
            if let Ok(expires) = parse_rfc3339(expires_str) {
                if self.clock.now() >= expires {
                    return Ok(None);
                }
            }
        }

        Ok(Some(to_cache_entry(meta, body)))
    }

    /// Return a cached entry even if expired, or None.
    ///
    /// Still checks integrity — corrupt entries are evicted.
    /// Filesystem errors are propagated as `io::Error` so callers can
    /// log them before deciding to treat the read as a miss.
    pub fn get_stale(&self, url: &str) -> io::Result<Option<CacheEntry>> {
        let path = self.entry_path(url);
        let (meta, body) = match self.read_envelope(&path) {
            Ok(Some(pair)) => pair,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        };

        if !self.check_integrity(url, &meta, &body, &path) {
            return Ok(None);
        }

        Ok(Some(to_cache_entry(meta, body)))
    }

    /// Store a response in the cache.
    ///
    /// If `ttl` is None, the entry never expires (indefinite).
    /// Uses atomic rename to prevent partial reads. Acquires an
    /// entry-scoped lock file to coordinate with `touch()`.
    pub fn put(
        &self,
        url: &str,
        body: &[u8],
        etag: Option<&str>,
        status_code: u16,
        ttl: Option<Duration>,
    ) -> io::Result<()> {
        let now = self.clock.now();
        let fetched_at = format_rfc3339(now);
        let (expires_at, ttl_seconds) = match ttl {
            Some(d) => (Some(format_rfc3339(now + d)), Some(d.as_secs())),
            None => (None, None),
        };

        let body_hash = sha256_hex(body);
        let meta = EntryMeta {
            url: url.to_string(),
            fetched_at,
            expires_at,
            ttl_seconds,
            etag: etag.map(|s| s.to_string()),
            status_code,
            body_sha256: body_hash,
            body_length: body.len() as u64,
        };

        let path = self.entry_path(url);
        let lock_path = self.lock_path(url);

        // Ensure shard directory exists (needed for both lock and entry)
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        lock_exclusive(&lock_file)?;

        let result = self.write_envelope_atomic(&path, &meta, body);

        unlock(&lock_file)?;
        result
    }

    /// Refresh the timestamp of an existing entry without changing its body.
    ///
    /// Recalculates `expires_at` from the stored `ttl_seconds`. If `etag`
    /// is `Some`, updates the ETag; if `None`, preserves the existing one.
    ///
    /// Uses a lock file separate from the .entry file to coordinate with
    /// `put()` -- the lock survives atomic renames.
    /// No-op if the entry does not exist.
    pub fn touch(&self, url: &str, etag: Option<&str>) -> io::Result<()> {
        let path = self.entry_path(url);
        if !path.exists() {
            return Ok(());
        }

        let lock_path = self.lock_path(url);

        // Acquire exclusive lock on the lock file
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        lock_exclusive(&lock_file)?;

        // Read the current envelope under lock
        let raw = match fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                unlock(&lock_file)?;
                return Ok(());
            }
            Err(e) => {
                unlock(&lock_file)?;
                return Err(e);
            }
        };
        let (mut meta, body) = match parse_envelope(&raw) {
            Some(pair) => pair,
            None => {
                unlock(&lock_file)?;
                return Ok(());
            }
        };

        // Update timestamps
        let now = self.clock.now();
        meta.fetched_at = format_rfc3339(now);
        meta.expires_at = meta
            .ttl_seconds
            .map(|secs| format_rfc3339(now + Duration::from_secs(secs)));

        // Optionally update etag
        if let Some(new_etag) = etag {
            meta.etag = Some(new_etag.to_string());
        }

        // Write atomically, then release lock
        let result = self.write_envelope_atomic(&path, &meta, &body);
        unlock(&lock_file)?;
        result
    }

    /// Remove a cached entry.
    ///
    /// Returns `Ok(())` if the entry was removed or did not exist.
    /// Other filesystem errors (permissions, etc.) are propagated so
    /// callers can log them.
    pub fn evict(&self, url: &str) -> io::Result<()> {
        let path = self.entry_path(url);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    // ── internal helpers ────────────────────────────────────────────────

    /// Compute the sharded file path for a URL.
    fn entry_path(&self, url: &str) -> PathBuf {
        let hash = sha256_hex(url.as_bytes());
        let shard = &hash[0..2];
        self.base_dir.join(shard).join(format!("{}.entry", hash))
    }

    /// Compute the lock file path for a URL (same shard dir, .lock extension).
    ///
    /// The lock file is separate from the .entry file so the lock survives
    /// atomic renames during `put()`.
    fn lock_path(&self, url: &str) -> PathBuf {
        let hash = sha256_hex(url.as_bytes());
        let shard = &hash[0..2];
        self.base_dir.join(shard).join(format!("{}.lock", hash))
    }

    /// Read and parse an envelope file, returning metadata + body.
    ///
    /// If the file exists but cannot be parsed (corrupt metadata, missing
    /// newline), the file is deleted so it does not cause repeated parse
    /// failures on every request.
    fn read_envelope(&self, path: &PathBuf) -> io::Result<Option<(EntryMeta, Vec<u8>)>> {
        let raw = match fs::read(path) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };

        match parse_envelope(&raw) {
            Some(pair) => Ok(Some(pair)),
            None => {
                eprintln!(
                    "Warning: evicting unparseable cache file: {}",
                    path.display()
                );
                let _ = fs::remove_file(path);
                Ok(None)
            }
        }
    }

    /// Write metadata + body atomically via temp file + rename.
    fn write_envelope_atomic(
        &self,
        target: &PathBuf,
        meta: &EntryMeta,
        body: &[u8],
    ) -> io::Result<()> {
        // Ensure shard directory exists
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }

        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp_name = format!(
            "{}.entry.{}.{}.tmp",
            target
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown"),
            pid,
            counter
        );
        let tmp_path = target.parent().unwrap().join(tmp_name);

        let meta_json = serde_json::to_string(meta).map_err(io::Error::other)?;

        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(meta_json.as_bytes())?;
        file.write_all(b"\n")?;
        file.write_all(body)?;
        file.sync_all()?;
        drop(file);

        fs::rename(&tmp_path, target)?;
        Ok(())
    }

    /// Validate integrity of a parsed envelope. Evicts and returns false on failure.
    fn check_integrity(&self, url: &str, meta: &EntryMeta, body: &[u8], path: &PathBuf) -> bool {
        // URL match
        if meta.url != url {
            eprintln!(
                "Warning: cache URL mismatch for {}: expected {}, got {}. Evicting.",
                path.display(),
                url,
                meta.url
            );
            let _ = fs::remove_file(path);
            return false;
        }

        // Body length
        if meta.body_length != body.len() as u64 {
            eprintln!(
                "Warning: cache body length mismatch for {}: expected {}, got {}. Evicting.",
                path.display(),
                meta.body_length,
                body.len()
            );
            let _ = fs::remove_file(path);
            return false;
        }

        // Body SHA-256
        let actual_hash = sha256_hex(body);
        if meta.body_sha256 != actual_hash {
            eprintln!(
                "Warning: cache body hash mismatch for {}. Evicting.",
                path.display()
            );
            let _ = fs::remove_file(path);
            return false;
        }

        true
    }
}

// ── Free functions ──────────────────────────────────────────────────────

/// Parse an envelope (first-line JSON + body after newline).
fn parse_envelope(raw: &[u8]) -> Option<(EntryMeta, Vec<u8>)> {
    let newline_pos = raw.iter().position(|&b| b == b'\n')?;
    let meta_bytes = &raw[..newline_pos];
    let body = raw[newline_pos + 1..].to_vec();

    let meta: EntryMeta = match serde_json::from_slice(meta_bytes) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Warning: corrupt cache metadata: {}", e);
            return None;
        }
    };

    Some((meta, body))
}

fn to_cache_entry(meta: EntryMeta, body: Vec<u8>) -> CacheEntry {
    CacheEntry {
        url: meta.url,
        body,
        fetched_at: meta.fetched_at,
        expires_at: meta.expires_at,
        ttl_seconds: meta.ttl_seconds,
        etag: meta.etag,
        status_code: meta.status_code,
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Format a SystemTime as RFC 3339 (simplified: always UTC, second precision).
fn format_rfc3339(t: SystemTime) -> String {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    // Convert to calendar date/time
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Civil date from days since epoch (algorithm from Howard Hinnant)
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hours, minutes, seconds
    )
}

/// Parse an RFC 3339 timestamp back to SystemTime.
fn parse_rfc3339(s: &str) -> Result<SystemTime, ()> {
    // Expected format: YYYY-MM-DDThh:mm:ssZ
    if s.len() < 20 {
        return Err(());
    }
    let b = s.as_bytes();
    let year: i64 = parse_int(&b[0..4]).ok_or(())?;
    let month: u64 = parse_int(&b[5..7]).ok_or(())?;
    let day: u64 = parse_int(&b[8..10]).ok_or(())?;
    let hour: u64 = parse_int(&b[11..13]).ok_or(())?;
    let min: u64 = parse_int(&b[14..16]).ok_or(())?;
    let sec: u64 = parse_int(&b[17..19]).ok_or(())?;

    // Convert civil date to days since epoch (inverse of format_rfc3339)
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146097) as u64 + doe - 719468;

    let total_secs = days * 86400 + hour * 3600 + min * 60 + sec;
    Ok(UNIX_EPOCH + Duration::from_secs(total_secs))
}

fn parse_int<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

// ── File locking (Unix) ─────────────────────────────────────────────────

#[cfg(unix)]
fn lock_exclusive(file: &fs::File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if ret != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn unlock(file: &fs::File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_UN) };
    if ret != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// Known limitation: on non-Unix platforms (Windows), file locking is a no-op.
// Concurrent put() and touch() calls may race, potentially producing a
// corrupted entry. This is acceptable because:
//   1. The collectors run on Linux in production (MicroShift cluster).
//   2. Integrity checks in get_fresh/get_stale detect and evict any corruption.
//   3. The worst case is a cache miss, not data loss.
#[cfg(not(unix))]
fn lock_exclusive(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn unlock(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    fn make_cache(tmp: &TempDir, clock: Arc<MockClock>) -> HttpCache {
        HttpCache::with_clock(tmp.path().to_str().unwrap(), "test-collector", clock)
            .expect("cache creation should succeed")
    }

    // ── Basic operations ────────────────────────────────────────────

    #[test]
    fn test_get_fresh_returns_fresh_entry() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put(
                "http://example.com/pkg",
                b"hello world",
                Some("\"etag-1\""),
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        let entry = cache
            .get_fresh("http://example.com/pkg")
            .unwrap()
            .expect("should return fresh entry");
        assert_eq!(entry.body, b"hello world");
        assert_eq!(entry.status_code, 200);
        assert_eq!(entry.etag.as_deref(), Some("\"etag-1\""));
        assert_eq!(entry.url, "http://example.com/pkg");
    }

    #[test]
    fn test_get_fresh_returns_none_for_expired() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put(
                "http://example.com/pkg",
                b"data",
                None,
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        // Advance past TTL
        clock.advance(3601);

        let result = cache.get_fresh("http://example.com/pkg").unwrap();
        assert!(
            result.is_none(),
            "expired entry should not be returned by get_fresh"
        );
    }

    #[test]
    fn test_get_stale_returns_expired_entry() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put(
                "http://example.com/pkg",
                b"stale-data",
                None,
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120); // Well past TTL

        let fresh = cache.get_fresh("http://example.com/pkg").unwrap();
        assert!(fresh.is_none(), "get_fresh should return None for expired");

        let stale = cache
            .get_stale("http://example.com/pkg")
            .unwrap()
            .expect("get_stale should return expired entry");
        assert_eq!(stale.body, b"stale-data");
    }

    #[test]
    fn test_get_stale_discards_corrupt_entry() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put("http://example.com/pkg", b"good data", None, 200, None)
            .unwrap();

        // Corrupt the body by writing garbage to the entry file
        let path = cache.entry_path("http://example.com/pkg");
        let raw = fs::read(&path).unwrap();
        let newline = raw.iter().position(|&b| b == b'\n').unwrap();
        let mut corrupted = raw[..newline + 1].to_vec();
        corrupted.extend_from_slice(b"CORRUPTED");
        fs::write(&path, &corrupted).unwrap();

        let result = cache.get_stale("http://example.com/pkg").unwrap();
        assert!(result.is_none(), "corrupt entry should be discarded");
        assert!(!path.exists(), "corrupt entry file should be evicted");
    }

    #[test]
    fn test_put_no_ttl_indefinite() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put("http://example.com/forever", b"eternal", None, 200, None)
            .unwrap();

        // Advance a huge amount of time
        clock.advance(365 * 86400 * 100); // 100 years

        let entry = cache
            .get_fresh("http://example.com/forever")
            .unwrap()
            .expect("indefinite entry should always be fresh");
        assert_eq!(entry.body, b"eternal");
        assert!(entry.expires_at.is_none());
        assert!(entry.ttl_seconds.is_none());
    }

    #[test]
    fn test_put_with_ttl_expires() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put(
                "http://example.com/timed",
                b"timed-data",
                None,
                200,
                Some(Duration::from_secs(300)),
            )
            .unwrap();

        // Still fresh
        clock.advance(299);
        assert!(cache
            .get_fresh("http://example.com/timed")
            .unwrap()
            .is_some());

        // Now expired
        clock.advance(2);
        assert!(cache
            .get_fresh("http://example.com/timed")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_put_404_with_short_ttl() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put(
                "http://example.com/missing",
                b"",
                None,
                404,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        let entry = cache
            .get_fresh("http://example.com/missing")
            .unwrap()
            .expect("404 should be cached");
        assert_eq!(entry.status_code, 404);
        assert_eq!(entry.body, b"");
    }

    #[test]
    fn test_status_dependent_ttl() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        // 404 with 1h TTL
        cache
            .put(
                "http://example.com/a",
                b"",
                None,
                404,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        // 200 with 24h TTL
        cache
            .put(
                "http://example.com/b",
                b"content",
                None,
                200,
                Some(Duration::from_secs(86400)),
            )
            .unwrap();

        // Advance 2 hours — 404 expired, 200 still fresh
        clock.advance(7200);

        assert!(
            cache.get_fresh("http://example.com/a").unwrap().is_none(),
            "404 with 1h TTL should be expired after 2h"
        );
        assert!(
            cache.get_fresh("http://example.com/b").unwrap().is_some(),
            "200 with 24h TTL should still be fresh after 2h"
        );
    }

    #[test]
    fn test_touch_refreshes_timestamp() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put(
                "http://example.com/pkg",
                b"data",
                Some("\"etag-1\""),
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        // Advance 3000s (still within TTL of 3600)
        clock.advance(3000);

        // Touch to refresh
        cache.touch("http://example.com/pkg", None).unwrap();

        // Advance another 3000s (would have expired without touch)
        clock.advance(3000);

        let entry = cache
            .get_fresh("http://example.com/pkg")
            .unwrap()
            .expect("touched entry should still be fresh");
        assert_eq!(entry.body, b"data");

        // Verify body hash is unchanged
        let body_hash = sha256_hex(b"data");
        // Read the entry file and check metadata
        let path = cache.entry_path("http://example.com/pkg");
        let raw = fs::read(&path).unwrap();
        let (meta, _) = parse_envelope(&raw).unwrap();
        assert_eq!(meta.body_sha256, body_hash);
    }

    #[test]
    fn test_touch_preserves_etag_when_none() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put(
                "http://example.com/pkg",
                b"data",
                Some("\"original-etag\""),
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        clock.advance(100);
        cache.touch("http://example.com/pkg", None).unwrap();

        let entry = cache
            .get_fresh("http://example.com/pkg")
            .unwrap()
            .expect("entry should exist");
        assert_eq!(
            entry.etag.as_deref(),
            Some("\"original-etag\""),
            "etag should be preserved when touch passes None"
        );
    }

    #[test]
    fn test_touch_updates_etag_when_provided() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put(
                "http://example.com/pkg",
                b"data",
                Some("\"old-etag\""),
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        clock.advance(100);
        cache
            .touch("http://example.com/pkg", Some("\"new-etag\""))
            .unwrap();

        let entry = cache
            .get_fresh("http://example.com/pkg")
            .unwrap()
            .expect("entry should exist");
        assert_eq!(entry.etag.as_deref(), Some("\"new-etag\""));
    }

    #[test]
    fn test_touch_nonexistent_is_noop() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        // Should not error
        cache.touch("http://example.com/nonexistent", None).unwrap();
    }

    #[test]
    fn test_evict_removes_entry() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put("http://example.com/pkg", b"data", None, 200, None)
            .unwrap();

        assert!(cache.get_stale("http://example.com/pkg").unwrap().is_some());

        cache.evict("http://example.com/pkg").unwrap();

        assert!(
            cache.get_stale("http://example.com/pkg").unwrap().is_none(),
            "evicted entry should not be returned"
        );
    }

    // ── Integrity ───────────────────────────────────────────────────

    #[test]
    fn test_corrupt_body_hash_evicted_get_fresh() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put(
                "http://example.com/pkg",
                b"original",
                None,
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        // Corrupt by modifying body bytes in-place
        let path = cache.entry_path("http://example.com/pkg");
        let raw = fs::read(&path).unwrap();
        let newline = raw.iter().position(|&b| b == b'\n').unwrap();
        let mut corrupted = raw[..newline + 1].to_vec();
        corrupted.extend_from_slice(b"tampered");
        fs::write(&path, &corrupted).unwrap();

        assert!(
            cache.get_fresh("http://example.com/pkg").unwrap().is_none(),
            "corrupt entry should be rejected by get_fresh"
        );
        assert!(!path.exists(), "corrupt entry should be evicted");
    }

    #[test]
    fn test_corrupt_body_hash_evicted_get_stale() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put("http://example.com/pkg", b"original", None, 200, None)
            .unwrap();

        let path = cache.entry_path("http://example.com/pkg");
        let raw = fs::read(&path).unwrap();
        let newline = raw.iter().position(|&b| b == b'\n').unwrap();
        let mut corrupted = raw[..newline + 1].to_vec();
        corrupted.extend_from_slice(b"tampered");
        fs::write(&path, &corrupted).unwrap();

        assert!(
            cache.get_stale("http://example.com/pkg").unwrap().is_none(),
            "corrupt entry should be rejected by get_stale"
        );
        assert!(!path.exists(), "corrupt entry should be evicted");
    }

    #[test]
    fn test_corrupt_metadata_json_evicted() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put("http://example.com/pkg", b"data", None, 200, None)
            .unwrap();

        let path = cache.entry_path("http://example.com/pkg");
        fs::write(&path, b"NOT VALID JSON\nbody bytes").unwrap();

        assert!(cache.get_fresh("http://example.com/pkg").unwrap().is_none());
        assert!(
            !path.exists(),
            "corrupt metadata file should be evicted on first access"
        );
    }

    #[test]
    fn test_truncated_envelope_no_newline() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        cache
            .put("http://example.com/pkg", b"data", None, 200, None)
            .unwrap();

        let path = cache.entry_path("http://example.com/pkg");
        fs::write(&path, b"truncated-no-newline").unwrap();

        assert!(cache.get_fresh("http://example.com/pkg").unwrap().is_none());
        assert!(
            !path.exists(),
            "truncated file should be evicted by get_fresh"
        );
    }

    #[test]
    fn test_url_mismatch_evicted() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        // Write a valid entry for URL "a"
        cache
            .put("http://example.com/a", b"body-a", None, 200, None)
            .unwrap();

        // Copy that entry file to the path where URL "b" would be stored
        let path_a = cache.entry_path("http://example.com/a");
        let path_b = cache.entry_path("http://example.com/b");
        if let Some(parent) = path_b.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::copy(&path_a, &path_b).unwrap();

        // Looking up "b" should detect the URL mismatch and evict
        assert!(
            cache.get_fresh("http://example.com/b").unwrap().is_none(),
            "URL mismatch should be rejected"
        );
        assert!(!path_b.exists(), "mismatched entry should be evicted");
        // Original entry should be unaffected
        assert!(cache.get_fresh("http://example.com/a").unwrap().is_some());
    }

    // ── Atomicity ───────────────────────────────────────────────────

    #[test]
    fn test_concurrent_put_both_succeed() {
        use std::sync::Barrier;

        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = Arc::new(make_cache(&tmp, clock.clone()));
        let barrier = Arc::new(Barrier::new(2));

        let mut handles = vec![];
        for i in 0..2 {
            let c = Arc::clone(&cache);
            let b = Arc::clone(&barrier);
            let body = format!("body-{}", i);
            handles.push(std::thread::spawn(move || {
                b.wait(); // Force both threads to contend
                c.put(
                    "http://example.com/concurrent",
                    body.as_bytes(),
                    None,
                    200,
                    Some(Duration::from_secs(3600)),
                )
                .unwrap();
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Final entry should be valid (one of the two writes wins)
        let entry = cache
            .get_fresh("http://example.com/concurrent")
            .unwrap()
            .expect("entry should exist after concurrent puts");
        assert!(
            entry.body == b"body-0" || entry.body == b"body-1",
            "entry should contain one of the written bodies"
        );
    }

    #[test]
    fn test_interrupted_write_tmp_only_no_entry() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        // Create a .tmp file in the shard directory but no .entry
        let url = "http://example.com/interrupted";
        let hash = sha256_hex(url.as_bytes());
        let shard = &hash[0..2];
        let shard_dir = tmp.path().join("test-collector").join(shard);
        fs::create_dir_all(&shard_dir).unwrap();
        fs::write(
            shard_dir.join(format!("{}.entry.999.0.tmp", hash)),
            b"partial write",
        )
        .unwrap();

        // get_fresh should return None, not crash
        assert!(cache.get_fresh(url).unwrap().is_none());
    }

    #[test]
    fn test_interrupted_put_does_not_corrupt_existing_entry() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        let url = "http://example.com/survive";

        // Write a valid entry
        cache
            .put(
                url,
                b"valid-data",
                None,
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        // Simulate a crashed put: .tmp exists alongside the valid .entry
        let hash = sha256_hex(url.as_bytes());
        let shard = &hash[0..2];
        let shard_dir = tmp.path().join("test-collector").join(shard);
        fs::write(
            shard_dir.join(format!("{}.entry.888.0.tmp", hash)),
            b"crashed-write-data",
        )
        .unwrap();

        // Original .entry should still be readable
        let entry = cache
            .get_fresh(url)
            .unwrap()
            .expect("original entry should survive a crashed concurrent put");
        assert_eq!(entry.body, b"valid-data");
    }

    #[test]
    fn test_interrupted_touch_leaves_entry_valid() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        let url = "http://example.com/touch-crash";

        // Write a valid entry
        cache
            .put(url, b"original", None, 200, Some(Duration::from_secs(3600)))
            .unwrap();

        // Simulate a crashed touch: .tmp left behind
        let hash = sha256_hex(url.as_bytes());
        let shard = &hash[0..2];
        let shard_dir = tmp.path().join("test-collector").join(shard);
        fs::write(
            shard_dir.join(format!("{}.entry.777.0.tmp", hash)),
            b"touch-crashed",
        )
        .unwrap();

        // Original .entry must remain valid
        let entry = cache
            .get_fresh(url)
            .unwrap()
            .expect("entry should be valid despite crashed touch .tmp");
        assert_eq!(entry.body, b"original");
    }

    #[test]
    fn test_touch_does_not_overwrite_newer_put() {
        use std::sync::Barrier;

        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = Arc::new(make_cache(&tmp, clock.clone()));

        let url = "http://example.com/lock-test";

        // Initial put (v1)
        cache
            .put(
                url,
                b"v1",
                Some("\"etag-v1\""),
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        // Use a barrier to force both threads to contend on the lock
        let barrier = Arc::new(Barrier::new(2));

        let cache2 = Arc::clone(&cache);
        let barrier2 = Arc::clone(&barrier);
        let put_handle = std::thread::spawn(move || {
            barrier2.wait();
            cache2
                .put(
                    "http://example.com/lock-test",
                    b"v2-newer",
                    Some("\"etag-v2\""),
                    200,
                    Some(Duration::from_secs(3600)),
                )
                .unwrap();
        });

        let cache3 = Arc::clone(&cache);
        let barrier3 = Arc::clone(&barrier);
        let touch_handle = std::thread::spawn(move || {
            barrier3.wait();
            cache3.touch("http://example.com/lock-test", None).unwrap();
        });

        put_handle.join().unwrap();
        touch_handle.join().unwrap();

        // Final entry must be valid regardless of ordering
        let entry = cache
            .get_fresh(url)
            .unwrap()
            .expect("entry should exist after concurrent put + touch");

        // The put wrote v2-newer. Regardless of whether touch ran before or
        // after the put, the final body must be v2-newer:
        //   - put before touch: touch refreshes v2-newer, body = v2-newer
        //   - touch before put: touch refreshes v1, then put overwrites with v2-newer
        assert_eq!(
            entry.body, b"v2-newer",
            "final body must be from the newer put, not stale v1"
        );

        // Also verify structural integrity
        let path = cache.entry_path(url);
        let raw = fs::read(&path).unwrap();
        let (meta, body) = parse_envelope(&raw).expect("entry must be parseable");
        assert_eq!(
            meta.body_sha256,
            sha256_hex(&body),
            "body hash must match after concurrent put + touch"
        );
    }

    // ── Sharding ────────────────────────────────────────────────────

    #[test]
    fn test_sharding_100_entries() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock.clone());

        let mut shard_dirs: HashSet<String> = HashSet::new();

        for i in 0..100 {
            let url = format!("http://example.com/pkg/{}", i);
            cache
                .put(&url, format!("body-{}", i).as_bytes(), None, 200, None)
                .unwrap();

            let hash = sha256_hex(url.as_bytes());
            shard_dirs.insert(hash[0..2].to_string());
        }

        let collector_dir = tmp.path().join("test-collector");

        // All entries should be in shard subdirectories, not in the base
        let base_entries: Vec<_> = fs::read_dir(&collector_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("entry"))
            .collect();
        assert!(
            base_entries.is_empty(),
            "no .entry files should be in the base collector directory"
        );

        // Verify entries are in shard dirs
        let mut total_entries = 0;
        let mut max_per_shard = 0;
        for entry in fs::read_dir(&collector_dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                let dir_name = entry.file_name().to_str().unwrap().to_string();
                assert_eq!(
                    dir_name.len(),
                    2,
                    "shard directory name should be 2 hex chars"
                );
                let count = fs::read_dir(entry.path())
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("entry"))
                    .count();
                total_entries += count;
                if count > max_per_shard {
                    max_per_shard = count;
                }
            }
        }

        assert_eq!(total_entries, 100, "all 100 entries should be stored");
        // With SHA-256 and 256 possible shard dirs, 100 entries should distribute
        // reasonably — no shard should have more than ~20 entries
        assert!(
            max_per_shard <= 20,
            "no shard should have more than 20 of 100 entries, got {}",
            max_per_shard
        );
    }

    // ── Time (MockClock only, no sleeps) ────────────────────────────

    #[test]
    fn test_all_time_uses_mock_clock() {
        // This test verifies the MockClock works correctly
        let clock = MockClock::new(0);
        assert_eq!(clock.now(), UNIX_EPOCH);

        clock.advance(86400);
        assert_eq!(clock.now(), UNIX_EPOCH + Duration::from_secs(86400));

        clock.advance(3600);
        assert_eq!(clock.now(), UNIX_EPOCH + Duration::from_secs(90000));
    }

    // ── RFC 3339 round-trip ─────────────────────────────────────────

    #[test]
    fn test_rfc3339_roundtrip() {
        let times = [
            0u64,            // epoch
            1_000_000,       // some time in 2001
            1_719_792_000,   // 2024-07-01 approx
            253_402_300_799, // 9999-12-31T23:59:59Z
        ];

        for &secs in &times {
            let t = UNIX_EPOCH + Duration::from_secs(secs);
            let formatted = format_rfc3339(t);
            let parsed = parse_rfc3339(&formatted)
                .expect(&format!("should parse formatted time for epoch={}", secs));
            let parsed_secs = parsed.duration_since(UNIX_EPOCH).unwrap().as_secs();
            assert_eq!(
                parsed_secs, secs,
                "round-trip failed for epoch={}: formatted={}, parsed_secs={}",
                secs, formatted, parsed_secs
            );
        }
    }

    #[test]
    fn test_get_fresh_nonexistent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock);

        assert!(cache
            .get_fresh("http://example.com/nope")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_get_stale_nonexistent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock);

        assert!(cache
            .get_stale("http://example.com/nope")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_evict_nonexistent_is_noop() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock);

        // Should not error
        cache.evict("http://example.com/nope").unwrap();
    }

    #[test]
    fn test_binary_body_preserved() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock);

        // Body with embedded newlines, nulls, and high bytes
        let body: Vec<u8> = (0..=255).collect();
        cache
            .put("http://example.com/binary", &body, None, 200, None)
            .unwrap();

        let entry = cache
            .get_fresh("http://example.com/binary")
            .unwrap()
            .expect("should return binary entry");
        assert_eq!(entry.body, body, "binary body should be preserved exactly");
    }

    #[test]
    fn test_empty_body() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let cache = make_cache(&tmp, clock);

        cache
            .put("http://example.com/empty", b"", None, 204, None)
            .unwrap();

        let entry = cache
            .get_fresh("http://example.com/empty")
            .unwrap()
            .expect("should return entry with empty body");
        assert!(entry.body.is_empty());
        assert_eq!(entry.status_code, 204);
    }
}
