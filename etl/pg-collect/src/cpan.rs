use crate::fetch_error::FetchError;
use crate::http_transport::{HttpTransport, StatsSnapshot};
use crate::ntriples::NTriplesWriter;
use crate::sparql::{SparqlAuth, SparqlBackend};
use crate::uris::*;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use crate::emit::rdf::write_package_identity;

pub struct CpanCollector {
    transport: HttpTransport,
    api_base: String,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MetaCpanRelease {
    distribution: String,
    version: String,
    #[serde(rename = "abstract")]
    abstract_text: Option<String>,
    author: String,
    status: Option<String>,
    license: Option<Vec<String>>,
    #[serde(default)]
    dependency: Vec<MetaCpanDependency>,
    resources: Option<MetaCpanResources>,
}

#[derive(Debug, Deserialize)]
struct MetaCpanDependency {
    module: String,
    phase: String,
    relationship: String,
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MetaCpanResources {
    homepage: Option<String>,
    repository: Option<MetaCpanRepo>,
}

#[derive(Debug, Deserialize)]
struct MetaCpanRepo {
    url: Option<String>,
}

impl CpanCollector {
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
        let names = crate::seed::discover_by_ecosystem(endpoint, "cpan", auth, backend.clone())?;
        let seed_path = "/tmp/seed-cpan-discover.txt";
        std::fs::write(seed_path, names.join("\n"))?;
        self.collect(seed_path, output_path)
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        let package_names = read_cpan_seed_file(packages_file)?;
        eprintln!(
            "Loaded {} CPAN distribution names from seed file",
            package_names.len()
        );

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 100;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, package_names.len());
            }

            match self.fetch_release_with_retry(name, &mut base_delay_ms) {
                Ok(release) => {
                    total_triples += self.emit_distribution_triples(&mut writer, &release)?;
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
        let dist_uri = distro_uri("cpan");
        let rel_uri = release_uri("cpan", "cpan");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "CPAN")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "cpan")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    /// Fetch one MetaCPAN release. Retry, backoff, `Retry-After` and
    /// pacing live in `HttpTransport`; `_base_delay_ms` is vestigial.
    fn fetch_release_with_retry(
        &self,
        name: &str,
        _base_delay_ms: &mut u64,
    ) -> std::result::Result<MetaCpanRelease, String> {
        let url = format!("{}/v1/release/{}", self.api_base, name);

        match self.transport.get(&url, None) {
            Ok(response) => {
                let text = std::str::from_utf8(&response.bytes).map_err(|e| e.to_string())?;
                serde_json::from_str(text).map_err(|e| e.to_string())
            }
            Err(FetchError::NotFound { .. }) => Err(format!("404: {}", name)),
            Err(e) => Err(e.to_string()),
        }
    }

