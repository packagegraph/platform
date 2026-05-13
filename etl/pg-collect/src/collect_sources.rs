//! Sources.gz parser for Debian source package metadata.
//!
//! Extracts Build-Depends (+ Build-Depends-Indep + Build-Depends-Arch) and Uploaders
//! from Sources.gz index files. Build dependencies are emitted on SourcePackage URIs
//! per the ontology (deb.ttl:40).

use crate::ntriples::NTriplesWriter;
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use flate2::read::GzDecoder;
use regex::Regex;
use reqwest::blocking::Client;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Result};

pub struct SourcesCollector {
    client: Client,
    repo_url: String,
    distro: String,
    distribution: String,
    component: String,
    source_cache: Option<SourceCache>,
}

impl SourcesCollector {
    pub fn new(repo_url: String, distro: String, distribution: String, component: String) -> Self {
        let client = crate::enricher::default_http_client();

        Self {
            client,
            repo_url,
            distro,
            distribution,
            component,
            source_cache: None,
        }
    }

    pub fn with_cache(mut self, cache_dir: &str) -> Result<Self> {
        self.source_cache = Some(SourceCache::new(cache_dir, "debian-sources")?);
        Ok(self)
    }

    /// Collect source package metadata from Sources.gz.
    /// Only processes source packages in the source_names set.
    pub fn collect(
        &self,
        writer: &mut NTriplesWriter,
        source_names: &HashSet<String>,
        source_identity_map: &HashMap<String, Vec<String>>,
        source_pkg_uris: &HashMap<String, String>,
        vcs_urls: &HashMap<String, String>,
        codename: &str,
        emit_builddeps: bool,
        emit_uploaders: bool,
    ) -> Result<(usize, usize)> {
        let sources_url = format!(
            "{}/dists/{}/{}/source/Sources.gz",
            self.repo_url.trim_end_matches('/'),
            self.distribution,
            self.component
        );

        eprintln!("Downloading Sources.gz from {}", sources_url);

        // Download Sources.gz
        let raw_bytes = match self.fetch_raw_bytes(&sources_url, "Sources.gz") {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("Sources.gz unavailable: {}", e);
                // DQ: Sources.gz not found
                let triples = crate::forge::emit_dq_issue(
                    writer, "debian-sources", "sources-gz", &self.distribution,
                    "sources-gz-unavailable", "warning"
                )?;
                return Ok((0, triples));
            }
        };

        // Decompress
        let decoder = GzDecoder::new(&raw_bytes[..]);
        let reader = BufReader::new(decoder);

        let mut src_count = 0;
        let mut triple_count = 0;

        // Parse line-by-line as RFC-822 stanzas
        let mut current_src: HashMap<String, String> = HashMap::new();
        let mut last_key = String::new();

        for line in reader.lines() {
            let line = line?;

            if line.is_empty() {
                // End of source entry
                if !current_src.is_empty() && current_src.contains_key("Package") {
                    let src_name = current_src.get("Package").unwrap();

                    // Only process if this source package is in our set
                    if source_names.contains(src_name) {
                        triple_count += self.emit_source_triples(
                            writer,
                            &current_src,
                            src_name,
                            source_pkg_uris,
                            source_identity_map,
                            vcs_urls,
                            codename,
                            emit_builddeps,
                            emit_uploaders,
                        )?;
                        src_count += 1;
                    }
                }
                current_src.clear();
                last_key.clear();
            } else if line.starts_with(' ') || line.starts_with('\t') {
                // Continuation of previous field
                if !last_key.is_empty() {
                    if let Some(value) = current_src.get_mut(&last_key) {
                        value.push(' ');
                        value.push_str(line.trim());
                    }
                }
            } else if let Some((key, value)) = line.split_once(':') {
                // New field
                let key = key.trim().to_string();
                last_key = key.clone();
                current_src.insert(key, value.trim().to_string());
            }
        }

