use crate::cached_fetch::{CachedFetcher, HttpResponse};
use crate::fetch_error::FetchError;
use crate::http_cache::HttpCache;
use crate::maven_version::{classify_version, VersionClass};
use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::uris::*;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

pub struct MavenCollector {
    client: Client,
    search_base: String,
    repo_base: String,
    http_cache: Option<HttpCache>,
    refresh: bool,
    pub max_depth: u32,
    pub max_roots: usize,
    pub max_packages: usize,
    pub delay_ms: u64,
    pub graph_uri: Option<String>,
}

/// Check whether a URL points to Maven Central over HTTPS.
///
/// Accepts `repo1.maven.org` and `repo.maven.apache.org` as Central hosts.
/// Requires HTTPS -- plain HTTP is rejected. Uses proper URL parsing.
pub fn is_maven_central(url: &str) -> bool {
    if let Ok(parsed) = url::Url::parse(url) {
        parsed.scheme() == "https"
            && matches!(
                parsed.host_str(),
                Some("repo1.maven.org") | Some("repo.maven.apache.org")
            )
    } else {
        false
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    response: SearchResponseBody,
}

#[derive(Debug, Deserialize)]
struct SearchResponseBody {
    #[serde(default)]
    docs: Vec<SearchDoc>,
}

#[derive(Debug, Deserialize)]
struct SearchDoc {
    #[serde(rename = "latestVersion")]
    latest_version: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ParentCoordinate {
    pub(crate) group_id: String,
    pub(crate) artifact_id: String,
    pub(crate) version: String,
}

#[derive(Debug, Default)]
pub(crate) struct PomMetadata {
    pub(crate) group_id: String,
    pub(crate) artifact_id: String,
    pub(crate) version: String,
    pub(crate) description: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) licenses: Vec<String>,
    pub(crate) dependencies: Vec<PomDependency>,
    pub(crate) dependency_management: Vec<PomDependency>,
    pub(crate) parent: Option<ParentCoordinate>,
    pub(crate) properties: HashMap<String, String>,
    pub(crate) scm_url: Option<String>,
    pub(crate) scm_connection: Option<String>,
    pub(crate) scm_tag: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct PomDependency {
    pub(crate) group_id: String,
    pub(crate) artifact_id: String,
    pub(crate) version: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) optional: bool,
    pub(crate) dependency_type: Option<String>,
    pub(crate) classifier: Option<String>,
    /// Exclusions as `(groupId, artifactId)` pairs.
    pub(crate) exclusions: Vec<(String, String)>,
}

/// A dependency with all coordinates fully resolved (interpolated).
///
/// Both emission AND traversal consume this single struct -- no separate
/// interpolation paths.
#[derive(Debug)]
pub(crate) struct ResolvedDependency {
    pub(crate) group_id: String,
    pub(crate) artifact_id: String,
    pub(crate) version: Option<String>,
    pub(crate) dependency_type: String,
    pub(crate) classifier: String,
    pub(crate) scope: String,
    pub(crate) optional: bool,
    pub(crate) exclusions: Vec<(String, String)>,
    #[allow(dead_code)] // retained for diagnostics/logging
    pub(crate) raw_version_expr: Option<String>,
    pub(crate) version_class: VersionClass,
    pub(crate) is_emittable: bool,
}

impl MavenCollector {
    pub fn new(search_base: String, repo_base: String) -> Self {
        let client = crate::enricher::default_http_client();

        Self {
            client,
            search_base,
            repo_base,
            http_cache: None,
            refresh: false,
            max_depth: 3,
            max_roots: 10_000,
            max_packages: 5_000,
            delay_ms: 500,
            graph_uri: None,
        }
    }

    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    /// Enable HTTP caching for search and POM requests.
    pub fn with_cache(mut self, cache_dir: &str) -> std::result::Result<Self, std::io::Error> {
        self.http_cache = Some(HttpCache::new(cache_dir, "maven")?);
        Ok(self)
    }

    /// Set a pre-built cache instance directly (avoids move issues in callers).
    pub fn set_cache(&mut self, cache: HttpCache) {
        self.http_cache = Some(cache);
    }

    /// Set cache-refresh mode (bypass fresh cache, force network).
    pub fn with_refresh(mut self, refresh: bool) -> Self {
        self.refresh = refresh;
        self
    }

    pub fn collect_discover(
        &self,
        endpoint: &str,
        auth: &crate::sparql::SparqlAuth,
        backend: crate::sparql::SparqlBackend,
        output_path: &str,
    ) -> Result<(usize, usize)> {
        let raw_names = crate::seed::discover_by_ecosystem(endpoint, "maven", auth, backend)?;
        let raw_count = raw_names.len();
        let mut seen = HashSet::new();
        let seeds: Vec<(String, String)> = raw_names
            .into_iter()
            .filter_map(|n| {
                let parts: Vec<&str> = n.splitn(3, ':').collect();
                if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                    let key = (parts[0].to_string(), parts[1].to_string());
                    if seen.insert(key.clone()) {
                        Some(key)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        eprintln!(
            "Normalized {} raw -> {} unique Maven coordinates",
            raw_count,
            seeds.len()
        );
        self.collect_recursive(seeds, output_path)
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let seeds = read_maven_seed_file(packages_file)?;
        self.collect_recursive(seeds, output_path)
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("maven");
        let rel_uri = release_uri("maven", "central");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Maven Central")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "central")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    /// Orchestrator: get latest version via search API, then fetch POM.
    ///
    /// Returns `Ok((PomMetadata, was_network_hit))` on success, or
    /// `Err((FetchError, was_network_hit))` on failure -- the bool is
    /// always available so the caller can apply a courtesy delay even
    /// on failed network requests.
    #[allow(dead_code)] // used by tests
    fn fetch_artifact_with_retry(
        &self,
        group_id: &str,
        artifact_id: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<(PomMetadata, bool), (FetchError, bool)> {
        let mut any_network_hit = false;

        // Step 1: get latest version (cached separately)
        let (version, search_hit) = self
            .get_latest_version(group_id, artifact_id, base_delay_ms)
            .map_err(|(e, hit)| {
                any_network_hit |= hit;
                (e, any_network_hit)
            })?;
        any_network_hit |= search_hit;

        // Step 2: fetch POM (cached separately)
        let (pom, pom_hit) = self
            .fetch_pom(group_id, artifact_id, &version, base_delay_ms)
            .map_err(|(e, hit)| {
                any_network_hit |= hit;
                (e, any_network_hit)
            })?;
        any_network_hit |= pom_hit;

        Ok((pom, any_network_hit))
    }

    /// Fetch latest version from the Maven search API.
    /// Returns `Ok((version_string, was_network_hit))` or `Err((FetchError, was_network_hit))`.
    fn get_latest_version(
        &self,
        group_id: &str,
        artifact_id: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<(String, bool), (FetchError, bool)> {
        let query = format!("g:{}+AND+a:{}", group_id, artifact_id);
        let url = format!(
            "{}/solrsearch/select?q={}&rows=1&wt=json",
            self.search_base, query
        );

        // Validator accepts all structurally valid JSON. Semantic checks
        // (empty docs, empty latestVersion) happen in parse_search_version().
        let search_validator =
            |body: &[u8]| -> std::result::Result<(), String> { validate_search_json(body) };

        let (bytes, was_hit) = if let Some(ref cache) = self.http_cache {
            let search_cache = match cache.sibling("maven-search") {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "  WARNING: search cache init failed: {}, proceeding without cache",
                        e
                    );
                    return self.get_latest_version_direct(
                        group_id,
                        artifact_id,
                        &url,
                        base_delay_ms,
                    );
                }
            };
            let neg_ttl = Duration::from_secs(6 * 3600);
            let fetcher = CachedFetcher::new(search_cache, neg_ttl, self.refresh);
            let outcome = fetcher.fetch(
                &url,
                Some(Duration::from_secs(24 * 3600)), // search TTL: 24h
                &search_validator,
                |req_url, etag| self.http_get_with_retry(req_url, etag, base_delay_ms, 3),
            );
            let hit = outcome.was_network_hit;
            match outcome.result {
                Ok(b) => (b, hit),
                Err(e) => return Err((e, hit)),
            }
        } else {
            self.direct_fetch_with_retry(&url, base_delay_ms, 3)
                .map_err(|e| (e, true))?
        };

        // Semantic check: empty docs → negative-cache as 404 with 6h TTL.
        // Empty latestVersion → InvalidResponse, NOT cached.
        self.parse_search_version_with_cache(group_id, artifact_id, &url, &bytes, was_hit)
    }

    /// Direct search fetch without cache (shared by no-cache path and cache-init fallback).
    fn get_latest_version_direct(
        &self,
        group_id: &str,
        artifact_id: &str,
        url: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<(String, bool), (FetchError, bool)> {
        let (bytes, was_hit) = self
            .direct_fetch_with_retry(url, base_delay_ms, 3)
            .map_err(|e| (e, true))?;
        self.parse_search_version(group_id, artifact_id, url, &bytes, was_hit)
    }

    /// Parse a search response and extract the latest version.
    ///
    /// Distinguishes two error cases:
    /// - Empty docs → `FetchError::NotFound` (legitimate "not found")
    /// - Empty `latestVersion` → `FetchError::InvalidResponse` (malformed)
    fn parse_search_version(
        &self,
        group_id: &str,
        artifact_id: &str,
        url: &str,
        bytes: &[u8],
        was_hit: bool,
    ) -> std::result::Result<(String, bool), (FetchError, bool)> {
        let text = std::str::from_utf8(bytes).map_err(|e| {
            (
                FetchError::Parse {
                    url: url.to_string(),
                    detail: e.to_string(),
                },
                was_hit,
            )
        })?;
        let resp: SearchResponse = serde_json::from_str(text).map_err(|e| {
            (
                FetchError::Parse {
                    url: url.to_string(),
                    detail: e.to_string(),
                },
                was_hit,
            )
        })?;

        if resp.response.docs.is_empty() {
            // Legitimate not-found
            return Err((
                FetchError::NotFound {
                    url: format!("{}:{}", group_id, artifact_id),
                },
                was_hit,
            ));
        }

        let version = &resp.response.docs[0].latest_version;
        if version.is_empty() {
            // Malformed result — NOT cached as negative
            return Err((
                FetchError::InvalidResponse {
                    url: url.to_string(),
                    detail: "search result doc has empty latestVersion".into(),
                },
                was_hit,
            ));
        }

        Ok((version.clone(), was_hit))
    }

    /// Parse search version with negative caching for empty docs.
    ///
    /// Called from the cached path. Empty docs are cached as 404 with 6h
    /// negative TTL so repeated lookups of missing coordinates don't hit
    /// the search API. Empty latestVersion is NOT negative-cached.
    fn parse_search_version_with_cache(
        &self,
        group_id: &str,
        artifact_id: &str,
        url: &str,
        bytes: &[u8],
        was_hit: bool,
    ) -> std::result::Result<(String, bool), (FetchError, bool)> {
        let result = self.parse_search_version(group_id, artifact_id, url, bytes, was_hit);

        // On empty-docs NotFound, overwrite the cached 200 with a 404
        // entry using 6h negative TTL so subsequent lookups are served
        // from cache without hitting the search API.
        if let Err((FetchError::NotFound { .. }, hit)) = &result {
            if *hit {
                if let Some(ref cache) = self.http_cache {
                    match cache.sibling("maven-search") {
                        Ok(neg_cache) => {
                            let neg_ttl = Duration::from_secs(6 * 3600);
                            if let Err(e) = neg_cache.put(url, b"", None, 404, Some(neg_ttl)) {
                                eprintln!(
                                    "  WARNING: negative cache write failed for {}: {}",
                                    url, e
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("  WARNING: negative cache init failed: {}", e);
                        }
                    }
                }
            }
        }

        result
    }

    /// Fetch a POM file from the repository.
    /// Returns `Ok((PomMetadata, was_network_hit))` or `Err((FetchError, was_network_hit))`.
    fn fetch_pom(
        &self,
        group_id: &str,
        artifact_id: &str,
        version: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<(PomMetadata, bool), (FetchError, bool)> {
        let pom_url = self.build_pom_url(group_id, artifact_id, version);

        let pom_validator =
            |body: &[u8]| -> std::result::Result<(), String> { validate_pom_xml(body) };

        // TTL based on version classification
        let pom_ttl = match classify_version(Some(version)) {
            VersionClass::ConcreteVersion(_) => None, // indefinite
            VersionClass::Snapshot(_) => Some(Duration::from_secs(3600)), // 1h
            _ => Some(Duration::from_secs(24 * 3600)), // 24h default
        };

        let (bytes, was_hit) = if let Some(ref cache) = self.http_cache {
            let pom_cache = match cache.sibling("maven-pom") {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "  WARNING: POM cache init failed: {}, proceeding without cache",
                        e
                    );
                    return self.fetch_pom_direct(
                        group_id,
                        artifact_id,
                        version,
                        &pom_url,
                        base_delay_ms,
                    );
                }
            };
            let fetcher = CachedFetcher::new(
                pom_cache,
                Duration::from_secs(6 * 3600), // negative_ttl: 6h for 404s
                self.refresh,
            );
            let outcome = fetcher.fetch(&pom_url, pom_ttl, &pom_validator, |req_url, etag| {
                self.http_get_with_retry(req_url, etag, base_delay_ms, 5)
            });
            let hit = outcome.was_network_hit;
            match outcome.result {
                Ok(b) => (b, hit),
                Err(e) => return Err((e, hit)),
            }
        } else {
            self.direct_fetch_with_retry(&pom_url, base_delay_ms, 5)
                .map_err(|e| (e, true))?
        };

        // Validate XML structure before parsing (the cached path already
        // validated via CachedFetcher, but the direct/no-cache path did not).
        validate_pom_xml(&bytes).map_err(|detail| {
            (
                FetchError::InvalidResponse {
                    url: pom_url.clone(),
                    detail,
                },
                was_hit,
            )
        })?;

        let xml = std::str::from_utf8(&bytes).map_err(|e| {
            (
                FetchError::Parse {
                    url: pom_url.clone(),
                    detail: e.to_string(),
                },
                was_hit,
            )
        })?;
        let pom = self
            .parse_pom(xml, group_id, artifact_id, version)
            .map_err(|e| {
                (
                    FetchError::Parse {
                        url: pom_url.clone(),
                        detail: e,
                    },
                    was_hit,
                )
            })?;
        Ok((pom, was_hit))
    }

    /// Direct POM fetch without cache (shared by no-cache path and cache-init fallback).
    fn fetch_pom_direct(
        &self,
        group_id: &str,
        artifact_id: &str,
        version: &str,
        pom_url: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<(PomMetadata, bool), (FetchError, bool)> {
        let (bytes, was_hit) = self
            .direct_fetch_with_retry(pom_url, base_delay_ms, 5)
            .map_err(|e| (e, true))?;

        // Validate XML structure before parsing
        validate_pom_xml(&bytes).map_err(|detail| {
            (
                FetchError::InvalidResponse {
                    url: pom_url.to_string(),
                    detail,
                },
                was_hit,
            )
        })?;

        let xml = std::str::from_utf8(&bytes).map_err(|e| {
            (
                FetchError::Parse {
                    url: pom_url.to_string(),
                    detail: e.to_string(),
                },
                was_hit,
            )
        })?;
        let pom = self
            .parse_pom(xml, group_id, artifact_id, version)
            .map_err(|e| {
                (
                    FetchError::Parse {
                        url: pom_url.to_string(),
                        detail: e,
                    },
                    was_hit,
                )
            })?;
        Ok((pom, was_hit))
    }

    /// HTTP GET with retry logic, returning `HttpResponse` for CachedFetcher.
    ///
    /// Retries on 429 (rate limit) and 5xx (server errors), matching
    /// `FetchError::is_retryable()`. Tracks the last status so the
    /// fallthrough error carries a real status code, not a fabricated 0.
    fn http_get_with_retry(
        &self,
        url: &str,
        etag: Option<&str>,
        base_delay_ms: &mut u64,
        max_attempts: u32,
    ) -> std::result::Result<HttpResponse, FetchError> {
        let mut last_status: Option<u16> = None;

        for attempt in 0..max_attempts {
            let mut request = self.client.get(url);
            if let Some(etag_val) = etag {
                request = request.header("If-None-Match", etag_val);
            }

            match request.send() {
                Ok(response) => {
                    let status = response.status().as_u16();
                    last_status = Some(status);

                    // Retryable: 429 and 5xx
                    if status == 429 || status >= 500 {
                        if attempt < max_attempts - 1 {
                            let delay_secs = if status == 429 {
                                response
                                    .headers()
                                    .get("retry-after")
                                    .and_then(|h| h.to_str().ok())
                                    .and_then(|s| s.parse::<u64>().ok())
                                    .unwrap_or_else(|| 2u64.pow(attempt))
                            } else {
                                2u64.pow(attempt + 1)
                            };
                            eprintln!("  HTTP {}, backing off {}s...", status, delay_secs);
                            std::thread::sleep(Duration::from_secs(delay_secs));
                            *base_delay_ms = (*base_delay_ms * 2).min(5000);
                            continue;
                        }
                        // Last attempt exhausted -- fall through to return error
                        return Err(FetchError::HttpStatus {
                            url: url.to_string(),
                            status,
                        });
                    }

                    let resp_etag = response
                        .headers()
                        .get("etag")
                        .and_then(|h| h.to_str().ok())
                        .map(|s| s.to_string());

                    let bytes = response.bytes().map_err(|e| FetchError::Transport {
                        url: url.to_string(),
                        source: e,
                    })?;

                    return Ok(HttpResponse {
                        status,
                        bytes: bytes.to_vec(),
                        etag: resp_etag,
                    });
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        let delay = Duration::from_secs(2u64.pow(attempt));
                        eprintln!("  Network error, retrying in {:?}...", delay);
                        std::thread::sleep(delay);
                        continue;
                    }
                    return Err(FetchError::Transport {
                        url: url.to_string(),
                        source: e,
                    });
                }
            }
        }

        // Should not reach here, but if it does, use tracked status
        Err(FetchError::HttpStatus {
            url: url.to_string(),
            status: last_status.unwrap_or(0),
        })
    }

    /// Direct fetch without cache (used when http_cache is None).
    fn direct_fetch_with_retry(
        &self,
        url: &str,
        base_delay_ms: &mut u64,
        max_attempts: u32,
    ) -> std::result::Result<(Vec<u8>, bool), FetchError> {
        let mut last_status: Option<u16> = None;

        for attempt in 0..max_attempts {
            match self.client.get(url).send() {
                Ok(response) => {
                    let status = response.status().as_u16();
                    last_status = Some(status);

                    if status == 429 || status >= 500 {
                        if attempt < max_attempts - 1 {
                            let delay = 2u64.pow(attempt + 1);
                            eprintln!("  HTTP {}, backing off {}s...", status, delay);
                            std::thread::sleep(Duration::from_secs(delay));
                            *base_delay_ms = (*base_delay_ms * 2).min(5000);
                            continue;
                        }
                        return Err(FetchError::HttpStatus {
                            url: url.to_string(),
                            status,
                        });
                    }

                    if status == 404 {
                        return Err(FetchError::NotFound {
                            url: url.to_string(),
                        });
                    }

                    if !response.status().is_success() {
                        return Err(FetchError::HttpStatus {
                            url: url.to_string(),
                            status,
                        });
                    }

                    let bytes = response.bytes().map_err(|e| FetchError::Transport {
                        url: url.to_string(),
                        source: e,
                    })?;
                    return Ok((bytes.to_vec(), true));
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        std::thread::sleep(Duration::from_secs(2u64.pow(attempt)));
                        continue;
                    }
                    return Err(FetchError::Transport {
                        url: url.to_string(),
                        source: e,
                    });
                }
            }
        }

        Err(FetchError::HttpStatus {
            url: url.to_string(),
            status: last_status.unwrap_or(0),
        })
    }

    fn build_pom_url(&self, group_id: &str, artifact_id: &str, version: &str) -> String {
        let group_path = group_id.replace('.', "/");
        format!(
            "{}/{}/{}/{}/{}-{}.pom",
            self.repo_base, group_path, artifact_id, version, artifact_id, version
        )
    }

    fn parse_pom(
        &self,
        xml: &str,
        group_id: &str,
        artifact_id: &str,
        version: &str,
    ) -> std::result::Result<PomMetadata, String> {
        parse_pom(xml, group_id, artifact_id, version)
    }

    /// Emit package metadata triples (everything except dependencies).
    /// Returns (triples_count, pkg_uri) so callers can emit deps separately.
    fn emit_package_metadata(
        &self,
        writer: &mut NTriplesWriter,
        pom: &PomMetadata,
    ) -> Result<(usize, String)> {
        let name = format!("{}/{}", pom.group_id, pom.artifact_id);
        let identity_name = format!("{}:{}", pom.group_id, pom.artifact_id);
        let pkg_uri = package_uri("maven", "central", "any", &name, &pom.version);
        let identity_uri = package_identity_uri("maven", "central", "any", &name);
        let mut triples = 0;

        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{MAVEN}MavenArtifact"))?;
        triples += 2;

        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &identity_name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&identity_uri, &format!("{PKG}identityName"), &identity_name)?;
        triples += 1;

        let purl = format!("pkg:maven/{}/{}", pom.group_id, pom.artifact_id);
        writer.write_literal(&identity_uri, &format!("{PKG}purl"), &purl)?;
        triples += 1;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &identity_name)?;
        triples += 1;

        writer.write_literal(&pkg_uri, &format!("{MAVEN}groupId"), &pom.group_id)?;
        writer.write_literal(&pkg_uri, &format!("{MAVEN}artifactId"), &pom.artifact_id)?;
        triples += 2;

        let ver_uri = version_uri("maven", "central", &name, &pom.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &pom.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        let dist_uri = distro_uri("maven");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        if let Some(desc) = &pom.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(url) = &pom.url {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), url)?;
            triples += 1;
        }
        for license in &pom.licenses {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }

        if let Some(scm_url) = &pom.scm_url {
            if let Some(repo_uri) = crate::uris::normalize_forge_url(scm_url) {
                writer.write_triple(
                    &identity_uri,
                    &format!("{PKG}upstreamRepository"),
                    &repo_uri,
                )?;
                writer.write_triple(&repo_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;

                if let Some(conn) = &pom.scm_connection {
                    let clone_url = conn.strip_prefix("scm:git:").unwrap_or(conn);
                    writer.write_literal(&repo_uri, &format!("{VCS}cloneUrl"), clone_url)?;
                    triples += 1;
                }
            }
        }

        if let Some(tag) = &pom.scm_tag {
            if tag != "HEAD" && !tag.is_empty() {
                let tag_uri = format!(
                    "{DATA}tag/maven/{}/{}/{}",
                    pom.group_id, pom.artifact_id, tag
                );
                writer.write_triple(&pkg_uri, &format!("{VCS}packagedFromTag"), &tag_uri)?;
                writer.write_triple(&tag_uri, RDF_TYPE, &format!("{VCS}Tag"))?;
                writer.write_literal(&tag_uri, &format!("{VCS}tagName"), tag)?;
                triples += 3;
            }
        }

        Ok((triples, pkg_uri))
    }

    /// Emit all triples for an artifact (package metadata + all dependencies).
    /// Dependencies are resolved through pom.properties before emission.
    #[allow(dead_code)] // used by tests
    fn emit_artifact_triples(
        &self,
        writer: &mut NTriplesWriter,
        pom: &PomMetadata,
    ) -> Result<usize> {
        let (mut triples, pkg_uri) = self.emit_package_metadata(writer, pom)?;

        for (ordinal, dep) in pom.dependencies.iter().enumerate() {
            let resolved = resolve_dependency(pom, dep);
            if !resolved.is_emittable {
                continue;
            }
            triples += self.emit_resolved_dep(writer, &pkg_uri, ordinal, dep, &resolved)?;
        }

        Ok(triples)
    }

    /// Emit dependency triples using resolved coordinates for the target URI.
    ///
    /// Takes both the raw `PomDependency` (for dep_identity blank node key)
    /// and the `ResolvedDependency` (for target URI and emitted values).
    fn emit_resolved_dep(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        ordinal: usize,
        raw_dep: &PomDependency,
        resolved: &ResolvedDependency,
    ) -> Result<usize> {
        let dep_name = format!("{}/{}", resolved.group_id, resolved.artifact_id);
        let target_uri = package_identity_uri("maven", "central", "any", &dep_name);
        let mut triples = 0;

        // Target identity typing
        let identity_name = format!("{}:{}", resolved.group_id, resolved.artifact_id);
        writer.write_triple(&target_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&target_uri, &format!("{PKG}identityName"), &identity_name)?;
        let purl = format!("pkg:maven/{}/{}", resolved.group_id, resolved.artifact_id);
        writer.write_literal(&target_uri, &format!("{PKG}purl"), &purl)?;
        triples += 3;

        writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
        triples += 1;

        let dep_key = dep_identity(pkg_uri, ordinal, raw_dep);
        let bnode = bnode_id("dep", &dep_key);
        writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
        writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
        writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
        writer.write_bnode_subject(
            &bnode,
            &format!("{PKG}dependencyType"),
            &dep_type_uri(&resolved.scope),
        )?;
        writer.write_bnode_literal(&bnode, &format!("{MAVEN}scope"), &resolved.scope)?;
        triples += 5;

        if resolved.optional {
            writer.write_bnode_literal(&bnode, &format!("{MAVEN}optional"), "true")?;
            triples += 1;
        }

        if resolved.dependency_type != "jar" {
            writer.write_bnode_literal(
                &bnode,
                &format!("{MAVEN}type"),
                &resolved.dependency_type,
            )?;
            triples += 1;
        }

        if !resolved.classifier.is_empty() {
            writer.write_bnode_literal(
                &bnode,
                &format!("{MAVEN}classifier"),
                &resolved.classifier,
            )?;
            triples += 1;
        }

        if let Some(version_constraint) = &resolved.version {
            let cb = bnode_id("constraint", &dep_key);
            writer.write_bnode_to_bnode(&bnode, &format!("{PKG}hasVersionConstraint"), &cb)?;
            writer.write_bnode_subject(&cb, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
            writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintOperator"), "maven")?;
            writer.write_bnode_literal(
                &cb,
                &format!("{PKG}versionConstraintValue"),
                version_constraint,
            )?;
            triples += 4;
        }

        for (excl_idx, (excl_g, excl_a)) in resolved.exclusions.iter().enumerate() {
            let excl_key = format!("{}#{}", dep_key, excl_idx);
            let excl_bnode = bnode_id("excl", &excl_key);
            writer.write_bnode_to_bnode(&bnode, &format!("{MAVEN}hasExclusion"), &excl_bnode)?;
            writer.write_bnode_subject(
                &excl_bnode,
                RDF_TYPE,
                &format!("{MAVEN}DependencyExclusion"),
            )?;
            writer.write_bnode_literal(&excl_bnode, &format!("{MAVEN}excludedGroupId"), excl_g)?;
            writer.write_bnode_literal(
                &excl_bnode,
                &format!("{MAVEN}excludedArtifactId"),
                excl_a,
            )?;
            triples += 4;
        }

        Ok(triples)
    }

    /// Recursive BFS traversal of Maven dependency graph.
    ///
    /// Emits raw POM declarations, NOT effective Maven resolution.
    /// Dependencies are traversed only when they have compile/runtime scope,
    /// are non-optional, and have concrete (non-SNAPSHOT, non-range) versions.
    pub fn collect_recursive(
        &self,
        seeds: Vec<(String, String)>,
        output_path: &str,
    ) -> Result<(usize, usize)> {
        // Finding 4: empty seed → truly empty output
        if seeds.is_empty() {
            eprintln!("WARNING: no seed coordinates provided, nothing to collect");
            let file = File::create(output_path)?;
            let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());
            writer.flush()?;
            return Ok((0, 0));
        }

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        let mut state = TraversalState {
            queue: VecDeque::new(),
            scheduled: HashSet::new(),
            roots_provided: seeds.len(),
            roots_unique: 0,
            roots_resolved: 0,
            root_resolution_failures: 0,
            fetched_ok: 0,
            fetch_errors: HashMap::new(),
            non_emittable_unresolved: 0,
            non_traversable_snapshot: 0,
            non_traversable_range: 0,
            non_traversable_unresolved: 0,
            non_traversable_special: 0,
            skipped_depth: 0,
            skipped_limit: 0,
            skipped_roots: 0,
        };

        // Deduplicate roots preserving order
        let mut seen_roots = HashSet::new();
        let unique_seeds: Vec<_> = seeds
            .into_iter()
            .filter(|c| seen_roots.insert((c.0.clone(), c.1.clone())))
            .collect();
        state.roots_unique = unique_seeds.len();
        eprintln!(
            "Loaded {} Maven coordinates ({} unique)",
            state.roots_provided, state.roots_unique
        );

        let mut base_delay_ms = self.delay_ms;

        // Root resolution phase
        for (idx, (group, artifact)) in unique_seeds.iter().enumerate() {
            if idx >= self.max_roots {
                state.skipped_roots += unique_seeds.len() - idx;
                break;
            }
            // Finding 5: count ALL remaining roots when limit hit
            if state.scheduled.len() >= self.max_packages {
                state.skipped_limit += unique_seeds.len() - idx;
                break;
            }
            match self.get_latest_version(group, artifact, &mut base_delay_ms) {
                Ok((version, was_hit)) => {
                    state.roots_resolved += 1;
                    try_enqueue(
                        &mut state,
                        group,
                        artifact,
                        &version,
                        0,
                        self.max_depth,
                        self.max_packages,
                    );
                    if was_hit && self.delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(self.delay_ms));
                    }
                }
                Err((_e, was_hit)) => {
                    state.root_resolution_failures += 1;
                    state.roots_resolved += 1;
                    eprintln!(
                        "  Root resolution failed for {}:{}: {}",
                        group, artifact, _e
                    );
                    if was_hit && self.delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(self.delay_ms));
                    }
                }
            }
        }

        let mut total_packages = 0usize;
        let mut total_triples = 0usize;

        // BFS traversal loop
        while let Some((g, a, v, depth)) = state.queue.pop_front() {
            if (total_packages + 1) % 100 == 0 {
                eprintln!(
                    "Progress: {} fetched, {} scheduled, queue={}",
                    total_packages,
                    state.scheduled.len(),
                    state.queue.len()
                );
            }

            match self.fetch_pom(&g, &a, &v, &mut base_delay_ms) {
                Ok((pom, was_hit)) => {
                    state.fetched_ok += 1;

                    // Emit package metadata (without deps)
                    let (pkg_triples, pkg_uri) = self.emit_package_metadata(&mut writer, &pom)?;
                    total_triples += pkg_triples;
                    total_packages += 1;

                    // Process dependencies via ResolvedDependency
                    for (ordinal, dep) in pom.dependencies.iter().enumerate() {
                        let resolved = resolve_dependency(&pom, dep);

                        if !resolved.is_emittable {
                            state.non_emittable_unresolved += 1;
                            continue;
                        }

                        // Emit dependency triples
                        total_triples +=
                            self.emit_resolved_dep(&mut writer, &pkg_uri, ordinal, dep, &resolved)?;

                        // Traverse only compile/runtime, non-optional
                        if !should_traverse_resolved(&resolved) {
                            continue;
                        }

                        match &resolved.version_class {
                            VersionClass::ConcreteVersion(cv) => {
                                try_enqueue(
                                    &mut state,
                                    &resolved.group_id,
                                    &resolved.artifact_id,
                                    cv,
                                    depth + 1,
                                    self.max_depth,
                                    self.max_packages,
                                );
                            }
                            VersionClass::Snapshot(_) => {
                                state.non_traversable_snapshot += 1;
                            }
                            VersionClass::VersionRange(_) => {
                                state.non_traversable_range += 1;
                            }
                            VersionClass::UnresolvedProperty(_) => {
                                state.non_traversable_unresolved += 1;
                            }
                            VersionClass::SpecialToken(_) => {
                                state.non_traversable_special += 1;
                            }
                            VersionClass::NoVersion => {
                                state.non_traversable_unresolved += 1;
                            }
                        }
                    }

                    if was_hit && self.delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(self.delay_ms));
                    }
                }
                Err((e, was_hit)) => {
                    *state
                        .fetch_errors
                        .entry(e.classification().to_string())
                        .or_insert(0) += 1;
                    eprintln!("  Error fetching {}:{}:{}: {}", g, a, v, e);
                    if was_hit && self.delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(self.delay_ms));
                    }
                }
            }
        }

        writer.flush()?;

        // Summary
        let total_fetch_errors: usize = state.fetch_errors.values().sum();
        let error_breakdown: String = state
            .fetch_errors
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join(", ");
        let non_traversable_sum = state.non_traversable_snapshot
            + state.non_traversable_range
            + state.non_traversable_unresolved
            + state.non_traversable_special;
        let non_traversable_breakdown = format!(
            "snapshot={}, range={}, unresolved={}, special={}",
            state.non_traversable_snapshot,
            state.non_traversable_range,
            state.non_traversable_unresolved,
            state.non_traversable_special,
        );

        eprintln!("\n=== Maven Collection Summary ===");
        eprintln!("Roots provided:       {}", state.roots_provided);
        eprintln!("Roots unique:         {}", state.roots_unique);
        eprintln!("Roots resolved:       {}", state.roots_resolved);
        eprintln!("Root failures:        {}", state.root_resolution_failures);
        eprintln!("Scheduled:            {}", state.scheduled.len());
        eprintln!("Fetched OK:           {}", state.fetched_ok);
        eprintln!(
            "Fetch errors:         {} ({})",
            total_fetch_errors, error_breakdown
        );
        eprintln!("Non-emittable:        {}", state.non_emittable_unresolved);
        eprintln!(
            "Non-traversable:      {} ({})",
            non_traversable_sum, non_traversable_breakdown
        );
        eprintln!("Skipped (depth):      {}", state.skipped_depth);
        eprintln!("Skipped (limit):      {}", state.skipped_limit);
        eprintln!("Skipped (roots):      {}", state.skipped_roots);

        // Error rate check (threshold 20%)
        let root_rate = if state.roots_resolved > 0 {
            state.root_resolution_failures as f64 / state.roots_resolved as f64
        } else {
            0.0
        };
        let scheduled_count = state.scheduled.len();
        let pom_rate = if scheduled_count > 0 {
            total_fetch_errors as f64 / scheduled_count as f64
        } else {
            0.0
        };
        if root_rate > 0.2 || pom_rate > 0.2 {
            return Err(std::io::Error::other(format!(
                "error rate exceeded threshold (root: {:.1}%, pom: {:.1}%)",
                root_rate * 100.0,
                pom_rate * 100.0,
            )));
        }

        Ok((total_packages, total_triples))
    }
}

