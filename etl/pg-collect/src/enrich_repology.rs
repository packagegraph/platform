//! Repology.org cross-distribution package equivalence enricher.
//!
//! Queries Fuseki for package names, looks them up on Repology, and emits
//! pkg:crossDistributionAlternative links between packages across distributions.
//! Renamed from equivalentInDistribution in ontology v0.7.0 to signal
//! non-transitive correspondence (symmetric but NOT transitive).

use crate::cache::FileCache;
use crate::enricher::{rate_limit, SLOW_RATE_LIMIT};
use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
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
    pub graph_uri: Option<String>,
}

impl RepologyEnricher {
    pub fn new(
        endpoint: &str,
        cache_dir: Option<&str>,
        auth: SparqlAuth,
        backend: SparqlBackend,
    ) -> Self {
        let sparql = make_sparql_client(endpoint, &auth, backend);
        let client = crate::enricher::default_http_client();

        let cache = cache_dir.map(|dir| {
            FileCache::new(dir, "repology", 168, None) // 7 days TTL
                .expect("Failed to create cache")
        });

        Self {
            sparql,
            client,
            cache,
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        // Get unique package names from the graph
        let packages = self.sparql.query_package_names_and_versions()?;
        let mut unique_names: Vec<String> = packages.iter().map(|(n, _)| n.clone()).collect();
        unique_names.sort();
        unique_names.dedup();

        eprintln!(
            "Found {} unique package names to check Repology",
            unique_names.len()
        );

        let mut total_links = 0;
        let mut total_triples = 0;
        let mut total_checked = 0;
        let mut total_errors = 0;
        let mut consecutive_errors = 0;

        for (idx, name) in unique_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {} / {} packages", idx + 1, unique_names.len());
            }
            total_checked += 1;

            // Note: query_repology maps 404 (package unknown to Repology, very
            // common) to Ok(0), so only genuine 5xx/network failures count as
            // errors. Abort on sustained failure so a Repology outage produces a
            // hard error rather than a near-empty "successful" result — mirrors
            // the npm-provenance and github enrichers.
            match self.query_repology(&mut writer, name) {
                Ok(triples) => {
                    if triples > 0 {
                        total_links += 1;
                        total_triples += triples;
                    }
                    consecutive_errors = 0;
                }
                Err(e) => {
                    eprintln!("  Error querying Repology for {}: {}", name, e);
                    total_errors += 1;
                    consecutive_errors += 1;
                    if consecutive_errors >= 20 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!(
                                "Aborting: {} consecutive failures — Repology may be down (last: {})",
                                consecutive_errors, e
                            ),
                        ));
                    }
                }
            }

            rate_limit(SLOW_RATE_LIMIT);
        }

        if total_checked > 0 && total_errors as f64 / total_checked as f64 > 0.5 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Aborting: error rate {}/{} ({:.0}%) exceeds 50% threshold",
                    total_errors,
                    total_checked,
                    total_errors as f64 / total_checked as f64 * 100.0
                ),
            ));
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
                let resp =
                    self.client.get(&url).send().map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    })?;

                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(0);
                }

                if !resp.status().is_success() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Repology API returned {}", resp.status()),
                    ));
                }

                let data: serde_json::Value = resp
                    .json()
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
                // Shortcut property (backward compatible)
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

                // Reified PackageRelationship with match method + confidence
                let rel_uri = package_relationship_uri(&distro_pkgs[i], &distro_pkgs[j]);
                writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}PackageRelationship"))?;
                writer.write_triple(
                    &distro_pkgs[i],
                    &format!("{PKG}hasPackageRelationship"),
                    &rel_uri,
                )?;
                writer.write_triple(
                    &rel_uri,
                    &format!("{PKG}relationshipTarget"),
                    &distro_pkgs[j],
                )?;
                writer.write_triple(
                    &rel_uri,
                    &format!("{PKG}matchMethod"),
                    &format!("{PKG}match-repology"),
                )?;
                writer.write_typed_literal(
                    &rel_uri,
                    &format!("{PKG}matchConfidence"),
                    "0.95",
                    &format!("{XSD}decimal"),
                )?;
                triples += 5;
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
        let _mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let enricher = RepologyEnricher::new(&server.url(), None, None, SparqlBackend::Fuseki);

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let data = serde_json::json!([
            {"repo": "debian_12", "version": "3.1.4"},
            {"repo": "fedora_41", "version": "3.1.4"},
            {"repo": "arch", "version": "3.2.0"},
            {"repo": "unknown_repo", "version": "1.0"}
        ]);

        let triples = enricher
            .emit_equivalence_links(&mut writer, "openssl", &data)
            .unwrap();
        writer.flush().unwrap();

        // 3 known distros → 3 pairs → 2 shortcut + 5 reified = 7 per pair → 21 total
        assert_eq!(
            triples, 21,
            "Should emit 21 triples (3 pairs × 7 triples each)"
        );

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("crossDistributionAlternative"),
            "Should have cross-distribution links"
        );
        assert!(
            content.contains("PackageRelationship"),
            "Should have reified relationships"
        );
        assert!(content.contains("matchMethod"), "Should have match method");
        assert!(
            content.contains("match-repology"),
            "Should use repology match method"
        );
        assert!(
            content.contains("matchConfidence"),
            "Should have match confidence"
        );
        assert!(content.contains("0.95"), "Confidence should be 0.95");
        assert!(
            content.contains("debian/bookworm"),
            "Should reference Debian"
        );
        assert!(content.contains("fedora/41"), "Should reference Fedora");
        assert!(content.contains("arch/arch"), "Should reference Arch");
        assert!(
            !content.contains("unknown_repo"),
            "Should NOT include unknown repos"
        );
    }

    #[test]
    fn test_emit_equivalence_empty_data() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let enricher = RepologyEnricher::new(&server.url(), None, None, SparqlBackend::Fuseki);

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let data = serde_json::json!([]);
        let triples = enricher
            .emit_equivalence_links(&mut writer, "nonexistent", &data)
            .unwrap();

        assert_eq!(triples, 0);
    }
}
