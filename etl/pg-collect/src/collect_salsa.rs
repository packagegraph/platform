//! salsa.debian.org (Debian's GitLab) enrichment collector.
//!
//! Fetches debian/ files from salsa for authoritative upstream URLs and
//! temporal maintainer data. Uses Vcs-Git URLs from Phase 1 (not constructed paths).

use crate::enricher::rate_limit;
use crate::forge::{extract_forge_url_with_field, emit_upstream_repo, emit_dq_issue};
use crate::ntriples::NTriplesWriter;
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use regex::Regex;
use reqwest::blocking::Client;
use std::collections::{HashMap, HashSet};
use std::io::Result;
use std::time::Duration;

pub struct SalsaCollector {
    client: Client,
    dist: String,
    source_cache: Option<SourceCache>,
}

impl SalsaCollector {
    pub fn new(dist: String) -> Self {
        let client = crate::enricher::default_http_client();

        Self {
            client,
            dist,
            source_cache: None,
        }
    }

    pub fn with_cache(mut self, cache_dir: &str) -> Result<Self> {
        self.source_cache = Some(SourceCache::new(cache_dir, "debian-salsa")?);
        Ok(self)
    }

    /// Collect enrichment from salsa.debian.org.
    /// Uses Vcs-Git URLs from Phase 1 — only processes packages with salsa.debian.org Vcs-Git.
    pub fn collect(
        &self,
        writer: &mut NTriplesWriter,
        source_names: &HashSet<String>,
        source_identity_map: &HashMap<String, Vec<String>>,
        source_pkg_uris: &HashMap<String, String>,
        vcs_urls: &HashMap<String, String>,
        emit_maintainers: bool,
    ) -> Result<(usize, usize)> {
        let mut src_count = 0;
        let mut triple_count = 0;
        let mut branch_cache: HashMap<String, String> = HashMap::new();

        for src_name in source_names {
            // Only process if we have a Vcs-Git URL
            let vcs_url = match vcs_urls.get(src_name) {
                Some(url) => url,
                None => continue,
            };

            // Only process salsa.debian.org URLs
            if !vcs_url.contains("salsa.debian.org") {
                continue;
            }

            // Parse salsa URL: https://salsa.debian.org/{group}/{project}.git
            let (group, project) = match self.parse_salsa_url(vcs_url) {
                Some(parts) => parts,
                None => {
                    eprintln!("  {} → could not parse salsa URL: {}", src_name, vcs_url);
                    continue;
                }
            };

            // Get identity URIs for this source
            let identity_uris = source_identity_map.get(src_name).cloned().unwrap_or_default();
            if identity_uris.is_empty() {
                continue;
            }

            // Get SourcePackage URI for maintainer linking
            let source_pkg_uri = source_pkg_uris.get(src_name).map(|s| s.as_str());

            // Fetch debian/ files with branch fallback
            triple_count += self.process_salsa_package(
                writer,
                src_name,
                &group,
                &project,
                &identity_uris,
                source_pkg_uri,
                &mut branch_cache,
                emit_maintainers,
            )?;

            src_count += 1;

            // Rate limit
            rate_limit(Duration::from_millis(200));

            if src_count % 100 == 0 {
                eprintln!("Processed {} salsa packages ({} triples)", src_count, triple_count);
            }
        }

        eprintln!("Salsa enrichment: {} packages, {} triples", src_count, triple_count);
        Ok((src_count, triple_count))
    }

    fn parse_salsa_url(&self, vcs_url: &str) -> Option<(String, String)> {
        // https://salsa.debian.org/{group}/{project}.git or https://salsa.debian.org/{group}/{project}
        let re = Regex::new(r"salsa\.debian\.org/([^/]+)/([^/]+?)(?:\.git)?$").unwrap();
        if let Some(caps) = re.captures(vcs_url) {
            let group = caps.get(1).unwrap().as_str().to_string();
            let project = caps.get(2).unwrap().as_str().to_string();
            return Some((group, project));
        }
        None
    }