struct TraversalState {
    queue: VecDeque<(String, String, String, u32)>,
    scheduled: HashSet<(String, String, String)>,
    roots_provided: usize,
    roots_unique: usize,
    roots_resolved: usize,
    root_resolution_failures: usize,
    fetched_ok: usize,
    fetch_errors: HashMap<String, usize>,
    non_emittable_unresolved: usize,
    non_traversable_snapshot: usize,
    non_traversable_range: usize,
    non_traversable_unresolved: usize,
    non_traversable_special: usize,
    skipped_depth: usize,
    skipped_limit: usize,
    skipped_roots: usize,
}

fn try_enqueue(
    state: &mut TraversalState,
    g: &str,
    a: &str,
    v: &str,
    depth: u32,
    max_depth: u32,
    max_packages: usize,
) {
    if depth > max_depth {
        state.skipped_depth += 1;
        return;
    }
    let key = (g.to_string(), a.to_string(), v.to_string());
    if !state.scheduled.insert(key) {
        return;
    }
    if state.scheduled.len() > max_packages {
        state
            .scheduled
            .remove(&(g.to_string(), a.to_string(), v.to_string()));
        state.skipped_limit += 1;
        return;
    }
    state
        .queue
        .push_back((g.to_string(), a.to_string(), v.to_string(), depth));
}

fn should_traverse_resolved(resolved: &ResolvedDependency) -> bool {
    if resolved.optional {
        return false;
    }
    matches!(resolved.scope.as_str(), "compile" | "runtime")
}

fn contains_unresolved(s: &str) -> bool {
    s.contains("${")
}

/// Resolve a PomDependency through pom.properties and dependency management.
///
/// Both emission AND traversal consume the resulting ResolvedDependency.
fn resolve_dependency(pom: &PomMetadata, dep: &PomDependency) -> ResolvedDependency {
    let interp = |field: &str| -> String {
        interpolate_or_raw(
            field,
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        )
    };

    let group_id = interp(&dep.group_id);
    let artifact_id = interp(&dep.artifact_id);
    let dependency_type = interp(dep.dependency_type.as_deref().unwrap_or("jar"));
    let classifier = interp(dep.classifier.as_deref().unwrap_or(""));
    let scope = dep.scope.as_deref().unwrap_or("compile").to_string();

    let raw_version_expr = dep.version.clone();

    // Version resolution: dep.version -> interpolate -> management fallback -> interpolate
    // Use resolved fields for the management lookup so ${...} coordinates match
    let resolved_dep_for_lookup = PomDependency {
        group_id: group_id.clone(),
        artifact_id: artifact_id.clone(),
        dependency_type: Some(dependency_type.clone()),
        classifier: if classifier.is_empty() {
            None
        } else {
            Some(classifier.clone())
        },
        ..dep.clone()
    };
    let mgmt_version = lookup_in_dependency_management(pom, &resolved_dep_for_lookup);
    let base_version = dep.version.as_deref().or(mgmt_version.as_deref());
    let version = base_version.map(interp);

    let version_class = classify_version(version.as_deref());
    let is_emittable = !contains_unresolved(&group_id) && !contains_unresolved(&artifact_id);

    ResolvedDependency {
        group_id,
        artifact_id,
        version,
        dependency_type,
        classifier,
        scope,
        optional: dep.optional,
        exclusions: dep.exclusions.clone(),
        raw_version_expr,
        version_class,
        is_emittable,
    }
}

