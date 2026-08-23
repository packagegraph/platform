//! Cached-fetch state machine for HTTP collector operations.
//!
//! Wraps [`HttpCache`] with a complete fetch-or-serve-from-cache workflow
//! that handles conditional GETs, validation, stale fallback on transport
//! errors, and negative caching for 404s. Collectors provide an `http_get`
//! closure -- `CachedFetcher` does NOT own the HTTP client.
//!
//! **Cache I/O is observationally nonfatal.** Read errors degrade to cache
//! misses; write errors are logged but the valid response is still returned.

use crate::fetch_error::FetchError;
use crate::http_cache::HttpCache;
use std::time::Duration;

/// Raw HTTP response returned by the `http_get` closure.
pub struct HttpResponse {
    pub status: u16,
    pub bytes: Vec<u8>,
    pub etag: Option<String>,
}

/// Outcome of a cached fetch operation.
///
/// `was_network_hit` is always available regardless of success or failure,
/// so callers can distinguish a cached 404 (no delay) from a network 404
/// (delay may be needed for rate limiting).
pub struct FetchOutcome {
    /// True if any network request was attempted (success or failure).
    /// False only on a fresh cache hit.
    pub was_network_hit: bool,
    /// The fetch result: body bytes on success, or a typed error.
    pub result: Result<Vec<u8>, FetchError>,
}

/// Cached-fetch state machine that wraps [`HttpCache`] with conditional
/// GET support, body validation, negative caching, and stale fallback.
///
/// The validator is passed per-request to `fetch()`, not stored in the
/// struct, so a single `CachedFetcher` can serve multiple content types
/// (e.g. Maven search JSON vs POM XML).
pub struct CachedFetcher {
    cache: HttpCache,
    negative_ttl: Duration,
    refresh: bool,
}

impl CachedFetcher {
    /// Create a new cached fetcher.
    ///
    /// - `cache`: The underlying HTTP cache
    /// - `negative_ttl`: TTL for cached 404 responses
    /// - `refresh`: If true, skip the fresh cache check and always go to
    ///   network (but still do conditional GET if stale ETag available)
    pub fn new(cache: HttpCache, negative_ttl: Duration, refresh: bool) -> Self {
        Self {
            cache,
            negative_ttl,
            refresh,
        }
    }

    /// Fetch a URL, using the cache when possible.
    ///
    /// - `validate`: Closure that validates a 200 response body. Returns
    ///   `Ok(())` if valid, `Err(detail)` if invalid. Only called for
    ///   status-200 bodies, never for 404s.
    /// - `http_get`: Closure that takes `(url, optional_etag)` where the
    ///   etag is provided for conditional `If-None-Match` requests.
    ///
    /// Returns a [`FetchOutcome`] with `was_network_hit` always available.
    pub fn fetch<G>(
        &self,
        url: &str,
        ttl: Option<Duration>,
        validate: &dyn Fn(&[u8]) -> Result<(), String>,
        mut http_get: G,
    ) -> FetchOutcome
    where
        G: FnMut(&str, Option<&str>) -> Result<HttpResponse, FetchError>,
    {
        // ── Step 1: Check fresh cache (skip if refresh mode) ────────
        if !self.refresh {
            if let Some(entry) = cache_get_fresh(&self.cache, url) {
                if entry.status_code == 404 {
                    return FetchOutcome {
                        was_network_hit: false,
                        result: Err(FetchError::NotFound {
                            url: url.to_string(),
                        }),
                    };
                }
                if entry.status_code == 200 {
                    // Validate the cached body
                    if validate(&entry.body).is_ok() {
                        return FetchOutcome {
                            was_network_hit: false,
                            result: Ok(entry.body),
                        };
                    }
                    // Invalid cached body -- evict and re-fetch
                    cache_evict(&self.cache, url);
                } else {
                    // Unexpected cached status
                    return FetchOutcome {
                        was_network_hit: false,
                        result: Err(FetchError::UnexpectedCachedStatus {
                            url: url.to_string(),
                            status: entry.status_code,
                        }),
                    };
                }
            }
        }

        // ── Step 2: Get stale entry for conditional GET / fallback ───
        let stale = cache_get_stale(&self.cache, url);
        let stale_etag = stale
            .as_ref()
            .and_then(|e| e.etag.as_deref())
            .map(|s| s.to_string());

        // ── Step 3: Network request (conditional if we have an ETag) ─
        let was_conditional = stale_etag.is_some();
        let net_result = http_get(url, stale_etag.as_deref());

        match net_result {
            Ok(response) => self.handle_response(
                url,
                ttl,
                validate,
                response,
                stale,
                was_conditional,
                &mut http_get,
            ),
            Err(net_err) => {
                // ── Step 4: Stale fallback -- ONLY for Transport errors ──
                if matches!(&net_err, FetchError::Transport { .. }) {
                    if let Some(stale_entry) = stale {
                        if stale_entry.status_code == 200 && validate(&stale_entry.body).is_ok() {
                            return FetchOutcome {
                                was_network_hit: true,
                                result: Ok(stale_entry.body),
                            };
                        }
                    }
                }
                // All other errors (HttpStatus, Parse, etc.) propagate immediately
                FetchOutcome {
                    was_network_hit: true,
                    result: Err(net_err),
                }
            }
        }
    }

