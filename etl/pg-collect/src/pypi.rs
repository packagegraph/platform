use crate::cached_fetch::{CachedFetcher, HttpResponse};
use crate::fetch_error::FetchError;
use crate::http_cache::HttpCache;
use crate::npm::read_seed_file;
use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::uris::*;
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

/// PEP 503 name normalization: lowercase, replace runs of [-_. ] with a single hyphen.
/// Also strips extras brackets (e.g. "aiohttp[speedups]" → "aiohttp").
pub fn normalize_pypi_name(name: &str) -> String {
    // Strip extras: everything from first '[' onward
    let base = name.split('[').next().unwrap_or(name).trim();
    // PEP 503: lowercase, then collapse [-_. ]+ runs into single '-'
    let re = Regex::new(r"[-_. ]+").unwrap();
    re.replace_all(&base.to_lowercase(), "-").to_string()
}

pub struct PypiCollector {
    client: Client,
    http_cache: Option<HttpCache>,
    cache_ttl_hours: u64,
    base_url: String,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PypiProjectResponse {
    info: PypiInfo,
}

#[derive(Debug, Deserialize)]
struct PypiInfo {
    name: String,
    version: String,
    summary: Option<String>,
    license: Option<String>,
    home_page: Option<String>,
    requires_python: Option<String>,
    requires_dist: Option<Vec<String>>,
    classifiers: Option<Vec<String>>,
}

/// Outcome of a PyPI package fetch, tracking whether the network was hit.
struct PypiOutcome {
    was_network_hit: bool,
    result: std::result::Result<PypiProjectResponse, FetchError>,
}

/// Whether a rate-limit delay should be applied after this fetch.
/// True only when a network request was actually made (cache hits skip delay).
fn should_delay(outcome: &PypiOutcome) -> bool {
    outcome.was_network_hit
}

impl PypiCollector {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            http_cache: None,
            cache_ttl_hours: 24,
            base_url: "https://pypi.org".to_string(),
            graph_uri: None,
        }
    }

    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    /// Override the PyPI API base URL (for testing).
    #[cfg(test)]
    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }

    /// Enable HTTP response caching in the given directory.
    pub fn with_cache(mut self, cache_dir: &str) -> Result<Self> {
        self.http_cache = Some(HttpCache::new(cache_dir, "pypi")?);
        Ok(self)
    }

    /// Set cache TTL for successful responses (in hours).
    pub fn with_cache_ttl_hours(mut self, hours: u64) -> Self {
        self.cache_ttl_hours = hours;
        self
    }

    pub fn collect_discover(
        &self,
        endpoint: &str,
        auth: &crate::sparql::SparqlAuth,
        backend: crate::sparql::SparqlBackend,
        max_depth: u32,
        max_packages: usize,
        output_path: &str,
    ) -> Result<(usize, usize)> {
        let names = crate::seed::discover_by_ecosystem(endpoint, "pypi", auth, backend)?;
        let seed_path = "/tmp/seed-pypi-discover.txt";
        std::fs::write(seed_path, names.join("\n"))?;
        self.collect(seed_path, max_depth, max_packages, output_path)
    }

    pub fn collect(
        &self,
        packages_file: &str,
        max_depth: u32,
        max_packages: usize,
        output_path: &str,
    ) -> Result<(usize, usize)> {
        use std::collections::{HashMap, HashSet, VecDeque};

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        // Build a CachedFetcher if cache is available
        let cached_fetcher = self.http_cache.as_ref().map(|cache| {
            CachedFetcher::new(
                cache.clone(),
                Duration::from_secs(3600), // 1h negative TTL for 404s
                false,
            )
        });

        let raw_seeds = read_seed_file(packages_file)?;
        let seeds: Vec<String> = raw_seeds.iter().map(|s| normalize_pypi_name(s)).collect();
        eprintln!("Loaded {} seed packages", seeds.len());
        eprintln!(
            "Spider config: max_depth={}, max_packages={}",
            max_depth, max_packages
        );

        // BFS state
        let mut queue: VecDeque<String> = seeds.into_iter().collect();
        let mut visited: HashSet<String> = HashSet::new();
        let mut depth_map: HashMap<String, u32> = HashMap::new();

        // Seeds start at depth 0
        for name in queue.iter() {
            depth_map.insert(name.clone(), 0);
        }

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 200;

        while let Some(name) = queue.pop_front() {
            if !visited.insert(name.clone()) {
                continue; // Already processed
            }

            if visited.len() > max_packages {
                eprintln!("Reached max_packages limit ({})", max_packages);
                break;
            }

            let depth = *depth_map.get(&name).unwrap_or(&0);

            if (visited.len()) % 50 == 0 {
                eprintln!("Progress: {} packages (depth {})", visited.len(), depth);
            }

            let outcome = self.fetch_package(&name, &mut base_delay_ms, &cached_fetcher);
            let needs_delay = should_delay(&outcome);

            match outcome.result {
                Ok(pkg) => {
                    let (pkg_triples, dep_names) = self.emit_package_triples(&mut writer, &pkg)?;
                    total_triples += pkg_triples;
                    total_packages += 1;

                    // Enqueue dependencies if under max_depth
                    if depth < max_depth {
                        for raw_dep in dep_names {
                            let dep_name = normalize_pypi_name(&raw_dep);
                            if dep_name.is_empty() {
                                continue;
                            }
                            if !visited.contains(&dep_name) && !depth_map.contains_key(&dep_name) {
                                depth_map.insert(dep_name.clone(), depth + 1);
                                queue.push_back(dep_name);
                            }
                        }
                    }
                }
                Err(e) => eprintln!("  Error fetching {}: {}", name, e),
            }

            if needs_delay {
                std::thread::sleep(Duration::from_millis(base_delay_ms));
            }
        }

        eprintln!(
            "Collected {} packages ({} total in graph)",
            total_packages,
            visited.len()
        );
        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("pypi");
        let rel_uri = release_uri("pypi", "index");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(
            &dist_uri,
            &format!("{PKG}projectName"),
            "Python Package Index",
        )?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "index")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    /// Fetch a package, using CachedFetcher when available.
    /// Returns a FetchOutcome containing was_network_hit and the parsed response or error.
    fn fetch_package(
        &self,
        name: &str,
        base_delay_ms: &mut u64,
        cached_fetcher: &Option<CachedFetcher>,
    ) -> PypiOutcome {
        let url = format!("{}/pypi/{}/json", self.base_url, name);
        let ttl = Duration::from_secs(self.cache_ttl_hours * 3600);

        let pypi_validator = |body: &[u8]| -> std::result::Result<(), String> {
            serde_json::from_slice::<PypiProjectResponse>(body)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };

        if let Some(fetcher) = cached_fetcher {
            let outcome = fetcher.fetch(&url, Some(ttl), &pypi_validator, |req_url, etag| {
                self.http_get_with_retry(req_url, etag, base_delay_ms)
            });

            PypiOutcome {
                was_network_hit: outcome.was_network_hit,
                result: outcome.result.and_then(|bytes| {
                    serde_json::from_slice::<PypiProjectResponse>(&bytes).map_err(|e| {
                        FetchError::Parse {
                            url: url.clone(),
                            detail: e.to_string(),
                        }
                    })
                }),
            }
        } else {
            // No cache -- direct fetch with retry (no etag for uncached requests)
            match self.http_get_with_retry(&url, None, base_delay_ms) {
                Ok(response) => match response.status {
                    200 => {
                        let result = serde_json::from_slice::<PypiProjectResponse>(&response.bytes)
                            .map_err(|e| FetchError::Parse {
                                url: url.clone(),
                                detail: e.to_string(),
                            });
                        PypiOutcome {
                            was_network_hit: true,
                            result,
                        }
                    }
                    404 => PypiOutcome {
                        was_network_hit: true,
                        result: Err(FetchError::NotFound { url }),
                    },
                    status => PypiOutcome {
                        was_network_hit: true,
                        result: Err(FetchError::HttpStatus { url, status }),
                    },
                },
                Err(e) => PypiOutcome {
                    was_network_hit: true,
                    result: Err(e),
                },
            }
        }
    }

    /// HTTP GET with retry and 429 backoff. Returns raw HttpResponse for
    /// the CachedFetcher's http_get closure. When `etag` is provided, sends
    /// an `If-None-Match` header for conditional GET (enabling 304 responses).
    fn http_get_with_retry(
        &self,
        url: &str,
        etag: Option<&str>,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<HttpResponse, FetchError> {
        let max_attempts = 5;

        for attempt in 0..max_attempts {
            let mut request = self.client.get(url);
            if let Some(etag_val) = etag {
                request = request.header("If-None-Match", etag_val);
            }
            match request.send() {
                Ok(response) => {
                    if response.status() == StatusCode::TOO_MANY_REQUESTS {
                        let retry_after_secs = response
                            .headers()
                            .get("retry-after")
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or_else(|| 2u64.pow(attempt as u32));

                        let delay_ms = retry_after_secs * 1000;
                        eprintln!(
                            "  Rate limited on {}, waiting {}s...",
                            url, retry_after_secs
                        );
                        std::thread::sleep(Duration::from_millis(delay_ms));
                        *base_delay_ms = (*base_delay_ms * 2).min(5000);
                        continue;
                    }

                    let status = response.status().as_u16();
                    let etag = response
                        .headers()
                        .get("etag")
                        .and_then(|h| h.to_str().ok())
                        .map(|s| s.to_string());
                    let bytes = response
                        .bytes()
                        .map_err(|e| FetchError::Transport {
                            url: url.to_string(),
                            source: e,
                        })?
                        .to_vec();

                    return Ok(HttpResponse {
                        status,
                        bytes,
                        etag,
                    });
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        let delay = Duration::from_millis(1000 * 2u64.pow(attempt as u32));
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

        Err(FetchError::HttpStatus {
            url: url.to_string(),
            status: 429,
        })
    }

    /// Emit package triples and return (triple_count, dep_names) for spidering.
    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        response: &PypiProjectResponse,
    ) -> Result<(usize, Vec<String>)> {
        let info = &response.info;
        let pkg_uri = package_uri("pypi", "index", "any", &info.name, &info.version);
        let identity_uri = package_identity_uri("pypi", "index", "any", &info.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PYPI}PythonPackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &info.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &info.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("pypi", "index", &info.name, &info.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &info.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("pypi");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(summary) = &info.summary {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), summary)?;
            triples += 1;
        }
        if let Some(homepage) = &info.home_page {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(license) = &info.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }

        // PyPI-specific
        if let Some(req_python) = &info.requires_python {
            writer.write_literal(&pkg_uri, &format!("{PYPI}requiresPython"), req_python)?;
            triples += 1;
        }

        // Classifiers
        if let Some(classifiers) = &info.classifiers {
            for classifier in classifiers {
                writer.write_literal(&pkg_uri, &format!("{PYPI}classifierString"), classifier)?;
                triples += 1;
            }
        }

        // Dependencies (requires_dist format: "package (>=1.0,<2.0)")
        let dep_names = if let Some(requires_dist) = &info.requires_dist {
            let (dep_triples, names) = self.parse_requires_dist(writer, &pkg_uri, requires_dist)?;
            triples += dep_triples;
            names
        } else {
            Vec::new()
        };

        Ok((triples, dep_names))
    }

    /// Parse requires_dist and emit dependency triples.
    /// Returns (triple_count, Vec<dep_names>) for spidering.
    fn parse_requires_dist(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        requires_dist: &[String],
    ) -> Result<(usize, Vec<String>)> {
        // Captures base name, optional extras, optional version spec
        let dep_re = Regex::new(r"^([a-zA-Z0-9._-]+)(?:\[.*?\])?\s*(\(.*\))?").unwrap();
        let mut triples = 0;
        let mut dep_names = Vec::new();

        for req in requires_dist {
            // Strip environment markers (after ';')
            let cleaned = req.split(';').next().unwrap_or(req).trim();

            if let Some(caps) = dep_re.captures(cleaned) {
                let raw_name = caps.get(1).unwrap().as_str();
                let dep_name = normalize_pypi_name(raw_name);
                if dep_name.is_empty() {
                    continue;
                }
                let version_spec = caps.get(2).map(|m| m.as_str());

                let target_uri = package_identity_uri("pypi", "index", "any", &dep_name);

                writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
                triples += 1;

                let bnode = bnode_id("depends", &format!("{}-{}", pkg_uri, &dep_name));
                writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
                writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
                writer.write_bnode_subject(
                    &bnode,
                    &format!("{PKG}dependencyTarget"),
                    &target_uri,
                )?;
                writer.write_bnode_subject(
                    &bnode,
                    &format!("{PKG}dependencyType"),
                    &dep_type_uri("depends"),
                )?;
                triples += 4;

                if let Some(spec) = version_spec {
                    let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, &dep_name));
                    writer.write_bnode_to_bnode(
                        &bnode,
                        &format!("{PKG}hasVersionConstraint"),
                        &cb,
                    )?;
                    writer.write_bnode_subject(
                        &cb,
                        RDF_TYPE,
                        &format!("{PKG}VersionConstraint"),
                    )?;
                    writer.write_bnode_literal(
                        &cb,
                        &format!("{PKG}versionConstraintOperator"),
                        "pep440",
                    )?;
                    writer.write_bnode_literal(
                        &cb,
                        &format!("{PKG}versionConstraintValue"),
                        spec,
                    )?;
                    triples += 4;
                }

                dep_names.push(dep_name);
            }
        }

        Ok((triples, dep_names))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_pypi_response_deserialization() {
        let json = r#"{
            "info": {
                "name": "requests",
                "version": "2.31.0",
                "summary": "Python HTTP for Humans",
                "license": "Apache 2.0",
                "home_page": "https://requests.readthedocs.io",
                "requires_python": ">=3.7",
                "requires_dist": ["charset-normalizer (<4,>=2)", "idna (<4,>=2.5)"],
                "classifiers": ["Development Status :: 5 - Production/Stable"]
            }
        }"#;

        let resp: PypiProjectResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.info.name, "requests");
        assert_eq!(resp.info.version, "2.31.0");
        assert_eq!(resp.info.requires_dist.unwrap().len(), 2);
    }

    #[test]
    fn test_emit_pypi_package_dual_typing() {
        let collector = PypiCollector::new();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let response = PypiProjectResponse {
            info: PypiInfo {
                name: "requests".into(),
                version: "2.31.0".into(),
                summary: Some("Python HTTP library".into()),
                license: Some("Apache 2.0".into()),
                home_page: Some("https://requests.readthedocs.io".into()),
                requires_python: Some(">=3.7".into()),
                requires_dist: Some(vec!["charset-normalizer (<4,>=2)".into()]),
                classifiers: Some(vec!["Development Status :: 5 - Production/Stable".into()]),
            },
        };

        let (triples, dep_names) = collector
            .emit_package_triples(&mut writer, &response)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("pypi#PythonPackage"));
        assert!(content.contains("\"requests\""));
        assert!(content.contains("\"2.31.0\""));
        assert!(content.contains("requiresPython"));
        assert!(content.contains("classifierString"));
        assert!(content.contains("directlyDependsOn"));
        assert!(triples > 15);
        assert_eq!(
            dep_names,
            vec!["charset-normalizer"],
            "Should extract dep name"
        );
    }

    #[test]
    fn test_parse_requires_dist() {
        let collector = PypiCollector::new();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg_uri = package_uri("pypi", "index", "any", "requests", "2.31.0");
        let requires = vec![
            "charset-normalizer (<4,>=2)".into(),
            "idna (<4,>=2.5)".into(),
            "urllib3 (<3,>=1.21.1)".into(),
        ];

        let (triples, dep_names) = collector
            .parse_requires_dist(&mut writer, &pkg_uri, &requires)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("charset-normalizer"));
        assert!(content.contains("idna"));
        assert!(content.contains("urllib3"));
        assert!(content.contains("directlyDependsOn"));
        assert!(content.contains("dependencyTarget"));
        assert!(triples >= 15); // 3 deps * 5 triples
        assert_eq!(dep_names.len(), 3, "Should extract 3 dep names");
        assert!(dep_names.contains(&"charset-normalizer".to_string()));
        assert!(dep_names.contains(&"idna".to_string()));
        assert!(dep_names.contains(&"urllib3".to_string()));
    }

    #[test]
    fn test_normalize_pypi_name() {
        // PEP 503: lowercase, collapse [-_. ]+ to single '-'
        assert_eq!(normalize_pypi_name("Requests"), "requests");
        assert_eq!(normalize_pypi_name("My_Package"), "my-package");
        assert_eq!(normalize_pypi_name("My.Package"), "my-package");
        assert_eq!(normalize_pypi_name("My__Package"), "my-package");
        assert_eq!(normalize_pypi_name("azure-core"), "azure-core");

        // Extras stripping
        assert_eq!(normalize_pypi_name("aiohttp[speedups]"), "aiohttp");
        assert_eq!(normalize_pypi_name("accelerate[rich]"), "accelerate");
        assert_eq!(normalize_pypi_name("azure-core[aio]"), "azure-core");
        assert_eq!(normalize_pypi_name("anyio[trio]"), "anyio");

        // Combined: extras + normalization
        assert_eq!(normalize_pypi_name("Azure_Core[aio]"), "azure-core");

        // Edge cases
        assert_eq!(normalize_pypi_name("  requests  "), "requests");
        assert_eq!(normalize_pypi_name(""), "");
    }

    #[test]
    fn test_parse_requires_dist_with_extras() {
        let collector = PypiCollector::new();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg_uri = package_uri("pypi", "index", "any", "myproject", "1.0.0");
        let requires = vec![
            "aiohttp[speedups] (>=3.0)".into(),
            "azure-core[aio]".into(),
            "My_Package (>=1.0); extra == \"dev\"".into(),
            "Normal.Dep".into(),
        ];

        let (_triples, dep_names) = collector
            .parse_requires_dist(&mut writer, &pkg_uri, &requires)
            .unwrap();

        assert_eq!(dep_names.len(), 4);
        assert!(
            dep_names.contains(&"aiohttp".to_string()),
            "Should strip extras from aiohttp[speedups]"
        );
        assert!(
            dep_names.contains(&"azure-core".to_string()),
            "Should strip extras from azure-core[aio]"
        );
        assert!(
            dep_names.contains(&"my-package".to_string()),
            "Should normalize My_Package"
        );
        assert!(
            dep_names.contains(&"normal-dep".to_string()),
            "Should normalize Normal.Dep"
        );
    }

    #[test]
    fn test_with_cache_creates_cache_dir() {
        let tmp = TempDir::new().unwrap();
        let cache_path = tmp.path().join("pypi-cache");
        let collector = PypiCollector::new()
            .with_cache(cache_path.to_str().unwrap())
            .unwrap();
        assert!(collector.http_cache.is_some());
        assert!(
            cache_path.join("pypi").exists(),
            "HttpCache should create the collector subdirectory"
        );
    }

    #[test]
    fn test_with_cache_ttl_hours() {
        let collector = PypiCollector::new().with_cache_ttl_hours(48);
        assert_eq!(collector.cache_ttl_hours, 48);
    }

    #[test]
    fn test_fetch_package_no_cache_parses_response() {
        // Verify the PypiOutcome struct works correctly with the FetchError types.
        let outcome = PypiOutcome {
            was_network_hit: true,
            result: Err(FetchError::NotFound {
                url: "https://pypi.org/pypi/nonexistent/json".into(),
            }),
        };
        assert!(outcome.was_network_hit);
        assert!(matches!(outcome.result, Err(FetchError::NotFound { .. })));
    }

    #[test]
    fn test_pypi_outcome_cache_hit_not_network() {
        let outcome = PypiOutcome {
            was_network_hit: false,
            result: Ok(PypiProjectResponse {
                info: PypiInfo {
                    name: "cached-pkg".into(),
                    version: "1.0.0".into(),
                    summary: None,
                    license: None,
                    home_page: None,
                    requires_python: None,
                    requires_dist: None,
                    classifiers: None,
                },
            }),
        };
        assert!(
            !outcome.was_network_hit,
            "cache hit should not be a network hit"
        );
        assert!(outcome.result.is_ok());
    }

    #[test]
    fn test_pypi_validator_rejects_malformed_json() {
        // The validator used in fetch_package rejects non-PyPI JSON
        let validator = |body: &[u8]| -> std::result::Result<(), String> {
            serde_json::from_slice::<PypiProjectResponse>(body)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };

        // Valid PyPI JSON
        let valid = br#"{"info":{"name":"x","version":"1.0","summary":null,"license":null,"home_page":null,"requires_python":null,"requires_dist":null,"classifiers":null}}"#;
        assert!(validator(valid).is_ok());

        // Malformed JSON
        assert!(validator(b"not json at all").is_err());

        // Valid JSON but wrong schema (missing required fields)
        assert!(validator(br#"{"other": "data"}"#).is_err());

        // Empty body
        assert!(validator(b"").is_err());
    }

    // ── should_delay tests ─────────────────────────────────────────

    #[test]
    fn test_should_delay_false_on_cache_hit() {
        let outcome = PypiOutcome {
            was_network_hit: false,
            result: Ok(PypiProjectResponse {
                info: PypiInfo {
                    name: "cached".into(),
                    version: "1.0".into(),
                    summary: None,
                    license: None,
                    home_page: None,
                    requires_python: None,
                    requires_dist: None,
                    classifiers: None,
                },
            }),
        };
        assert!(!should_delay(&outcome), "cache hit should not delay");
    }

    #[test]
    fn test_should_delay_true_on_network_fetch() {
        let outcome = PypiOutcome {
            was_network_hit: true,
            result: Ok(PypiProjectResponse {
                info: PypiInfo {
                    name: "fetched".into(),
                    version: "1.0".into(),
                    summary: None,
                    license: None,
                    home_page: None,
                    requires_python: None,
                    requires_dist: None,
                    classifiers: None,
                },
            }),
        };
        assert!(should_delay(&outcome), "network fetch should delay");
    }

    #[test]
    fn test_should_delay_true_on_network_error() {
        let outcome = PypiOutcome {
            was_network_hit: true,
            result: Err(FetchError::NotFound {
                url: "https://pypi.org/pypi/gone/json".into(),
            }),
        };
        assert!(should_delay(&outcome), "network error should still delay");
    }

    // ── Collector-level acceptance tests (mockito) ─────────────────

    fn valid_pypi_json(name: &str, version: &str) -> String {
        format!(
            r#"{{"info":{{"name":"{}","version":"{}","summary":null,"license":null,"home_page":null,"requires_python":null,"requires_dist":null,"classifiers":null}}}}"#,
            name, version
        )
    }

    /// Build a CachedFetcher from a collector's stored HttpCache (mirrors collect()).
    fn make_cached_fetcher(collector: &PypiCollector) -> Option<CachedFetcher> {
        collector
            .http_cache
            .as_ref()
            .map(|cache| CachedFetcher::new(cache.clone(), Duration::from_secs(3600), false))
    }

    #[test]
    fn test_collector_cache_hit_skips_network_and_delay() {
        let mut server = mockito::Server::new();
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");

        let collector = PypiCollector::new()
            .with_base_url(&server.url())
            .with_cache(cache_dir.to_str().unwrap())
            .unwrap();

        // Pre-populate cache with valid response for the URL the collector will construct
        let url = format!("{}/pypi/requests/json", server.url());
        let body = valid_pypi_json("requests", "2.31.0");
        collector
            .http_cache
            .as_ref()
            .unwrap()
            .put(
                &url,
                body.as_bytes(),
                Some("\"etag-1\""),
                200,
                Some(Duration::from_secs(86400)),
            )
            .unwrap();

        // No mock registered -- any HTTP request will fail, proving cache hit
        let cached_fetcher = make_cached_fetcher(&collector);

        let mut base_delay = 200u64;
        let outcome = collector.fetch_package("requests", &mut base_delay, &cached_fetcher);

        assert!(
            !outcome.was_network_hit,
            "cache hit should not touch network"
        );
        let pkg = outcome.result.expect("cache hit should return Ok");
        assert_eq!(pkg.info.name, "requests");
    }

    #[test]
    fn test_collector_cache_miss_fetches_stores_and_delays() {
        let mut server = mockito::Server::new();
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");

        let body = valid_pypi_json("flask", "3.0.0");
        let mock = server
            .mock("GET", "/pypi/flask/json")
            .with_status(200)
            .with_header("etag", "\"flask-etag\"")
            .with_body(&body)
            .expect(1)
            .create();

        let collector = PypiCollector::new()
            .with_base_url(&server.url())
            .with_cache(cache_dir.to_str().unwrap())
            .unwrap();

        let cached_fetcher = make_cached_fetcher(&collector);

        let mut base_delay = 200u64;
        let outcome = collector.fetch_package("flask", &mut base_delay, &cached_fetcher);

        assert!(outcome.was_network_hit, "cache miss should hit network");
        let pkg = outcome.result.expect("should parse response");
        assert_eq!(pkg.info.name, "flask");
        mock.assert(); // Verify exactly 1 request

        // Verify response was cached -- second fetch should NOT hit network
        let cached_fetcher2 = make_cached_fetcher(&collector);
        let outcome2 = collector.fetch_package("flask", &mut base_delay, &cached_fetcher2);
        assert!(
            !outcome2.was_network_hit,
            "second fetch should be cache hit"
        );
        assert!(outcome2.result.is_ok());
    }

    #[test]
    fn test_collector_429_retry_succeeds() {
        let mut server = mockito::Server::new();
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");

        let body = valid_pypi_json("retry-pkg", "1.0.0");

        // First request returns 429 with retry-after
        let mock_429 = server
            .mock("GET", "/pypi/retry-pkg/json")
            .with_status(429)
            .with_header("retry-after", "1")
            .expect(1)
            .create();

        // Second request succeeds
        let mock_200 = server
            .mock("GET", "/pypi/retry-pkg/json")
            .with_status(200)
            .with_body(&body)
            .expect(1)
            .create();

        let collector = PypiCollector::new()
            .with_base_url(&server.url())
            .with_cache(cache_dir.to_str().unwrap())
            .unwrap();

        let cached_fetcher = make_cached_fetcher(&collector);

        let mut base_delay = 200u64;
        let outcome = collector.fetch_package("retry-pkg", &mut base_delay, &cached_fetcher);

        assert!(outcome.was_network_hit);
        let pkg = outcome.result.expect("should succeed after retry");
        assert_eq!(pkg.info.name, "retry-pkg");
        mock_429.assert();
        mock_200.assert();
        assert!(base_delay > 200, "base_delay should increase after 429");
    }

    #[test]
    fn test_collector_malformed_json_not_cached() {
        let mut server = mockito::Server::new();
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");

        // Server returns HTML with 200 (not valid PyPI JSON)
        let mock = server
            .mock("GET", "/pypi/broken/json")
            .with_status(200)
            .with_body("<html>Not Found</html>")
            .expect_at_least(2)
            .create();

        let collector = PypiCollector::new()
            .with_base_url(&server.url())
            .with_cache(cache_dir.to_str().unwrap())
            .unwrap();

        let cached_fetcher = make_cached_fetcher(&collector);

        let mut base_delay = 200u64;

        // First fetch -- malformed response
        let outcome1 = collector.fetch_package("broken", &mut base_delay, &cached_fetcher);
        assert!(outcome1.was_network_hit);
        assert!(outcome1.result.is_err(), "malformed JSON should fail");

        // Second fetch -- should hit network again (not cached)
        let cached_fetcher2 = make_cached_fetcher(&collector);
        let outcome2 = collector.fetch_package("broken", &mut base_delay, &cached_fetcher2);
        assert!(
            outcome2.was_network_hit,
            "malformed response should not be cached -- second fetch must hit network"
        );

        mock.assert();
    }

    #[test]
    fn test_collector_etag_conditional_request() {
        use crate::http_cache::MockClock;
        use std::sync::Arc;

        let mut server = mockito::Server::new();
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");

        let body = valid_pypi_json("etag-pkg", "2.0.0");
        let url = format!("{}/pypi/etag-pkg/json", server.url());
        let clock = Arc::new(MockClock::new(1_000_000));

        // Pre-populate cache with an entry that will expire, WITH an ETag
        let cache =
            HttpCache::with_clock(cache_dir.to_str().unwrap(), "pypi", clock.clone()).unwrap();
        cache
            .put(
                &url,
                body.as_bytes(),
                Some("\"v2-etag\""),
                200,
                Some(Duration::from_secs(60)),
            )
            .unwrap();

        // Expire the cache entry
        clock.advance(120);

        // Mock expects If-None-Match header and returns 304
        let mock = server
            .mock("GET", "/pypi/etag-pkg/json")
            .match_header("If-None-Match", "\"v2-etag\"")
            .with_status(304)
            .expect(1)
            .create();

        let collector = PypiCollector::new().with_base_url(&server.url());

        let cached_fetcher = Some(CachedFetcher::new(
            HttpCache::with_clock(cache_dir.to_str().unwrap(), "pypi", clock.clone()).unwrap(),
            Duration::from_secs(3600),
            false,
        ));

        let mut base_delay = 200u64;
        let outcome = collector.fetch_package("etag-pkg", &mut base_delay, &cached_fetcher);

        assert!(outcome.was_network_hit, "304 counts as network hit");
        let pkg = outcome.result.expect("304 should serve stale body");
        assert_eq!(pkg.info.name, "etag-pkg");
        mock.assert(); // Verify the conditional request was made with the correct header
    }

    #[test]
    fn test_collector_no_cache_direct_fetch() {
        let mut server = mockito::Server::new();
        let body = valid_pypi_json("direct-pkg", "1.0.0");

        let mock = server
            .mock("GET", "/pypi/direct-pkg/json")
            .with_status(200)
            .with_body(&body)
            .expect(1)
            .create();

        // No cache configured
        let collector = PypiCollector::new().with_base_url(&server.url());

        let mut base_delay = 200u64;
        let outcome = collector.fetch_package("direct-pkg", &mut base_delay, &None);

        assert!(outcome.was_network_hit);
        let pkg = outcome.result.expect("direct fetch should work");
        assert_eq!(pkg.info.name, "direct-pkg");
        mock.assert();
    }
}
