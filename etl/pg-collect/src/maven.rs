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
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

pub struct MavenCollector {
    client: Client,
    search_base: String,
    repo_base: String,
    http_cache: Option<HttpCache>,
    refresh: bool,
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

#[derive(Debug, Default)]
struct PomMetadata {
    group_id: String,
    artifact_id: String,
    version: String,
    description: Option<String>,
    url: Option<String>,
    licenses: Vec<String>,
    dependencies: Vec<PomDependency>,
    scm_url: Option<String>,
    scm_connection: Option<String>,
    scm_tag: Option<String>,
}

#[derive(Debug, Clone)]
struct PomDependency {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
    scope: Option<String>,
    optional: bool,
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
        }
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

    pub fn collect_discover(&self, endpoint: &str, output_path: &str) -> Result<(usize, usize)> {
        let raw_names = crate::seed::discover_by_ecosystem(endpoint, "maven")?;
        let raw_count = raw_names.len();
        // Normalize: strip version/classifier from old-format entries (g:a:v:c → g:a),
        // keep only valid groupId:artifactId pairs, deduplicate
        let mut seen = std::collections::HashSet::new();
        let names: Vec<String> = raw_names.into_iter().filter_map(|n| {
            let parts: Vec<&str> = n.splitn(3, ':').collect();
            if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                let coord = format!("{}:{}", parts[0], parts[1]);
                if seen.insert(coord.clone()) { Some(coord) } else { None }
            } else {
                None
            }
        }).collect();
        eprintln!("Normalized {} raw → {} unique Maven coordinates", raw_count, names.len());
        let seed_path = "/tmp/seed-maven-discover.txt";
        std::fs::write(seed_path, names.join("\n"))?;
        self.collect(seed_path, output_path)
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let coordinates = read_maven_seed_file(packages_file)?;
        eprintln!("Loaded {} Maven coordinates from seed file", coordinates.len());

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 500;