    /// Handle a successful HTTP response through the status/validation pipeline.
    #[allow(clippy::too_many_arguments)]
    fn handle_response<G>(
        &self,
        url: &str,
        ttl: Option<Duration>,
        validate: &dyn Fn(&[u8]) -> Result<(), String>,
        response: HttpResponse,
        stale: Option<crate::http_cache::CacheEntry>,
        was_conditional: bool,
        http_get: &mut G,
    ) -> FetchOutcome
    where
        G: FnMut(&str, Option<&str>) -> Result<HttpResponse, FetchError>,
    {
        match response.status {
            200 => {
                // Validate the body
                if let Err(detail) = validate(&response.bytes) {
                    return FetchOutcome {
                        was_network_hit: true,
                        result: Err(FetchError::InvalidResponse {
                            url: url.to_string(),
                            detail,
                        }),
                    };
                }
                // Cache the valid response
                cache_put(
                    &self.cache,
                    url,
                    &response.bytes,
                    response.etag.as_deref(),
                    200,
                    ttl,
                );
                FetchOutcome {
                    was_network_hit: true,
                    result: Ok(response.bytes),
                }
            }
            404 => {
                // Cache the 404 with the negative TTL
                cache_put(
                    &self.cache,
                    url,
                    &[],
                    response.etag.as_deref(),
                    404,
                    Some(self.negative_ttl),
                );
                FetchOutcome {
                    was_network_hit: true,
                    result: Err(FetchError::NotFound {
                        url: url.to_string(),
                    }),
                }
            }
            304 if was_conditional => {
                // Legitimate 304 -- the request had If-None-Match
                match stale {
                    Some(stale_entry) => match stale_entry.status_code {
                        200 => {
                            // Validate the stale body
                            if validate(&stale_entry.body).is_ok() {
                                cache_touch(&self.cache, url, response.etag.as_deref());
                                FetchOutcome {
                                    was_network_hit: true,
                                    result: Ok(stale_entry.body),
                                }
                            } else {
                                // Invalid stale body -- unconditional retry
                                cache_evict(&self.cache, url);
                                match http_get(url, None) {
                                    Ok(retry) => self
                                        .handle_unconditional_response(url, ttl, validate, retry),
                                    Err(e) => FetchOutcome {
                                        was_network_hit: true,
                                        result: Err(e),
                                    },
                                }
                            }
                        }
                        404 => {
                            // 304 confirming a negative (404) cache entry
                            cache_touch(&self.cache, url, response.etag.as_deref());
                            FetchOutcome {
                                was_network_hit: true,
                                result: Err(FetchError::NotFound {
                                    url: url.to_string(),
                                }),
                            }
                        }
                        status => FetchOutcome {
                            was_network_hit: true,
                            result: Err(FetchError::UnexpectedCachedStatus {
                                url: url.to_string(),
                                status,
                            }),
                        },
                    },
                    None => {
                        // 304 but stale entry vanished (evicted between steps) -- retry
                        match http_get(url, None) {
                            Ok(retry) => {
                                self.handle_unconditional_response(url, ttl, validate, retry)
                            }
                            Err(e) => FetchOutcome {
                                was_network_hit: true,
                                result: Err(e),
                            },
                        }
                    }
                }
            }
            304 => {
                // Unsolicited 304 -- request had no If-None-Match, so
                // the server returned 304 incorrectly. Do unconditional retry.
                match http_get(url, None) {
                    Ok(retry) => self.handle_unconditional_response(url, ttl, validate, retry),
                    Err(e) => FetchOutcome {
                        was_network_hit: true,
                        result: Err(e),
                    },
                }
            }
            status => {
                // Non-cacheable status (429, 5xx, others)
                FetchOutcome {
                    was_network_hit: true,
                    result: Err(FetchError::HttpStatus {
                        url: url.to_string(),
                        status,
                    }),
                }
            }
        }
    }

