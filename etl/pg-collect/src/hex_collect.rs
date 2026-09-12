use crate::fetch_error::FetchError;
use crate::http_transport::{HttpTransport, StatsSnapshot};
use crate::ntriples::NTriplesWriter;
use crate::sparql::{SparqlAuth, SparqlBackend};
use crate::uris::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use crate::emit::rdf::write_package_identity;

pub struct HexCollector {
    transport: HttpTransport,
    api_base: String,
    pub graph_uri: Option<String>,
}

/// Package listing from /api/packages/{name}
#[derive(Debug, Deserialize)]
struct HexPackageResponse {
    name: String,
    #[serde(default)]
    releases: Vec<HexReleaseSummary>,
    /// Map of version → retirement info. Present at the package level.
    #[serde(default)]
    retirements: HashMap<String, HexRetirement>,
    meta: Option<HexMeta>,
}

/// Release entry in the package listing (minimal fields only)
#[derive(Debug, Deserialize)]
struct HexReleaseSummary {
    version: String,
}

/// Individual release from /api/packages/{name}/releases/{version}
#[derive(Debug, Deserialize)]
struct HexReleaseDetail {
    version: String,
    #[serde(default)]
    requirements: HashMap<String, HexRequirement>,
    checksum: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HexRetirement {
    reason: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HexRequirement {
    requirement: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HexMeta {
    description: Option<String>,
    licenses: Option<Vec<String>>,
    links: Option<HashMap<String, String>>,
}

impl HexCollector {
    pub fn new(api_base: String) -> Self {
        Self {
            transport: HttpTransport::new(),
            api_base,
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
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
        let names = crate::seed::discover_by_ecosystem(endpoint, "hex", auth, backend.clone())?;
        let seed_path = "/tmp/seed-hex-discover.txt";
        std::fs::write(seed_path, names.join("\n"))?;
        self.collect(seed_path, output_path)
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        let package_names = read_hex_seed_file(packages_file)?;
        eprintln!(
            "Loaded {} Hex package names from seed file",
            package_names.len()
        );

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 600;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, package_names.len());
            }

            match self.fetch_and_emit_package(&mut writer, name, &mut base_delay_ms) {
                Ok(triples) => {
                    total_triples += triples;
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
        let dist_uri = distro_uri("hex");
        let rel_uri = release_uri("hex", "pm");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Hex.pm")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "pm")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    /// Fetch package listing, select latest release, fetch its details, emit triples.
    fn fetch_and_emit_package(
        &self,
        writer: &mut NTriplesWriter,
        name: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<usize, String> {
        let pkg = self.fetch_package_with_retry(name, base_delay_ms)?;

        // Select latest non-retired release
        let version = pkg
            .releases
            .iter()
            .find(|r| !pkg.retirements.contains_key(&r.version))
            .or_else(|| pkg.releases.first())
            .ok_or_else(|| format!("No releases for {}", name))?
            .version
            .clone();

        // Fetch individual release details for requirements/checksum
        let release_detail = self.fetch_release_detail(name, &version, base_delay_ms)?;

        self.emit_package_triples(writer, &pkg, &release_detail)
            .map_err(|e| e.to_string())
    }

    /// Fetch one package listing. Retry, backoff, `Retry-After` and pacing
    /// live in `HttpTransport`; `_base_delay_ms` is vestigial.
    fn fetch_package_with_retry(
        &self,
        name: &str,
        _base_delay_ms: &mut u64,
    ) -> std::result::Result<HexPackageResponse, String> {
        let url = format!("{}/api/packages/{}", self.api_base, name);

        match self.transport.get(&url, None) {
            Ok(response) => {
                let text = std::str::from_utf8(&response.bytes).map_err(|e| e.to_string())?;
                serde_json::from_str(text).map_err(|e| e.to_string())
            }
            Err(FetchError::NotFound { .. }) => Err(format!("404: {}", name)),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Fetch one release's detail.
    ///
    /// Pre-migration this had no 404 check at all -- unlike
    /// `fetch_package_with_retry` above -- so a missing release was handed
    /// to serde_json and surfaced as "EOF while parsing a value". The
    /// transport classifies it, so it now reports as a 404 like every
    /// other missing resource.
    fn fetch_release_detail(
        &self,
        name: &str,
        version: &str,
        _base_delay_ms: &mut u64,
    ) -> std::result::Result<HexReleaseDetail, String> {
        let url = format!(
            "{}/api/packages/{}/releases/{}",
            self.api_base, name, version
        );

        match self.transport.get(&url, None) {
            Ok(response) => {
                let text = std::str::from_utf8(&response.bytes).map_err(|e| e.to_string())?;
                serde_json::from_str(text).map_err(|e| e.to_string())
            }
            Err(FetchError::NotFound { .. }) => Err(format!("404: {}/{}", name, version)),
            Err(e) => Err(e.to_string()),
        }
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &HexPackageResponse,
        release: &HexReleaseDetail,
    ) -> Result<usize> {
        let pkg_uri = package_uri("hex", "pm", "any", &pkg.name, &release.version);
        let identity_uri = package_identity_uri("hex", "pm", "any", &pkg.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{HEX}HexPackage"))?;
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
        let ver_uri = version_uri("hex", "pm", &pkg.name, &release.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &release.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("hex");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(meta) = &pkg.meta {
            if let Some(desc) = &meta.description {
                writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
                triples += 1;
            }
            if let Some(licenses) = &meta.licenses {
                for license in licenses {
                    writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
                    triples += 1;
                    // License entity (SPDX)
                    let license_uri = crate::uris::spdx_license_uri(license);
                    writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
                    writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
                    triples += 2;
                }
            }
            if let Some(links) = &meta.links {
                if let Some(homepage) = links.get("Homepage").or_else(|| links.get("GitHub")) {
                    writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
                    triples += 1;
                }
            }
        }

        if let Some(checksum) = &release.checksum {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), checksum)?;
            triples += 1;
        }

        // Dependencies from individual release details
        for (dep_name, _req) in &release.requirements {
            let target = package_identity_uri("hex", "pm", "any", dep_name);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target)?;
            triples += 1;
        }

        Ok(triples)
    }
}

pub fn read_hex_seed_file(path: &str) -> Result<Vec<String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut names = Vec::new();

    for line_result in reader.lines() {
        let line = line_result?;
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        names.push(trimmed.to_string());
    }

    names.sort();
    names.dedup();

    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_hex_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment").unwrap();
        writeln!(temp, "phoenix").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "ecto").unwrap();
        temp.flush().unwrap();

        let names = read_hex_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "ecto");
        assert_eq!(names[1], "phoenix");
    }

    #[test]
    fn test_hex_package_deserialization() {
        let json = r#"{
            "name": "phoenix",
            "releases": [{"version": "1.7.11"}],
            "retirements": {},
            "meta": {
                "description": "Web framework",
                "licenses": ["MIT"],
                "links": {"Homepage": "https://www.phoenixframework.org"}
            }
        }"#;

        let pkg: HexPackageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(pkg.name, "phoenix");
        assert_eq!(pkg.releases[0].version, "1.7.11");
        assert!(pkg.retirements.is_empty());
    }

    #[test]
    fn test_hex_release_detail_deserialization() {
        let json = r#"{
            "version": "1.7.11",
            "requirements": {
                "jason": {"optional": false, "app": "jason", "requirement": ">= 0.0.0"},
                "plug": {"optional": false, "app": "plug", "requirement": "~> 1.14"}
            },
            "checksum": "abc123def456"
        }"#;

        let detail: HexReleaseDetail = serde_json::from_str(json).unwrap();
        assert_eq!(detail.version, "1.7.11");
        assert_eq!(detail.requirements.len(), 2);
        assert!(detail.requirements.contains_key("jason"));
        assert!(detail.requirements.contains_key("plug"));
        assert_eq!(detail.checksum, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_hex_retirements_deserialization() {
        let json = r#"{
            "name": "oldpkg",
            "releases": [{"version": "1.0.0"}, {"version": "2.0.0"}],
            "retirements": {
                "1.0.0": {"reason": "security", "message": "CVE-2024-1234"}
            },
            "meta": null
        }"#;

        let pkg: HexPackageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(pkg.retirements.len(), 1);
        assert!(pkg.retirements.contains_key("1.0.0"));
        assert!(!pkg.retirements.contains_key("2.0.0"));
    }

    #[test]
    fn test_emit_hex_package() {
        let collector = HexCollector::new("https://hex.pm".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg = HexPackageResponse {
            name: "ecto".to_string(),
            releases: vec![HexReleaseSummary {
                version: "3.11.1".to_string(),
            }],
            retirements: HashMap::new(),
            meta: Some(HexMeta {
                description: Some("Database wrapper".to_string()),
                licenses: Some(vec!["Apache-2.0".to_string()]),
                links: None,
            }),
        };

        let release = HexReleaseDetail {
            version: "3.11.1".to_string(),
            requirements: {
                let mut m = HashMap::new();
                m.insert(
                    "decimal".to_string(),
                    HexRequirement {
                        requirement: Some("~> 2.0".to_string()),
                    },
                );
                m
            },
            checksum: Some("abc123".to_string()),
        };

        let triples = collector
            .emit_package_triples(&mut writer, &pkg, &release)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("hex#HexPackage"));
        assert!(content.contains("\"ecto\""));
        assert!(content.contains("\"3.11.1\""));
        assert!(
            content.contains("directlyDependsOn"),
            "Should emit dependency from release detail"
        );
        assert!(
            content.contains("\"abc123\""),
            "Should emit checksum from release detail"
        );
        assert!(triples > 10);
    }

    // ── Characterization: hex fetch paths, pre-migration ───────────────

    #[test]
    fn characterize_hex_returns_package_on_200() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/api/packages/phoenix")
            .with_status(200)
            .with_body(r#"{"name":"phoenix"}"#)
            .expect(1)
            .create();

        let c = HexCollector::new(server.url());
        let mut base = 200u64;
        let pkg = c
            .fetch_package_with_retry("phoenix", &mut base)
            .expect("200 should yield a package");

        mock.assert();
        assert_eq!(pkg.name, "phoenix");
    }

    #[test]
    fn characterize_hex_404_is_terminal() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/api/packages/nope")
            .with_status(404)
            .expect(1)
            .create();

        let c = HexCollector::new(server.url());
        let mut base = 200u64;
        let err = c
            .fetch_package_with_retry("nope", &mut base)
            .expect_err("404 should be an error");

        mock.assert();
        assert_eq!(err, "404: nope");
    }

    #[test]
    fn migrated_hex_release_detail_404_is_reported_as_a_404() {
        // Pre-migration fetch_release_detail had no NOT_FOUND check and
        // handed the empty 404 body to serde_json, surfacing as "EOF while
        // parsing a value". The transport classifies it.
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/api/packages/phoenix/releases/9.9.9")
            .with_status(404)
            .expect(1)
            .create();

        let c = HexCollector::new(server.url());
        let mut base = 200u64;
        let err = c
            .fetch_release_detail("phoenix", "9.9.9", &mut base)
            .expect_err("a missing release should be an error");

        mock.assert();
        assert!(
            err.starts_with("404:"),
            "a missing release should now report as a 404, got: {err}"
        );
    }

    #[test]
    fn characterize_hex_malformed_body_is_a_parse_error() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/api/packages/bad")
            .with_status(200)
            .with_body("{not json")
            .expect(1)
            .create();

        let c = HexCollector::new(server.url());
        let mut base = 200u64;
        let err = c.fetch_package_with_retry("bad", &mut base).unwrap_err();

        mock.assert();
        assert!(!err.starts_with("404:"), "got: {err}");
    }
}