/// Build a length-delimited, injective blank-node identity string for a Maven dependency.
fn dep_identity(pkg_uri: &str, ordinal: usize, dep: &PomDependency) -> String {
    let fields = [
        pkg_uri,
        &ordinal.to_string(),
        &dep.group_id,
        &dep.artifact_id,
        dep.version.as_deref().unwrap_or(""),
        dep.dependency_type.as_deref().unwrap_or("jar"),
        dep.classifier.as_deref().unwrap_or(""),
        dep.scope.as_deref().unwrap_or("compile"),
        if dep.optional { "true" } else { "false" },
    ];
    fields
        .iter()
        .map(|f| format!("{}:{}", f.len(), f))
        .collect::<Vec<_>>()
        .join("/")
}

/// Parse a POM XML string into a `PomMetadata` struct.
///
/// Uses a path stack to distinguish between different `<dependencies>` contexts:
/// - `/project/dependencies/dependency` -> `pom.dependencies`
/// - `/project/dependencyManagement/dependencies/dependency` -> `pom.dependency_management`
/// - Plugin and profile dependencies are ignored.
///
/// Also parses `<properties>`, `<parent>`, `<type>`, `<classifier>`, and `<exclusions>`.
fn parse_pom(
    xml: &str,
    group_id: &str,
    artifact_id: &str,
    version: &str,
) -> std::result::Result<PomMetadata, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut pom = PomMetadata {
        group_id: group_id.to_string(),
        artifact_id: artifact_id.to_string(),
        version: version.to_string(),
        ..Default::default()
    };

    let mut buf = Vec::new();
    // Path stack tracks the full element path, e.g. ["project", "dependencies", "dependency"]
    let mut path_stack: Vec<String> = Vec::new();

    // Current dependency being built (used for both project deps and depMgmt deps)
    let mut current_dep = PomDependency {
        group_id: String::new(),
        artifact_id: String::new(),
        version: None,
        scope: None,
        optional: false,
        dependency_type: None,
        classifier: None,
        exclusions: Vec::new(),
    };

    // Current exclusion being built
    let mut current_exclusion_group_id = String::new();
    let mut current_exclusion_artifact_id = String::new();

    // Current property name being parsed
    let mut current_property_name = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                path_stack.push(name.clone());

                let path = path_stack.join("/");

                // Reset dependency when entering a dependency element in recognized contexts
                if name == "dependency"
                    && (path == "project/dependencies/dependency"
                        || path == "project/dependencyManagement/dependencies/dependency")
                {
                    current_dep = PomDependency {
                        group_id: String::new(),
                        artifact_id: String::new(),
                        version: None,
                        scope: None,
                        optional: false,
                        dependency_type: None,
                        classifier: None,
                        exclusions: Vec::new(),
                    };
                }

                // Reset exclusion fields when entering an exclusion element
                if name == "exclusion" && path.ends_with("/exclusions/exclusion") {
                    let dep_path_prefix_a = "project/dependencies/dependency/exclusions/exclusion";
                    let dep_path_prefix_b =
                        "project/dependencyManagement/dependencies/dependency/exclusions/exclusion";
                    if path == dep_path_prefix_a || path == dep_path_prefix_b {
                        current_exclusion_group_id.clear();
                        current_exclusion_artifact_id.clear();
                    }
                }

                // Track property name
                if path_stack.len() == 3
                    && path_stack[0] == "project"
                    && path_stack[1] == "properties"
                {
                    current_property_name = name;
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let path = path_stack.join("/");

                // Finalize dependency
                if name == "dependency"
                    && !current_dep.group_id.is_empty()
                    && !current_dep.artifact_id.is_empty()
                {
                    if path == "project/dependencies/dependency" {
                        pom.dependencies.push(current_dep.clone());
                    } else if path == "project/dependencyManagement/dependencies/dependency" {
                        pom.dependency_management.push(current_dep.clone());
                    }
                }

                // Finalize exclusion
                if name == "exclusion" {
                    let dep_path_prefix_a = "project/dependencies/dependency/exclusions/exclusion";
                    let dep_path_prefix_b =
                        "project/dependencyManagement/dependencies/dependency/exclusions/exclusion";
                    if (path == dep_path_prefix_a || path == dep_path_prefix_b)
                        && !current_exclusion_group_id.is_empty()
                        && !current_exclusion_artifact_id.is_empty()
                    {
                        current_dep.exclusions.push((
                            current_exclusion_group_id.clone(),
                            current_exclusion_artifact_id.clone(),
                        ));
                    }
                }

                // Clear property name when leaving property element
                if name == current_property_name
                    && path_stack.len() == 3
                    && path_stack[0] == "project"
                    && path_stack[1] == "properties"
                {
                    current_property_name.clear();
                }

                path_stack.pop();
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().trim().to_string();
                if text.is_empty() {
                    continue;
                }

                let path = path_stack.join("/");
                let current_element = path_stack.last().map(|s| s.as_str()).unwrap_or("");

                match current_element {
                    // Project-level fields (only at /project/description, etc.)
                    "description" if path == "project/description" => {
                        pom.description = Some(text);
                    }
                    "url" if path == "project/url" => {
                        pom.url = Some(text);
                    }

                    // License names
                    "name" if path == "project/licenses/license/name" => {
                        if !pom.licenses.contains(&text) {
                            pom.licenses.push(text);
                        }
                    }

                    // SCM fields (at /project/scm/*)
                    "url" if path == "project/scm/url" => {
                        pom.scm_url = Some(text);
                    }
                    "connection" if path == "project/scm/connection" => {
                        pom.scm_connection = Some(text);
                    }
                    "tag" if path == "project/scm/tag" => {
                        pom.scm_tag = Some(text);
                    }

                    // Parent coordinates
                    "groupId" if path == "project/parent/groupId" => {
                        pom.parent
                            .get_or_insert_with(ParentCoordinate::default)
                            .group_id = text;
                    }
                    "artifactId" if path == "project/parent/artifactId" => {
                        pom.parent
                            .get_or_insert_with(ParentCoordinate::default)
                            .artifact_id = text;
                    }
                    "version" if path == "project/parent/version" => {
                        pom.parent
                            .get_or_insert_with(ParentCoordinate::default)
                            .version = text;
                    }

                    // Properties
                    _ if path_stack.len() == 3
                        && path_stack[0] == "project"
                        && path_stack[1] == "properties"
                        && !current_property_name.is_empty() =>
                    {
                        pom.properties.insert(current_property_name.clone(), text);
                    }

                    // Dependency fields — only for recognized dependency paths
                    "groupId" if is_dep_child_path(&path, "groupId") => {
                        current_dep.group_id = text;
                    }
                    "artifactId" if is_dep_child_path(&path, "artifactId") => {
                        current_dep.artifact_id = text;
                    }
                    "version" if is_dep_child_path(&path, "version") => {
                        current_dep.version = Some(text);
                    }
                    "scope" if is_dep_child_path(&path, "scope") => {
                        current_dep.scope = Some(text);
                    }
                    "optional" if is_dep_child_path(&path, "optional") => {
                        current_dep.optional = text == "true";
                    }
                    "type" if is_dep_child_path(&path, "type") => {
                        current_dep.dependency_type = Some(text);
                    }
                    "classifier" if is_dep_child_path(&path, "classifier") => {
                        current_dep.classifier = Some(text);
                    }

                    // Exclusion fields
                    "groupId" if is_exclusion_child_path(&path, "groupId") => {
                        current_exclusion_group_id = text;
                    }
                    "artifactId" if is_exclusion_child_path(&path, "artifactId") => {
                        current_exclusion_artifact_id = text;
                    }

                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    Ok(pom)
}

/// Check if a path is a direct child of a recognized dependency element.
/// Recognized dependency paths:
/// - `project/dependencies/dependency/<field>`
/// - `project/dependencyManagement/dependencies/dependency/<field>`
fn is_dep_child_path(path: &str, field: &str) -> bool {
    path == format!("project/dependencies/dependency/{}", field)
        || path
            == format!(
                "project/dependencyManagement/dependencies/dependency/{}",
                field
            )
}

/// Check if a path is a direct child of a recognized exclusion element.
fn is_exclusion_child_path(path: &str, field: &str) -> bool {
    path == format!(
        "project/dependencies/dependency/exclusions/exclusion/{}",
        field
    ) || path
        == format!(
            "project/dependencyManagement/dependencies/dependency/exclusions/exclusion/{}",
            field
        )
}

/// Interpolate `${...}` property references in a value string.
///
/// Handles:
/// - `${project.version}` -> pom.version
/// - `${project.groupId}` -> pom.group_id
/// - `${project.artifactId}` -> pom.artifact_id
/// - `${custom.prop}` -> lookup in properties map
///
/// Supports nested property references (e.g. `${dep.group}` -> `${base.group}` -> `org.example`)
/// up to 10 levels deep. Detects cycles via an active expansion chain — a property can be
/// referenced multiple times without being a cycle (e.g. `${name}-${name}` works).
///
/// Returns `None` if a cycle is found or if any property cannot be resolved.
///
/// Used by the dependency traversal pipeline (Unit 6c) to resolve all coordinate fields.
pub(crate) fn interpolate_property(
    value: &str,
    properties: &HashMap<String, String>,
    pom_group_id: &str,
    pom_artifact_id: &str,
    pom_version: &str,
) -> Option<String> {
    interpolate_recursive(
        value,
        properties,
        pom_group_id,
        pom_artifact_id,
        pom_version,
        &mut std::collections::HashSet::new(),
        0,
    )
}

/// Recursive property interpolation with active-chain cycle detection.
///
/// `active_chain` tracks which keys are currently being expanded up the call stack.
/// A key is added before recursing into its resolved value and removed after, so the
/// same key can appear multiple times in the input without triggering a false cycle.
fn interpolate_recursive(
    value: &str,
    properties: &HashMap<String, String>,
    pom_group_id: &str,
    pom_artifact_id: &str,
    pom_version: &str,
    active_chain: &mut std::collections::HashSet<String>,
    depth: usize,
) -> Option<String> {
    if depth > 10 {
        return None;
    }
    if !value.contains("${") {
        return Some(value.to_string());
    }

    let mut result = value.to_string();
    let mut pos = 0;

    while let Some(start) = result[pos..].find("${") {
        let abs_start = pos + start;
        let end = result[abs_start..].find('}')?;
        let abs_end = abs_start + end;
        let key = result[abs_start + 2..abs_end].to_string();

        // Cycle: this key is already being expanded up the call stack
        if active_chain.contains(&key) {
            return None;
        }

        // Resolve the key to its raw value
        let resolved = match key.as_str() {
            "project.version" | "pom.version" => Some(pom_version.to_string()),
            "project.groupId" | "pom.groupId" => Some(pom_group_id.to_string()),
            "project.artifactId" | "pom.artifactId" => Some(pom_artifact_id.to_string()),
            other => properties.get(other).cloned(),
        }?;

        // If the resolved value itself contains ${...}, expand recursively
        let expanded = if resolved.contains("${") {
            active_chain.insert(key.clone());
            let r = interpolate_recursive(
                &resolved,
                properties,
                pom_group_id,
                pom_artifact_id,
                pom_version,
                active_chain,
                depth + 1,
            )?;
            active_chain.remove(&key);
            r
        } else {
            resolved
        };

        result = format!(
            "{}{}{}",
            &result[..abs_start],
            expanded,
            &result[abs_end + 1..]
        );
        pos = abs_start + expanded.len();
    }

    Some(result)
}

/// Look up a dependency's version in the dependency management section.
///
/// Matches on the full key `(groupId, artifactId, type, classifier)` with defaults
/// (`"jar"` for type, `""` for classifier). Management entry coordinates are
/// interpolated before comparison so `<groupId>${managed.group}</groupId>` matches
/// a resolved `org.example` dependency.
///
/// Returns the raw version from the matching management entry (which may itself
/// need interpolation by the caller).
///
/// Used by the dependency traversal pipeline (Unit 6c) to resolve managed versions.
pub(crate) fn lookup_in_dependency_management(
    pom: &PomMetadata,
    dep: &PomDependency,
) -> Option<String> {
    let dep_type = dep.dependency_type.as_deref().unwrap_or("jar");
    let dep_classifier = dep.classifier.as_deref().unwrap_or("");

    for mgmt in &pom.dependency_management {
        let mgmt_g = interpolate_or_raw(
            &mgmt.group_id,
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        );
        let mgmt_a = interpolate_or_raw(
            &mgmt.artifact_id,
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        );
        let mgmt_type_raw = mgmt.dependency_type.as_deref().unwrap_or("jar");
        let mgmt_type = interpolate_or_raw(
            mgmt_type_raw,
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        );
        let mgmt_classifier_raw = mgmt.classifier.as_deref().unwrap_or("");
        let mgmt_classifier = interpolate_or_raw(
            mgmt_classifier_raw,
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        );

        if mgmt_g == dep.group_id
            && mgmt_a == dep.artifact_id
            && mgmt_type == dep_type
            && mgmt_classifier == dep_classifier
        {
            return mgmt.version.clone();
        }
    }

    None
}

/// Interpolate a field value, falling back to the raw string if resolution fails.
fn interpolate_or_raw(
    field: &str,
    properties: &HashMap<String, String>,
    pom_group_id: &str,
    pom_artifact_id: &str,
    pom_version: &str,
) -> String {
    if field.contains("${") {
        interpolate_property(
            field,
            properties,
            pom_group_id,
            pom_artifact_id,
            pom_version,
        )
        .unwrap_or_else(|| field.to_string())
    } else {
        field.to_string()
    }
}

/// Validate that `body` is a well-formed POM XML with a `<project>` root element.
///
/// Uses the `quick_xml` parser rather than substring matching so that proxy
/// error pages, `<projects>` wrappers, comments, and truncated documents are
/// all rejected.
fn validate_pom_xml(body: &[u8]) -> std::result::Result<(), String> {
    let text = std::str::from_utf8(body).map_err(|e| format!("invalid UTF-8: {}", e))?;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut buf = Vec::new();
    let mut depth: usize = 0;
    let mut found_project_root = false;
    let mut seen_decl = false;
    let mut seen_non_decl = false;
    let mut seen_doctype = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name_bytes = e.name();
                if !found_project_root {
                    if name_bytes.as_ref() == b"project" {
                        found_project_root = true;
                        depth = 1;
                    } else {
                        let n = String::from_utf8_lossy(name_bytes.as_ref());
                        return Err(format!("expected <project> root, got <{}>", n));
                    }
                } else {
                    depth += 1;
                }
            }
            Ok(Event::End(ref e)) => {
                if found_project_root {
                    depth -= 1;
                    if depth == 0 && e.name().as_ref() == b"project" {
                        return validate_pom_trailing(&mut reader);
                    }
                }
                // End before root is unreachable in well-formed XML
                // (quick_xml would emit a parse error), but guard anyway.
            }
            Ok(Event::Empty(ref e)) => {
                let name_bytes = e.name();
                if !found_project_root {
                    if name_bytes.as_ref() == b"project" {
                        return validate_pom_trailing(&mut reader);
                    } else {
                        let n = String::from_utf8_lossy(name_bytes.as_ref());
                        return Err(format!("expected <project> root, got <{}/>", n));
                    }
                }
                // Self-closing child elements don't affect depth
            }
            Ok(Event::Text(ref t)) => {
                if !found_project_root {
                    let text_val = t.unescape().unwrap_or_default();
                    if !text_val.trim().is_empty() {
                        return Err("non-whitespace text before <project>".into());
                    }
                    seen_non_decl = true;
                }
            }
            Ok(Event::Eof) => {
                if found_project_root {
                    return Err("truncated POM: missing </project>".into());
                }
                return Err("empty document".into());
            }
            Ok(Event::Decl(_)) if !found_project_root => {
                if seen_decl {
                    return Err("duplicate XML declaration".into());
                }
                if seen_non_decl {
                    return Err("XML declaration must be first".into());
                }
                seen_decl = true;
            }
            Ok(Event::Comment(_)) | Ok(Event::PI(_)) if !found_project_root => {
                seen_non_decl = true;
            }
            Ok(Event::DocType(_)) if !found_project_root => {
                if seen_doctype {
                    return Err("duplicate DOCTYPE declaration".into());
                }
                seen_doctype = true;
                seen_non_decl = true;
            }
            Ok(Event::CData(_)) if !found_project_root => {
                return Err("CDATA outside document root".into());
            }
            // Inside root: reject Decl and DocType (illegal in document body)
            Ok(Event::Decl(_)) if found_project_root => {
                return Err("XML declaration inside document root".into());
            }
            Ok(Event::DocType(_)) if found_project_root => {
                return Err("DOCTYPE inside document root".into());
            }
            Ok(_) => {
                // Inside root: text, CDATA, comments, PI are fine.
            }
            Err(e) => return Err(format!("XML parse error: {}", e)),
        }
        buf.clear();
    }
}

/// After root `</project>` (or `<project/>`), verify only whitespace,
/// comments, and PI remain before EOF. Reject trailing elements or
/// non-whitespace text.
fn validate_pom_trailing(reader: &mut quick_xml::Reader<&[u8]>) -> std::result::Result<(), String> {
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => return Ok(()),
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let n = String::from_utf8_lossy(name.as_ref());
                return Err(format!("trailing element <{}> after </project>", n));
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.name();
                let n = String::from_utf8_lossy(name.as_ref());
                return Err(format!("trailing element <{}> after </project>", n));
            }
            Ok(Event::Text(ref t)) => {
                let text_val = t.unescape().unwrap_or_default();
                if !text_val.trim().is_empty() {
                    return Err("trailing non-whitespace text after </project>".into());
                }
            }
            Ok(Event::Comment(_)) | Ok(Event::PI(_)) => {}
            Ok(Event::CData(_)) => {
                return Err("CDATA after document root".into());
            }
            Ok(Event::Decl(_)) => {
                return Err("XML declaration after document root".into());
            }
            Ok(Event::DocType(_)) => {
                return Err("DOCTYPE after document root".into());
            }
            Ok(Event::End(_)) => {
                return Err("unmatched end tag after document root".into());
            }
            Err(e) => return Err(format!("XML parse error after root: {}", e)),
        }
        buf.clear();
    }
}