    fn emit_distribution_triples(
        &self,
        writer: &mut NTriplesWriter,
        release: &MetaCpanRelease,
    ) -> Result<usize> {
        let pkg_uri = package_uri(
            "cpan",
            "cpan",
            "any",
            &release.distribution,
            &release.version,
        );
        let identity_uri = package_identity_uri("cpan", "cpan", "any", &release.distribution);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{CPAN}Distribution"))?;
        triples += 2;

        // Identity
        triples += write_package_identity(writer, &identity_uri, &release.distribution,)?;
        // identityName + rdfs:label, not packageName: see
        // emit::rdf::write_package_identity for why.
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 1;

        writer.write_literal(
            &pkg_uri,
            &format!("{PKG}packageName"),
            &release.distribution,
        )?;
        triples += 1;

        // Version
        let ver_uri = version_uri("cpan", "cpan", &release.distribution, &release.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &release.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("cpan");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // CPAN-specific properties
        writer.write_literal(&pkg_uri, &format!("{CPAN}authorPAUSEID"), &release.author)?;
        triples += 1;

        if let Some(abstract_text) = &release.abstract_text {
            writer.write_literal(&pkg_uri, &format!("{CPAN}abstractText"), abstract_text)?;
            triples += 1;
        }
        if let Some(status) = &release.status {
            writer.write_literal(&pkg_uri, &format!("{CPAN}maturity"), status)?;
            triples += 1;
        }
        if let Some(licenses) = &release.license {
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
        if let Some(resources) = &release.resources {
            if let Some(homepage) = &resources.homepage {
                writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
                triples += 1;
            }
            if let Some(repo) = &resources.repository {
                if let Some(repo_url) = &repo.url {
                    let repo_uri = repo_uri(repo_url);
                    writer.write_triple(&pkg_uri, &format!("{PKG}hasRepository"), &repo_uri)?;
                    triples += 1;
                }
            }
        }

        // Dependencies (runtime phase, requires relationship)
        for dep in &release.dependency {
            if dep.phase == "runtime" && dep.relationship == "requires" {
                let target_uri = package_identity_uri("cpan", "cpan", "any", &dep.module);
                writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
                triples += 1;
            }
        }

        Ok(triples)
    }
}

pub fn read_cpan_seed_file(path: &str) -> Result<Vec<String>> {
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
    fn test_read_cpan_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment").unwrap();
        writeln!(temp, "Moose").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "DBI").unwrap();
        temp.flush().unwrap();

        let names = read_cpan_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "DBI");
        assert_eq!(names[1], "Moose");
    }

    #[test]
    fn test_metacpan_release_deserialization() {
        let json = r#"{
            "distribution": "Moose",
            "version": "2.2206",
            "abstract": "A postmodern object system for Perl 5",
            "author": "ETHER",
            "status": "latest",
            "license": ["perl_5"],
            "dependency": []
        }"#;

        let release: MetaCpanRelease = serde_json::from_str(json).unwrap();
        assert_eq!(release.distribution, "Moose");
        assert_eq!(release.version, "2.2206");
        assert_eq!(release.author, "ETHER");
    }

    #[test]
    fn test_emit_cpan_distribution_dual_typing() {
        let collector = CpanCollector::new("https://fastapi.metacpan.org".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let release = MetaCpanRelease {
            distribution: "DBI".to_string(),
            version: "1.643".to_string(),
            abstract_text: Some("Database independent interface for Perl".to_string()),
            author: "TIMB".to_string(),
            status: Some("latest".to_string()),
            license: Some(vec!["perl_5".to_string()]),
            dependency: vec![],
            resources: None,
        };

        let triples = collector
            .emit_distribution_triples(&mut writer, &release)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("cpan#Distribution"));
        assert!(content.contains("\"DBI\""));
        assert!(content.contains("\"1.643\""));
        assert!(content.contains("cpan#authorPAUSEID"));
        assert!(content.contains("\"TIMB\""));
        assert!(triples > 10);
    }

    // ── Characterization: fetch_release_with_retry, pre-migration ──────

    #[test]
    fn characterize_cpan_returns_release_on_200() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/v1/release/Try-Tiny")
            .with_status(200)
            .with_body(r#"{"distribution":"Try-Tiny","version":"0.31","author":"ETHER"}"#)
            .expect(1)
            .create();

        let c = CpanCollector::new(server.url());
        let mut base = 200u64;
        let rel = c
            .fetch_release_with_retry("Try-Tiny", &mut base)
            .expect("200 should yield a release");

        mock.assert();
        assert_eq!(rel.distribution, "Try-Tiny");
        assert_eq!(rel.version, "0.31");
    }

    #[test]
    fn characterize_cpan_404_is_terminal() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/v1/release/No-Such-Dist")
            .with_status(404)
            .expect(1)
            .create();

        let c = CpanCollector::new(server.url());
        let mut base = 200u64;
        let err = c
            .fetch_release_with_retry("No-Such-Dist", &mut base)
            .expect_err("404 should be an error");

        mock.assert();
        assert_eq!(err, "404: No-Such-Dist");
    }

    #[test]
    fn characterize_cpan_429_is_retried_then_succeeds() {
        let mut server = mockito::Server::new();
        let limited = server
            .mock("GET", "/v1/release/Slow-Dist")
            .with_status(429)
            .with_header("retry-after", "1")
            .expect(1)
            .create();
        let ok = server
            .mock("GET", "/v1/release/Slow-Dist")
            .with_status(200)
            .with_body(r#"{"distribution":"Slow-Dist","version":"1.0","author":"X"}"#)
            .expect(1)
            .create();

        let c = CpanCollector::new(server.url());
        let mut base = 200u64;
        let rel = c.fetch_release_with_retry("Slow-Dist", &mut base).unwrap();

        limited.assert();
        ok.assert();
        assert_eq!(rel.distribution, "Slow-Dist");
    }

    #[test]
    fn characterize_cpan_malformed_body_is_a_parse_error() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/v1/release/Bad-Dist")
            .with_status(200)
            .with_body("{not json")
            .expect(1)
            .create();

        let c = CpanCollector::new(server.url());
        let mut base = 200u64;
        let err = c
            .fetch_release_with_retry("Bad-Dist", &mut base)
            .unwrap_err();

        mock.assert();
        assert!(!err.starts_with("404:"), "got: {err}");
    }
}