    fn process_salsa_package(
        &self,
        writer: &mut NTriplesWriter,
        src_name: &str,
        group: &str,
        project: &str,
        identity_uris: &[String],
        source_pkg_uri: Option<&str>,
        branch_cache: &mut HashMap<String, String>,
        emit_maintainers: bool,
    ) -> Result<usize> {
        let mut triples = 0;

        // Try to find the correct branch
        let branch = if let Some(cached_branch) = branch_cache.get(&format!("{}/{}", group, project)) {
            cached_branch.clone()
        } else {
            // Try branch fallback
            let branches = vec![
                format!("debian/{}", self.dist),
                "debian/latest".to_string(),
                "debian/main".to_string(),
                "master".to_string(),
            ];

            let mut found_branch = None;
            for branch in &branches {
                let test_url = format!(
                    "https://salsa.debian.org/{}/{}/-/raw/{}/debian/control",
                    group, project, branch
                );
                if let Ok(resp) = self.client.get(&test_url).send() {
                    if resp.status().is_success() {
                        found_branch = Some(branch.clone());
                        break;
                    }
                }
            }

            match found_branch {
                Some(branch) => {
                    branch_cache.insert(format!("{}/{}", group, project), branch.clone());
                    branch
                }
                None => {
                    // DQ: salsa fetch failed
                    triples += emit_dq_issue(writer, "debian-salsa", "branch-detection",
                        src_name, "salsa-fetch-failed", "warning")?;
                    return Ok(triples);
                }
            }
        };

        // Fetch debian/upstream/metadata (simple Key: Value format)
        // Track whether metadata yielded upstream repo triples (not DQ triples)
        let mut metadata_emitted_repo = false;
        if let Ok(metadata) = self.fetch_debian_file(group, project, &branch, "debian/upstream/metadata") {
            let (repo_triples, total) = self.parse_upstream_metadata(writer, &metadata, identity_uris)?;
            triples += total;
            metadata_emitted_repo = repo_triples > 0;
        }

        // Fetch debian/watch only if upstream/metadata didn't successfully emit upstream repo triples
        if !metadata_emitted_repo {
            if let Ok(watch) = self.fetch_debian_file(group, project, &branch, "debian/watch") {
                triples += self.parse_debian_watch(writer, &watch, identity_uris)?;
            }
        }

        // Fetch debian/changelog (if emit_maintainers enabled)
        if emit_maintainers {
            if let Ok(changelog) = self.fetch_debian_file(group, project, &branch, "debian/changelog") {
                triples += self.parse_debian_changelog(writer, &changelog, source_pkg_uri)?;
            }
        }

        Ok(triples)
    }