/// Validate search JSON: parseable, with valid `latestVersion` when docs exist.
///
/// - Empty `docs` array is accepted (legitimate "not found"). The caller
///   handles negative caching via `parse_search_version_with_cache()`.
/// - Non-empty docs with empty or missing `latestVersion` are REJECTED so
///   CachedFetcher does not cache malformed responses for 24h.
/// - Unparseable JSON / non-JSON (proxy errors, Cloudflare) is rejected.
fn validate_search_json(body: &[u8]) -> std::result::Result<(), String> {
    let text = std::str::from_utf8(body).map_err(|e| format!("non-UTF8: {}", e))?;
    let resp: SearchResponse =
        serde_json::from_str(text).map_err(|e| format!("JSON parse: {}", e))?;
    // Empty docs is valid (not-found) — accepted for caching
    // But if docs exist, every doc must have a nonempty latestVersion
    for doc in &resp.response.docs {
        if doc.latest_version.is_empty() {
            return Err("search result doc has empty latestVersion".into());
        }
    }
    Ok(())
}

/// Read Maven coordinates from seed file (one "groupId:artifactId" per line).
pub fn read_maven_seed_file(path: &str) -> Result<Vec<(String, String)>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut coords = Vec::new();

    for line_result in reader.lines() {
        let line = line_result?;
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some((group_id, artifact_id)) = trimmed.split_once(':') {
            coords.push((group_id.to_string(), artifact_id.to_string()));
        }
    }

    coords.sort();
    coords.dedup();

    Ok(coords)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_maven_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment").unwrap();
        writeln!(temp, "org.apache.commons:commons-lang3").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "com.google.guava:guava").unwrap();
        writeln!(temp, "org.apache.commons:commons-lang3").unwrap();
        temp.flush().unwrap();

        let coords = read_maven_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(coords.len(), 2);
        assert_eq!(
            coords[0],
            ("com.google.guava".to_string(), "guava".to_string())
        );
        assert_eq!(
            coords[1],
            (
                "org.apache.commons".to_string(),
                "commons-lang3".to_string()
            )
        );
    }

    #[test]
    fn test_parse_simple_pom() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-lib</artifactId>
  <version>1.0.0</version>
  <description>Example library</description>
  <url>https://example.org</url>
  <licenses>
    <license>
      <name>Apache-2.0</name>
    </license>
  </licenses>
  <dependencies>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
      <version>4.13.2</version>
      <scope>test</scope>
    </dependency>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>32.0.0-jre</version>
      <optional>true</optional>
    </dependency>
  </dependencies>