        // Process last source if file doesn't end with blank line
        if !current_src.is_empty() && current_src.contains_key("Package") {
            let src_name = current_src.get("Package").unwrap();
            if source_names.contains(src_name) {
                triple_count += self.emit_source_triples(
                    writer,
                    &current_src,
                    src_name,
                    source_pkg_uris,
                    source_identity_map,
                    vcs_urls,
                    codename,
                    emit_builddeps,
                    emit_uploaders,
                )?;
                src_count += 1;
            }
        }

        eprintln!("Sources.gz: processed {} source packages, {} triples", src_count, triple_count);
        Ok((src_count, triple_count))
    }

    fn fetch_raw_bytes(&self, url: &str, logical_name: &str) -> Result<Vec<u8>> {
        if let Some(ref cache) = self.source_cache {
            let scope = CacheScope {
                collector: "debian-sources".to_string(),
                distro: self.distro.clone(),
                release: self.distribution.clone(),
                repo: Some(self.component.clone()),
                arch: None,
            };
            match cache.fetch_or_reuse(url, &scope, logical_name)? {
                CacheResult::Fresh(bytes) => {
                    eprintln!("Downloaded {} ({} bytes, cached)", logical_name, bytes.len());
                    Ok(bytes)
                }
                CacheResult::Cached(path) | CacheResult::NotModified(path) => {
                    eprintln!("Using cached {} ({})", logical_name, path.display());
                    std::fs::read(&path)
                }
            }
        } else {
            let response = self.client.get(url).send()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            if !response.status().is_success() {
                return Err(std::io::Error::other(format!("HTTP {}", response.status())));
            }
            let bytes = response.bytes()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(bytes.to_vec())
        }
    }

    fn emit_source_triples(
        &self,
        writer: &mut NTriplesWriter,
        src_data: &HashMap<String, String>,
        src_name: &str,
        source_pkg_uris: &HashMap<String, String>,
        source_identity_map: &HashMap<String, Vec<String>>,
        vcs_urls: &HashMap<String, String>,
        codename: &str,
        emit_builddeps: bool,
        emit_uploaders: bool,
    ) -> Result<usize> {
        let mut triples = 0;

        // Get the SourcePackage URI (from Phase 1)
        let src_pkg_uri = match source_pkg_uris.get(src_name) {
            Some(uri) => uri,
            None => {
                eprintln!("Warning: No SourcePackage URI for {}", src_name);
                return Ok(0);
            }
        };

        // Go-Import-Path: authoritative Go module path from Sources.gz
        // Overrides the heuristic name-prefix detection for Go packages
        if let Some(go_import_path) = src_data.get("Go-Import-Path") {
            let import_path = go_import_path.trim();
            if !import_path.is_empty() {
                // Emit on all binary package identities that map to this source
                if let Some(binary_uris) = source_identity_map.get(src_name) {
                    for identity_uri in binary_uris {
                        let eco_uri = ecosystem_uri("gomod");
                        writer.write_triple(identity_uri, &format!("{PKG}upstreamEcosystem"), &eco_uri)?;
                        writer.write_triple(&eco_uri, RDF_TYPE, &format!("{PKG}Ecosystem"))?;
                        writer.write_literal(&eco_uri, RDFS_LABEL, "gomod")?;
                        writer.write_literal(identity_uri, &format!("{PKG}upstreamPackageName"), import_path)?;
                        triples += 4;
                    }
                }
                // Also on the source package itself
                writer.write_literal(src_pkg_uri, &format!("{PKG}upstreamPackageName"), import_path)?;
                triples += 1;
            }
        }

        // Build-Depends emission (on SourcePackage URI per deb.ttl:40)
        if emit_builddeps {
            if let Some(build_deps) = src_data.get("Build-Depends") {
                triples += self.emit_build_deps(writer, src_pkg_uri, build_deps, codename, "deb:buildDepends")?;
            }
            if let Some(build_deps_indep) = src_data.get("Build-Depends-Indep") {
                triples += self.emit_build_deps(writer, src_pkg_uri, build_deps_indep, codename, "deb:buildDependsIndep")?;
            }
            if let Some(build_deps_arch) = src_data.get("Build-Depends-Arch") {
                // Fallback to deb:buildDepends if buildDependsArch not in ontology
                triples += self.emit_build_deps(writer, src_pkg_uri, build_deps_arch, codename, "deb:buildDepends")?;
            }
        }

        // Uploaders emission (co-maintainers on packaging repo)
        if emit_uploaders {
            if let Some(uploaders) = src_data.get("Uploaders") {
                triples += self.emit_uploaders_triples(writer, src_name, uploaders, vcs_urls)?;
            }
        }

        Ok(triples)
    }

    fn emit_build_deps(
        &self,
        writer: &mut NTriplesWriter,
        src_pkg_uri: &str,
        dep_string: &str,
        codename: &str,
        predicate: &str,
    ) -> Result<usize> {
        let mut triples = 0;

        // Parse dependency list: "pkg1 (>= 1.0), pkg2 | pkg3, pkg4 [arch]"
        for part in dep_string.split(',') {
            // Take first alternative (ignore | alternatives for now)
            let first_alt = part.split('|').next().unwrap_or(part).trim();

            // Strip version constraints and annotations
            // Pattern: "package-name (>= 1.0) [arch] <!profile>"
            let re = Regex::new(r"^([\w.-]+)").unwrap();
            if let Some(caps) = re.captures(first_alt) {
                let dep_name = caps.get(1).unwrap().as_str();

                // Build-dep targets use arch="source" per plan
                let dep_uri = package_identity_uri(&self.distro, codename, "source", dep_name);

                // Ensure identity exists
                writer.write_triple(&dep_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
                writer.write_literal(&dep_uri, &format!("{PKG}packageName"), dep_name)?;
                triples += 2;

                // Emit Build-Depends triple with ontology-specific predicate
                let pred_uri = if predicate == "deb:buildDepends" {
                    format!("{DEB}buildDepends")
                } else if predicate == "deb:buildDependsIndep" {
                    format!("{DEB}buildDependsIndep")
                } else {
                    format!("{DEB}buildDepends") // fallback for buildDependsArch
                };
                writer.write_triple(src_pkg_uri, &pred_uri, &dep_uri)?;
                triples += 1;
            }
        }

        Ok(triples)
    }

    fn emit_uploaders_triples(
        &self,
        writer: &mut NTriplesWriter,
        src_name: &str,
        uploaders_str: &str,
        vcs_urls: &HashMap<String, String>,
    ) -> Result<usize> {
        let mut triples = 0;

        // Get packaging repository URI from vcs_urls (tracked in Phase 1)
        let pkg_repo_uri = if let Some(vcs_url) = vcs_urls.get(src_name) {
            Some(repo_uri(vcs_url))
        } else {
            None
        };

        // Parse "Name <email>, Name2 <email2>"
        let re = Regex::new(r"([^<,]+?)\s*<(.+?)>").unwrap();
        for caps in re.captures_iter(uploaders_str) {
            let name = caps.get(1).unwrap().as_str().trim();
            let email = caps.get(2).unwrap().as_str().trim();

            let maint_uri = maintainer_uri(email);

            // Type as Person
            writer.write_triple(&maint_uri, RDF_TYPE, &format!("{PKG}Person"))?;
            writer.write_literal(&maint_uri, &format!("{FOAF}name"), name)?;
            writer.write_literal(&maint_uri, RDFS_LABEL, name)?;
            writer.write_triple(&maint_uri, &format!("{FOAF}mbox"), &format!("mailto:{email}"))?;
            triples += 4;

            // Link to packaging repository (if Vcs-Git available from Phase 1)
            if let Some(ref repo_uri_str) = pkg_repo_uri {
                writer.write_triple(repo_uri_str, &format!("{PKG}hasContributor"), &maint_uri)?;
                writer.write_triple(&maint_uri, &format!("{PKG}contributesTo"), repo_uri_str)?;
                triples += 2;
            }
        }

        Ok(triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_sources_collector_creation() {
        let collector = SourcesCollector::new(
            "http://deb.debian.org/debian".to_string(),
            "debian".to_string(),
            "trixie".to_string(),
            "main".to_string(),
        );

        // Just verify it compiles
        assert_eq!(collector.distro, "debian");
    }

    #[test]
    fn test_emit_build_deps() {
        use std::io::Read;

        let collector = SourcesCollector::new(
            "http://deb.debian.org/debian".to_string(),
            "debian".to_string(),
            "trixie".to_string(),
            "main".to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let src_pkg_uri = "https://packagegraph.github.io/d/src/debian/trixie/openssl/3.2.2-1".to_string();

        // Test Build-Depends with version constraints and alternatives
        let build_deps = "debhelper-compat (= 13), dpkg-dev (>= 1.22.5), gcc | clang, perl:any [arch]";
        let triples = collector.emit_build_deps(&mut writer, &src_pkg_uri, build_deps, "trixie", "deb:buildDepends").unwrap();

        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should emit Build-Depends triples for first alternatives (debhelper-compat, dpkg-dev, gcc, perl)
        assert!(content.contains("buildDepends"), "Should emit buildDepends predicate");
        assert!(content.contains("debhelper-compat"), "Should extract debhelper-compat");
        assert!(content.contains("dpkg-dev"), "Should extract dpkg-dev");
        assert!(content.contains("\"gcc\""), "Should extract gcc (first alternative)");
        assert!(triples > 0, "Should emit triples");
    }

    #[test]
    fn test_emit_uploaders() {
        use std::io::Read;

        let collector = SourcesCollector::new(
            "http://deb.debian.org/debian".to_string(),
            "debian".to_string(),
            "trixie".to_string(),
            "main".to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut vcs_urls = HashMap::new();
        vcs_urls.insert("openssl".to_string(), "https://salsa.debian.org/debian/openssl.git".to_string());

        let uploaders = "Alice Developer <alice@debian.org>, Bob Maintainer <bob@example.com>";
        let triples = collector.emit_uploaders_triples(&mut writer, "openssl", uploaders, &vcs_urls).unwrap();

        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should emit Person entities for both uploaders
        assert!(content.contains("Person"), "Should type as Person");
        assert!(content.contains("Alice Developer"), "Should emit Alice's name");
        assert!(content.contains("Bob Maintainer"), "Should emit Bob's name");
        assert!(content.contains("mailto:alice@debian.org"), "Should emit Alice's email");
        assert!(content.contains("mailto:bob@example.com"), "Should emit Bob's email");

        // Should link to packaging repository
        assert!(content.contains("hasContributor"), "Should emit hasContributor on repo");
        assert!(content.contains("contributesTo"), "Should emit contributesTo on person");

        assert!(triples >= 12, "Should emit at least 12 triples (6 per person: 4 Person + 2 links)");
    }

    #[test]
    fn test_sources_gz_stanza_parsing() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::{Read, Write};

        // Create mock Sources.gz content (RFC-822 stanzas)
        let mock_sources = r#"Package: openssl
Version: 3.2.2-1
Binary: openssl, libssl3t64, libssl-dev
Maintainer: Debian OpenSSL Team <pkg-openssl-devel@alioth-lists.debian.net>
Uploaders: Christoph Martin <christoph.martin@uni-mainz.de>,
 Kurt Roeckx <kurt@roeckx.be>
Build-Depends: debhelper-compat (= 13), dpkg-dev (>= 1.22.5),
 libssl-dev, perl
Build-Depends-Indep: perl
Homepage: https://www.openssl.org/
Vcs-Git: https://salsa.debian.org/debian/openssl.git

Package: curl
Version: 8.5.0-2
Binary: curl, libcurl4t64, libcurl4-openssl-dev
Maintainer: Alessandro Ghedini <ghedo@debian.org>
Build-Depends: debhelper-compat (= 13), cmake, libssl-dev
Homepage: https://curl.se

"#;

        // Compress to gzip
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(mock_sources.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();

        // Write to temp file and serve via SourcesCollector
        // Since we can't easily mock HTTP, test the stanza parsing logic directly
        // by calling emit_source_triples for each parsed stanza

        let collector = SourcesCollector::new(
            "http://example.com".to_string(),
            "debian".to_string(),
            "trixie".to_string(),
            "main".to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // Set up source tracking data (from Phase 1)
        let mut source_names = HashSet::new();
        source_names.insert("openssl".to_string());
        source_names.insert("curl".to_string());

        let mut source_pkg_uris = HashMap::new();
        source_pkg_uris.insert("openssl".to_string(), "https://packagegraph.github.io/d/src/debian/trixie/openssl/3.2.2-1".to_string());
        source_pkg_uris.insert("curl".to_string(), "https://packagegraph.github.io/d/src/debian/trixie/curl/8.5.0-2".to_string());

        let mut vcs_urls = HashMap::new();
        vcs_urls.insert("openssl".to_string(), "https://salsa.debian.org/debian/openssl.git".to_string());

        let source_identity_map: HashMap<String, Vec<String>> = HashMap::new();

        // Parse stanzas manually (same logic as collect() but from bytes instead of HTTP)
        let decoder = flate2::read::GzDecoder::new(&compressed[..]);
        let reader = std::io::BufReader::new(decoder);
        let mut current_src: HashMap<String, String> = HashMap::new();
        let mut last_key = String::new();
        let mut parsed_count = 0;
        let mut total_triples = 0;

        for line in reader.lines() {
            let line = line.unwrap();
            if line.is_empty() {
                if !current_src.is_empty() && current_src.contains_key("Package") {
                    let src_name = current_src.get("Package").unwrap().clone();
                    if source_names.contains(&src_name) {
                        total_triples += collector.emit_source_triples(
                            &mut writer, &current_src, &src_name,
                            &source_pkg_uris, &source_identity_map, &vcs_urls, "trixie", true, true,
                        ).unwrap();
                        parsed_count += 1;
                    }
                }
                current_src.clear();
                last_key.clear();
            } else if line.starts_with(' ') || line.starts_with('\t') {
                if !last_key.is_empty() {
                    if let Some(value) = current_src.get_mut(&last_key) {
                        value.push(' ');
                        value.push_str(line.trim());
                    }
                }
            } else if let Some((key, value)) = line.split_once(':') {
                let key = key.trim().to_string();
                last_key = key.clone();
                current_src.insert(key, value.trim().to_string());
            }
        }

        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Verify both source packages were parsed
        assert_eq!(parsed_count, 2, "Should parse both openssl and curl");

        // Verify Build-Depends emitted on SourcePackage URI
        assert!(content.contains("d/src/debian/trixie/openssl"), "Should emit on openssl SourcePackage");
        assert!(content.contains("buildDepends"), "Should emit buildDepends");
        assert!(content.contains("debhelper-compat"), "Should parse debhelper-compat");

        // Verify Build-Depends-Indep emitted with correct predicate
        assert!(content.contains("buildDependsIndep"), "Should emit buildDependsIndep for openssl");

        // Verify Uploaders parsed (openssl has Uploaders, curl does not)
        assert!(content.contains("Christoph Martin"), "Should parse Christoph from Uploaders");
        assert!(content.contains("Kurt Roeckx"), "Should parse Kurt from multi-line Uploaders");
        assert!(content.contains("hasContributor"), "Should link Uploaders to packaging repo");

        assert!(total_triples > 20, "Should emit significant number of triples");
    }
}