        for (idx, coord) in coordinates.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, coordinates.len());
            }

            let (group_id, artifact_id) = coord;
            match self.fetch_artifact_with_retry(group_id, artifact_id, &mut base_delay_ms) {
                Ok((pom, was_network_hit)) => {
                    total_triples += self.emit_artifact_triples(&mut writer, &pom)?;
                    total_packages += 1;
                    if was_network_hit {
                        std::thread::sleep(Duration::from_millis(base_delay_ms));
                    }
                }
                Err((e, was_network_hit)) => {
                    eprintln!("  Error fetching {}:{}: {}", group_id, artifact_id, e);
                    // Apply courtesy delay even on failed network requests
                    if was_network_hit {
                        std::thread::sleep(Duration::from_millis(base_delay_ms));
                    }
                }
            }
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
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
                        group_id, artifact_id, &url, base_delay_ms,
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
        self.parse_search_version_with_cache(
            group_id, artifact_id, &url, &bytes, was_hit,
        )
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
                            eprintln!(
                                "  WARNING: negative cache init failed: {}", e
                            );
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

        let pom_validator = |body: &[u8]| -> std::result::Result<(), String> {
            validate_pom_xml(body)
        };

        // TTL based on version classification
        let pom_ttl = match classify_version(Some(version)) {
            VersionClass::ConcreteVersion(_) => None,             // indefinite
            VersionClass::Snapshot(_) => Some(Duration::from_secs(3600)), // 1h
            _ => Some(Duration::from_secs(24 * 3600)),           // 24h default
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
                        group_id, artifact_id, version, &pom_url, base_delay_ms,
                    );
                }
            };
            let fetcher = CachedFetcher::new(
                pom_cache,
                Duration::from_secs(6 * 3600), // negative_ttl: 6h for 404s
                self.refresh,
            );
            let outcome = fetcher.fetch(
                &pom_url,
                pom_ttl,
                &pom_validator,
                |req_url, etag| self.http_get_with_retry(req_url, etag, base_delay_ms, 5),
            );
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
                            eprintln!(
                                "  HTTP {}, backing off {}s...",
                                status, delay_secs
                            );
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
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut pom = PomMetadata {
            group_id: group_id.to_string(),
            artifact_id: artifact_id.to_string(),
            version: version.to_string(),
            ..Default::default()
        };

        let mut buf = Vec::new();
        let mut current_element = String::new();
        let mut in_dependencies = false;
        let mut in_licenses = false;
        let mut in_scm = false;
        let mut current_dep = PomDependency {
            group_id: String::new(),
            artifact_id: String::new(),
            version: None,
            scope: None,
            optional: false,
        };

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    current_element = name.clone();
                    if name == "dependencies" {
                        in_dependencies = true;
                    } else if name == "licenses" {
                        in_licenses = true;
                    } else if name == "scm" && !in_dependencies {
                        in_scm = true;
                    } else if in_dependencies && name == "dependency" {
                        current_dep = PomDependency {
                            group_id: String::new(),
                            artifact_id: String::new(),
                            version: None,
                            scope: None,
                            optional: false,
                        };
                    }
                }
                Ok(Event::End(e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "dependencies" {
                        in_dependencies = false;
                    } else if name == "licenses" {
                        in_licenses = false;
                    } else if name == "scm" {
                        in_scm = false;
                    } else if in_dependencies && name == "dependency" {
                        if !current_dep.group_id.is_empty() && !current_dep.artifact_id.is_empty() {
                            pom.dependencies.push(current_dep.clone());
                        }
                    }
                    current_element.clear();
                }
                Ok(Event::Text(e)) => {
                    let text = e.unescape().unwrap_or_default().trim().to_string();
                    if text.is_empty() {
                        continue;
                    }

                    match current_element.as_str() {
                        "description" if !in_dependencies => pom.description = Some(text),
                        "url" if !in_dependencies && !in_licenses && !in_scm => pom.url = Some(text),
                        "name" if in_licenses => {
                            if !pom.licenses.contains(&text) {
                                pom.licenses.push(text);
                            }
                        }
                        "groupId" if in_dependencies => current_dep.group_id = text,
                        "artifactId" if in_dependencies => current_dep.artifact_id = text,
                        "version" if in_dependencies => current_dep.version = Some(text),
                        "scope" if in_dependencies => current_dep.scope = Some(text),
                        "optional" if in_dependencies => current_dep.optional = text == "true",
                        "url" if in_scm => pom.scm_url = Some(text),
                        "connection" if in_scm => pom.scm_connection = Some(text),
                        "tag" if in_scm => pom.scm_tag = Some(text),
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

    fn emit_artifact_triples(&self, writer: &mut NTriplesWriter, pom: &PomMetadata) -> Result<usize> {
        let name = format!("{}/{}", pom.group_id, pom.artifact_id);
        let identity_name = format!("{}:{}", pom.group_id, pom.artifact_id);
        let pkg_uri = package_uri("maven", "central", "any", &name, &pom.version);
        let identity_uri = package_identity_uri("maven", "central", "any", &name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{MAVEN}MavenArtifact"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &identity_name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        // Canonical identity name (colon form for OSV join) and PURL
        writer.write_literal(&identity_uri, &format!("{PKG}identityName"), &identity_name)?;
        triples += 1;

        let purl = format!("pkg:maven/{}/{}", pom.group_id, pom.artifact_id);
        writer.write_literal(&identity_uri, &format!("{PKG}purl"), &purl)?;
        triples += 1;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &identity_name)?;
        triples += 1;

        // Maven-specific coordinates
        writer.write_literal(&pkg_uri, &format!("{MAVEN}groupId"), &pom.group_id)?;
        writer.write_literal(&pkg_uri, &format!("{MAVEN}artifactId"), &pom.artifact_id)?;
        triples += 2;

        // Version
        let ver_uri = version_uri("maven", "central", &name, &pom.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &pom.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("maven");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
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
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }

        // SCM → upstream repository
        if let Some(scm_url) = &pom.scm_url {
            if let Some(repo_uri) = crate::uris::normalize_forge_url(scm_url) {
                writer.write_triple(&identity_uri, &format!("{PKG}upstreamRepository"), &repo_uri)?;
                writer.write_triple(&repo_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;

                if let Some(conn) = &pom.scm_connection {
                    let clone_url = conn.strip_prefix("scm:git:").unwrap_or(conn);
                    writer.write_literal(&repo_uri, &format!("{VCS}cloneUrl"), clone_url)?;
                    triples += 1;
                }
            }
        }

        // SCM tag → packagedFromTag
        if let Some(tag) = &pom.scm_tag {
            if tag != "HEAD" && !tag.is_empty() {
                let tag_uri = format!("{DATA}tag/maven/{}/{}/{}", pom.group_id, pom.artifact_id, tag);
                writer.write_triple(&pkg_uri, &format!("{VCS}packagedFromTag"), &tag_uri)?;
                writer.write_triple(&tag_uri, RDF_TYPE, &format!("{VCS}Tag"))?;
                writer.write_literal(&tag_uri, &format!("{VCS}tagName"), tag)?;
                triples += 3;
            }
        }

        // Dependencies
        for dep in &pom.dependencies {
            triples += self.emit_maven_dependency(writer, &pkg_uri, dep)?;
        }

        Ok(triples)
    }

    fn emit_maven_dependency(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        dep: &PomDependency,
    ) -> Result<usize> {
        let dep_name = format!("{}/{}", dep.group_id, dep.artifact_id);
        let target_uri = package_identity_uri("maven", "central", "any", &dep_name);
        let mut triples = 0;

        writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
        triples += 1;

        let scope = dep.scope.as_deref().unwrap_or("compile");
        let bnode = bnode_id(scope, &format!("{}-{}", pkg_uri, &dep_name));
        writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
        writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
        writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
        writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyType"), &dep_type_uri(scope))?;
        writer.write_bnode_literal(&bnode, &format!("{MAVEN}scope"), scope)?;
        triples += 5;

        if dep.optional {
            writer.write_bnode_literal(&bnode, &format!("{MAVEN}optional"), "true")?;
            triples += 1;
        }

        if let Some(version_constraint) = &dep.version {
            let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, &dep_name));
            writer.write_bnode_to_bnode(&bnode, &format!("{PKG}hasVersionConstraint"), &cb)?;
            writer.write_bnode_subject(&cb, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
            writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintOperator"), "maven")?;
            writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintValue"), version_constraint)?;
            triples += 4;
        }

        Ok(triples)
    }
}

/// Validate that `body` is a well-formed POM XML with a `<project>` root element.
///
/// Uses the `quick_xml` parser rather than substring matching so that proxy
/// error pages, `<projects>` wrappers, comments, and truncated documents are
/// all rejected.
fn validate_pom_xml(body: &[u8]) -> std::result::Result<(), String> {
    let text =
        std::str::from_utf8(body).map_err(|e| format!("invalid UTF-8: {}", e))?;
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
                        return Err(
                            "non-whitespace text before <project>".into(),
                        );
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
            Ok(Event::Comment(_)) | Ok(Event::PI(_))
                if !found_project_root =>
            {
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
fn validate_pom_trailing(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> std::result::Result<(), String> {
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => return Ok(()),
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let n = String::from_utf8_lossy(name.as_ref());
                return Err(format!(
                    "trailing element <{}> after </project>",
                    n
                ));
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.name();
                let n = String::from_utf8_lossy(name.as_ref());
                return Err(format!(
                    "trailing element <{}> after </project>",
                    n
                ));
            }
            Ok(Event::Text(ref t)) => {
                let text_val = t.unescape().unwrap_or_default();
                if !text_val.trim().is_empty() {
                    return Err(
                        "trailing non-whitespace text after </project>"
                            .into(),
                    );
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
            Err(e) => {
                return Err(format!("XML parse error after root: {}", e))
            }
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
    let text =
        std::str::from_utf8(body).map_err(|e| format!("non-UTF8: {}", e))?;
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
        assert_eq!(coords[0], ("com.google.guava".to_string(), "guava".to_string()));
        assert_eq!(coords[1], ("org.apache.commons".to_string(), "commons-lang3".to_string()));
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
        let pom = collector.parse_pom(pom_xml, "org.example", "my-lib", "1.0.0").unwrap();

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
            scm_url: None,
            scm_connection: None,
            scm_tag: None,
        };

        let triples = collector.emit_artifact_triples(&mut writer, &pom).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

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

        let pom = collector.parse_pom(pom_xml, "org.springframework", "spring-core", "6.2.0").unwrap();
        assert_eq!(pom.scm_url.as_deref(), Some("https://github.com/spring-projects/spring-framework"));
        assert_eq!(pom.scm_connection.as_deref(), Some("scm:git:git://github.com/spring-projects/spring-framework.git"));
        assert_eq!(pom.scm_tag.as_deref(), Some("v6.2.0"));
        // Verify project URL is NOT overwritten by SCM URL
        assert_eq!(pom.url.as_deref(), Some("https://spring.io/projects/spring-framework"));
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
        assert!(!is_maven_central("https://nexus.internal.org/repository/maven-public"));
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
            http_cache: Some(
                HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap(),
            ),
            refresh: false,
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
            http_cache: Some(
                HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap(),
            ),
            refresh: false,
        };

        let mut delay = 1u64;
        let r1 = collector.fetch_pom("org.ex", "lib", "1.0-SNAPSHOT", &mut delay);
        assert!(r1.is_ok());

        // Advance past 1h TTL
        clock.advance(2 * 3600);

        let r2 = collector.fetch_pom("org.ex", "lib", "1.0-SNAPSHOT", &mut delay);
        assert!(r2.is_ok());
        assert!(r2.unwrap().1, "second call should hit network after TTL expiry");
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
            .mock("GET", mockito::Matcher::Regex(r"solrsearch/select.*".into()))
            .with_status(200)
            .with_body(r#"{"response":{"docs":[{"latestVersion":"1.0"}]}}"#)
            .expect(2)
            .create();

        let collector = MavenCollector {
            client: crate::enricher::default_http_client(),
            search_base: server.url(),
            repo_base: format!("{}/maven2", server.url()),
            http_cache: Some(
                HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap(),
            ),
            refresh: false,
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
            .mock("GET", mockito::Matcher::Regex(r"solrsearch/select.*".into()))
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

        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        )
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

        let cache =
            HttpCache::with_clock(cache_dir, "maven-refresh-test", clock.clone()).unwrap();

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
        assert!(result.unwrap_err().contains("XML declaration after document root"));
    }

    #[test]
    fn test_validate_pom_xml_rejects_decl_after_comment() {
        let xml = b"<!-- comment --><?xml version=\"1.0\"?><project/>";
        let result = validate_pom_xml(xml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("XML declaration must be first"));
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
        assert!(result.unwrap_err().contains("XML declaration inside document root"));
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
        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        );

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

        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        );

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
            .mock("GET", mockito::Matcher::Regex(r"solrsearch/select.*".into()))
            .with_status(200)
            .with_body(r#"{"response":{"docs":[{"latestVersion":""}]}}"#)
            .expect(2) // both calls hit network (not cached)
            .create();

        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        )
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
            .mock("GET", mockito::Matcher::Regex(r"solrsearch/select.*".into()))
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

        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        )
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
        let pom_cache =
            HttpCache::with_clock(cache_dir, "maven-pom", clock.clone()).unwrap();
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
            .mock("GET", mockito::Matcher::Regex(r"solrsearch/select.*".into()))
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
            http_cache: Some(
                HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap(),
            ),
            refresh: false,
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
        let pom_cache =
            HttpCache::with_clock(cache_dir, "maven-pom", clock.clone()).unwrap();
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
            .mock("GET", mockito::Matcher::Regex(r"solrsearch/select.*".into()))
            .with_status(200)
            .with_body(r#"{"response":{"docs":[]}}"#)
            .expect(1) // only one network call
            .create();

        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        )
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
            .mock("GET", mockito::Matcher::Regex(r"solrsearch/select.*".into()))
            .with_status(200)
            .with_body(r#"{"response":{"docs":[]}}"#)
            .expect(2) // hit twice: initial + after negative TTL expires
            .create();

        let collector = MavenCollector {
            client: crate::enricher::default_http_client(),
            search_base: server.url(),
            repo_base: format!("{}/maven2", server.url()),
            http_cache: Some(
                HttpCache::with_clock(cache_dir, "maven", clock.clone()).unwrap(),
            ),
            refresh: false,
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

        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        )
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

        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        )
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

        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        )
        .with_cache(tmp.path().to_str().unwrap())
        .unwrap();

        let mut delay = 1u64;
        let result = collector.fetch_pom("org.retry", "lib", "1.0", &mut delay);
        assert!(result.is_ok(), "Expected retry to succeed, got: {:?}", result.err());
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
            .mock("GET", mockito::Matcher::Regex(r"solrsearch/select.*".into()))
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

        let collector = MavenCollector::new(
            server.url(),
            format!("{}/maven2", server.url()),
        )
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
}
