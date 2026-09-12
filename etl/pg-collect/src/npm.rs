use crate::fetch_error::FetchError;
use crate::http_transport::{HttpTransport, StatsSnapshot};
use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::sparql::{SparqlAuth, SparqlBackend};
use crate::uris::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use crate::emit::rdf::write_package_identity;

pub struct NpmCollector {
    transport: HttpTransport,
    registry_url: String,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NpmPackageDoc {
    name: String,
    description: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
    #[serde(rename = "dist-tags")]
    dist_tags: Option<HashMap<String, String>>,
    versions: Option<HashMap<String, NpmVersion>>,
}

#[derive(Debug, Deserialize)]
struct NpmVersion {
    dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies")]
    dev_dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "peerDependencies")]
    peer_dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "optionalDependencies")]
    optional_dependencies: Option<HashMap<String, String>>,
    dist: Option<NpmDist>,
}

#[derive(Debug, Deserialize)]
struct NpmDist {
    shasum: Option<String>,
    integrity: Option<String>,
}

impl NpmCollector {
    pub fn new(registry_url: String) -> Self {
        Self {
            transport: HttpTransport::new(),
            registry_url,
            graph_uri: None,
        }
    }

    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    /// Override the transport, for tests that need fast retries.
    pub fn with_transport(mut self, transport: HttpTransport) -> Self {
        self.transport = transport;
        self
    }

    /// One-line fetch summary for the end of a run.
    pub fn transport_stats(&self) -> StatsSnapshot {
        self.transport.stats()
    }

    pub fn collect_discover(
        &self,
        endpoint: &str,
        auth: &SparqlAuth,
        backend: SparqlBackend,
        output_path: &str,
    ) -> Result<(usize, usize)> {
        let names = crate::seed::discover_by_ecosystem(endpoint, "npm", auth, backend.clone())?;
        let seed_path = "/tmp/seed-npm-discover.txt";
        std::fs::write(seed_path, names.join("\n"))?;
        self.collect(seed_path, output_path)
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        let package_names = read_seed_file(packages_file)?;
        eprintln!(
            "Loaded {} package names from seed file",
            package_names.len()
        );

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 200;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, package_names.len());
            }

            match self.fetch_package_with_retry(name, &mut base_delay_ms) {
                Ok(pkg) => {
                    total_triples += self.emit_package_triples(&mut writer, &pkg)?;
                    total_packages += 1;
                }
                Err(e) => eprintln!("  Error fetching {}: {}", name, e),
            }
        }

        eprintln!("  {}", self.transport_stats());

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("npm");
        let rel_uri = release_uri("npm", "registry");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "NPM Registry")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "registry")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    /// Fetch one package document.
    ///
    /// Retry, backoff, `Retry-After`, and inter-request pacing all live in
    /// `HttpTransport`, so what remains here is URL construction and JSON
    /// decoding. `_base_delay_ms` is vestigial -- the transport's per-host
    /// limiter paces requests now -- and goes away when the remaining
    /// registry collectors migrate.
    fn fetch_package_with_retry(
        &self,
        name: &str,
        _base_delay_ms: &mut u64,
    ) -> std::result::Result<NpmPackageDoc, String> {
        let url = format!("{}/{}", self.registry_url, name);

        match self.transport.get(&url, None) {
            Ok(response) => {
                let text = std::str::from_utf8(&response.bytes).map_err(|e| e.to_string())?;
                serde_json::from_str(text).map_err(|e| e.to_string())
            }
            Err(FetchError::NotFound { .. }) => Err(format!("404: {}", name)),
            Err(e) => Err(e.to_string()),
        }
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &NpmPackageDoc,
    ) -> Result<usize> {
        let version = pkg
            .dist_tags
            .as_ref()
            .and_then(|tags| tags.get("latest"))
            .map(|s| s.as_str())
            .unwrap_or("unknown");

        let pkg_uri = package_uri("npm", "registry", "any", &pkg.name, version);
        let identity_uri = package_identity_uri("npm", "registry", "any", &pkg.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{NPM}NpmPackage"))?;
        triples += 2;

        // Identity
        triples += write_package_identity(writer, &identity_uri, &pkg.name)?;
        // identityName + rdfs:label, not packageName: see
        // emit::rdf::write_package_identity for why.
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 1;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("npm", "registry", &pkg.name, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("npm");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(desc) = &pkg.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(homepage) = &pkg.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(license) = &pkg.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }

        // Dependencies from the latest version
        if let Some(versions) = &pkg.versions {
            if let Some(ver_data) = versions.get(version) {
                if let Some(deps) = &ver_data.dependencies {
                    triples += self.emit_npm_deps(writer, &pkg_uri, deps, "depends")?;
                }
                if let Some(deps) = &ver_data.peer_dependencies {
                    triples += self.emit_npm_deps(writer, &pkg_uri, deps, "peer_depends")?;
                }
                if let Some(deps) = &ver_data.optional_dependencies {
                    triples += self.emit_npm_deps(writer, &pkg_uri, deps, "optional_depends")?;
                }

                // Integrity hash
                if let Some(dist) = &ver_data.dist {
                    if let Some(integrity) = &dist.integrity {
                        writer.write_literal(&pkg_uri, &format!("{NPM}integrity"), integrity)?;
                        triples += 1;
                    }
                    if let Some(shasum) = &dist.shasum {
                        writer.write_literal(&pkg_uri, &format!("{NPM}shasum"), shasum)?;
                        triples += 1;
                    }
                }
            }
        }

        Ok(triples)
    }

    fn emit_npm_deps(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        deps: &HashMap<String, String>,
        dep_type: &str,
    ) -> Result<usize> {
        let mut triples = 0;
        for (dep_name, version_range) in deps {
            let target_uri = package_identity_uri("npm", "registry", "any", dep_name);

            writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id(dep_type, &format!("{}-{}", pkg_uri, dep_name));
            writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_subject(
                &bnode,
                &format!("{PKG}dependencyType"),
                &dep_type_uri(dep_type),
            )?;
            triples += 4;

            // Version constraint (NPM uses semver ranges like "^1.2.3", "~2.0.0")
            if !version_range.is_empty() {
                let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, dep_name));
                writer.write_bnode_to_bnode(&bnode, &format!("{PKG}hasVersionConstraint"), &cb)?;
                writer.write_bnode_subject(&cb, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
                writer.write_bnode_literal(
                    &cb,
                    &format!("{PKG}versionConstraintOperator"),
                    "semver",
                )?;
                writer.write_bnode_literal(
                    &cb,
                    &format!("{PKG}versionConstraintValue"),
                    version_range,
                )?;
                triples += 4;
            }
        }
        Ok(triples)
    }
}

