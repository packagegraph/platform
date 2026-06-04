//! OSS Taxonomy classification enricher (v1: technology + role facets).
//!
//! Classifies package identities using pattern matching on package names
//! and upstream ecosystem metadata. Emits pkg:hasClassification triples
//! linking PackageIdentity to tax:* SKOS concepts.

use crate::ntriples::NTriplesWriter;
use crate::sparql::SparqlClient;
use crate::uris::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::Result;

pub struct TaxonomyEnricher {
    sparql: SparqlClient,
}

impl TaxonomyEnricher {
    pub fn new(endpoint: &str) -> Self {
        let sparql = SparqlClient::new(endpoint);
        Self { sparql }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        let packages = self.query_package_identities()?;
        eprintln!("Found {} package identities to classify", packages.len());

        let mut classified = 0usize;
        let mut total_triples = 0usize;

        for (identity_uri, name, ecosystem) in &packages {
            let classifications = classify_package(name, ecosystem.as_deref());
            if !classifications.is_empty() {
                classified += 1;
                for concept_uri in &classifications {
                    writer.write_triple(
                        identity_uri,
                        &format!("{PKG}hasClassification"),
                        concept_uri,
                    )?;
                    total_triples += 1;
                }
            }
        }

        writer.flush()?;
        eprintln!(
            "Classified {} / {} packages ({} triples)",
            classified,
            packages.len(),
            total_triples
        );
        Ok((classified, total_triples))
    }

    fn query_package_identities(&self) -> Result<Vec<(String, String, Option<String>)>> {
        // Collectors emit ecosystem URIs like .../d/ecosystem/cargo (no rdfs:label).
        // Extract the ecosystem name from the URI tail via STRAFTER.
        let eco_base = format!("{DATA}ecosystem/");
        let query = format!(
            r#"SELECT ?identity ?name ?ecosystemName WHERE {{
              ?identity a <{PKG}PackageIdentity> ;
                        <{PKG}packageName> ?name .
              OPTIONAL {{
                ?identity <{PKG}upstreamEcosystem> ?ecoUri .
                BIND(STRAFTER(STR(?ecoUri), "{eco_base}") AS ?ecosystemName)
              }}
            }}"#,
            PKG = PKG,
            eco_base = eco_base,
        );

        let results = self.sparql.query(&query)?;
        let mut packages = Vec::new();
        for row in results {
            if let (Some(uri), Some(name)) = (row.get("identity"), row.get("name")) {
                let ecosystem = row
                    .get("ecosystemName")
                    .filter(|s| !s.is_empty())
                    .cloned();
                packages.push((uri.clone(), name.clone(), ecosystem));
            }
        }
        Ok(packages)
    }
}

/// Classify a package based on name patterns and ecosystem.
/// Returns taxonomy concept URIs for matched classifications.
fn classify_package(name: &str, ecosystem: Option<&str>) -> Vec<String> {
    let mut concepts = Vec::new();

    // Technology facet: direct mapping from upstream ecosystem
    if let Some(eco) = ecosystem {
        if let Some(tech) = ecosystem_to_technology(eco) {
            concepts.push(format!("{TAX}technology-{tech}"));
        }
    }

    // Role facet: pattern matching on package name
    if let Some(role) = infer_role(name) {
        concepts.push(format!("{TAX}role-{role}"));
    }

    concepts
}

fn ecosystem_to_technology(ecosystem: &str) -> Option<&'static str> {
    match ecosystem.to_ascii_lowercase().as_str() {
        "npm" | "javascript" | "nodejs" => Some("javascript"),
        "pypi" | "python" => Some("python"),
        "cargo" | "rust" | "crates.io" => Some("rust"),
        "gomod" | "go" | "golang" => Some("go"),
        "maven" | "java" => Some("java"),
        "nuget" | "csharp" | "dotnet" => Some("csharp"),
        "rubygems" | "ruby" => Some("ruby"),
        "cpan" | "perl" => Some("perl"),
        "hackage" | "haskell" => Some("haskell"),
        "hex" | "elixir" | "erlang" => Some("elixir"),
        "cran" | "r" => Some("r"),
        "conda" => Some("python"),
        _ => None,
    }
}

fn infer_role(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();

    // Library patterns
    if lower.starts_with("lib")
        || lower.ends_with("-dev")
        || lower.ends_with("-devel")
        || lower.ends_with("-libs")
        || lower.ends_with("-lib")
    {
        return Some("library");
    }

    // CLI tool patterns
    if lower.ends_with("-cli")
        || lower.ends_with("-tools")
        || lower.ends_with("-utils")
        || lower.ends_with("-tool")
        || lower.ends_with("-bin")
    {
        return Some("cli-tool");
    }

    // Framework patterns
    if lower.ends_with("-framework") || lower.contains("framework") {
        return Some("framework");
    }

    // Service / daemon patterns
    if lower.ends_with("-server")
        || lower.ends_with("-daemon")
        || lower.ends_with("d") && lower.len() > 3 && lower.as_bytes()[lower.len() - 2] != b'-'
        && (lower.ends_with("httpd") || lower.ends_with("sshd") || lower.ends_with("crond"))
    {
        return Some("service");
    }

    // Documentation
    if lower.ends_with("-doc") || lower.ends_with("-docs") || lower.ends_with("-man") {
        return Some("documentation");
    }

    // Plugin / extension
    if lower.ends_with("-plugin") || lower.ends_with("-plugins") || lower.ends_with("-extension") {
        return Some("plugin");
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ecosystem_to_technology() {
        assert_eq!(ecosystem_to_technology("npm"), Some("javascript"));
        assert_eq!(ecosystem_to_technology("pypi"), Some("python"));
        assert_eq!(ecosystem_to_technology("cargo"), Some("rust"));
        assert_eq!(ecosystem_to_technology("unknown"), None);
    }

    #[test]
    fn test_infer_role_library() {
        assert_eq!(infer_role("libssl-dev"), Some("library"));
        assert_eq!(infer_role("libcurl4"), Some("library"));
        assert_eq!(infer_role("openssl-devel"), Some("library"));
        assert_eq!(infer_role("glibc-libs"), Some("library"));
    }

    #[test]
    fn test_infer_role_cli() {
        assert_eq!(infer_role("cargo-audit"), None); // not a -cli suffix
        assert_eq!(infer_role("podman-cli"), Some("cli-tool"));
        assert_eq!(infer_role("coreutils"), None); // doesn't match -utils
        assert_eq!(infer_role("bind-utils"), Some("cli-tool"));
    }

    #[test]
    fn test_infer_role_framework() {
        assert_eq!(infer_role("qt5-framework"), Some("framework"));
    }

    #[test]
    fn test_infer_role_doc() {
        assert_eq!(infer_role("python3-doc"), Some("documentation"));
        assert_eq!(infer_role("gcc-docs"), Some("documentation"));
    }

    #[test]
    fn test_classify_package_combined() {
        let classes = classify_package("libssl-dev", Some("pypi"));
        assert!(classes.iter().any(|c| c.contains("technology-python")));
        assert!(classes.iter().any(|c| c.contains("role-library")));
    }

    #[test]
    fn test_classify_package_no_ecosystem() {
        let classes = classify_package("libssl-dev", None);
        assert_eq!(classes.len(), 1);
        assert!(classes[0].contains("role-library"));
    }

    #[test]
    fn test_classify_package_no_match() {
        let classes = classify_package("openssl", None);
        assert!(classes.is_empty());
    }
}
