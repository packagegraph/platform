//! Repology.org cross-distribution package equivalence enricher.
//!
//! Queries Fuseki for package names, looks them up on Repology, and emits
//! pkg:crossDistributionAlternative links between packages across distributions.
//! Renamed from equivalentInDistribution in ontology v0.7.0 to signal
//! non-transitive correspondence (symmetric but NOT transitive).

use crate::cache::FileCache;
use crate::enricher::{rate_limit, SLOW_RATE_LIMIT};
use crate::ntriples::NTriplesWriter;
use crate::sparql::SparqlClient;
use crate::uris::*;
use reqwest::blocking::Client;
use std::fs::File;
use std::io::Result;

/// Mapping from Repology repo names to our (distro, release) format.
fn repo_mapping(repo: &str) -> Option<(&str, &str)> {
    match repo {
        "debian_12" => Some(("debian", "bookworm")),
        "debian_13" => Some(("debian", "trixie")),
        "fedora_41" => Some(("fedora", "41")),
        "fedora_42" => Some(("fedora", "42")),
        "fedora_rawhide" => Some(("fedora", "rawhide")),
        "opensuse_tumbleweed" => Some(("opensuse", "tumbleweed")),
        "alpine_3_20" | "alpine_3_21" => Some(("alpine", "v3.20")),
        "arch" => Some(("arch", "arch")),
        "gentoo" => Some(("gentoo", "gentoo")),
        "void_x86_64" => Some(("void", "void")),
        "freebsd" => Some(("freebsd", "freebsd")),
        "nix_unstable" => Some(("nix", "unstable")),
        "homebrew" => Some(("homebrew", "homebrew")),
        _ => None,
    }
}

pub struct RepologyEnricher {
    sparql: SparqlClient,
    client: Client,
    cache: Option<FileCache>,
}

impl RepologyEnricher {
    pub fn new(endpoint: &str, cache_dir: Option<&str>) -> Self {
        let sparql = SparqlClient::new(endpoint);
        let client = crate::enricher::default_http_client();

        let cache = cache_dir.map(|dir| {
            FileCache::new(dir, "repology", 168, None) // 7 days TTL
                .expect("Failed to create cache")
        });

        Self { sparql, client, cache }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        // Get unique package names from the graph
        let packages = self.sparql.query_package_names_and_versions()?;
        let mut unique_names: Vec<String> = packages.iter().map(|(n, _)| n.clone()).collect();
        unique_names.sort();
        unique_names.dedup();

        eprintln!("Found {} unique package names to check Repology", unique_names.len());

        let mut total_links = 0;
        let mut total_triples = 0;

        for (idx, name) in unique_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {} / {} packages", idx + 1, unique_names.len());
            }

            match self.query_repology(&mut writer, name) {
                Ok(triples) if triples > 0 => {
                    total_links += 1;
                    total_triples += triples;
                }
                Ok(_) => {}
                Err(e) => eprintln!("  Error querying Repology for {}: {}", name, e),
            }

            rate_limit(SLOW_RATE_LIMIT);
        }

        writer.flush()?;
        Ok((total_links, total_triples))
    }

    fn query_repology(&self, writer: &mut NTriplesWriter, name: &str) -> Result<usize> {
        let cache_key = format!("repology-{}", name);

        let data = match self.cached_get(&cache_key) {
            Some(d) => d,
            None => {
                let url = format!("https://repology.org/api/v1/project/{}", name);
                let resp = self.client.get(&url)
                    .send()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(0);
                }

                if !resp.status().is_success() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Repology API returned {}", resp.status()),
                    ));
                }

                let data: serde_json::Value = resp.json()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                self.cache_put(&cache_key, &data);
                data
            }
        };

        self.emit_equivalence_links(writer, name, &data)
    }

    fn emit_equivalence_links(
        &self,
        writer: &mut NTriplesWriter,
        name: &str,
        data: &serde_json::Value,
    ) -> Result<usize> {
        let entries = match data.as_array() {
            Some(arr) => arr,
            None => return Ok(0),
        };

        // Collect package URIs for each known distribution
        let mut distro_pkgs: Vec<String> = Vec::new();

        for entry in entries {
            let repo = match entry.get("repo").and_then(|v| v.as_str()) {
                Some(r) => r,
                None => continue,
            };

            let (distro, release) = match repo_mapping(repo) {
                Some(pair) => pair,
                None => continue,
            };

            let identity = package_identity_uri(distro, release, "any", name);
            if !distro_pkgs.contains(&identity) {
                distro_pkgs.push(identity);
            }
        }

        // Create bidirectional equivalence links between all pairs
        let mut triples = 0;
        for i in 0..distro_pkgs.len() {
            for j in (i + 1)..distro_pkgs.len() {
                writer.write_triple(
                    &distro_pkgs[i],
                    &format!("{PKG}crossDistributionAlternative"),
                    &distro_pkgs[j],
                )?;
                writer.write_triple(
                    &distro_pkgs[j],
                    &format!("{PKG}crossDistributionAlternative"),
                    &distro_pkgs[i],
                )?;
                triples += 2;
            }
        }

        Ok(triples)
    }

    fn cached_get(&self, key: &str) -> Option<serde_json::Value> {
        self.cache.as_ref()?.get(key)
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
    fn test_repo_mapping() {
        assert_eq!(repo_mapping("debian_12"), Some(("debian", "bookworm")));
        assert_eq!(repo_mapping("fedora_41"), Some(("fedora", "41")));
        assert_eq!(repo_mapping("arch"), Some(("arch", "arch")));
        assert_eq!(repo_mapping("unknown_repo"), None);
    }

    #[test]
    fn test_emit_equivalence_links() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let enricher = RepologyEnricher::new(&server.url(), None);

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let data = serde_json::json!([
            {"repo": "debian_12", "version": "3.1.4"},
            {"repo": "fedora_41", "version": "3.1.4"},
            {"repo": "arch", "version": "3.2.0"},
            {"repo": "unknown_repo", "version": "1.0"}
        ]);

        let triples = enricher.emit_equivalence_links(&mut writer, "openssl", &data).unwrap();
        writer.flush().unwrap();

        // 3 known distros → 3 pairs → 6 bidirectional links
        assert_eq!(triples, 6, "Should emit 6 equivalence triples (3 pairs × 2 directions)");

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("crossDistributionAlternative"), "Should have cross-distribution links");
        assert!(content.contains("debian/bookworm"), "Should reference Debian");
        assert!(content.contains("fedora/41"), "Should reference Fedora");
        assert!(content.contains("arch/arch"), "Should reference Arch");
        assert!(!content.contains("unknown_repo"), "Should NOT include unknown repos");
    }

    #[test]
    fn test_emit_equivalence_empty_data() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let enricher = RepologyEnricher::new(&server.url(), None);

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let data = serde_json::json!([]);
        let triples = enricher.emit_equivalence_links(&mut writer, "nonexistent", &data).unwrap();

        assert_eq!(triples, 0);
    }
}