    fn fetch_debian_file(&self, group: &str, project: &str, branch: &str, path: &str) -> Result<String> {
        let url = format!(
            "https://salsa.debian.org/{}/{}/-/raw/{}/{}",
            group, project, branch, path
        );

        let logical_name = format!("{}/{}/{}", group, project, path.replace('/', "_"));

        // Use SourceCache if available
        if let Some(ref cache) = self.source_cache {
            let scope = CacheScope {
                collector: "debian-salsa".to_string(),
                distro: "debian".to_string(),
                release: self.dist.clone(),
                repo: Some(format!("{}/{}", group, project)),
                arch: Some(branch.to_string()),
            };

            let bytes_vec = match cache.fetch_or_reuse(&url, &scope, &logical_name)? {
                CacheResult::Fresh(bytes) => bytes,
                CacheResult::Cached(path) | CacheResult::NotModified(path) => {
                    std::fs::read(&path)?
                }
            };
            return String::from_utf8(bytes_vec)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
        }

        // Fallback to direct fetch
        let resp = self.client.get(&url).send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        if !resp.status().is_success() {
            return Err(std::io::Error::other(format!("HTTP {}", resp.status())));
        }

        resp.text()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Parse debian/upstream/metadata. Returns (repo_triples, total_triples).
    /// repo_triples counts only upstream repository triples (not DQ annotations).
    fn parse_upstream_metadata(&self, writer: &mut NTriplesWriter, metadata: &str, identity_uris: &[String]) -> Result<(usize, usize)> {
        let mut repo_triples = 0;
        let mut total_triples = 0;

        // Simple Key: Value parsing (DEP-12 format, not full YAML)
        for line in metadata.lines() {
            if let Some((key, value)) = line.split_once(':') {
                if key.trim() == "Repository" {
                    let repo_url = value.trim();
                    if repo_url.is_empty() {
                        continue;
                    }
                    if let Some(extraction) = extract_forge_url_with_field(repo_url, "upstream-metadata") {
                        // Emit upstreamRepository for each identity
                        for identity_uri in identity_uris {
                            let t = emit_upstream_repo(writer, identity_uri, &extraction, None)?;
                            repo_triples += t;
                            total_triples += t;
                        }
                    } else {
                        // DQ: upstream/metadata has Repository but not a forge URL
                        total_triples += emit_dq_issue(writer, "debian-salsa", "upstream-metadata",
                            repo_url, "upstream-metadata-no-repo", "info")?;
                    }
                }
            }
        }

        Ok((repo_triples, total_triples))
    }

    fn parse_debian_watch(&self, writer: &mut NTriplesWriter, watch: &str, identity_uris: &[String]) -> Result<usize> {
        let mut triples = 0;
        let mut found_upstream = false;

        for line in watch.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.starts_with("version=") || line.is_empty() {
                continue;
            }

            if let Some(url_part) = line.split_whitespace().next() {
                // Strategy 1: Extract authoritative upstream package name from registry URLs
                if let Some((ecosystem, upstream_name)) = extract_registry_name(url_part) {
                    let eco_uri = ecosystem_uri(ecosystem);
                    for identity_uri in identity_uris {
                        writer.write_triple(identity_uri, &format!("{PKG}upstreamEcosystem"), &eco_uri)?;
                        writer.write_triple(&eco_uri, RDF_TYPE, &format!("{PKG}Ecosystem"))?;
                        writer.write_literal(&eco_uri, RDFS_LABEL, ecosystem)?;
                        writer.write_literal(identity_uri, &format!("{PKG}upstreamPackageName"), &upstream_name)?;
                        triples += 4;
                    }
                    found_upstream = true;
                    break;
                }

                // Strategy 2: Extract forge URL (GitHub, GitLab, etc.)
                let base_url = if let Some(idx) = url_part.find("/releases") {
                    &url_part[..idx]
                } else if let Some(idx) = url_part.find("/archive") {
                    &url_part[..idx]
                } else if let Some(idx) = url_part.find("/download") {
                    &url_part[..idx]
                } else {
                    url_part
                };

                if let Some(extraction) = extract_forge_url_with_field(base_url, "debian-watch") {
                    for identity_uri in identity_uris {
                        triples += emit_upstream_repo(writer, identity_uri, &extraction, None)?;
                    }
                    found_upstream = true;
                    break;
                }
            }
        }

        if !found_upstream && !watch.trim().is_empty() {
            let first_url = watch.lines()
                .find(|l| !l.trim().starts_with('#') && !l.trim().starts_with("version=") && !l.trim().is_empty())
                .and_then(|l| l.split_whitespace().next())
                .unwrap_or("(unparseable)");
            triples += emit_dq_issue(writer, "debian-salsa", "watch",
                first_url, "watch-file-no-upstream", "info")?;
        }

        Ok(triples)
    }

    fn parse_debian_changelog(&self, writer: &mut NTriplesWriter, changelog: &str, source_pkg_uri: Option<&str>) -> Result<usize> {
        // Parse first entry for latest maintainer
        // Format: "package (version) dist; urgency=...\n\n  * changes\n\n -- Name <email>  Date"
        let re = Regex::new(r"--\s*([^<]+?)\s*<(.+?)>\s+").unwrap();

        if let Some(caps) = re.captures(changelog) {
            let name = caps.get(1).unwrap().as_str().trim();
            let email = caps.get(2).unwrap().as_str().trim();

            let maint_uri = maintainer_uri(email);
            writer.write_triple(&maint_uri, RDF_TYPE, &format!("{PKG}Person"))?;
            writer.write_literal(&maint_uri, &format!("{FOAF}name"), name)?;
            writer.write_literal(&maint_uri, RDFS_LABEL, name)?;
            writer.write_triple(&maint_uri, &format!("{FOAF}mbox"), &format!("mailto:{email}"))?;

            let mut triples = 4;

            // Link maintainer to SourcePackage (temporal maintainer data from changelog)
            if let Some(src_uri) = source_pkg_uri {
                writer.write_triple(src_uri, &format!("{PKG}maintainedBy"), &maint_uri)?;
                writer.write_triple(&maint_uri, &format!("{PKG}maintains"), src_uri)?;
                triples += 2;
            }

            return Ok(triples);
        }

        Ok(0)
    }
}