</project>"#;

        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        let pom = collector
            .parse_pom(pom_xml, "org.example", "my-lib", "1.0.0")
            .unwrap();

        assert_eq!(pom.group_id, "org.example");
        assert_eq!(pom.artifact_id, "my-lib");
        assert_eq!(pom.version, "1.0.0");
        assert_eq!(pom.description.unwrap(), "Example library");
        assert_eq!(pom.url.unwrap(), "https://example.org");
        assert_eq!(pom.licenses.len(), 1);
        assert_eq!(pom.licenses[0], "Apache-2.0");
        assert_eq!(pom.dependencies.len(), 2);
        assert_eq!(pom.dependencies[0].group_id, "junit");
        assert_eq!(pom.dependencies[0].scope.as_deref(), Some("test"));
        assert!(pom.dependencies[1].optional);
    }

    #[test]
    fn test_emit_maven_artifact_with_coordinates() {
        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pom = PomMetadata {
            group_id: "org.apache.commons".to_string(),
            artifact_id: "commons-lang3".to_string(),
            version: "3.14.0".to_string(),
            description: Some("Apache Commons Lang".to_string()),
            url: Some("https://commons.apache.org/proper/commons-lang/".to_string()),
            licenses: vec!["Apache-2.0".to_string()],
            dependencies: vec![],
            dependency_management: vec![],
            parent: None,
            properties: HashMap::new(),
            scm_url: None,
            scm_connection: None,
            scm_tag: None,
        };

        let triples = collector.emit_artifact_triples(&mut writer, &pom).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("maven#MavenArtifact"));
        assert!(content.contains("maven#groupId"));
        assert!(content.contains("maven#artifactId"));
        assert!(content.contains("\"org.apache.commons\""));
        assert!(content.contains("\"commons-lang3\""));
        assert!(content.contains("\"3.14.0\""));

        // Verify colon-form identityName and PURL
        assert!(content.contains("\"org.apache.commons:commons-lang3\""));
        assert!(content.contains("core#identityName"));
        assert!(content.contains("core#purl"));
        assert!(content.contains("\"pkg:maven/org.apache.commons/commons-lang3\""));

        assert!(triples > 10);
    }

    #[test]
    fn test_parse_pom_with_scm() {
        let collector = MavenCollector::new(
            "https://search.maven.org".to_string(),
            "https://repo1.maven.org/maven2".to_string(),
        );
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.springframework</groupId>
  <artifactId>spring-core</artifactId>
  <version>6.2.0</version>
  <url>https://spring.io/projects/spring-framework</url>
  <scm>
    <url>https://github.com/spring-projects/spring-framework</url>
    <connection>scm:git:git://github.com/spring-projects/spring-framework.git</connection>
    <tag>v6.2.0</tag>
  </scm>
</project>"#;

        let pom = collector
            .parse_pom(pom_xml, "org.springframework", "spring-core", "6.2.0")
            .unwrap();
        assert_eq!(
            pom.scm_url.as_deref(),
            Some("https://github.com/spring-projects/spring-framework")
        );
        assert_eq!(
            pom.scm_connection.as_deref(),
            Some("scm:git:git://github.com/spring-projects/spring-framework.git")
        );
        assert_eq!(pom.scm_tag.as_deref(), Some("v6.2.0"));
        // Verify project URL is NOT overwritten by SCM URL
        assert_eq!(
            pom.url.as_deref(),
            Some("https://spring.io/projects/spring-framework")
        );
    }

    // ── is_maven_central tests ──────────────────────────────────────────

    #[test]
    fn test_is_maven_central_repo1() {
        assert!(is_maven_central("https://repo1.maven.org/maven2"));
    }

    #[test]
    fn test_is_maven_central_apache() {
        assert!(is_maven_central("https://repo.maven.apache.org/maven2"));
    }

    #[test]
    fn test_is_maven_central_with_trailing_slash() {
        assert!(is_maven_central("https://repo1.maven.org/maven2/"));
    }

    #[test]
    fn test_not_maven_central_jfrog() {
        assert!(!is_maven_central("https://jfrog.example.com/maven"));
    }

    #[test]
    fn test_not_maven_central_nexus() {
        assert!(!is_maven_central(
            "https://nexus.internal.org/repository/maven-public"
        ));
    }

    #[test]
    fn test_not_maven_central_invalid_url() {
        assert!(!is_maven_central("not-a-url"));
    }

    // ── with_cache builder tests ────────────────────────────────────────

    #[test]
    fn test_with_cache_creates_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        )
        .with_cache(tmp.path().to_str().unwrap())
        .unwrap();

        assert!(collector.http_cache.is_some());
    }

    #[test]
    fn test_with_refresh() {
        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        )
        .with_refresh(true);

        assert!(collector.refresh);
    }

    // ── Cache TTL tests via collector methods against mock servers ──────

    #[test]
    fn test_concrete_pom_cached_indefinitely_via_collector() {
        use crate::http_cache::{HttpCache, MockClock};
        use std::sync::Arc;

        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_str().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));

        // Mock POM — should only be hit once
        let pom_mock = server
            .mock("GET", "/maven2/org/ex/lib/1.0/lib-1.0.pom")
            .with_status(200)
            .with_body(r#"<?xml version="1.0"?><project><groupId>org.ex</groupId><artifactId>lib</artifactId><version>1.0</version></project>"#)
            .expect(1)
            .create();

        let collector = MavenCollector {
            client: crate::enricher::default_http_client(),
            search_base: server.url(),
            repo_base: format!("{}/maven2", server.url()),
            http_cache: Some(HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap()),
            refresh: false,
            max_depth: 3,
            max_roots: 10_000,
            max_packages: 5_000,
            delay_ms: 0,
            graph_uri: None,
        };

        let mut delay = 1u64;
        let r1 = collector.fetch_pom("org.ex", "lib", "1.0", &mut delay);
        assert!(r1.is_ok());

        // Advance 30 days — concrete version should still be cached
        clock.advance(30 * 24 * 3600);

        let r2 = collector.fetch_pom("org.ex", "lib", "1.0", &mut delay);
        assert!(r2.is_ok());
        assert!(!r2.unwrap().1, "second call should use cache");

        pom_mock.assert(); // only 1 network hit
    }

    #[test]
    fn test_snapshot_pom_expires_after_1h_via_collector() {
        use crate::http_cache::{HttpCache, MockClock};
        use std::sync::Arc;

        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_str().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));

        // Mock returns POM — should be hit twice (once fresh, once after expiry)
        let _pom = server
            .mock("GET", mockito::Matcher::Regex(r".*1\.0-SNAPSHOT.*".into()))
            .with_status(200)
            .with_body(r#"<?xml version="1.0"?><project><groupId>org.ex</groupId><artifactId>lib</artifactId><version>1.0-SNAPSHOT</version></project>"#)
            .expect(2)
            .create();

        let collector = MavenCollector {
            client: crate::enricher::default_http_client(),
            search_base: server.url(),
            repo_base: format!("{}/maven2", server.url()),
            http_cache: Some(HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap()),
            refresh: false,
            max_depth: 3,
            max_roots: 10_000,
            max_packages: 5_000,
            delay_ms: 0,
            graph_uri: None,
        };

        let mut delay = 1u64;
        let r1 = collector.fetch_pom("org.ex", "lib", "1.0-SNAPSHOT", &mut delay);
        assert!(r1.is_ok());

        // Advance past 1h TTL
        clock.advance(2 * 3600);

        let r2 = collector.fetch_pom("org.ex", "lib", "1.0-SNAPSHOT", &mut delay);
        assert!(r2.is_ok());
        assert!(
            r2.unwrap().1,
            "second call should hit network after TTL expiry"
        );
    }

    #[test]
    fn test_search_cache_24h_ttl_via_collector() {
        use crate::http_cache::{HttpCache, MockClock};
        use std::sync::Arc;

        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_str().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));

        // Search should be hit twice (once fresh, once after 24h expiry)
        let _search = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch/select.*".into()),
            )
            .with_status(200)
            .with_body(r#"{"response":{"docs":[{"latestVersion":"1.0"}]}}"#)
            .expect(2)
            .create();

        let collector = MavenCollector {
            client: crate::enricher::default_http_client(),
            search_base: server.url(),
            repo_base: format!("{}/maven2", server.url()),
            http_cache: Some(HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap()),
            refresh: false,
            max_depth: 3,
            max_roots: 10_000,
            max_packages: 5_000,
            delay_ms: 0,
            graph_uri: None,
        };

        let mut delay = 1u64;
        let r1 = collector.get_latest_version("org.ex", "lib", &mut delay);
        assert!(r1.is_ok());
        assert!(r1.unwrap().1, "first call hits network");

        // Still cached at 23h
        clock.advance(23 * 3600);
        let r2 = collector.get_latest_version("org.ex", "lib", &mut delay);
        assert!(r2.is_ok());
        assert!(!r2.unwrap().1, "within 24h, should use cache");

        // Expired past 24h
        clock.advance(2 * 3600);
        let r3 = collector.get_latest_version("org.ex", "lib", &mut delay);
        assert!(r3.is_ok());
        assert!(r3.unwrap().1, "after 25h, should hit network again");
    }

    // ── Cache integration tests ─────────────────────────────────────────

    #[test]
    fn test_search_and_pom_use_separate_cache_namespaces() {
        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();

        // Set up search and POM mocks (each hit exactly once)
        let search = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch/select.*".into()),
            )
            .with_status(200)
            .with_body(r#"{"response":{"docs":[{"latestVersion":"1.0"}]}}"#)
            .expect(1)
            .create();
        let pom = server
            .mock("GET", "/maven2/org/ns/lib/1.0/lib-1.0.pom")
            .with_status(200)
            .with_body(r#"<?xml version="1.0"?><project><groupId>org.ns</groupId><artifactId>lib</artifactId><version>1.0</version></project>"#)
            .expect(1)
            .create();

        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()))
            .with_cache(tmp.path().to_str().unwrap())
            .unwrap();

        let mut delay = 1u64;

        // First call populates both caches
        let r = collector.fetch_artifact_with_retry("org.ns", "lib", &mut delay);
        assert!(r.is_ok());

        // Second call should be fully cached (neither endpoint hit)
        let r2 = collector.fetch_artifact_with_retry("org.ns", "lib", &mut delay);
        assert!(r2.is_ok());
        assert!(!r2.unwrap().1);

        search.assert();
        pom.assert();
    }

    #[test]
    fn test_cache_refresh_bypasses_fresh_entry() {
        use crate::http_cache::{HttpCache, MockClock};
        use std::sync::Arc;

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_str().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));

        let cache = HttpCache::with_clock(cache_dir, "maven-refresh-test", clock.clone()).unwrap();

        // Put a fresh entry
        cache
            .put(
                "https://example.com/test",
                b"cached-body",
                None,
                200,
                Some(Duration::from_secs(86400)),
            )
            .unwrap();

        // Without refresh: should get cache hit (no network)
        let fetcher_normal = CachedFetcher::new(
            HttpCache::with_clock(cache_dir, "maven-refresh-test", clock.clone()).unwrap(),
            Duration::from_secs(3600),
            false, // refresh=false
        );
        let outcome = fetcher_normal.fetch(
            "https://example.com/test",
            Some(Duration::from_secs(86400)),
            &|_| Ok(()),
            |_url, _etag| {
                panic!("should not hit network when cache is fresh");
            },
        );
        assert!(!outcome.was_network_hit);
        assert!(outcome.result.is_ok());

        // With refresh: should hit network even though entry is fresh
        let fetcher_refresh = CachedFetcher::new(
            HttpCache::with_clock(cache_dir, "maven-refresh-test", clock.clone()).unwrap(),
            Duration::from_secs(3600),
            true, // refresh=true
        );
        let outcome = fetcher_refresh.fetch(
            "https://example.com/test",
            Some(Duration::from_secs(86400)),
            &|_| Ok(()),
            |_url, _etag| {
                Ok(HttpResponse {
                    status: 200,
                    bytes: b"fresh-from-network".to_vec(),
                    etag: None,
                })
            },
        );
        assert!(outcome.was_network_hit);
        assert_eq!(outcome.result.unwrap(), b"fresh-from-network");
    }

    // ── HTTPS enforcement test ──────────────────────────────────────────

    #[test]
    fn test_not_maven_central_http() {
        // Plain HTTP should be rejected even if the host is correct
        assert!(!is_maven_central("http://repo1.maven.org/maven2"));
    }

    // ── POM validator tests ─────────────────────────────────────────────

    #[test]
    fn test_validate_pom_xml_valid() {
        let xml = b"<?xml version=\"1.0\"?>\n<project><groupId>org.example</groupId></project>";
        assert!(validate_pom_xml(xml).is_ok());
    }

    #[test]
    fn test_validate_pom_xml_rejects_html() {
        let html = b"<html><body>502 Bad Gateway</body></html>";
        let result = validate_pom_xml(html);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expected <project> root"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_empty() {
        let result = validate_pom_xml(b"");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty document"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_projects_wrapper() {
        let xml = b"<projects><project>stuff</project></projects>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expected <project> root"));
    }

    #[test]
    fn test_validate_pom_xml_skips_xml_decl() {
        // XML declaration + comment before <project>
        let xml = b"<?xml version=\"1.0\"?>\n<!-- comment -->\n<project></project>";
        assert!(validate_pom_xml(xml).is_ok());
    }

    #[test]
    fn test_validate_pom_xml_rejects_truncated() {
        let xml = b"<project><groupId>x";
        let result = validate_pom_xml(xml);
        assert!(result.is_err(), "truncated POM should be rejected");
        assert!(result.unwrap_err().contains("truncated POM"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_truncated_after_children() {
        let xml = b"<project><groupId>org.example</groupId><artifactId>lib</artifactId>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("truncated POM"));
    }

    #[test]
    fn test_validate_pom_xml_nested_project_truncated() {
        // Inner </project> closes a child, but outer <project> is still open
        let xml = b"<project><project></project>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err(), "outer <project> never closed");
        assert!(result.unwrap_err().contains("truncated POM"));
    }

    #[test]
    fn test_validate_pom_xml_self_closing() {
        // Self-closing <project/> is valid (unusual but well-formed)
        assert!(validate_pom_xml(b"<project/>").is_ok());
    }

    #[test]
    fn test_validate_pom_xml_self_closing_non_project() {
        let result = validate_pom_xml(b"<html/>");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expected <project> root"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_trailing_element() {
        let xml = b"<project></project><html/>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("trailing element"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_leading_text() {
        let xml = b"garbage<project></project>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-whitespace text before"));
    }

    #[test]
    fn test_validate_pom_xml_accepts_trailing_whitespace() {
        let xml = b"<project></project>\n  \n";
        assert!(validate_pom_xml(xml).is_ok());
    }

    #[test]
    fn test_validate_pom_xml_accepts_leading_comment() {
        let xml = b"<!-- comment --><project></project>";
        assert!(validate_pom_xml(xml).is_ok());
    }

    #[test]
    fn test_validate_pom_xml_rejects_cdata_before_root() {
        let xml = b"<![CDATA[x]]><project></project>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("CDATA outside document root"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_cdata_after_root() {
        let xml = b"<project></project><![CDATA[x]]>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("CDATA after document root"));
    }

    #[test]
    fn test_validate_pom_xml_accepts_xml_decl_before_root() {
        let xml = b"<?xml version=\"1.0\"?><project></project>";
        assert!(validate_pom_xml(xml).is_ok());
    }

    #[test]
    fn test_validate_pom_xml_rejects_xml_decl_after_root() {
        let xml = b"<project></project><?xml version=\"1.0\"?>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("XML declaration after document root"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_decl_after_comment() {
        let xml = b"<!-- comment --><?xml version=\"1.0\"?><project/>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("XML declaration must be first"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_duplicate_decl() {
        let xml = b"<?xml version=\"1.0\"?><?xml version=\"1.0\"?><project/>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("duplicate XML declaration"));
    }

    #[test]
    fn test_validate_pom_xml_accepts_single_decl() {
        let xml = b"<?xml version=\"1.0\"?><project/>";
        assert!(validate_pom_xml(xml).is_ok());
    }

    #[test]
    fn test_validate_pom_xml_rejects_decl_inside_root() {
        let xml = b"<project><?xml version=\"1.0\"?></project>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("XML declaration inside document root"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_doctype_inside_root() {
        let xml = b"<project><!DOCTYPE x></project>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("DOCTYPE inside document root"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_duplicate_doctype() {
        let xml = b"<!DOCTYPE project><!DOCTYPE project><project/>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("duplicate DOCTYPE"));
    }

    #[test]
    fn test_validate_pom_xml_accepts_single_doctype() {
        let xml = b"<!DOCTYPE project><project/>";
        assert!(validate_pom_xml(xml).is_ok());
    }

    // ── Search validator tests ──────────────────────────────────────────

    #[test]
    fn test_validate_search_json_valid() {
        let json = br#"{"response":{"docs":[{"latestVersion":"1.0"}]}}"#;
        assert!(validate_search_json(json).is_ok());
    }

    #[test]
    fn test_validate_search_json_accepts_empty_docs() {
        // Validator accepts all valid JSON structure. Empty docs is
        // handled semantically in parse_search_version(), not here.
        let json = br#"{"response":{"docs":[]}}"#;
        assert!(validate_search_json(json).is_ok());
    }

    #[test]
    fn test_validate_search_json_rejects_empty_latest_version() {
        // Empty latestVersion is malformed — validator rejects so
        // CachedFetcher does NOT cache this 200 body.
        let json = br#"{"response":{"docs":[{"latestVersion":""}]}}"#;
        let result = validate_search_json(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty latestVersion"));
    }

    #[test]
    fn test_validate_search_json_rejects_html() {
        let html = b"<html>Cloudflare challenge</html>";
        assert!(validate_search_json(html).is_err());
    }

    // ── Uncached path validation tests ──────────────────────────────────

    #[test]
    fn test_uncached_pom_path_rejects_html() {
        let mut server = mockito::Server::new();

        // Mock returns HTML with 200 (proxy error) — no cache enabled
        let _mock = server
            .mock("GET", "/maven2/org/proxy/lib/1.0/lib-1.0.pom")
            .with_status(200)
            .with_body("<html><body>502 Bad Gateway</body></html>")
            .create();

        // No cache — exercises the direct path
        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));

        let mut delay = 1u64;
        let result = collector.fetch_pom("org.proxy", "lib", "1.0", &mut delay);
        assert!(result.is_err(), "uncached HTML response should be rejected");
        let (err, _) = result.unwrap_err();
        assert!(
            matches!(err, FetchError::InvalidResponse { .. }),
            "expected InvalidResponse, got: {}",
            err
        );
    }

    #[test]
    fn test_uncached_pom_path_rejects_truncated() {
        let mut server = mockito::Server::new();

        let _mock = server
            .mock("GET", "/maven2/org/trunc2/lib/1.0/lib-1.0.pom")
            .with_status(200)
            .with_body("<project><groupId>org.trunc2</groupId>")
            .create();

        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));

        let mut delay = 1u64;
        let result = collector.fetch_pom("org.trunc2", "lib", "1.0", &mut delay);
        assert!(result.is_err());
        let (err, _) = result.unwrap_err();
        assert!(
            matches!(err, FetchError::InvalidResponse { .. }),
            "expected InvalidResponse for truncated POM, got: {}",
            err
        );
    }

    #[test]
    fn test_empty_latest_version_not_negative_cached() {
        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();

        // Returns a doc with empty latestVersion — should NOT be negative-cached.
        // Validator rejects → CachedFetcher does NOT cache the 200 body.
        let mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch/select.*".into()),
            )
            .with_status(200)
            .with_body(r#"{"response":{"docs":[{"latestVersion":""}]}}"#)
            .expect(2) // both calls hit network (not cached)
            .create();

        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()))
            .with_cache(tmp.path().to_str().unwrap())
            .unwrap();

        let mut delay = 1u64;

        let r1 = collector.get_latest_version("com.malformed", "lib", &mut delay);
        assert!(r1.is_err());
        let (err1, _) = r1.unwrap_err();
        assert!(
            matches!(err1, FetchError::InvalidResponse { .. }),
            "expected InvalidResponse for empty latestVersion, got: {}",
            err1
        );

        // Second call should also hit network (NOT negative-cached)
        let r2 = collector.get_latest_version("com.malformed", "lib", &mut delay);
        assert!(r2.is_err());

        mock.assert(); // verify both calls actually hit network
    }

    // ── Mockito-based collector acceptance tests ────────────────────────

    #[test]
    fn test_search_and_pom_cached_with_different_ttls() {
        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();

        let search_mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch/select.*".into()),
            )
            .with_status(200)
            .with_body(r#"{"response":{"docs":[{"latestVersion":"1.0.0"}]}}"#)
            .expect(1)
            .create();

        let pom_mock = server
            .mock("GET", "/maven2/org/example/lib/1.0.0/lib-1.0.0.pom")
            .with_status(200)
            .with_body(
                r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>lib</artifactId>
  <version>1.0.0</version>
</project>"#,
            )
            .expect(1)
            .create();

        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()))
            .with_cache(tmp.path().to_str().unwrap())
            .unwrap();

        let mut delay = 1u64;

        let result = collector.fetch_artifact_with_retry("org.example", "lib", &mut delay);
        assert!(result.is_ok());
        let (pom, was_hit) = result.unwrap();
        assert!(was_hit);
        assert_eq!(pom.version, "1.0.0");

        let result2 = collector.fetch_artifact_with_retry("org.example", "lib", &mut delay);
        assert!(result2.is_ok());
        assert!(!result2.unwrap().1);

        search_mock.assert();
        pom_mock.assert();
    }

    #[test]
    fn test_conditional_304_on_expired_pom_with_etag() {
        use crate::http_cache::{HttpCache, MockClock};
        use std::sync::Arc;

        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_str().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));

        // Pre-seed POM cache with expired entry containing ETag
        let pom_cache = HttpCache::with_clock(cache_dir, "maven-pom", clock.clone()).unwrap();
        pom_cache
            .put(
                &format!(
                    "{}/maven2/org/test/lib/2.0.0/lib-2.0.0.pom",
                    server.url()
                ),
                br#"<?xml version="1.0"?><project><groupId>org.test</groupId><artifactId>lib</artifactId><version>2.0.0</version></project>"#,
                Some("\"etag-abc\""),
                200,
                Some(Duration::from_secs(1)),
            )
            .unwrap();
        clock.advance(60);

        let _search = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch/select.*".into()),
            )
            .with_status(200)
            .with_body(r#"{"response":{"docs":[{"latestVersion":"2.0.0"}]}}"#)
            .create();

        let pom_304 = server
            .mock("GET", "/maven2/org/test/lib/2.0.0/lib-2.0.0.pom")
            .match_header("If-None-Match", "\"etag-abc\"")
            .with_status(304)
            .expect(1)
            .create();

        let collector = MavenCollector {
            client: crate::enricher::default_http_client(),
            search_base: server.url(),
            repo_base: format!("{}/maven2", server.url()),
            http_cache: Some(HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap()),
            refresh: false,
            max_depth: 3,
            max_roots: 10_000,
            max_packages: 5_000,
            delay_ms: 0,
            graph_uri: None,
        };

        let mut delay = 1u64;
        let result = collector.fetch_pom("org.test", "lib", "2.0.0", &mut delay);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().0.version, "2.0.0");
        pom_304.assert();
    }

    #[test]
    fn test_stale_fallback_on_transport_error() {
        use crate::http_cache::{HttpCache, MockClock};
        use std::sync::Arc;

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_str().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));

        let test_url = "http://dead-server:9999/maven2/org/stale/lib/1.0/lib-1.0.pom";
        let pom_body = br#"<?xml version="1.0"?><project><groupId>org.stale</groupId><artifactId>lib</artifactId><version>1.0</version></project>"#;

        // Pre-seed stale POM entry
        let pom_cache = HttpCache::with_clock(cache_dir, "maven-pom", clock.clone()).unwrap();
        pom_cache
            .put(test_url, pom_body, None, 200, Some(Duration::from_secs(1)))
            .unwrap();
        clock.advance(60);

        // Exercise CachedFetcher directly with a closure that returns
        // Transport error immediately — avoids the 15s retry loop in
        // http_get_with_retry (stale fallback is a CachedFetcher concern,
        // not a retry concern).
        let fetcher = CachedFetcher::new(
            HttpCache::with_clock(cache_dir, "maven-pom", clock.clone()).unwrap(),
            Duration::from_secs(6 * 3600),
            false,
        );

        // Get a real reqwest::Error for FetchError::Transport.
        // Wrapped in Option because FnMut closure may be called >1 time
        // but reqwest::Error is not Clone.
        let mut transport_err = Some(
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_millis(1))
                .build()
                .unwrap()
                .get("http://[::1]:1")
                .send()
                .unwrap_err(),
        );

        let outcome = fetcher.fetch(
            test_url,
            Some(Duration::from_secs(3600)),
            &|body| validate_pom_xml(body),
            |req_url, _etag| {
                Err(FetchError::Transport {
                    url: req_url.to_string(),
                    source: transport_err.take().unwrap_or_else(|| {
                        reqwest::blocking::Client::builder()
                            .timeout(Duration::from_millis(1))
                            .build()
                            .unwrap()
                            .get("http://[::1]:1")
                            .send()
                            .unwrap_err()
                    }),
                })
            },
        );

        assert!(outcome.was_network_hit);
        let body = outcome.result.expect("should fall back to stale entry");
        assert_eq!(body, pom_body.as_slice());
    }

    #[test]
    fn test_empty_search_docs_cached_as_negative() {
        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();

        // Mock returns empty docs (artifact not found).
        // Validator rejects → CachedFetcher doesn't cache as 200.
        // Collector manually caches 404 with 6h negative TTL.
        let empty_mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch/select.*".into()),
            )
            .with_status(200)
            .with_body(r#"{"response":{"docs":[]}}"#)
            .expect(1) // only one network call
            .create();

        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()))
            .with_cache(tmp.path().to_str().unwrap())
            .unwrap();

        let mut delay = 1u64;

        // First call: hits network, returns NotFound
        let result = collector.get_latest_version("com.missing", "artifact", &mut delay);
        assert!(result.is_err());
        let (err, was_hit) = result.unwrap_err();
        assert!(was_hit, "first call should hit network");
        assert!(
            matches!(err, FetchError::NotFound { .. }),
            "expected NotFound, got: {}",
            err
        );

        // Second call: should NOT hit network (cached 404)
        let result2 = collector.get_latest_version("com.missing", "artifact", &mut delay);
        assert!(result2.is_err());
        let (err2, was_hit2) = result2.unwrap_err();
        assert!(!was_hit2, "second call should use cached 404");
        assert!(matches!(err2, FetchError::NotFound { .. }));

        empty_mock.assert();
    }

    #[test]
    fn test_empty_search_docs_negative_cache_expires_at_6h() {
        use crate::http_cache::{HttpCache, MockClock};
        use std::sync::Arc;

        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_str().unwrap();
        let clock = Arc::new(MockClock::new(1_000_000));

        // First call returns empty docs, second (after 6h+) also returns empty docs
        let _empty = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch/select.*".into()),
            )
            .with_status(200)
            .with_body(r#"{"response":{"docs":[]}}"#)
            .expect(2) // hit twice: initial + after negative TTL expires
            .create();

        let collector = MavenCollector {
            client: crate::enricher::default_http_client(),
            search_base: server.url(),
            repo_base: format!("{}/maven2", server.url()),
            http_cache: Some(HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap()),
            refresh: false,
            max_depth: 3,
            max_roots: 10_000,
            max_packages: 5_000,
            delay_ms: 0,
            graph_uri: None,
        };

        let mut delay = 1u64;

        // First call: network hit, cached as 404 with 6h TTL
        let r1 = collector.get_latest_version("com.missing", "lib", &mut delay);
        assert!(r1.is_err());
        assert!(r1.unwrap_err().1, "first call should hit network");

        // At 5h: still cached
        clock.advance(5 * 3600);
        let r2 = collector.get_latest_version("com.missing", "lib", &mut delay);
        assert!(r2.is_err());
        assert!(!r2.unwrap_err().1, "at 5h, should use cached 404");

        // At 7h: negative cache expired, hits network again
        clock.advance(2 * 3600);
        let r3 = collector.get_latest_version("com.missing", "lib", &mut delay);
        assert!(r3.is_err());
        assert!(r3.unwrap_err().1, "at 7h, negative cache should be expired");
    }

    #[test]
    fn test_invalid_pom_not_cached() {
        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();

        let invalid_mock = server
            .mock("GET", "/maven2/org/bad/lib/1.0/lib-1.0.pom")
            .with_status(200)
            .with_body("<html><body>502 Bad Gateway</body></html>")
            .expect(2)
            .create();

        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()))
            .with_cache(tmp.path().to_str().unwrap())
            .unwrap();

        let mut delay = 1u64;
        let r1 = collector.fetch_pom("org.bad", "lib", "1.0", &mut delay);
        assert!(r1.is_err());
        let r2 = collector.fetch_pom("org.bad", "lib", "1.0", &mut delay);
        assert!(r2.is_err());
        invalid_mock.assert();
    }

    #[test]
    fn test_truncated_pom_not_cached() {
        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();

        // Truncated POM: has <project> but no </project>
        let truncated_mock = server
            .mock("GET", "/maven2/org/trunc/lib/1.0/lib-1.0.pom")
            .with_status(200)
            .with_body("<project><groupId>org.trunc</groupId>")
            .expect(2) // not cached, so hit twice
            .create();

        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()))
            .with_cache(tmp.path().to_str().unwrap())
            .unwrap();

        let mut delay = 1u64;
        let r1 = collector.fetch_pom("org.trunc", "lib", "1.0", &mut delay);
        assert!(r1.is_err());
        let r2 = collector.fetch_pom("org.trunc", "lib", "1.0", &mut delay);
        assert!(r2.is_err());
        truncated_mock.assert();
    }

    #[test]
    fn test_retry_429_then_success() {
        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();

        let pom_mock = server
            .mock("GET", "/maven2/org/retry/lib/1.0/lib-1.0.pom")
            .with_status(429)
            .with_header("retry-after", "0")
            .expect(1)
            .create();

        let pom_ok = server
            .mock("GET", "/maven2/org/retry/lib/1.0/lib-1.0.pom")
            .with_status(200)
            .with_body(r#"<?xml version="1.0"?><project><groupId>org.retry</groupId><artifactId>lib</artifactId><version>1.0</version></project>"#)
            .expect(1)
            .create();

        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()))
            .with_cache(tmp.path().to_str().unwrap())
            .unwrap();

        let mut delay = 1u64;
        let result = collector.fetch_pom("org.retry", "lib", "1.0", &mut delay);
        assert!(
            result.is_ok(),
            "Expected retry to succeed, got: {:?}",
            result.err()
        );
        let (pom, was_hit) = result.unwrap();
        assert_eq!(pom.group_id, "org.retry");
        assert!(was_hit);
        pom_mock.assert();
        pom_ok.assert();
    }

    #[test]
    fn test_failed_network_reports_was_network_hit() {
        let mut server = mockito::Server::new();
        let tmp = tempfile::tempdir().unwrap();

        let _search = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch/select.*".into()),
            )
            .with_status(200)
            .with_body(r#"{"response":{"docs":[{"latestVersion":"1.0"}]}}"#)
            .create();

        // Use 403 (non-retryable) instead of 500 to avoid exponential
        // backoff sleeps that would add ~30s to the test run.
        let _pom_403 = server
            .mock("GET", "/maven2/org/fail/lib/1.0/lib-1.0.pom")
            .with_status(403)
            .expect(1)
            .create();

        let collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()))
            .with_cache(tmp.path().to_str().unwrap())
            .unwrap();

        let mut delay = 1u64;
        let result = collector.fetch_artifact_with_retry("org.fail", "lib", &mut delay);
        assert!(result.is_err());
        let (err, was_hit) = result.unwrap_err();
        assert!(was_hit, "failed network should report was_network_hit=true");
        assert!(
            matches!(err, FetchError::HttpStatus { status: 403, .. }),
            "expected HttpStatus 403, got: {}",
            err
        );
    }

    // ── Path stack tests ───────────────────────────────────────────────

    #[test]
    fn test_parse_pom_dependencies_vs_dependency_management() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>org.springframework</groupId>
        <artifactId>spring-core</artifactId>
        <version>6.0.0</version>
      </dependency>
      <dependency>
        <groupId>org.slf4j</groupId>
        <artifactId>slf4j-api</artifactId>
        <version>2.0.0</version>
      </dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency>
      <groupId>org.springframework</groupId>
      <artifactId>spring-core</artifactId>
    </dependency>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
      <version>4.13.2</version>
      <scope>test</scope>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        // Project dependencies
        assert_eq!(pom.dependencies.len(), 2);
        assert_eq!(pom.dependencies[0].group_id, "org.springframework");
        assert_eq!(pom.dependencies[0].artifact_id, "spring-core");
        assert!(pom.dependencies[0].version.is_none());
        assert_eq!(pom.dependencies[1].group_id, "junit");
        assert_eq!(pom.dependencies[1].scope.as_deref(), Some("test"));

        // Dependency management
        assert_eq!(pom.dependency_management.len(), 2);
        assert_eq!(pom.dependency_management[0].group_id, "org.springframework");
        assert_eq!(
            pom.dependency_management[0].version.as_deref(),
            Some("6.0.0")
        );
        assert_eq!(pom.dependency_management[1].group_id, "org.slf4j");
        assert_eq!(
            pom.dependency_management[1].version.as_deref(),
            Some("2.0.0")
        );
    }

    #[test]
    fn test_parse_pom_plugin_dependencies_ignored() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <build>
    <plugins>
      <plugin>
        <groupId>org.apache.maven.plugins</groupId>
        <artifactId>maven-compiler-plugin</artifactId>
        <version>3.11.0</version>
        <dependencies>
          <dependency>
            <groupId>org.plugin.dep</groupId>
            <artifactId>plugin-lib</artifactId>
            <version>1.0</version>
          </dependency>
        </dependencies>
      </plugin>
    </plugins>
  </build>
  <dependencies>
    <dependency>
      <groupId>org.real</groupId>
      <artifactId>real-dep</artifactId>
      <version>2.0</version>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        // Only the real project dependency should be present
        assert_eq!(pom.dependencies.len(), 1);
        assert_eq!(pom.dependencies[0].group_id, "org.real");
        assert_eq!(pom.dependencies[0].artifact_id, "real-dep");

        // Plugin dependency should NOT appear
        assert!(pom.dependency_management.is_empty());
    }

    #[test]
    fn test_parse_pom_profile_dependencies_ignored() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <profiles>
    <profile>
      <id>dev</id>
      <dependencies>
        <dependency>
          <groupId>org.profile</groupId>
          <artifactId>profile-dep</artifactId>
          <version>1.0</version>
        </dependency>
      </dependencies>
    </profile>
  </profiles>
  <dependencies>
    <dependency>
      <groupId>org.real</groupId>
      <artifactId>real-dep</artifactId>
      <version>2.0</version>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        // Only the real project dependency
        assert_eq!(pom.dependencies.len(), 1);
        assert_eq!(pom.dependencies[0].group_id, "org.real");
    }

    // ── Property interpolation tests ───────────────────────────────────

    #[test]
    fn test_interpolate_project_version() {
        let props = HashMap::new();
        let result = interpolate_property("${project.version}", &props, "g", "a", "1.2.3");
        assert_eq!(result, Some("1.2.3".to_string()));
    }

    #[test]
    fn test_interpolate_project_group_id() {
        let props = HashMap::new();
        let result = interpolate_property("${project.groupId}", &props, "org.example", "a", "1.0");
        assert_eq!(result, Some("org.example".to_string()));
    }

    #[test]
    fn test_interpolate_project_artifact_id() {
        let props = HashMap::new();
        let result = interpolate_property("${project.artifactId}", &props, "g", "my-lib", "1.0");
        assert_eq!(result, Some("my-lib".to_string()));
    }

    #[test]
    fn test_interpolate_custom_property() {
        let mut props = HashMap::new();
        props.insert("spring.version".to_string(), "6.0.0".to_string());
        let result = interpolate_property("${spring.version}", &props, "g", "a", "1.0");
        assert_eq!(result, Some("6.0.0".to_string()));
    }

    #[test]
    fn test_interpolate_unresolvable_returns_none() {
        let props = HashMap::new();
        let result = interpolate_property("${parent.version}", &props, "g", "a", "1.0");
        assert_eq!(result, None);
    }

    #[test]
    fn test_interpolate_no_placeholder() {
        let props = HashMap::new();
        let result = interpolate_property("1.2.3", &props, "g", "a", "1.0");
        assert_eq!(result, Some("1.2.3".to_string()));
    }

    #[test]
    fn test_interpolate_all_five_fields() {
        // Verify interpolation works on all coordinate fields
        let mut props = HashMap::new();
        props.insert("my.type".to_string(), "test-jar".to_string());
        props.insert("my.classifier".to_string(), "sources".to_string());

        assert_eq!(
            interpolate_property("${project.groupId}", &props, "org.ex", "lib", "1.0"),
            Some("org.ex".to_string())
        );
        assert_eq!(
            interpolate_property("${project.artifactId}", &props, "org.ex", "lib", "1.0"),
            Some("lib".to_string())
        );
        assert_eq!(
            interpolate_property("${project.version}", &props, "org.ex", "lib", "1.0"),
            Some("1.0".to_string())
        );
        assert_eq!(
            interpolate_property("${my.type}", &props, "org.ex", "lib", "1.0"),
            Some("test-jar".to_string())
        );
        assert_eq!(
            interpolate_property("${my.classifier}", &props, "org.ex", "lib", "1.0"),
            Some("sources".to_string())
        );
    }

    #[test]
    fn test_interpolate_pom_version_alias() {
        let props = HashMap::new();
        let result = interpolate_property("${pom.version}", &props, "g", "a", "3.0.0");
        assert_eq!(result, Some("3.0.0".to_string()));
    }

    // ── Exclusion parsing tests ────────────────────────────────────────

    #[test]
    fn test_parse_pom_exclusions() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>org.springframework</groupId>
      <artifactId>spring-core</artifactId>
      <version>6.0.0</version>
      <exclusions>
        <exclusion>
          <groupId>commons-logging</groupId>
          <artifactId>commons-logging</artifactId>
        </exclusion>
        <exclusion>
          <groupId>log4j</groupId>
          <artifactId>log4j</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        assert_eq!(pom.dependencies.len(), 1);
        let dep = &pom.dependencies[0];
        assert_eq!(dep.exclusions.len(), 2);
        assert_eq!(
            dep.exclusions[0],
            ("commons-logging".to_string(), "commons-logging".to_string())
        );
        assert_eq!(
            dep.exclusions[1],
            ("log4j".to_string(), "log4j".to_string())
        );
    }

    #[test]
    fn test_parse_pom_exclusion_wildcard() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>org.heavy</groupId>
      <artifactId>heavy-lib</artifactId>
      <version>1.0</version>
      <exclusions>
        <exclusion>
          <groupId>*</groupId>
          <artifactId>*</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        assert_eq!(pom.dependencies[0].exclusions.len(), 1);
        assert_eq!(
            pom.dependencies[0].exclusions[0],
            ("*".to_string(), "*".to_string())
        );
    }

    // ── Dependency management lookup tests ─────────────────────────────

    #[test]
    fn test_lookup_dependency_management_full_key_matching() {
        let pom = PomMetadata {
            group_id: "org.example".to_string(),
            artifact_id: "my-app".to_string(),
            version: "1.0.0".to_string(),
            dependency_management: vec![
                PomDependency {
                    group_id: "org.test".to_string(),
                    artifact_id: "test-lib".to_string(),
                    version: Some("1.0".to_string()),
                    dependency_type: None, // defaults to "jar"
                    classifier: None,
                    scope: None,
                    optional: false,
                    exclusions: vec![],
                },
                PomDependency {
                    group_id: "org.test".to_string(),
                    artifact_id: "test-lib".to_string(),
                    version: Some("2.0".to_string()),
                    dependency_type: Some("test-jar".to_string()),
                    classifier: None,
                    scope: None,
                    optional: false,
                    exclusions: vec![],
                },
            ],
            ..Default::default()
        };

        // Lookup with default type (jar) -> matches first entry
        let dep_jar = PomDependency {
            group_id: "org.test".to_string(),
            artifact_id: "test-lib".to_string(),
            version: None,
            dependency_type: None, // defaults to "jar"
            classifier: None,
            scope: None,
            optional: false,
            exclusions: vec![],
        };
        assert_eq!(
            lookup_in_dependency_management(&pom, &dep_jar),
            Some("1.0".to_string())
        );

        // Lookup with type=test-jar -> matches second entry
        let dep_test_jar = PomDependency {
            group_id: "org.test".to_string(),
            artifact_id: "test-lib".to_string(),
            version: None,
            dependency_type: Some("test-jar".to_string()),
            classifier: None,
            scope: None,
            optional: false,
            exclusions: vec![],
        };
        assert_eq!(
            lookup_in_dependency_management(&pom, &dep_test_jar),
            Some("2.0".to_string())
        );
    }

    #[test]
    fn test_lookup_dependency_management_returns_uninterpolated_version() {
        let pom = PomMetadata {
            group_id: "org.example".to_string(),
            artifact_id: "my-app".to_string(),
            version: "1.0.0".to_string(),
            dependency_management: vec![PomDependency {
                group_id: "org.dep".to_string(),
                artifact_id: "dep-lib".to_string(),
                version: Some("${foo.version}".to_string()),
                dependency_type: None,
                classifier: None,
                scope: None,
                optional: false,
                exclusions: vec![],
            }],
            ..Default::default()
        };

        let dep = PomDependency {
            group_id: "org.dep".to_string(),
            artifact_id: "dep-lib".to_string(),
            version: None,
            dependency_type: None,
            classifier: None,
            scope: None,
            optional: false,
            exclusions: vec![],
        };

        // Returns the raw expression — interpolation is caller's responsibility
        assert_eq!(
            lookup_in_dependency_management(&pom, &dep),
            Some("${foo.version}".to_string())
        );
    }

    #[test]
    fn test_lookup_dependency_management_no_match() {
        let pom = PomMetadata {
            group_id: "org.example".to_string(),
            artifact_id: "my-app".to_string(),
            version: "1.0.0".to_string(),
            dependency_management: vec![PomDependency {
                group_id: "org.other".to_string(),
                artifact_id: "other-lib".to_string(),
                version: Some("1.0".to_string()),
                dependency_type: None,
                classifier: None,
                scope: None,
                optional: false,
                exclusions: vec![],
            }],
            ..Default::default()
        };

        let dep = PomDependency {
            group_id: "org.missing".to_string(),
            artifact_id: "missing-lib".to_string(),
            version: None,
            dependency_type: None,
            classifier: None,
            scope: None,
            optional: false,
            exclusions: vec![],
        };

        assert_eq!(lookup_in_dependency_management(&pom, &dep), None);
    }

    // ── New PomDependency field tests ──────────────────────────────────

    #[test]
    fn test_parse_pom_dependency_type_and_classifier() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>org.test</groupId>
      <artifactId>test-lib</artifactId>
      <version>1.0</version>
      <type>test-jar</type>
      <classifier>sources</classifier>
    </dependency>
    <dependency>
      <groupId>org.plain</groupId>
      <artifactId>plain-lib</artifactId>
      <version>2.0</version>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        assert_eq!(pom.dependencies.len(), 2);

        // First dep: explicit type and classifier
        assert_eq!(
            pom.dependencies[0].dependency_type.as_deref(),
            Some("test-jar")
        );
        assert_eq!(pom.dependencies[0].classifier.as_deref(), Some("sources"));

        // Second dep: no type or classifier -> None (caller defaults)
        assert!(pom.dependencies[1].dependency_type.is_none());
        assert!(pom.dependencies[1].classifier.is_none());
    }

    // ── Properties parsing tests ───────────────────────────────────────

    #[test]
    fn test_parse_pom_properties() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <properties>
    <spring.version>6.0.0</spring.version>
    <java.version>17</java.version>
    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
  </properties>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        assert_eq!(pom.properties.len(), 3);
        assert_eq!(pom.properties.get("spring.version").unwrap(), "6.0.0");
        assert_eq!(pom.properties.get("java.version").unwrap(), "17");
        assert_eq!(
            pom.properties.get("project.build.sourceEncoding").unwrap(),
            "UTF-8"
        );
    }

    // ── Parent coordinate tests ────────────────────────────────────────

    #[test]
    fn test_parse_pom_parent() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <parent>
    <groupId>org.springframework.boot</groupId>
    <artifactId>spring-boot-starter-parent</artifactId>
    <version>3.2.0</version>
  </parent>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        let parent = pom.parent.unwrap();
        assert_eq!(parent.group_id, "org.springframework.boot");
        assert_eq!(parent.artifact_id, "spring-boot-starter-parent");
        assert_eq!(parent.version, "3.2.0");
    }

    #[test]
    fn test_parse_pom_no_parent() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();
        assert!(pom.parent.is_none());
    }

    // ── Integration: properties + depMgmt in parsed POM ────────────────

    #[test]
    fn test_parse_pom_with_properties_and_dep_mgmt_version_ref() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <properties>
    <spring.version>6.1.0</spring.version>
  </properties>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>org.springframework</groupId>
        <artifactId>spring-core</artifactId>
        <version>${spring.version}</version>
      </dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency>
      <groupId>org.springframework</groupId>
      <artifactId>spring-core</artifactId>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        // depMgmt has the raw ${spring.version}
        assert_eq!(
            pom.dependency_management[0].version.as_deref(),
            Some("${spring.version}")
        );

        // Interpolation resolves it
        let resolved = interpolate_property(
            pom.dependency_management[0].version.as_deref().unwrap(),
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        );
        assert_eq!(resolved, Some("6.1.0".to_string()));

        // lookup_in_dependency_management returns the raw version
        let ver = lookup_in_dependency_management(&pom, &pom.dependencies[0]);
        assert_eq!(ver, Some("${spring.version}".to_string()));
    }

    // ── Exclusions in dependency management ────────────────────────────

    #[test]
    fn test_parse_pom_exclusions_in_dep_mgmt() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>org.heavy</groupId>
        <artifactId>heavy-lib</artifactId>
        <version>1.0</version>
        <exclusions>
          <exclusion>
            <groupId>org.unwanted</groupId>
            <artifactId>unwanted-dep</artifactId>
          </exclusion>
        </exclusions>
      </dependency>
    </dependencies>
  </dependencyManagement>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        assert_eq!(pom.dependency_management.len(), 1);
        assert_eq!(pom.dependency_management[0].exclusions.len(), 1);
        assert_eq!(
            pom.dependency_management[0].exclusions[0],
            ("org.unwanted".to_string(), "unwanted-dep".to_string())
        );
    }

    // ── End-to-end composed resolution test ────────────────────────────

    #[test]
    fn test_end_to_end_composed_resolution() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <properties>
    <my.group>org.example</my.group>
    <my.artifact>my-lib</my.artifact>
    <foo.version>2.0.0</foo.version>
    <dep.type>test-jar</dep.type>
  </properties>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>org.example</groupId>
        <artifactId>my-lib</artifactId>
        <version>${foo.version}</version>
        <type>test-jar</type>
      </dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency>
      <groupId>${my.group}</groupId>
      <artifactId>${my.artifact}</artifactId>
      <type>${dep.type}</type>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        let dep = &pom.dependencies[0];

        // Raw dependency has unresolved property references
        assert_eq!(dep.group_id, "${my.group}");
        assert_eq!(dep.artifact_id, "${my.artifact}");
        assert_eq!(dep.dependency_type.as_deref(), Some("${dep.type}"));
        assert!(dep.version.is_none());

        // Step 1: Interpolate all coordinate fields
        let resolved_group = interpolate_property(
            &dep.group_id,
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        )
        .unwrap();
        let resolved_artifact = interpolate_property(
            &dep.artifact_id,
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        )
        .unwrap();
        let resolved_type = interpolate_property(
            dep.dependency_type.as_deref().unwrap(),
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        )
        .unwrap();

        assert_eq!(resolved_group, "org.example");
        assert_eq!(resolved_artifact, "my-lib");
        assert_eq!(resolved_type, "test-jar");

        // Step 2: Build a resolved dependency for depMgmt lookup
        let resolved_dep = PomDependency {
            group_id: resolved_group.clone(),
            artifact_id: resolved_artifact.clone(),
            version: None,
            scope: None,
            optional: false,
            dependency_type: Some(resolved_type.clone()),
            classifier: None,
            exclusions: vec![],
        };

        // Step 3: Look up version in dependency management
        let mgmt_version = lookup_in_dependency_management(&pom, &resolved_dep);
        assert_eq!(mgmt_version, Some("${foo.version}".to_string()));

        // Step 4: Interpolate the management version
        let resolved_version = interpolate_property(
            mgmt_version.as_deref().unwrap(),
            &pom.properties,
            &pom.group_id,
            &pom.artifact_id,
            &pom.version,
        )
        .unwrap();

        // Final result: fully resolved, no ${...} values remain
        assert_eq!(resolved_group, "org.example");
        assert_eq!(resolved_artifact, "my-lib");
        assert_eq!(resolved_version, "2.0.0");
        assert_eq!(resolved_type, "test-jar");
        assert!(!resolved_group.contains("${"));
        assert!(!resolved_artifact.contains("${"));
        assert!(!resolved_version.contains("${"));
        assert!(!resolved_type.contains("${"));
    }

    // ── Nested property interpolation tests ────────────────────────────

    #[test]
    fn test_interpolate_nested_two_levels() {
        let mut props = HashMap::new();
        props.insert("base.group".to_string(), "org.example".to_string());
        props.insert("dep.group".to_string(), "${base.group}".to_string());

        let result = interpolate_property("${dep.group}", &props, "g", "a", "1.0");
        assert_eq!(result, Some("org.example".to_string()));
    }

    #[test]
    fn test_interpolate_cycle_returns_none() {
        let mut props = HashMap::new();
        props.insert("a".to_string(), "${b}".to_string());
        props.insert("b".to_string(), "${a}".to_string());

        let result = interpolate_property("${a}", &props, "g", "a", "1.0");
        assert_eq!(result, None);
    }

    #[test]
    fn test_interpolate_direct_still_works() {
        let mut props = HashMap::new();
        props.insert("ver".to_string(), "3.0.0".to_string());

        let result = interpolate_property("${ver}", &props, "g", "a", "1.0");
        assert_eq!(result, Some("3.0.0".to_string()));
    }

    #[test]
    fn test_interpolate_three_level_nesting() {
        let mut props = HashMap::new();
        props.insert("level3".to_string(), "resolved".to_string());
        props.insert("level2".to_string(), "${level3}".to_string());
        props.insert("level1".to_string(), "${level2}".to_string());

        let result = interpolate_property("${level1}", &props, "g", "a", "1.0");
        assert_eq!(result, Some("resolved".to_string()));
    }

    // ── Exclusion validation: require both fields ──────────────────────

    #[test]
    fn test_parse_pom_exclusion_missing_artifact_ignored() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>org.dep</groupId>
      <artifactId>dep-lib</artifactId>
      <version>1.0</version>
      <exclusions>
        <exclusion>
          <groupId>org.partial</groupId>
        </exclusion>
        <exclusion>
          <groupId>org.complete</groupId>
          <artifactId>complete-lib</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "my-app", "1.0.0").unwrap();

        // Only the complete exclusion (with both fields) should be kept
        assert_eq!(pom.dependencies[0].exclusions.len(), 1);
        assert_eq!(
            pom.dependencies[0].exclusions[0],
            ("org.complete".to_string(), "complete-lib".to_string())
        );
    }

    // ── Repeated property references (not cycles) ─────────────────────

    #[test]
    fn test_interpolate_repeated_ref_not_a_cycle() {
        let mut props = HashMap::new();
        props.insert("name".to_string(), "resolved".to_string());

        let result = interpolate_property("${name}-${name}", &props, "g", "a", "1.0");
        assert_eq!(result, Some("resolved-resolved".to_string()));
    }

    #[test]
    fn test_interpolate_multiple_distinct_refs() {
        let mut props = HashMap::new();
        props.insert("a".to_string(), "x".to_string());
        props.insert("b".to_string(), "y".to_string());

        let result = interpolate_property("${a}-${b}", &props, "g", "a", "1.0");
        assert_eq!(result, Some("x-y".to_string()));
    }

    // ── Management lookup with interpolated keys ──────────────────────

    #[test]
    fn test_lookup_dep_mgmt_interpolates_management_keys() {
        let mut props = HashMap::new();
        props.insert("managed.group".to_string(), "org.example".to_string());
        props.insert("managed.artifact".to_string(), "my-lib".to_string());

        let pom = PomMetadata {
            group_id: "org.parent".to_string(),
            artifact_id: "parent-app".to_string(),
            version: "1.0.0".to_string(),
            properties: props,
            dependency_management: vec![PomDependency {
                group_id: "${managed.group}".to_string(),
                artifact_id: "${managed.artifact}".to_string(),
                version: Some("3.0.0".to_string()),
                dependency_type: None,
                classifier: None,
                scope: None,
                optional: false,
                exclusions: vec![],
            }],
            ..Default::default()
        };

        // Dependency has already-resolved coordinates
        let dep = PomDependency {
            group_id: "org.example".to_string(),
            artifact_id: "my-lib".to_string(),
            version: None,
            dependency_type: None,
            classifier: None,
            scope: None,
            optional: false,
            exclusions: vec![],
        };

        // Should match after interpolating management entry keys
        assert_eq!(
            lookup_in_dependency_management(&pom, &dep),
            Some("3.0.0".to_string())
        );
    }

    // ── dep_identity + bnode uniqueness tests ──────────────────────────

    #[test]
    fn test_dep_identity_different_types_produce_distinct_bnodes() {
        let dep_jar = PomDependency {
            group_id: "org.example".into(),
            artifact_id: "lib".into(),
            version: Some("1.0".into()),
            scope: Some("compile".into()),
            optional: false,
            dependency_type: Some("jar".into()),
            classifier: None,
            exclusions: vec![],
        };
        let dep_test_jar = PomDependency {
            group_id: "org.example".into(),
            artifact_id: "lib".into(),
            version: Some("1.0".into()),
            scope: Some("compile".into()),
            optional: false,
            dependency_type: Some("test-jar".into()),
            classifier: None,
            exclusions: vec![],
        };
        let pkg = "https://example.org/pkg/1";
        let id_jar = dep_identity(pkg, 0, &dep_jar);
        let id_test_jar = dep_identity(pkg, 1, &dep_test_jar);
        assert_ne!(
            id_jar, id_test_jar,
            "jar vs test-jar must produce distinct identities"
        );

        let bnode_jar = bnode_id("dep", &id_jar);
        let bnode_test_jar = bnode_id("dep", &id_test_jar);
        assert_ne!(bnode_jar, bnode_test_jar);
    }

    #[test]
    fn test_dep_identity_different_scopes_produce_distinct_bnodes() {
        let dep_compile = PomDependency {
            group_id: "org.example".into(),
            artifact_id: "lib".into(),
            version: Some("1.0".into()),
            scope: Some("compile".into()),
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let dep_test = PomDependency {
            group_id: "org.example".into(),
            artifact_id: "lib".into(),
            version: Some("1.0".into()),
            scope: Some("test".into()),
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let pkg = "https://example.org/pkg/1";
        let id_compile = dep_identity(pkg, 0, &dep_compile);
        let id_test = dep_identity(pkg, 1, &dep_test);
        assert_ne!(id_compile, id_test);
    }

    #[test]
    fn test_dep_identity_different_ordinals_produce_distinct_bnodes() {
        let dep = PomDependency {
            group_id: "org.example".into(),
            artifact_id: "lib".into(),
            version: Some("1.0".into()),
            scope: None,
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![("com.unwanted".into(), "bad-lib".into())],
        };
        let pkg = "https://example.org/pkg/1";
        let id_0 = dep_identity(pkg, 0, &dep);
        let id_1 = dep_identity(pkg, 1, &dep);
        assert_ne!(
            id_0, id_1,
            "different ordinals must produce distinct identities"
        );
    }

    #[test]
    fn test_dep_identity_delimiter_bearing_values() {
        let dep_slash = PomDependency {
            group_id: "org/example".into(),
            artifact_id: "lib".into(),
            version: None,
            scope: None,
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let dep_colon = PomDependency {
            group_id: "org:example".into(),
            artifact_id: "lib".into(),
            version: None,
            scope: None,
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let dep_normal = PomDependency {
            group_id: "org.example".into(),
            artifact_id: "lib".into(),
            version: None,
            scope: None,
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let pkg = "https://example.org/pkg/1";
        let id_slash = dep_identity(pkg, 0, &dep_slash);
        let id_colon = dep_identity(pkg, 0, &dep_colon);
        let id_normal = dep_identity(pkg, 0, &dep_normal);
        assert_ne!(id_slash, id_colon);
        assert_ne!(id_slash, id_normal);
        assert_ne!(id_colon, id_normal);
    }

    #[test]
    fn test_emit_exclusion_triples() {
        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pom = PomMetadata {
            group_id: "org.example".to_string(),
            artifact_id: "app".to_string(),
            version: "2.0.0".to_string(),
            dependencies: vec![PomDependency {
                group_id: "org.unwanted".into(),
                artifact_id: "core".into(),
                version: Some("1.0".into()),
                scope: Some("compile".into()),
                optional: false,
                dependency_type: None,
                classifier: None,
                exclusions: vec![
                    ("com.excluded".into(), "bad-lib".into()),
                    ("*".into(), "*".into()),
                ],
            }],
            ..Default::default()
        };

        let triples = collector.emit_artifact_triples(&mut writer, &pom).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("maven#hasExclusion"),
            "missing hasExclusion predicate"
        );
        assert!(
            content.contains("maven#DependencyExclusion"),
            "missing DependencyExclusion type"
        );
        assert!(
            content.contains("maven#excludedGroupId"),
            "missing excludedGroupId"
        );
        assert!(
            content.contains("maven#excludedArtifactId"),
            "missing excludedArtifactId"
        );
        assert!(
            content.contains("\"com.excluded\""),
            "missing excluded groupId value"
        );
        assert!(
            content.contains("\"bad-lib\""),
            "missing excluded artifactId value"
        );
        assert!(
            content.contains("\"*\""),
            "wildcard exclusion must be emitted as literal '*'"
        );

        let excl_count = content
            .lines()
            .filter(|l| l.contains("maven#hasExclusion"))
            .count();
        assert_eq!(
            excl_count, 2,
            "expected 2 hasExclusion triples, got {}",
            excl_count
        );
        assert!(
            triples > 10,
            "expected at least base + exclusion triples, got {}",
            triples
        );
    }

    #[test]
    fn test_emit_type_and_classifier_triples() {
        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pom = PomMetadata {
            group_id: "org.example".to_string(),
            artifact_id: "app".to_string(),
            version: "1.0.0".to_string(),
            dependencies: vec![
                PomDependency {
                    group_id: "org.example".into(),
                    artifact_id: "lib".into(),
                    version: Some("1.0".into()),
                    scope: None,
                    optional: false,
                    dependency_type: Some("test-jar".into()),
                    classifier: Some("sources".into()),
                    exclusions: vec![],
                },
                PomDependency {
                    group_id: "org.example".into(),
                    artifact_id: "other".into(),
                    version: Some("2.0".into()),
                    scope: None,
                    optional: false,
                    dependency_type: Some("jar".into()),
                    classifier: None,
                    exclusions: vec![],
                },
            ],
            ..Default::default()
        };

        collector.emit_artifact_triples(&mut writer, &pom).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("maven#type"),
            "missing type predicate for test-jar"
        );
        assert!(content.contains("\"test-jar\""), "missing test-jar literal");
        assert!(
            content.contains("maven#classifier"),
            "missing classifier predicate"
        );
        assert!(
            content.contains("\"sources\""),
            "missing sources classifier literal"
        );

        let type_count = content.lines().filter(|l| l.contains("maven#type")).count();
        assert_eq!(
            type_count, 1,
            "jar type should not be emitted, got {} type triples",
            type_count
        );
    }

    #[test]
    fn test_same_ga_different_types_produce_distinct_dep_nodes() {
        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pom = PomMetadata {
            group_id: "com.app".to_string(),
            artifact_id: "main".to_string(),
            version: "3.0.0".to_string(),
            dependencies: vec![
                PomDependency {
                    group_id: "org.example".into(),
                    artifact_id: "lib".into(),
                    version: Some("1.0".into()),
                    scope: Some("compile".into()),
                    optional: false,
                    dependency_type: Some("jar".into()),
                    classifier: None,
                    exclusions: vec![("com.x".into(), "y".into())],
                },
                PomDependency {
                    group_id: "org.example".into(),
                    artifact_id: "lib".into(),
                    version: Some("1.0".into()),
                    scope: Some("test".into()),
                    optional: false,
                    dependency_type: Some("test-jar".into()),
                    classifier: Some("tests".into()),
                    exclusions: vec![("com.z".into(), "w".into())],
                },
            ],
            ..Default::default()
        };

        collector.emit_artifact_triples(&mut writer, &pom).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        let dep_triples: Vec<&str> = content
            .lines()
            .filter(|l| l.contains("core#hasDependency"))
            .collect();
        assert_eq!(dep_triples.len(), 2, "expected 2 hasDependency triples");
        assert_ne!(
            dep_triples[0], dep_triples[1],
            "dependency blank nodes must be distinct"
        );
        assert!(
            content.contains("\"com.x\""),
            "missing first dep's exclusion groupId"
        );
        assert!(
            content.contains("\"com.z\""),
            "missing second dep's exclusion groupId"
        );
    }

    #[test]
    fn test_parse_pom_with_exclusions_and_type() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>app</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>org.dep</groupId>
      <artifactId>lib</artifactId>
      <version>2.0</version>
      <type>test-jar</type>
      <classifier>sources</classifier>
      <scope>test</scope>
      <exclusions>
        <exclusion>
          <groupId>com.unwanted</groupId>
          <artifactId>bad</artifactId>
        </exclusion>
        <exclusion>
          <groupId>*</groupId>
          <artifactId>*</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
  </dependencies>
</project>"#;

        let pom = parse_pom(pom_xml, "org.example", "app", "1.0.0").unwrap();

        assert_eq!(pom.dependencies.len(), 1);
        let dep = &pom.dependencies[0];
        assert_eq!(dep.group_id, "org.dep");
        assert_eq!(dep.artifact_id, "lib");
        assert_eq!(dep.version.as_deref(), Some("2.0"));
        assert_eq!(dep.scope.as_deref(), Some("test"));
        assert_eq!(dep.dependency_type.as_deref(), Some("test-jar"));
        assert_eq!(dep.classifier.as_deref(), Some("sources"));
        assert_eq!(dep.exclusions.len(), 2);
        assert_eq!(
            dep.exclusions[0],
            ("com.unwanted".to_string(), "bad".to_string())
        );
        assert_eq!(dep.exclusions[1], ("*".to_string(), "*".to_string()));
    }

    #[test]
    fn test_two_identical_exclusions_produce_distinct_nodes() {
        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pom = PomMetadata {
            group_id: "org.example".to_string(),
            artifact_id: "app".to_string(),
            version: "1.0.0".to_string(),
            dependencies: vec![PomDependency {
                group_id: "org.dep".into(),
                artifact_id: "lib".into(),
                version: Some("1.0".into()),
                scope: None,
                optional: false,
                dependency_type: None,
                classifier: None,
                exclusions: vec![
                    ("com.dup".into(), "same".into()),
                    ("com.dup".into(), "same".into()),
                ],
            }],
            ..Default::default()
        };

        collector.emit_artifact_triples(&mut writer, &pom).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        let excl_count = content
            .lines()
            .filter(|l| l.contains("maven#hasExclusion"))
            .count();
        assert_eq!(
            excl_count, 2,
            "identical exclusions must produce 2 distinct nodes"
        );

        let type_count = content
            .lines()
            .filter(|l| l.contains("maven#DependencyExclusion"))
            .count();
        assert_eq!(type_count, 2, "expected 2 DependencyExclusion type triples");
    }

    // ── ResolvedDependency unit tests ─────────────────────────────────

    #[test]
    fn test_resolve_dependency_interpolates_group_and_artifact() {
        let pom = PomMetadata {
            group_id: "org.parent".into(),
            artifact_id: "parent-app".into(),
            version: "3.0".into(),
            properties: {
                let mut m = HashMap::new();
                m.insert("my.group".into(), "com.resolved".into());
                m
            },
            ..Default::default()
        };
        let dep = PomDependency {
            group_id: "${my.group}".into(),
            artifact_id: "lib".into(),
            version: Some("1.0".into()),
            scope: None,
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let resolved = resolve_dependency(&pom, &dep);
        assert_eq!(resolved.group_id, "com.resolved");
        assert!(resolved.is_emittable);
    }

    #[test]
    fn test_resolve_dependency_unresolvable_not_emittable() {
        let pom = PomMetadata {
            group_id: "org.a".into(),
            artifact_id: "a".into(),
            version: "1.0".into(),
            ..Default::default()
        };
        let dep = PomDependency {
            group_id: "${unknown.group}".into(),
            artifact_id: "lib".into(),
            version: Some("1.0".into()),
            scope: None,
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let resolved = resolve_dependency(&pom, &dep);
        assert!(!resolved.is_emittable);
    }

    #[test]
    fn test_resolve_dependency_managed_version() {
        let pom = PomMetadata {
            group_id: "org.a".into(),
            artifact_id: "a".into(),
            version: "1.0".into(),
            dependency_management: vec![PomDependency {
                group_id: "org.dep".into(),
                artifact_id: "lib".into(),
                version: Some("2.5.0".into()),
                scope: None,
                optional: false,
                dependency_type: None,
                classifier: None,
                exclusions: vec![],
            }],
            ..Default::default()
        };
        let dep = PomDependency {
            group_id: "org.dep".into(),
            artifact_id: "lib".into(),
            version: None, // no version, should come from management
            scope: None,
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let resolved = resolve_dependency(&pom, &dep);
        assert_eq!(resolved.version.as_deref(), Some("2.5.0"));
        assert!(matches!(
            resolved.version_class,
            VersionClass::ConcreteVersion(_)
        ));
    }

    #[test]
    fn test_resolve_dependency_interpolated_coords_match_management() {
        // Direct dep has ${dep.group}/${dep.artifact}, management has ${managed.group}/mylib.
        // Both resolve to org.dep/mylib. Management version must be found.
        let pom = PomMetadata {
            group_id: "org.a".into(),
            artifact_id: "a".into(),
            version: "1.0".into(),
            properties: {
                let mut m = HashMap::new();
                m.insert("dep.group".into(), "org.dep".into());
                m.insert("dep.artifact".into(), "mylib".into());
                m.insert("managed.group".into(), "org.dep".into());
                m
            },
            dependency_management: vec![PomDependency {
                group_id: "${managed.group}".into(),
                artifact_id: "mylib".into(),
                version: Some("2.5.0".into()),
                scope: None,
                optional: false,
                dependency_type: None,
                classifier: None,
                exclusions: vec![],
            }],
            ..Default::default()
        };
        let dep = PomDependency {
            group_id: "${dep.group}".into(),
            artifact_id: "${dep.artifact}".into(),
            version: None, // must come from management
            scope: None,
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let resolved = resolve_dependency(&pom, &dep);
        assert_eq!(resolved.group_id, "org.dep");
        assert_eq!(resolved.artifact_id, "mylib");
        assert_eq!(
            resolved.version.as_deref(),
            Some("2.5.0"),
            "management version must be found via interpolated coordinates"
        );
        assert!(resolved.is_emittable);
    }

    #[test]
    fn test_resolve_dependency_property_version() {
        let pom = PomMetadata {
            group_id: "org.a".into(),
            artifact_id: "a".into(),
            version: "1.0".into(),
            properties: {
                let mut m = HashMap::new();
                m.insert("foo.version".into(), "3.2.1".into());
                m
            },
            ..Default::default()
        };
        let dep = PomDependency {
            group_id: "org.dep".into(),
            artifact_id: "lib".into(),
            version: Some("${foo.version}".into()),
            scope: None,
            optional: false,
            dependency_type: None,
            classifier: None,
            exclusions: vec![],
        };
        let resolved = resolve_dependency(&pom, &dep);
        assert_eq!(resolved.version.as_deref(), Some("3.2.1"));
        assert!(matches!(
            resolved.version_class,
            VersionClass::ConcreteVersion(_)
        ));
    }

    #[test]
    fn test_resolve_dependency_interpolates_type_and_classifier() {
        let pom = PomMetadata {
            group_id: "org.a".into(),
            artifact_id: "a".into(),
            version: "1.0".into(),
            properties: {
                let mut m = HashMap::new();
                m.insert("dep.type".into(), "test-jar".into());
                m.insert("dep.classifier".into(), "tests".into());
                m
            },
            ..Default::default()
        };
        let dep = PomDependency {
            group_id: "org.dep".into(),
            artifact_id: "lib".into(),
            version: Some("1.0".into()),
            scope: None,
            optional: false,
            dependency_type: Some("${dep.type}".into()),
            classifier: Some("${dep.classifier}".into()),
            exclusions: vec![],
        };
        let resolved = resolve_dependency(&pom, &dep);
        assert_eq!(resolved.dependency_type, "test-jar");
        assert_eq!(resolved.classifier, "tests");
    }

    #[test]
    fn test_should_traverse_resolved() {
        let base = ResolvedDependency {
            group_id: "g".into(),
            artifact_id: "a".into(),
            version: Some("1.0".into()),
            dependency_type: "jar".into(),
            classifier: String::new(),
            scope: "compile".into(),
            optional: false,
            exclusions: vec![],
            raw_version_expr: None,
            version_class: VersionClass::ConcreteVersion("1.0".into()),
            is_emittable: true,
        };
        assert!(should_traverse_resolved(&base));

        let test_scope = ResolvedDependency {
            group_id: "g".into(),
            artifact_id: "a".into(),
            version: Some("1.0".into()),
            dependency_type: "jar".into(),
            classifier: String::new(),
            scope: "test".into(),
            optional: false,
            exclusions: vec![],
            raw_version_expr: None,
            version_class: VersionClass::ConcreteVersion("1.0".into()),
            is_emittable: true,
        };
        assert!(!should_traverse_resolved(&test_scope));

        let optional = ResolvedDependency {
            group_id: "g".into(),
            artifact_id: "a".into(),
            version: Some("1.0".into()),
            dependency_type: "jar".into(),
            classifier: String::new(),
            scope: "compile".into(),
            optional: true,
            exclusions: vec![],
            raw_version_expr: None,
            version_class: VersionClass::ConcreteVersion("1.0".into()),
            is_emittable: true,
        };
        assert!(!should_traverse_resolved(&optional));
    }

    #[test]
    fn test_contains_unresolved() {
        assert!(contains_unresolved("${project.groupId}"));
        assert!(contains_unresolved("org.${x}.core"));
        assert!(!contains_unresolved("org.example"));
        assert!(!contains_unresolved(""));
    }

    // ── Traversal unit tests ──────────────────────────────────────────

    #[test]
    fn test_try_enqueue_respects_depth_limit() {
        let mut state = TraversalState {
            queue: VecDeque::new(),
            scheduled: HashSet::new(),
            roots_provided: 0,
            roots_unique: 0,
            roots_resolved: 0,
            root_resolution_failures: 0,
            fetched_ok: 0,
            fetch_errors: HashMap::new(),
            non_emittable_unresolved: 0,
            non_traversable_snapshot: 0,
            non_traversable_range: 0,
            non_traversable_unresolved: 0,
            non_traversable_special: 0,
            skipped_depth: 0,
            skipped_limit: 0,
            skipped_roots: 0,
        };
        try_enqueue(&mut state, "g", "a", "1.0", 1, 1, 100);
        assert_eq!(state.queue.len(), 1);
        try_enqueue(&mut state, "g", "b", "1.0", 2, 1, 100);
        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.skipped_depth, 1);
    }

    #[test]
    fn test_try_enqueue_deduplicates() {
        let mut state = TraversalState {
            queue: VecDeque::new(),
            scheduled: HashSet::new(),
            roots_provided: 0,
            roots_unique: 0,
            roots_resolved: 0,
            root_resolution_failures: 0,
            fetched_ok: 0,
            fetch_errors: HashMap::new(),
            non_emittable_unresolved: 0,
            non_traversable_snapshot: 0,
            non_traversable_range: 0,
            non_traversable_unresolved: 0,
            non_traversable_special: 0,
            skipped_depth: 0,
            skipped_limit: 0,
            skipped_roots: 0,
        };
        try_enqueue(&mut state, "g", "a", "1.0", 0, 3, 100);
        try_enqueue(&mut state, "g", "a", "1.0", 0, 3, 100);
        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.scheduled.len(), 1);
    }

    #[test]
    fn test_try_enqueue_respects_max_packages() {
        let mut state = TraversalState {
            queue: VecDeque::new(),
            scheduled: HashSet::new(),
            roots_provided: 0,
            roots_unique: 0,
            roots_resolved: 0,
            root_resolution_failures: 0,
            fetched_ok: 0,
            fetch_errors: HashMap::new(),
            non_emittable_unresolved: 0,
            non_traversable_snapshot: 0,
            non_traversable_range: 0,
            non_traversable_unresolved: 0,
            non_traversable_special: 0,
            skipped_depth: 0,
            skipped_limit: 0,
            skipped_roots: 0,
        };
        try_enqueue(&mut state, "g", "a", "1.0", 0, 3, 2);
        try_enqueue(&mut state, "g", "b", "1.0", 0, 3, 2);
        assert_eq!(state.queue.len(), 2);
        try_enqueue(&mut state, "g", "c", "1.0", 0, 3, 2);
        assert_eq!(state.queue.len(), 2);
        assert_eq!(state.skipped_limit, 1);
    }

    #[test]
    fn test_max_depth_zero_is_seed_only() {
        let mut state = TraversalState {
            queue: VecDeque::new(),
            scheduled: HashSet::new(),
            roots_provided: 0,
            roots_unique: 0,
            roots_resolved: 0,
            root_resolution_failures: 0,
            fetched_ok: 0,
            fetch_errors: HashMap::new(),
            non_emittable_unresolved: 0,
            non_traversable_snapshot: 0,
            non_traversable_range: 0,
            non_traversable_unresolved: 0,
            non_traversable_special: 0,
            skipped_depth: 0,
            skipped_limit: 0,
            skipped_roots: 0,
        };
        try_enqueue(&mut state, "g", "a", "1.0", 0, 0, 100);
        assert_eq!(state.queue.len(), 1);
        try_enqueue(&mut state, "g", "b", "1.0", 1, 0, 100);
        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.skipped_depth, 1);
    }

    #[test]
    fn test_cycle_detection_via_scheduled_set() {
        let mut state = TraversalState {
            queue: VecDeque::new(),
            scheduled: HashSet::new(),
            roots_provided: 0,
            roots_unique: 0,
            roots_resolved: 0,
            root_resolution_failures: 0,
            fetched_ok: 0,
            fetch_errors: HashMap::new(),
            non_emittable_unresolved: 0,
            non_traversable_snapshot: 0,
            non_traversable_range: 0,
            non_traversable_unresolved: 0,
            non_traversable_special: 0,
            skipped_depth: 0,
            skipped_limit: 0,
            skipped_roots: 0,
        };
        try_enqueue(&mut state, "g", "A", "1.0", 0, 3, 100);
        try_enqueue(&mut state, "g", "B", "1.0", 1, 3, 100);
        try_enqueue(&mut state, "g", "A", "1.0", 2, 3, 100);
        assert_eq!(state.queue.len(), 2);
        assert_eq!(state.scheduled.len(), 2);
    }

    #[test]
    fn test_dense_fan_out_bounded() {
        let mut state = TraversalState {
            queue: VecDeque::new(),
            scheduled: HashSet::new(),
            roots_provided: 0,
            roots_unique: 0,
            roots_resolved: 0,
            root_resolution_failures: 0,
            fetched_ok: 0,
            fetch_errors: HashMap::new(),
            non_emittable_unresolved: 0,
            non_traversable_snapshot: 0,
            non_traversable_range: 0,
            non_traversable_unresolved: 0,
            non_traversable_special: 0,
            skipped_depth: 0,
            skipped_limit: 0,
            skipped_roots: 0,
        };
        for i in 0..100 {
            try_enqueue(&mut state, "g", &format!("dep-{}", i), "1.0", 1, 3, 5);
        }
        assert_eq!(state.scheduled.len(), 5);
        assert_eq!(state.skipped_limit, 95);
    }

    // ── Traversal integration tests (mockito) ─────────────────────────

    fn make_pom_xml(
        group: &str,
        artifact: &str,
        version: &str,
        deps: &[(&str, &str, &str, &str, bool)],
    ) -> String {
        let dep_xml: String = deps.iter().map(|(dg, da, dv, scope, optional)| {
            let opt = if *optional { "<optional>true</optional>" } else { "" };
            format!(
                "<dependency><groupId>{}</groupId><artifactId>{}</artifactId><version>{}</version><scope>{}</scope>{}</dependency>",
                dg, da, dv, scope, opt
            )
        }).collect();
        format!(
            r#"<?xml version="1.0"?><project><groupId>{}</groupId><artifactId>{}</artifactId><version>{}</version><dependencies>{}</dependencies></project>"#,
            group, artifact, version, dep_xml
        )
    }

    fn make_search_json(version: &str) -> String {
        format!(
            r#"{{"response":{{"docs":[{{"latestVersion":"{}"}}]}}}}"#,
            version
        )
    }

    #[test]
    fn test_recursive_depth_limit_a_b_c() {
        let mut server = mockito::Server::new();

        let _sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let _pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml(
                "org.a",
                "art-a",
                "1.0",
                &[("org.b", "art-b", "1.0", "compile", false)],
            ))
            .create();
        let _pb = server
            .mock("GET", "/maven2/org/b/art-b/1.0/art-b-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml(
                "org.b",
                "art-b",
                "1.0",
                &[("org.c", "art-c", "1.0", "compile", false)],
            ))
            .create();
        let pc = server
            .mock("GET", "/maven2/org/c/art-c/1.0/art-c-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml("org.c", "art-c", "1.0", &[]))
            .expect(0)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 1;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(
            result.is_ok(),
            "collect_recursive failed: {:?}",
            result.err()
        );
        let (packages, _) = result.unwrap();
        assert_eq!(packages, 2, "should fetch A and B (depth=0 and 1)");
        pc.assert();
    }

    #[test]
    fn test_recursive_cycle_detection() {
        let mut server = mockito::Server::new();
        let _sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml(
                "org.a",
                "art-a",
                "1.0",
                &[("org.b", "art-b", "1.0", "compile", false)],
            ))
            .expect(1)
            .create();
        let pb = server
            .mock("GET", "/maven2/org/b/art-b/1.0/art-b-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml(
                "org.b",
                "art-b",
                "1.0",
                &[("org.a", "art-a", "1.0", "compile", false)],
            ))
            .expect(1)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 5;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());
        let (packages, _) = result.unwrap();
        assert_eq!(packages, 2);
        pa.assert();
        pb.assert();
    }

    #[test]
    fn test_recursive_test_scope_emitted_not_traversed() {
        let mut server = mockito::Server::new();
        let _sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let _pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml(
                "org.a",
                "art-a",
                "1.0",
                &[
                    ("org.b", "art-b", "1.0", "compile", false),
                    ("org.c", "art-c", "1.0", "test", false),
                ],
            ))
            .create();
        let _pb = server
            .mock("GET", "/maven2/org/b/art-b/1.0/art-b-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml("org.b", "art-b", "1.0", &[]))
            .expect(1)
            .create();
        let pc = server
            .mock("GET", "/maven2/org/c/art-c/1.0/art-c-1.0.pom")
            .expect(0)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 3;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());

        let mut content = String::new();
        out.reopen().unwrap().read_to_string(&mut content).unwrap();
        assert!(content.contains("art-c"), "test dep should be emitted");
        pc.assert();
    }

    #[test]
    fn test_recursive_optional_emitted_not_traversed() {
        let mut server = mockito::Server::new();
        let _sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let _pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml(
                "org.a",
                "art-a",
                "1.0",
                &[("org.opt", "opt-lib", "1.0", "compile", true)],
            ))
            .create();
        let popt = server
            .mock("GET", "/maven2/org/opt/opt-lib/1.0/opt-lib-1.0.pom")
            .expect(0)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 3;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());

        let mut content = String::new();
        out.reopen().unwrap().read_to_string(&mut content).unwrap();
        assert!(content.contains("opt-lib"), "optional dep emitted");
        popt.assert();
    }

    #[test]
    fn test_recursive_snapshot_not_traversed() {
        let mut server = mockito::Server::new();
        let _sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let _pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml(
                "org.a",
                "art-a",
                "1.0",
                &[("org.snap", "snap-lib", "2.0-SNAPSHOT", "compile", false)],
            ))
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 3;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());
        let (packages, _) = result.unwrap();
        assert_eq!(packages, 1);

        let mut content = String::new();
        out.reopen().unwrap().read_to_string(&mut content).unwrap();
        assert!(content.contains("snap-lib"), "snapshot dep is emitted");
    }

    #[test]
    fn test_recursive_duplicate_roots_deduplicated() {
        let mut server = mockito::Server::new();
        let sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .expect(1)
            .create();
        let _pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml("org.a", "art-a", "1.0", &[]))
            .expect(1)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 0;
        collector.delay_ms = 0;

        let seeds = vec![
            ("org.a".into(), "art-a".into()),
            ("org.a".into(), "art-a".into()),
            ("org.a".into(), "art-a".into()),
        ];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());
        let (packages, _) = result.unwrap();
        assert_eq!(packages, 1);
        sa.assert();
    }

    #[test]
    fn test_recursive_all_roots_fail_error() {
        let mut server = mockito::Server::new();
        let _sa = server
            .mock("GET", mockito::Matcher::Regex(r"solrsearch".into()))
            .with_status(200)
            .with_body(r#"{"response":{"docs":[]}}"#)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 1;
        collector.delay_ms = 0;

        let seeds = vec![
            ("org.missing".into(), "lib1".into()),
            ("org.missing".into(), "lib2".into()),
        ];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_err(), "100% root failure should return error");
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("error rate exceeded threshold"));
    }

    #[test]
    fn test_recursive_empty_seed_empty_output() {
        let mut collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        collector.delay_ms = 0;

        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(vec![], out.path().to_str().unwrap());
        assert!(result.is_ok());
        let (packages, triples) = result.unwrap();
        assert_eq!(packages, 0);
        assert_eq!(triples, 0);

        // Output file should be empty (no distribution metadata)
        let mut content = String::new();
        out.reopen().unwrap().read_to_string(&mut content).unwrap();
        assert!(
            content.is_empty(),
            "empty seed should produce empty output, got: {}",
            content
        );
    }

    #[test]
    fn test_recursive_max_roots_boundary() {
        let mut server = mockito::Server::new();
        let _s = server
            .mock("GET", mockito::Matcher::Regex(r"solrsearch".into()))
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .expect(2)
            .create();
        let _p1 = server
            .mock("GET", "/maven2/org/a/a1/1.0/a1-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml("org.a", "a1", "1.0", &[]))
            .create();
        let _p2 = server
            .mock("GET", "/maven2/org/a/a2/1.0/a2-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml("org.a", "a2", "1.0", &[]))
            .create();
        let p3 = server
            .mock("GET", "/maven2/org/a/a3/1.0/a3-1.0.pom")
            .expect(0)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 0;
        collector.max_roots = 2;
        collector.delay_ms = 0;

        let seeds = vec![
            ("org.a".into(), "a1".into()),
            ("org.a".into(), "a2".into()),
            ("org.a".into(), "a3".into()),
        ];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());
        let (packages, _) = result.unwrap();
        assert_eq!(packages, 2);
        p3.assert();
    }

    #[test]
    fn test_recursive_skipped_roots_count_all_remaining() {
        // Finding 5: 10 roots, max_packages=2 → skipped count should be 8
        let mut server = mockito::Server::new();
        let _s = server
            .mock("GET", mockito::Matcher::Regex(r"solrsearch".into()))
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        for i in 0..2 {
            let art = format!("a{}", i);
            server
                .mock(
                    "GET",
                    format!("/maven2/org/a/{}/1.0/{}-1.0.pom", art, art).as_str(),
                )
                .with_status(200)
                .with_body(make_pom_xml("org.a", &art, "1.0", &[]))
                .create();
        }

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 0;
        collector.max_packages = 2;
        collector.delay_ms = 0;

        let seeds: Vec<_> = (0..10)
            .map(|i| ("org.a".into(), format!("a{}", i)))
            .collect();
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());
        let (packages, _) = result.unwrap();
        assert_eq!(packages, 2, "only 2 of 10 roots should be fetched");
    }

    #[test]
    fn test_recursive_unresolved_groupid_not_emitted() {
        let mut server = mockito::Server::new();
        let pom_with_unresolved = r#"<?xml version="1.0"?><project>
            <groupId>org.a</groupId><artifactId>art-a</artifactId><version>1.0</version>
            <dependencies>
                <dependency>
                    <groupId>${unresolvable.group}</groupId>
                    <artifactId>lib</artifactId>
                    <version>1.0</version>
                    <scope>compile</scope>
                </dependency>
            </dependencies>
        </project>"#;

        let _sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let _pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(pom_with_unresolved)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 3;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());

        let mut content = String::new();
        out.reopen().unwrap().read_to_string(&mut content).unwrap();
        assert!(
            !content.contains("${unresolvable.group}"),
            "unresolved groupId dep should not be emitted"
        );
    }

    #[test]
    fn test_recursive_resolved_groupid_is_traversed() {
        let mut server = mockito::Server::new();
        let pom_with_prop = r#"<?xml version="1.0"?><project>
            <groupId>org.a</groupId><artifactId>art-a</artifactId><version>1.0</version>
            <properties><dep.group>org.b</dep.group></properties>
            <dependencies>
                <dependency>
                    <groupId>${dep.group}</groupId>
                    <artifactId>art-b</artifactId>
                    <version>2.0</version>
                    <scope>compile</scope>
                </dependency>
            </dependencies>
        </project>"#;

        let _sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let _pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(pom_with_prop)
            .create();
        let pb = server
            .mock("GET", "/maven2/org/b/art-b/2.0/art-b-2.0.pom")
            .with_status(200)
            .with_body(make_pom_xml("org.b", "art-b", "2.0", &[]))
            .expect(1)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 3;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());
        let (packages, _) = result.unwrap();
        assert_eq!(packages, 2);
        pb.assert();
    }

    #[test]
    fn test_managed_version_in_rdf() {
        // Dep missing version, management provides it → emitted triple contains resolved version
        let mut server = mockito::Server::new();
        let pom_xml = r#"<?xml version="1.0"?><project>
            <groupId>org.a</groupId><artifactId>art-a</artifactId><version>1.0</version>
            <dependencyManagement><dependencies>
                <dependency>
                    <groupId>org.dep</groupId><artifactId>lib</artifactId><version>4.2.0</version>
                </dependency>
            </dependencies></dependencyManagement>
            <dependencies>
                <dependency>
                    <groupId>org.dep</groupId><artifactId>lib</artifactId><scope>compile</scope>
                </dependency>
            </dependencies>
        </project>"#;

        let _sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let _pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(pom_xml)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 0;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());

        let mut content = String::new();
        out.reopen().unwrap().read_to_string(&mut content).unwrap();
        assert!(
            content.contains("\"4.2.0\""),
            "managed version 4.2.0 should appear in emitted triples"
        );
    }

    #[test]
    fn test_property_version_resolved_in_rdf() {
        // Dep with ${foo.version}, property foo.version=1.0 → emitted triple contains 1.0
        let mut server = mockito::Server::new();
        let pom_xml = r#"<?xml version="1.0"?><project>
            <groupId>org.a</groupId><artifactId>art-a</artifactId><version>1.0</version>
            <properties><foo.version>5.3.1</foo.version></properties>
            <dependencies>
                <dependency>
                    <groupId>org.dep</groupId><artifactId>lib</artifactId>
                    <version>${foo.version}</version><scope>compile</scope>
                </dependency>
            </dependencies>
        </project>"#;

        let _sa = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"solrsearch.*g:org\.a".into()),
            )
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let _pa = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(pom_xml)
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 0;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());

        let mut content = String::new();
        out.reopen().unwrap().read_to_string(&mut content).unwrap();
        assert!(
            content.contains("\"5.3.1\""),
            "interpolated version should appear in emitted triples"
        );
        assert!(
            !content.contains("${foo.version}"),
            "raw property ref should not appear"
        );
    }

    #[test]
    fn test_depth_zero_through_collect_recursive() {
        // Same as flat collect: seeds processed, no traversal
        let mut server = mockito::Server::new();
        let _s = server
            .mock("GET", mockito::Matcher::Regex(r"solrsearch".into()))
            .with_status(200)
            .with_body(make_search_json("1.0"))
            .create();
        let _p = server
            .mock("GET", "/maven2/org/a/art-a/1.0/art-a-1.0.pom")
            .with_status(200)
            .with_body(make_pom_xml(
                "org.a",
                "art-a",
                "1.0",
                &[("org.b", "art-b", "1.0", "compile", false)],
            ))
            .create();
        let pb = server
            .mock("GET", "/maven2/org/b/art-b/1.0/art-b-1.0.pom")
            .expect(0) // depth=0 means no traversal
            .create();

        let mut collector = MavenCollector::new(server.url(), format!("{}/maven2", server.url()));
        collector.max_depth = 0;
        collector.delay_ms = 0;

        let seeds = vec![("org.a".into(), "art-a".into())];
        let out = NamedTempFile::new().unwrap();
        let result = collector.collect_recursive(seeds, out.path().to_str().unwrap());
        assert!(result.is_ok());
        let (packages, _) = result.unwrap();
        assert_eq!(packages, 1, "depth=0 should only fetch seed");
        pb.assert();
    }

    #[test]
    fn test_traversal_fields_wired_to_collector() {
        let mut collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        assert_eq!(collector.max_depth, 3);
        assert_eq!(collector.max_roots, 10_000);
        assert_eq!(collector.max_packages, 5_000);
        assert_eq!(collector.delay_ms, 500);

        collector.max_depth = 0;
        collector.max_roots = 50;
        collector.max_packages = 100;
        collector.delay_ms = 10;
        assert_eq!(collector.max_depth, 0);
        assert_eq!(collector.max_roots, 50);
        assert_eq!(collector.max_packages, 100);
        assert_eq!(collector.delay_ms, 10);
    }
}
