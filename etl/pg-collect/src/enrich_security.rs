//! Per-package OSV API security enricher.
//!
//! Queries Fuseki for packages by ecosystem, then queries OSV.dev API
//! per-package for vulnerabilities. Complementary to the bulk OSV collector
//! (osv.rs) which downloads ecosystem ZIPs from GCS.

use crate::cache::FileCache;
use crate::enricher::rate_limit;
use crate::ntriples::NTriplesWriter;
use crate::osv::{emit_vulnerability_triples, OsvVulnerability};
use crate::sparql::SparqlClient;
use reqwest::blocking::Client;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct SecurityEnricher {
    sparql: SparqlClient,
    client: Client,
    cache: Option<FileCache>,
    ecosystem: String,
}

impl SecurityEnricher {
    pub fn new(
        endpoint: &str,
        ecosystem: &str,
        cache_dir: Option<&str>,
    ) -> Self {
        let sparql = SparqlClient::new(endpoint);
        let client = crate::enricher::default_http_client();

        let cache = cache_dir.map(|dir| {
            FileCache::new(dir, &format!("security-{}", ecosystem), 24, None)
                .expect("Failed to create cache")
        });

        Self { sparql, client, cache, ecosystem: ecosystem.to_string() }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        // Map ecosystem name to RDF type — accepts both packaging system names (preferred)
        // and legacy distro names for backward compatibility
        let rdf_type = match self.ecosystem.as_str() {
            // Packaging system names (preferred)
            "deb" => "https://purl.org/packagegraph/ontology/deb#BinaryPackage",
            "apk" => "https://purl.org/packagegraph/ontology/apk#ApkPackage",
            "rpm" => "https://purl.org/packagegraph/ontology/rpm#BinaryRPM",
            "npm" => "https://purl.org/packagegraph/ontology/npm#NpmPackage",
            "pypi" => "https://purl.org/packagegraph/ontology/pypi#PythonPackage",
            "cargo" => "https://purl.org/packagegraph/ontology/cargo#Crate",
            "gomod" => "https://purl.org/packagegraph/ontology/gomod#GoModule",
            "maven" => "https://purl.org/packagegraph/ontology/maven#MavenArtifact",
            // Legacy distro names (backward compat)
            "debian" => "https://purl.org/packagegraph/ontology/deb#BinaryPackage",
            "alpine" => "https://purl.org/packagegraph/ontology/apk#ApkPackage",
            "fedora" | "rhel" | "centos" | "opensuse" => "https://purl.org/packagegraph/ontology/rpm#BinaryRPM",
            _ => return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Unsupported ecosystem: {}. Use packaging system name: deb, apk, rpm, npm, pypi, cargo, gomod, maven", self.ecosystem),
            )),
        };

        let packages = self.sparql.query_packages_by_type(rdf_type)?;
        eprintln!("Found {} {} packages to check for vulnerabilities", packages.len(), self.ecosystem);

        let mut total_checked = 0;
        let mut total_triples = 0;

        for (_pkg_uri, name, _version) in &packages {
            total_checked += 1;
            if total_checked % 100 == 0 {
                eprintln!("Progress: {} packages checked", total_checked);
            }

            match self.query_osv_for_package(&mut writer, name) {
                Ok(triples) => total_triples += triples,
                Err(e) => eprintln!("  Error checking {}: {}", name, e),
            }

            rate_limit(Duration::from_millis(500));
        }

        writer.flush()?;
        Ok((total_checked, total_triples))
    }

    fn query_osv_for_package(&self, writer: &mut NTriplesWriter, name: &str) -> Result<usize> {
        let cache_key = format!("osv-{}-{}", self.ecosystem, name);

        let vulns: Vec<OsvVulnerability> = match self.cached_get(&cache_key) {
            Some(v) => v,
            None => {
                let url = "https://api.osv.dev/v1/query";
                let payload = serde_json::json!({
                    "package": {"name": name, "ecosystem": self.osv_ecosystem_name()},
                });

                let resp = self.client.post(url)
                    .json(&payload)
                    .send()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                if !resp.status().is_success() {
                    return Ok(0);
                }

                let data: serde_json::Value = resp.json()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                let vulns_data = data.get("vulns").and_then(|v| v.as_array()).map(|arr| {
                    arr.iter().filter_map(|v| {
                        serde_json::from_value(v.clone()).ok()
                    }).collect::<Vec<OsvVulnerability>>()
                }).unwrap_or_default();

                self.cache_put(&cache_key, &serde_json::to_value(&vulns_data).unwrap());
                vulns_data
            }
        };

        let mut triples = 0;
        for vuln in vulns {
            triples += emit_vulnerability_triples(writer, &vuln)?;
        }

        Ok(triples)
    }

    fn osv_ecosystem_name(&self) -> &str {
        match self.ecosystem.as_str() {
            "debian" => "Debian",
            "alpine" => "Alpine",
            "npm" => "npm",
            "pypi" => "PyPI",
            "cargo" => "crates.io",
            "gomod" => "Go",
            "maven" => "Maven",
            _ => &self.ecosystem,
        }
    }

    fn cached_get(&self, key: &str) -> Option<Vec<OsvVulnerability>> {
        let val = self.cache.as_ref()?.get(key)?;
        serde_json::from_value(val).ok()
    }

    fn cache_put(&self, key: &str, data: &serde_json::Value) {
        if let Some(ref cache) = self.cache {
            cache.put(key, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_osv_ecosystem_mapping() {
        let enricher = SecurityEnricher::new("http://localhost:3030/test", "debian", None);
        assert_eq!(enricher.osv_ecosystem_name(), "Debian");

        let enricher2 = SecurityEnricher::new("http://localhost:3030/test", "pypi", None);
        assert_eq!(enricher2.osv_ecosystem_name(), "PyPI");
    }

    #[test]
    fn test_maven_ecosystem_mapping() {
        let enricher = SecurityEnricher::new("http://localhost:3030/test", "maven", None);
        assert_eq!(enricher.osv_ecosystem_name(), "Maven");
    }

    #[test]
    fn test_unsupported_ecosystem() {
        let enricher = SecurityEnricher::new("http://localhost:3030/test", "unsupported", None);
        let temp_file = NamedTempFile::new().unwrap();
        let result = enricher.enrich(temp_file.path().to_str().unwrap());

        assert!(result.is_err(), "Should reject unsupported ecosystem");
        assert!(result.unwrap_err().to_string().contains("Unsupported ecosystem"));
    }
}