    /// Handle a response from an unconditional retry (after 304 fallback).
    fn handle_unconditional_response(
        &self,
        url: &str,
        ttl: Option<Duration>,
        validate: &dyn Fn(&[u8]) -> Result<(), String>,
        response: HttpResponse,
    ) -> FetchOutcome {
        match response.status {
            200 => {
                if let Err(detail) = validate(&response.bytes) {
                    return FetchOutcome {
                        was_network_hit: true,
                        result: Err(FetchError::InvalidResponse {
                            url: url.to_string(),
                            detail,
                        }),
                    };
                }
                cache_put(
                    &self.cache,
                    url,
                    &response.bytes,
                    response.etag.as_deref(),
                    200,
                    ttl,
                );
                FetchOutcome {
                    was_network_hit: true,
                    result: Ok(response.bytes),
                }
            }
            404 => {
                cache_put(
                    &self.cache,
                    url,
                    &[],
                    response.etag.as_deref(),
                    404,
                    Some(self.negative_ttl),
                );
                FetchOutcome {
                    was_network_hit: true,
                    result: Err(FetchError::NotFound {
                        url: url.to_string(),
                    }),
                }
            }
            status => FetchOutcome {
                was_network_hit: true,
                result: Err(FetchError::HttpStatus {
                    url: url.to_string(),
                    status,
                }),
            },
        }
    }
}

// ── Cache I/O wrappers (nonfatal) ───────────────────────────────────────
//
// Every cache call is wrapped so I/O errors are logged and swallowed.
// A failed cache read degrades to a miss; a failed write still returns
// the valid response.

fn cache_get_fresh(cache: &HttpCache, url: &str) -> Option<crate::http_cache::CacheEntry> {
    match cache.get_fresh(url) {
        Ok(entry) => entry,
        Err(e) => {
            eprintln!("WARNING: cache read error for {}: {}", url, e);
            None
        }
    }
}

fn cache_get_stale(cache: &HttpCache, url: &str) -> Option<crate::http_cache::CacheEntry> {
    match cache.get_stale(url) {
        Ok(entry) => entry,
        Err(e) => {
            eprintln!("WARNING: cache stale-read error for {}: {}", url, e);
            None
        }
    }
}

fn cache_put(
    cache: &HttpCache,
    url: &str,
    body: &[u8],
    etag: Option<&str>,
    status: u16,
    ttl: Option<Duration>,
) {
    if let Err(e) = cache.put(url, body, etag, status, ttl) {
        eprintln!("WARNING: cache write error for {}: {}", url, e);
    }
}

fn cache_touch(cache: &HttpCache, url: &str, etag: Option<&str>) {
    if let Err(e) = cache.touch(url, etag) {
        eprintln!("WARNING: cache touch error for {}: {}", url, e);
    }
}