/// Read package names from a seed file (one per line).
pub fn read_seed_file(path: &str) -> Result<Vec<String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut names = Vec::new();

    for line_result in reader.lines() {
        let line = line_result?;
        let trimmed = line.trim();

        // Skip comments and blank lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        names.push(trimmed.to_string());
    }

    // Dedup
    names.sort();
    names.dedup();

    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_transport::{HostLimiter, RetryPolicy};
    use std::io::{Read, Write};
    use std::time::Duration;
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment line").unwrap();
        writeln!(temp, "express").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "lodash").unwrap();
        writeln!(temp, "express").unwrap();
        temp.flush().unwrap();

        let names = read_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "express");
        assert_eq!(names[1], "lodash");
    }

    #[test]
    fn test_npm_package_deserialization() {
        let json = r#"{
            "name": "express",
            "description": "Fast web framework",
            "license": "MIT",
            "homepage": "https://expressjs.com",
            "dist-tags": {"latest": "4.19.2"},
            "versions": {
                "4.19.2": {
                    "dependencies": {"accepts": "~1.3.8", "body-parser": "1.20.2"},
                    "peerDependencies": {},
                    "dist": {"shasum": "abc123", "integrity": "sha512-xyz"}
                }
            }
        }"#;

        let pkg: NpmPackageDoc = serde_json::from_str(json).unwrap();
        assert_eq!(pkg.name, "express");
        assert_eq!(pkg.dist_tags.unwrap().get("latest").unwrap(), "4.19.2");
    }

    #[test]
    fn test_emit_npm_package_dual_typing() {
        let collector = NpmCollector::new("https://registry.npmjs.org".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut versions = HashMap::new();
        let mut deps = HashMap::new();
        deps.insert("accepts".into(), "~1.3.8".into());

        versions.insert(
            "4.19.2".into(),
            NpmVersion {
                dependencies: Some(deps),
                dev_dependencies: None,
                peer_dependencies: None,
                optional_dependencies: None,
                dist: Some(NpmDist {
                    shasum: Some("abc123".into()),
                    integrity: Some("sha512-xyz".into()),
                }),
            },
        );

        let mut dist_tags = HashMap::new();
        dist_tags.insert("latest".into(), "4.19.2".into());

        let pkg = NpmPackageDoc {
            name: "express".into(),
            description: Some("Fast web framework".into()),
            license: Some("MIT".into()),
            homepage: Some("https://expressjs.com".into()),
            dist_tags: Some(dist_tags),
            versions: Some(versions),
        };

        let triples = collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("npm#NpmPackage"));
        assert!(content.contains("\"express\""));
        assert!(content.contains("\"4.19.2\""));
        assert!(content.contains("directlyDependsOn"));
        assert!(content.contains("npm#integrity"));
        assert!(triples > 15);
    }

    // ── Characterization: what fetch_package_with_retry does TODAY ──────
    // These pin existing behaviour so the shared-transport migration can
    // prove it changed nothing unintended. No transport-error test: those
    // retries sleep 1+2+4+8 = 15 seconds.

    #[test]
    fn characterize_fetch_returns_doc_on_200() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/left-pad")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"name":"left-pad","versions":{}}"#)
            .expect(1)
            .create();

        let collector = NpmCollector::new(server.url());
        let mut base_delay_ms = 200u64;
        let doc = collector
            .fetch_package_with_retry("left-pad", &mut base_delay_ms)
            .expect("200 should yield a package doc");

        mock.assert();
        assert_eq!(doc.name, "left-pad");
    }

    #[test]
    fn characterize_fetch_404_returns_prefixed_error_without_retry() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/definitely-missing")
            .with_status(404)
            .expect(1) // exactly one request: 404 is terminal today
            .create();

        let collector = NpmCollector::new(server.url());
        let mut base_delay_ms = 200u64;
        let err = collector
            .fetch_package_with_retry("definitely-missing", &mut base_delay_ms)
            .expect_err("404 should be an error");

        mock.assert();
        assert_eq!(err, "404: definitely-missing");
    }

    #[test]
    fn characterize_429_with_retry_after_is_retried_then_succeeds() {
        let mut server = mockito::Server::new();
        let rate_limited = server
            .mock("GET", "/slowpkg")
            .with_status(429)
            .with_header("retry-after", "1")
            .expect(1)
            .create();
        let ok = server
            .mock("GET", "/slowpkg")
            .with_status(200)
            .with_body(r#"{"name":"slowpkg"}"#)
            .expect(1)
            .create();

        let collector = NpmCollector::new(server.url());
        let mut base_delay_ms = 200u64;
        let doc = collector
            .fetch_package_with_retry("slowpkg", &mut base_delay_ms)
            .expect("should succeed after the rate limit clears");

        rate_limited.assert();
        ok.assert();
        assert_eq!(doc.name, "slowpkg");
        // Pre-migration this also asserted base_delay_ms doubled to 400.
        // That described npm's manual inter-request pacing, which the
        // transport's per-host limiter replaces; the retry behaviour this
        // test exists to pin is unchanged.
    }

    #[test]
    fn migrated_500_is_retried_then_reported_as_an_http_error() {
        // Deliberate change from the pre-migration behaviour: npm used to
        // hand a 500 body to serde_json after a single request, because it
        // checked only for 429 and 404. is_retryable() now treats 5xx as
        // transient.
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/brokenpkg")
            .with_status(500)
            .with_body("upstream exploded")
            .expect(3)
            .create();

        let collector = NpmCollector::new(server.url()).with_transport(
            HttpTransport::new()
                .with_policy(RetryPolicy {
                    max_attempts: 3,
                    base_delay: Duration::from_millis(1),
                    max_delay: Duration::from_millis(4),
                    jitter: false,
                })
                .with_limiter(HostLimiter::new(Duration::from_millis(0))),
        );
        let mut base_delay_ms = 200u64;
        let err = collector
            .fetch_package_with_retry("brokenpkg", &mut base_delay_ms)
            .expect_err("500 should still be an error after retries");

        mock.assert();
        assert!(
            err.contains("500"),
            "error should name the status, got: {err}"
        );
    }

    #[test]
    fn characterize_malformed_json_body_is_a_parse_error() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/badjson")
            .with_status(200)
            .with_body("{not json")
            .expect(1)
            .create();

        let collector = NpmCollector::new(server.url());
        let mut base_delay_ms = 200u64;
        let err = collector
            .fetch_package_with_retry("badjson", &mut base_delay_ms)
            .expect_err("malformed JSON should fail");

        mock.assert();
        assert!(
            !err.starts_with("404:"),
            "parse failure must not be reported as a 404, got: {err}"
        );
    }
}