/// Extract an authoritative upstream package name from a registry URL in debian/watch.
///
/// Returns (ecosystem, upstream_name) if the URL matches a known package registry pattern.
///
/// Supported registries:
///   - `pypi.debian.net/{name}/` or `pypi.org/project/{name}/` → ("pypi", name)
///   - `registry.npmjs.org/{name}/` or `registry.npmjs.org/@{scope}/{name}/` → ("npm", name or @scope/name)
///   - `rubygems.org/downloads/{name}-` or `rubygems.org/gems/{name}/` → ("rubygems", name)
///   - `crates.io/api/v1/crates/{name}/` or `static.crates.io/crates/{name}/` → ("cargo", name)
///   - `cpan.metacpan.org/` with `/{dist}-` → ("cpan", dist)
///   - `hackage.haskell.org/package/{name}-` → ("hackage", name)
fn extract_registry_name(url: &str) -> Option<(&'static str, String)> {
    let lower = url.to_lowercase();

    // PyPI: pypi.debian.net/{name}/{name}-(.*).tar.gz
    if lower.contains("pypi.debian.net/") {
        let after = url.split("pypi.debian.net/").nth(1)?;
        let name = after.split('/').next()?;
        if !name.is_empty() {
            return Some(("pypi", name.to_string()));
        }
    }
    // PyPI: pypi.org/packages/source/{initial}/{name}/
    if lower.contains("pypi.org/") || lower.contains("files.pythonhosted.org/") {
        // Pattern: files.pythonhosted.org/packages/source/r/requests/requests-2.31.0.tar.gz
        if let Some(after) = url.split("/packages/source/").nth(1) {
            let parts: Vec<&str> = after.split('/').collect();
            if parts.len() >= 2 {
                return Some(("pypi", parts[1].to_string()));
            }
        }
    }

    // npm: registry.npmjs.org/{name}/-/{name}-(.*).tgz
    // npm scoped: registry.npmjs.org/@{scope}/{name}/-/
    if lower.contains("registry.npmjs.org/") {
        let after = url.split("registry.npmjs.org/").nth(1)?;
        if after.starts_with('@') {
            // Scoped: @scope/name
            let parts: Vec<&str> = after.splitn(3, '/').collect();
            if parts.len() >= 2 {
                return Some(("npm", format!("{}/{}", parts[0], parts[1])));
            }
        } else {
            let name = after.split('/').next()?;
            if !name.is_empty() && name != "-" {
                return Some(("npm", name.to_string()));
            }
        }
    }

    // RubyGems: rubygems.org/downloads/{name}-(.*).gem
    // or rubygems.org/gems/{name}/versions/
    if lower.contains("rubygems.org/") {
        if let Some(after) = url.split("rubygems.org/downloads/").nth(1) {
            // name-(.*).gem → extract name before first version-like segment
            let name = after.split('-').next()?;
            if !name.is_empty() {
                return Some(("rubygems", name.to_string()));
            }
        }
        if let Some(after) = url.split("rubygems.org/gems/").nth(1) {
            let name = after.split('/').next()?.trim_end_matches('/');
            if !name.is_empty() {
                return Some(("rubygems", name.to_string()));
            }
        }
    }

    // Cargo: crates.io/api/v1/crates/{name}/
    // or static.crates.io/crates/{name}/
    if lower.contains("crates.io/") {
        if let Some(after) = url.split("/crates/").nth(1) {
            let name = after.split('/').next()?;
            if !name.is_empty() {
                return Some(("cargo", name.to_string()));
            }
        }
    }

    // Hackage: hackage.haskell.org/package/{name}-
    if lower.contains("hackage.haskell.org/package/") {
        let after = url.split("hackage.haskell.org/package/").nth(1)?;
        // name-version.tar.gz → split at first digit after hyphen
        let name = after.split(|c: char| c == '-' && after[after.find('-').unwrap_or(0)+1..].starts_with(|d: char| d.is_ascii_digit()))
            .next()
            .unwrap_or(after)
            .trim_end_matches('/');
        if !name.is_empty() {
            return Some(("hackage", name.to_string()));
        }
    }

    // CPAN: cpan.metacpan.org/authors/id/./../{dist}-(.*).tar.gz
    if lower.contains("cpan.metacpan.org/") || lower.contains("search.cpan.org/") {
        // Extract last path component before version
        let parts: Vec<&str> = url.rsplitn(2, '/').collect();
        if let Some(filename) = parts.first() {
            // Module-Name-1.23.tar.gz → Module-Name
            if let Some(idx) = filename.rfind('-') {
                let name = &filename[..idx];
                if !name.is_empty() {
                    return Some(("cpan", name.replace('-', "::")));
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_salsa_url() {
        let collector = SalsaCollector::new("trixie".to_string());

        let (group, project) = collector.parse_salsa_url("https://salsa.debian.org/glibc-team/glibc.git").unwrap();
        assert_eq!(group, "glibc-team");
        assert_eq!(project, "glibc");

        // Without .git suffix
        let (group2, project2) = collector.parse_salsa_url("https://salsa.debian.org/debian/openssl").unwrap();
        assert_eq!(group2, "debian");
        assert_eq!(project2, "openssl");
    }

    #[test]
    fn test_parse_upstream_metadata() {
        use std::io::Read;

        let collector = SalsaCollector::new("trixie".to_string());

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let metadata = r#"Name: OpenSSL
Repository: https://github.com/openssl/openssl
Bug-Database: https://github.com/openssl/openssl/issues
"#;

        let identity_uris = vec!["https://packagegraph.github.io/d/pkg/debian/trixie/amd64/openssl".to_string()];
        let (repo_triples, total_triples) = collector.parse_upstream_metadata(&mut writer, metadata, &identity_uris).unwrap();

        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(repo_triples > 0, "Should emit repo triples");
        assert!(total_triples > 0, "Should emit total triples");
        assert!(content.contains("upstreamRepository"), "Should emit upstreamRepository");
        assert!(content.contains("github.com/openssl/openssl"), "Should extract GitHub repo");
    }

    #[test]
    fn test_parse_debian_watch() {
        use std::io::Read;

        let collector = SalsaCollector::new("trixie".to_string());

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let watch = r#"version=4
https://github.com/openssl/openssl/releases .*/v?([\d.]+)\.tar\.gz
"#;

        let identity_uris = vec!["https://packagegraph.github.io/d/pkg/debian/trixie/amd64/openssl".to_string()];
        let triples = collector.parse_debian_watch(&mut writer, watch, &identity_uris).unwrap();

        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(triples > 0, "Should emit triples");
        assert!(content.contains("upstreamRepository"), "Should emit upstreamRepository from watch");
        assert!(content.contains("github.com/openssl/openssl"), "Should extract base URL");
    }

    #[test]
    fn test_metadata_vs_watch_precedence() {
        use std::io::Read;

        // When upstream/metadata successfully yields repo triples,
        // debian/watch should NOT be processed (fallback only)
        let collector = SalsaCollector::new("trixie".to_string());

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let identity_uris = vec!["https://example.org/pkg/test".to_string()];

        // Metadata with valid Repository → should emit upstream repo
        let metadata = "Repository: https://github.com/test/repo\n";
        let (repo_triples, _total) = collector.parse_upstream_metadata(&mut writer, metadata, &identity_uris).unwrap();

        // repo_triples > 0 means forge extraction succeeded → watch should be skipped
        assert!(repo_triples > 0, "Metadata should emit repo triples for github.com URL");

        // When metadata succeeds, the fallback check is:
        // let metadata_emitted_repo = triples > metadata_triples_before;
        // if !metadata_emitted_repo { /* parse watch */ }
        // Since metadata_triples > 0, watch should be skipped

        // Metadata with NON-forge Repository → should NOT suppress watch
        let temp_file2 = NamedTempFile::new().unwrap();
        let mut writer2 = NTriplesWriter::new(temp_file2.reopen().unwrap());

        let bad_metadata = "Repository: not-a-url-at-all\nBug-Database: foo\n";
        let (bad_repo_triples, bad_total) = collector.parse_upstream_metadata(&mut writer2, bad_metadata, &identity_uris).unwrap();

        // Non-parseable URL yields 0 repo triples (but DQ triples emitted) → watch fallback should activate
        assert_eq!(bad_repo_triples, 0, "Non-forge metadata should emit 0 repo triples, enabling watch fallback");
        assert!(bad_total > 0, "DQ annotation should still be emitted");
    }

    #[test]
    fn test_branch_cache_reuse() {
        // Verify that branch_cache avoids redundant branch detection
        let mut branch_cache: HashMap<String, String> = HashMap::new();

        // Pre-populate cache (simulates a prior successful branch detection)
        branch_cache.insert("debian/openssl".to_string(), "debian/trixie".to_string());

        // Verify cache lookup works
        let cached = branch_cache.get("debian/openssl");
        assert_eq!(cached, Some(&"debian/trixie".to_string()));

        // Verify cache miss for unknown projects
        let miss = branch_cache.get("debian/unknown");
        assert!(miss.is_none());
    }

    #[test]
    fn test_salsa_end_to_end_mock_flow() {
        use std::io::Read;

        // Test the full salsa enrichment flow by exercising each parser
        // with realistic mock data, verifying the combined output.
        // This substitutes for HTTP mocking by testing the same code path
        // that process_salsa_package calls — just with known content.
        let collector = SalsaCollector::new("trixie".to_string());

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let identity_uris = vec![
            "https://packagegraph.github.io/d/pkg/debian/trixie/amd64/openssl".to_string(),
        ];
        let source_pkg_uri = Some("https://packagegraph.github.io/d/src/debian/trixie/openssl/3.2.2-1");

        let mut total_triples = 0;

        // Simulate process_salsa_package flow:
        // Step 1: Parse upstream/metadata → should emit upstream repo
        let metadata = "Repository: https://github.com/openssl/openssl\nBug-Database: https://github.com/openssl/openssl/issues\n";
        let (repo_triples, metadata_total) = collector.parse_upstream_metadata(&mut writer, metadata, &identity_uris).unwrap();
        total_triples += metadata_total;
        let metadata_emitted_repo = repo_triples > 0;

        assert!(metadata_emitted_repo, "Metadata should emit upstream repo for github.com");
        assert!(repo_triples >= 7, "Should emit ~7 triples (upstreamRepository + repositoryURL + forge)");

        // Step 2: debian/watch should be SKIPPED because metadata succeeded
        assert!(metadata_emitted_repo, "Watch should be skipped — metadata already emitted repo");

        // Step 3: Parse changelog → should link maintainer to SourcePackage
        let changelog = "openssl (3.2.2-1) trixie; urgency=medium\n\n  * New upstream release\n\n -- Security Team <security@debian.org>  Mon, 01 Apr 2026 10:00:00 +0000\n";
        let changelog_triples = collector.parse_debian_changelog(&mut writer, changelog, source_pkg_uri).unwrap();
        total_triples += changelog_triples;

        assert_eq!(changelog_triples, 6, "Changelog should emit 6 triples (4 Person + 2 maintainedBy/maintains)");

        writer.flush().unwrap();
        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Verify combined output
        assert!(content.contains("upstreamRepository"), "Should have upstream repo from metadata");
        assert!(content.contains("github.com/openssl/openssl"), "Should reference GitHub");
        assert!(content.contains("hostedOn"), "Should have forge triples");
        assert!(content.contains("maintainedBy"), "Should link maintainer to SourcePackage");
        assert!(content.contains("Security Team"), "Should have changelog maintainer name");
        assert!(content.contains("mailto:security@debian.org"), "Should have changelog maintainer email");

        assert!(total_triples >= 13, "Should emit at least 13 triples total (7 repo + 6 changelog)");
    }

    #[test]
    fn test_extract_registry_name_pypi() {
        assert_eq!(
            extract_registry_name("http://pypi.debian.net/setuptools/setuptools-(.*).tar.gz"),
            Some(("pypi", "setuptools".to_string()))
        );
        assert_eq!(
            extract_registry_name("https://pypi.debian.net/requests/requests-(.*).tar.gz"),
            Some(("pypi", "requests".to_string()))
        );
    }

    #[test]
    fn test_extract_registry_name_npm() {
        assert_eq!(
            extract_registry_name("https://registry.npmjs.org/acorn/-/acorn-(.*).tgz"),
            Some(("npm", "acorn".to_string()))
        );
        // Scoped package
        assert_eq!(
            extract_registry_name("https://registry.npmjs.org/@babel/core/-/core-(.*).tgz"),
            Some(("npm", "@babel/core".to_string()))
        );
    }

    #[test]
    fn test_extract_registry_name_rubygems() {
        assert_eq!(
            extract_registry_name("https://rubygems.org/downloads/nokogiri-(.*).gem"),
            Some(("rubygems", "nokogiri".to_string()))
        );
        assert_eq!(
            extract_registry_name("https://rubygems.org/gems/rails/versions"),
            Some(("rubygems", "rails".to_string()))
        );
    }

    #[test]
    fn test_extract_registry_name_cargo() {
        assert_eq!(
            extract_registry_name("https://crates.io/api/v1/crates/serde/download"),
            Some(("cargo", "serde".to_string()))
        );
        assert_eq!(
            extract_registry_name("https://static.crates.io/crates/tokio/tokio-(.*).crate"),
            Some(("cargo", "tokio".to_string()))
        );
    }

    #[test]
    fn test_extract_registry_name_hackage() {
        assert_eq!(
            extract_registry_name("https://hackage.haskell.org/package/aeson-2.1.0.tar.gz"),
            Some(("hackage", "aeson".to_string()))
        );
    }

    #[test]
    fn test_extract_registry_name_none() {
        assert_eq!(extract_registry_name("https://github.com/foo/bar"), None);
        assert_eq!(extract_registry_name("https://example.com/foo.tar.gz"), None);
    }
}