fn cache_evict(cache: &HttpCache, url: &str) {
    if let Err(e) = cache.evict(url) {
        eprintln!("WARNING: cache evict error for {}: {}", url, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_cache::MockClock;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn make_fetcher(
        tmp: &TempDir,
        clock: Arc<MockClock>,
        negative_ttl: Duration,
        refresh: bool,
    ) -> CachedFetcher {
        let cache =
            HttpCache::with_clock(tmp.path().to_str().unwrap(), "test-fetcher", clock).unwrap();
        CachedFetcher::new(cache, negative_ttl, refresh)
    }

    fn ok_validator(body: &[u8]) -> Result<(), String> {
        if body.is_empty() {
            Err("empty body".to_string())
        } else {
            Ok(())
        }
    }

    fn always_valid(_body: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn always_invalid(_body: &[u8]) -> Result<(), String> {
        Err("always invalid".to_string())
    }

    fn json_validator(body: &[u8]) -> Result<(), String> {
        serde_json::from_slice::<serde_json::Value>(body)
            .map(|_| ())
            .map_err(|e| format!("invalid JSON: {}", e))
    }

    fn xml_validator(body: &[u8]) -> Result<(), String> {
        let s = std::str::from_utf8(body).map_err(|e| format!("not UTF-8: {}", e))?;
        if s.starts_with('<') {
            Ok(())
        } else {
            Err("does not look like XML".to_string())
        }
    }

    // ── Scenario: Fresh cache hit, status 200 ───────────────────────

    #[test]
    fn test_fresh_cache_hit_200() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed the cache
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"cached-body",
                Some("\"etag-1\""),
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(3600)),
            &ok_validator,
            |_url, _etag| {
                panic!("should not make network request on fresh cache hit");
            },
        );

        assert!(!outcome.was_network_hit);
        assert_eq!(outcome.result.unwrap(), b"cached-body");
    }

    // ── Scenario: Fresh cache hit, status 200, invalid body ─────────

    #[test]
    fn test_fresh_cache_hit_200_invalid_body_refetches() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed with data that will fail validation
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"bad-cached-body",
                None,
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        // The re-fetch also produces invalid data, so we get InvalidResponse
        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(3600)),
            &always_invalid,
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 200,
                    bytes: b"also-invalid".to_vec(),
                    etag: None,
                })
            },
        );

        assert!(outcome.was_network_hit);
        match outcome.result {
            Err(FetchError::InvalidResponse { .. }) => {}
            other => panic!("expected InvalidResponse, got {:?}", other.err()),
        }
    }

    // ── Scenario: Fresh cache hit, status 404 ───────────────────────

    #[test]
    fn test_fresh_cache_hit_404() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed 404
        fetcher
            .cache
            .put(
                "http://example.com/missing",
                b"",
                None,
                404,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        let outcome = fetcher.fetch(
            "http://example.com/missing",
            None,
            &always_valid,
            |_url, _etag| {
                panic!("should not make network request for cached 404");
            },
        );

        assert!(
            !outcome.was_network_hit,
            "cached 404 should not be a network hit"
        );
        match outcome.result {
            Err(FetchError::NotFound { url }) => {
                assert_eq!(url, "http://example.com/missing");
            }
            other => panic!("expected NotFound, got {:?}", other.err()),
        }
    }

    // ── was_network_hit on error paths ──────────────────────────────

    #[test]
    fn test_was_network_hit_on_network_404() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let outcome = fetcher.fetch(
            "http://example.com/gone",
            None,
            &always_valid,
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 404,
                    bytes: vec![],
                    etag: None,
                })
            },
        );

        assert!(
            outcome.was_network_hit,
            "network 404 must report was_network_hit=true"
        );
        assert!(matches!(outcome.result, Err(FetchError::NotFound { .. })));
    }

    #[test]
    fn test_was_network_hit_on_transport_error() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            None,
            &ok_validator,
            |url, _etag| {
                let err = reqwest::blocking::get("http://[::0]:1/__invalid__").unwrap_err();
                Err(FetchError::Transport {
                    url: url.to_string(),
                    source: err,
                })
            },
        );

        assert!(
            outcome.was_network_hit,
            "transport error must report was_network_hit=true"
        );
        assert!(matches!(outcome.result, Err(FetchError::Transport { .. })));
    }

    // ── Scenario: Cache miss -> GET 200 ─────────────────────────────

    #[test]
    fn test_cache_miss_200() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(86400)),
            &ok_validator,
            |_url, etag| {
                assert!(etag.is_none(), "no etag on cache miss");
                Ok(HttpResponse {
                    status: 200,
                    bytes: b"fresh-from-network".to_vec(),
                    etag: Some("\"new-etag\"".to_string()),
                })
            },
        );

        assert!(outcome.was_network_hit);
        assert_eq!(outcome.result.unwrap(), b"fresh-from-network");

        // Verify it was cached
        let cached = fetcher.cache.get_fresh("http://example.com/pkg").unwrap();
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().body, b"fresh-from-network");
    }

    // ── Scenario: Cache miss -> GET 404 (negative cache) ────────────

    #[test]
    fn test_cache_miss_404_negative_cache() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(1800), false);

        let outcome = fetcher.fetch(
            "http://example.com/gone",
            None,
            &always_valid,
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 404,
                    bytes: vec![],
                    etag: None,
                })
            },
        );

        assert!(outcome.was_network_hit);
        assert!(matches!(outcome.result, Err(FetchError::NotFound { .. })));

        // Verify negative cache was stored
        let cached = fetcher.cache.get_fresh("http://example.com/gone").unwrap();
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().status_code, 404);
    }

    // ── Scenario: Conditional GET -> 304 + valid stale body ─────────

    #[test]
    fn test_304_with_valid_stale_body() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed an expired entry with an ETag
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"stale-body",
                Some("\"etag-1\""),
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120); // Expire it

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(3600)),
            &ok_validator,
            |_url, etag| {
                assert_eq!(etag, Some("\"etag-1\""), "should send stale ETag");
                Ok(HttpResponse {
                    status: 304,
                    bytes: vec![],
                    etag: Some("\"etag-1\"".to_string()),
                })
            },
        );

        assert!(outcome.was_network_hit, "304 counts as a network hit");
        assert_eq!(outcome.result.unwrap(), b"stale-body");

        // Entry should now be fresh (touch refreshed it)
        let fresh = fetcher.cache.get_fresh("http://example.com/pkg").unwrap();
        assert!(fresh.is_some(), "entry should be fresh after 304 touch");
    }

    // ── Scenario: 304 + invalid stale body -> unconditional retry ────

    #[test]
    fn test_304_with_invalid_stale_body_retries() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed an expired entry with empty body (will fail ok_validator)
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"",
                Some("\"etag-old\""),
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        let mut call_count = 0u32;
        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(3600)),
            &ok_validator,
            |_url, etag| {
                call_count += 1;
                if call_count == 1 {
                    assert_eq!(etag, Some("\"etag-old\""));
                    Ok(HttpResponse {
                        status: 304,
                        bytes: vec![],
                        etag: Some("\"etag-old\"".to_string()),
                    })
                } else {
                    assert!(etag.is_none(), "retry should be unconditional");
                    Ok(HttpResponse {
                        status: 200,
                        bytes: b"fresh-valid-body".to_vec(),
                        etag: Some("\"etag-new\"".to_string()),
                    })
                }
            },
        );

        assert!(outcome.was_network_hit);
        assert_eq!(outcome.result.unwrap(), b"fresh-valid-body");
        assert_eq!(call_count, 2, "should have made two requests");
    }

    // ── Scenario: 304 without stale entry -> unconditional retry ─────

    #[test]
    fn test_304_without_stale_entry_retries() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let mut call_count = 0u32;
        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(3600)),
            &ok_validator,
            |_url, _etag| {
                call_count += 1;
                if call_count == 1 {
                    Ok(HttpResponse {
                        status: 304,
                        bytes: vec![],
                        etag: None,
                    })
                } else {
                    Ok(HttpResponse {
                        status: 200,
                        bytes: b"actual-body".to_vec(),
                        etag: None,
                    })
                }
            },
        );

        assert_eq!(outcome.result.unwrap(), b"actual-body");
        assert_eq!(call_count, 2);
    }

    // ── Scenario: Transport error + stale 200 fallback ──────────────

    #[test]
    fn test_transport_error_stale_200_fallback() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed an expired entry
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"stale-but-valid",
                None,
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            None,
            &ok_validator,
            |url, _etag| {
                let err = reqwest::blocking::get("http://[::0]:1/__invalid__").unwrap_err();
                Err(FetchError::Transport {
                    url: url.to_string(),
                    source: err,
                })
            },
        );

        assert!(
            outcome.was_network_hit,
            "network was attempted even though it failed"
        );
        assert_eq!(outcome.result.unwrap(), b"stale-but-valid");
    }

    // ── Scenario: Transport error + stale 404 -> propagate ──────────

    #[test]
    fn test_transport_error_stale_404_propagates() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed an expired 404
        fetcher
            .cache
            .put(
                "http://example.com/gone",
                b"",
                None,
                404,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        let outcome = fetcher.fetch(
            "http://example.com/gone",
            None,
            &always_valid,
            |url, _etag| {
                let err = reqwest::blocking::get("http://[::0]:1/__invalid__").unwrap_err();
                Err(FetchError::Transport {
                    url: url.to_string(),
                    source: err,
                })
            },
        );

        assert!(outcome.was_network_hit);
        assert!(matches!(outcome.result, Err(FetchError::Transport { .. })));
    }

    // ── Scenario: Transport error + no stale -> propagate ───────────

    #[test]
    fn test_transport_error_no_stale_propagates() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            None,
            &ok_validator,
            |url, _etag| {
                let err = reqwest::blocking::get("http://[::0]:1/__invalid__").unwrap_err();
                Err(FetchError::Transport {
                    url: url.to_string(),
                    source: err,
                })
            },
        );

        assert!(outcome.was_network_hit);
        assert!(matches!(outcome.result, Err(FetchError::Transport { .. })));
    }

    // ── Fix #2: Non-transport errors do NOT get stale fallback ───────

    #[test]
    fn test_http_429_does_not_use_stale_fallback() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed a stale 200 entry
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"stale-data",
                None,
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            None,
            &ok_validator,
            |url, _etag| {
                Err(FetchError::HttpStatus {
                    url: url.to_string(),
                    status: 429,
                })
            },
        );

        assert!(outcome.was_network_hit);
        match outcome.result {
            Err(FetchError::HttpStatus { status: 429, .. }) => {}
            other => panic!(
                "expected HttpStatus 429 (not stale fallback), got {:?}",
                other.err()
            ),
        }
    }

    #[test]
    fn test_http_401_does_not_use_stale_fallback() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed a stale 200 entry
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"stale-data",
                None,
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            None,
            &ok_validator,
            |url, _etag| {
                Err(FetchError::HttpStatus {
                    url: url.to_string(),
                    status: 401,
                })
            },
        );

        assert!(outcome.was_network_hit);
        match outcome.result {
            Err(FetchError::HttpStatus { status: 401, .. }) => {}
            other => panic!(
                "expected HttpStatus 401 (not stale fallback), got {:?}",
                other.err()
            ),
        }
    }

    // ── Only 200 and 404 are cached ─────────────────────────────────

    #[test]
    fn test_5xx_never_cached() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            None,
            &always_valid,
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 503,
                    bytes: b"service unavailable".to_vec(),
                    etag: None,
                })
            },
        );

        assert!(matches!(
            outcome.result,
            Err(FetchError::HttpStatus { status: 503, .. })
        ));
        assert!(fetcher
            .cache
            .get_stale("http://example.com/pkg")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_429_never_cached() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            None,
            &always_valid,
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 429,
                    bytes: b"rate limited".to_vec(),
                    etag: None,
                })
            },
        );

        assert!(matches!(
            outcome.result,
            Err(FetchError::HttpStatus { status: 429, .. })
        ));
        assert!(fetcher
            .cache
            .get_stale("http://example.com/pkg")
            .unwrap()
            .is_none());
    }

    // ── Invalid 200 bodies are never cached ─────────────────────────

    #[test]
    fn test_invalid_200_never_cached() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(3600)),
            &always_invalid,
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 200,
                    bytes: b"invalid-data".to_vec(),
                    etag: None,
                })
            },
        );

        assert!(matches!(
            outcome.result,
            Err(FetchError::InvalidResponse { .. })
        ));
        assert!(fetcher
            .cache
            .get_stale("http://example.com/pkg")
            .unwrap()
            .is_none());
    }

    // ── was_network_hit flag ────────────────────────────────────────

    #[test]
    fn test_was_network_hit_false_on_fresh_cache() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"data",
                None,
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        let outcome = fetcher.fetch("http://example.com/pkg", None, &ok_validator, |_, _| {
            panic!("should not call network");
        });

        assert!(!outcome.was_network_hit);
        assert!(outcome.result.is_ok());
    }

    #[test]
    fn test_was_network_hit_true_on_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let outcome = fetcher.fetch("http://example.com/pkg", None, &ok_validator, |_, _| {
            Ok(HttpResponse {
                status: 200,
                bytes: b"from-network".to_vec(),
                etag: None,
            })
        });

        assert!(outcome.was_network_hit);
        assert_eq!(outcome.result.unwrap(), b"from-network");
    }

    // ── Refresh mode ────────────────────────────────────────────────

    #[test]
    fn test_refresh_skips_fresh_cache() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), true);

        // Seed fresh cache
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"old-data",
                Some("\"old-etag\""),
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(3600)),
            &ok_validator,
            |_url, etag| {
                assert_eq!(etag, Some("\"old-etag\""));
                Ok(HttpResponse {
                    status: 200,
                    bytes: b"refreshed-data".to_vec(),
                    etag: Some("\"new-etag\"".to_string()),
                })
            },
        );

        assert!(outcome.was_network_hit);
        assert_eq!(outcome.result.unwrap(), b"refreshed-data");
    }

    // ── Fix #3: Per-request validator / cross-format test ───────────

    #[test]
    fn test_cross_format_validator_evicts_and_refetches() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Cache a JSON body under a URL
        fetcher
            .cache
            .put(
                "http://example.com/artifact",
                b"{\"type\":\"search\"}",
                None,
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        // Fetch with JSON validator: should get cached body
        let outcome = fetcher.fetch(
            "http://example.com/artifact",
            Some(Duration::from_secs(3600)),
            &json_validator,
            |_url, _etag| {
                panic!("should not hit network for valid JSON cache");
            },
        );
        assert!(!outcome.was_network_hit);
        assert_eq!(outcome.result.unwrap(), b"{\"type\":\"search\"}");

        // Now fetch the SAME URL with XML validator: cached JSON fails
        // validation, gets evicted, triggers re-fetch
        let outcome = fetcher.fetch(
            "http://example.com/artifact",
            Some(Duration::from_secs(3600)),
            &xml_validator,
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 200,
                    bytes: b"<pom>content</pom>".to_vec(),
                    etag: None,
                })
            },
        );

        assert!(
            outcome.was_network_hit,
            "XML validator should reject cached JSON and re-fetch"
        );
        assert_eq!(outcome.result.unwrap(), b"<pom>content</pom>");

        // Cache should now have the XML body
        let cached = fetcher
            .cache
            .get_fresh("http://example.com/artifact")
            .unwrap()
            .expect("XML body should be cached");
        assert_eq!(cached.body, b"<pom>content</pom>");
    }

    // ── Cache I/O errors are nonfatal ───────────────────────────────

    #[test]
    fn test_cache_read_error_degrades_to_miss() {
        // Simulate a cache read error by making the shard directory
        // unreadable. On Unix, chmod 000 prevents reads. On other
        // platforms this test verifies the empty-cache fallback path.
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Put a valid entry so there's something to fail on
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"cached-data",
                None,
                200,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        // Make the shard directory unreadable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hash = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(b"http://example.com/pkg");
                format!("{:x}", h.finalize())
            };
            let shard_dir = tmp.path().join("test-fetcher").join(&hash[0..2]);
            std::fs::set_permissions(&shard_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

            // The cache_get_fresh wrapper should swallow the error and
            // degrade to a cache miss, causing a network request.
            let outcome = fetcher.fetch(
                "http://example.com/pkg",
                Some(Duration::from_secs(3600)),
                &ok_validator,
                |_url, _etag| {
                    Ok(HttpResponse {
                        status: 200,
                        bytes: b"from-network".to_vec(),
                        etag: None,
                    })
                },
            );

            // Restore permissions before asserting (so TempDir cleanup succeeds)
            std::fs::set_permissions(&shard_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

            assert!(
                outcome.was_network_hit,
                "read error should degrade to cache miss"
            );
            assert_eq!(outcome.result.unwrap(), b"from-network");
        }

        // Non-Unix: just verify the empty-cache fallback path works
        #[cfg(not(unix))]
        {
            let outcome = fetcher.fetch(
                "http://example.com/other",
                Some(Duration::from_secs(3600)),
                &ok_validator,
                |_url, _etag| {
                    Ok(HttpResponse {
                        status: 200,
                        bytes: b"from-network".to_vec(),
                        etag: None,
                    })
                },
            );

            assert!(outcome.was_network_hit);
            assert_eq!(outcome.result.unwrap(), b"from-network");
        }
    }

    // ── Fix #1: Unsolicited 304 triggers unconditional retry ────────

    #[test]
    fn test_unsolicited_304_triggers_retry() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Cache a 200 WITHOUT an ETag, then expire it
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"no-etag-body",
                None, // no ETag
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        let mut call_count = 0u32;
        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(3600)),
            &ok_validator,
            |_url, etag| {
                call_count += 1;
                if call_count == 1 {
                    // First request is unconditional (no ETag available)
                    assert!(etag.is_none(), "no ETag means no If-None-Match");
                    // Server incorrectly returns 304
                    Ok(HttpResponse {
                        status: 304,
                        bytes: vec![],
                        etag: None,
                    })
                } else {
                    // Retry must also be unconditional
                    assert!(etag.is_none(), "retry should be unconditional");
                    Ok(HttpResponse {
                        status: 200,
                        bytes: b"proper-response".to_vec(),
                        etag: None,
                    })
                }
            },
        );

        assert!(outcome.was_network_hit);
        assert_eq!(outcome.result.unwrap(), b"proper-response");
        assert_eq!(call_count, 2, "unsolicited 304 should trigger a retry");
    }

    // ── Fix #2: 304 confirming a cached 404 ─────────────────────────

    #[test]
    fn test_304_confirms_cached_404() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Cache a 404 with an ETag, then expire it
        fetcher
            .cache
            .put(
                "http://example.com/gone",
                b"",
                Some("\"404-etag\""),
                404,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        let mut call_count = 0u32;
        let outcome = fetcher.fetch(
            "http://example.com/gone",
            None,
            &always_valid,
            |_url, etag| {
                call_count += 1;
                assert_eq!(etag, Some("\"404-etag\""), "should send stale 404's ETag");
                Ok(HttpResponse {
                    status: 304,
                    bytes: vec![],
                    etag: Some("\"404-etag\"".to_string()),
                })
            },
        );

        assert!(outcome.was_network_hit, "304 is a network hit");
        assert_eq!(call_count, 1, "should NOT retry -- 304 confirms the 404");
        match outcome.result {
            Err(FetchError::NotFound { url }) => {
                assert_eq!(url, "http://example.com/gone");
            }
            other => panic!("expected NotFound, got {:?}", other.err()),
        }

        // Entry should now be fresh again (touch refreshed it)
        let cached = fetcher.cache.get_fresh("http://example.com/gone").unwrap();
        assert!(
            cached.is_some(),
            "404 entry should be fresh after 304 touch"
        );
        assert_eq!(cached.unwrap().status_code, 404);
    }

    // ── Fix #4: Additional state-machine edge cases ─────────────────

    #[test]
    fn test_invalid_stale_body_plus_transport_failure() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed an expired entry with empty body (fails ok_validator)
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"",
                None,
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            None,
            &ok_validator,
            |url, _etag| {
                let err = reqwest::blocking::get("http://[::0]:1/__invalid__").unwrap_err();
                Err(FetchError::Transport {
                    url: url.to_string(),
                    source: err,
                })
            },
        );

        // Stale body is invalid (empty), so fallback should NOT use it.
        // Transport error should propagate.
        assert!(outcome.was_network_hit);
        assert!(
            matches!(outcome.result, Err(FetchError::Transport { .. })),
            "invalid stale body should not be used as fallback"
        );
    }

    #[test]
    fn test_304_always_reports_was_network_hit() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed an expired entry with ETag
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"data",
                Some("\"etag\""),
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            Some(Duration::from_secs(3600)),
            &ok_validator,
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 304,
                    bytes: vec![],
                    etag: Some("\"etag\"".to_string()),
                })
            },
        );

        assert!(
            outcome.was_network_hit,
            "304 must always set was_network_hit=true"
        );
        assert!(outcome.result.is_ok());
    }

    #[test]
    fn test_403_not_cached_returns_http_status() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        let outcome = fetcher.fetch(
            "http://example.com/forbidden",
            None,
            &always_valid,
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 403,
                    bytes: b"forbidden".to_vec(),
                    etag: None,
                })
            },
        );

        assert!(outcome.was_network_hit);
        match outcome.result {
            Err(FetchError::HttpStatus { status: 403, .. }) => {}
            other => panic!("expected HttpStatus 403, got {:?}", other.err()),
        }

        // Must NOT be cached
        assert!(fetcher
            .cache
            .get_stale("http://example.com/forbidden")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_unexpected_cached_status_on_fresh_hit() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Manually put an entry with status 500 (would never happen
        // normally, but tests the guard in the fresh-hit path)
        fetcher
            .cache
            .put(
                "http://example.com/weird",
                b"server-error-body",
                None,
                500,
                Some(Duration::from_secs(3600)),
            )
            .unwrap();

        let outcome = fetcher.fetch(
            "http://example.com/weird",
            None,
            &always_valid,
            |_url, _etag| {
                panic!("should not reach network for UnexpectedCachedStatus");
            },
        );

        assert!(!outcome.was_network_hit);
        match outcome.result {
            Err(FetchError::UnexpectedCachedStatus { status: 500, .. }) => {}
            other => panic!("expected UnexpectedCachedStatus 500, got {:?}", other.err()),
        }
    }

    #[test]
    fn test_failed_revalidation_falls_back_to_stale() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));
        let fetcher = make_fetcher(&tmp, clock.clone(), Duration::from_secs(3600), false);

        // Seed an expired 200 WITH an ETag
        fetcher
            .cache
            .put(
                "http://example.com/pkg",
                b"stale-revalidation-body",
                Some("\"reval-etag\""),
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        clock.advance(120);

        // Conditional GET fails with a transport error (not a 304)
        let outcome = fetcher.fetch(
            "http://example.com/pkg",
            None,
            &ok_validator,
            |url, etag| {
                assert_eq!(
                    etag,
                    Some("\"reval-etag\""),
                    "should attempt conditional GET with stale ETag"
                );
                let err = reqwest::blocking::get("http://[::0]:1/__invalid__").unwrap_err();
                Err(FetchError::Transport {
                    url: url.to_string(),
                    source: err,
                })
            },
        );

        // Transport error triggers stale fallback for valid 200 bodies
        assert!(
            outcome.was_network_hit,
            "failed revalidation is still a network hit"
        );
        assert_eq!(
            outcome.result.unwrap(),
            b"stale-revalidation-body",
            "stale body should be returned on failed revalidation"
        );
    }
}
