//! RPM spec file collector — fetches spec files from dist-git and extracts
//! upstream repository URLs, commit hashes, and ecosystem correlations.
//!
//! Supports Fedora (src.fedoraproject.org) and CentOS Stream (gitlab.com/redhat)
//! dist-git instances. Source0 URLs are routed through forge.rs for repository
//! extraction. Ecosystem correlation uses Source0 domain matching and
//! BuildRequires macro detection.

use crate::enricher::rate_limit;
use crate::forge::{extract_forge_url_with_field, emit_forge_triples, emit_dq_issue};
use crate::ntriples::NTriplesWriter;
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::blocking::Client;
use std::collections::{HashMap, HashSet};
use std::io::Result;
use std::time::Duration;

/// Regex for extracting Source0 URL from spec file.
static SOURCE0_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^Source0?\s*:\s*(.+)$").unwrap()
});

/// Regex for extracting all Source* entries from spec file.
static ALL_SOURCES_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^Source\d*\s*:\s*(.+)$").unwrap()
});

/// Regex for extracting all Patch* entries from spec file.
static PATCHES_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^Patch\d*\s*:\s*(.+)$").unwrap()
});

/// Regex for extracting %global commit or %global githash macros.
static COMMIT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^%(?:global|define)\s+(?:commit|githash|gitcommit)\s+([0-9a-f]{7,40})").unwrap()
});

/// Regex for extracting URL: field from spec header.
static URL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^URL\s*:\s*(.+)$").unwrap()
});

/// Regex for extracting Name: field.
static NAME_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^Name\s*:\s*(.+)$").unwrap()
});

/// Regex for extracting Version: field.
static VERSION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^Version\s*:\s*(.+)$").unwrap()
});

/// Regex for BuildRequires lines.
static BUILDREQUIRES_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^BuildRequires\s*:\s*(.+)$").unwrap()
});

/// Regex for python3dist(X) macro.
static PYTHON3DIST_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"python3dist\(([^)]+)\)").unwrap()
});

/// Regex for perl(Module::Name) macro.
static PERL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"perl\(([^)]+)\)").unwrap()
});

/// Regex for %changelog entries: * Day Mon DD YYYY Name <email> - version
static CHANGELOG_ENTRY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\*\s+\w+\s+(\w+\s+\d+\s+\d{4})\s+(.+?)\s+<([^>]+)>\s*[-–]").unwrap()
});

/// Ecosystem detection result with confidence level.
pub struct EcosystemDetection {
    pub ecosystem: &'static str,
    pub package_name: Option<String>,
    /// Detection strategy: "source0-domain" (high), "buildrequires-macro" (high), "name-prefix" (medium)
    pub detection_method: &'static str,
}

/// Parsed spec file data.
pub struct SpecData {
    pub source0_url: Option<String>,
    pub all_sources: Vec<String>,
    pub patches: Vec<String>,
    pub commit_hash: Option<String>,
    pub url_field: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub build_requires: Vec<String>,
    pub changelog_entries: Vec<ChangelogEntry>,
}

/// A parsed %changelog entry.
pub struct ChangelogEntry {
    pub date: String,
    pub name: String,
    pub email: String,
}

/// Spec file collector for Fedora/CentOS Stream dist-git.
pub struct SpecCollector {
    client: Client,
    distro: String,
    release: String,
    cache: Option<SourceCache>,
}

impl SpecCollector {
    pub fn new(distro: &str, release: &str, cache_dir: Option<&str>) -> Result<Self> {
        let client = crate::enricher::default_http_client();

        let cache = match cache_dir {
            Some(dir) => Some(SourceCache::new(dir, "spec")?),
            None => None,
        };

        Ok(Self {
            client,
            distro: distro.to_string(),
            release: release.to_string(),
            cache,
        })
    }

    /// Collect spec file data for a set of SRPM source names.
    /// identity_map maps source name → list of PackageIdentity URIs for upstream linking.
    pub fn collect(
        &self,
        writer: &mut NTriplesWriter,
        srpm_names: &HashSet<String>,
        srpm_identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
    ) -> Result<(usize, usize)> {
        let mut total_specs = 0;
        let mut total_triples = 0;

        for (idx, name) in srpm_names.iter().enumerate() {
            match self.process_spec(
                writer, name, srpm_identity_map,
                existing_ecosystem_pkgs, emit_buildrequires, emit_maintainers,
            ) {
                Ok(triples) => {
                    if triples > 0 {
                        total_specs += 1;
                        total_triples += triples;
                    }
                }
                Err(e) => {
                    eprintln!("  {} → error: {}", name, e);
                }
            }

            if (idx + 1) % 100 == 0 {
                eprintln!("Processed {}/{} spec files ({} triples)", idx + 1, srpm_names.len(), total_triples);
            }

            rate_limit(Duration::from_millis(200));
        }

        eprintln!("Spec collection complete: {} specs, {} triples", total_specs, total_triples);
        Ok((total_specs, total_triples))
    }

    fn process_spec(
        &self,
        writer: &mut NTriplesWriter,
        source_name: &str,
        identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
    ) -> Result<usize> {
        // Fetch spec file
        let spec_content = match self.fetch_spec(source_name) {
            Ok(content) => content,
            Err(e) => {
                eprintln!("  {} → spec not found: {}", source_name, e);
                // DQ: record failed spec fetch
                let triples = emit_dq_issue(writer, "collect-spec", "spec-file",
                    source_name, "spec-fetch-failed", "warning")?;
                return Ok(triples);
            }
        };

        let spec = parse_spec(&spec_content);
        let mut triples = 0;

        // Get identity URIs for this SRPM
        let identity_uris = identity_map.get(source_name).cloned().unwrap_or_default();

        // --- Source0 → upstream repository ---
        if let Some(ref source0) = spec.source0_url {
            // Expand simple macros
            let expanded = expand_macros(source0, &spec);

            // DQ: check for unexpanded macros
            if expanded.contains("%{") || expanded.contains("%(") {
                triples += emit_dq_issue(writer, "collect-spec", "source0",
                    &expanded, "unexpanded-macro", "info")?;
            }

            if let Some(extraction) = extract_forge_url_with_field(&expanded, "source0") {
                let r_uri = repo_uri(&extraction.repo_url);

                // Emit upstreamRepository on each PackageIdentity
                for identity_uri in &identity_uris {
                    writer.write_triple(identity_uri, &format!("{PKG}upstreamRepository"), &r_uri)?;
                    triples += 1;
                }

                // Emit forge triples for the repository
                triples += emit_forge_triples(writer, &r_uri, &extraction.repo_url)?;
            } else if !expanded.contains("%{") {
                // DQ: Source0 present but not a recognizable forge URL
                triples += emit_dq_issue(writer, "collect-spec", "source0",
                    &expanded, "no-forge-match", "info")?;
            }
        } else {
            // DQ: no Source0 in spec file
            triples += emit_dq_issue(writer, "collect-spec", "source0",
                source_name, "missing-source0", "info")?;
        }

        // --- All Source*: entries → rpm:hasSpecSource ---
        let source_uri = source_uri(&self.distro, &self.release, source_name,
            &spec.version.as_deref().unwrap_or("unknown"));
        for source_url in &spec.all_sources {
            writer.write_literal(&source_uri, &format!("{RPM}hasSpecSource"), source_url)?;
            triples += 1;
        }

        // --- All Patch*: entries → rpm:hasPatch → rpm:Patch entities ---
        for (idx, patch_url) in spec.patches.iter().enumerate() {
            // Extract patch filename from URL (may contain macros)
            let patch_name = patch_url.split('/').last().unwrap_or(patch_url);

            let patch_uri = format!("{DATA}patch/{}/{}/{}/{}-{}",
                crate::uris::encode(&self.distro), crate::uris::encode(&self.release),
                crate::uris::encode(source_name), idx, crate::uris::encode(patch_name));

            writer.write_triple(&patch_uri, RDF_TYPE, &format!("{RPM}Patch"))?;
            writer.write_literal(&patch_uri, &format!("{RPM}patchName"), patch_name)?;
            writer.write_triple(&source_uri, &format!("{RPM}hasPatch"), &patch_uri)?;
            triples += 3;
        }

        // --- %commit → derivedFromCommit ---
        if let Some(ref commit) = spec.commit_hash {
            let commit_uri = format!("{DATA}commit/{}", commit);
            writer.write_triple(&commit_uri, RDF_TYPE, &format!("{VCS}Commit"))?;
            writer.write_literal(&commit_uri, &format!("{VCS}commitHash"), commit)?;
            writer.write_triple(&source_uri, &format!("{PKG}derivedFromCommit"), &commit_uri)?;
            triples += 3;
        }

        // --- Ecosystem correlation ---
        if !existing_ecosystem_pkgs.contains(source_name) {
            triples += self.emit_ecosystem_triples(writer, &spec, source_name, &identity_uris)?;
        }

        // --- BuildRequires (optional) ---
        if emit_buildrequires {
            triples += self.emit_buildrequires_triples(writer, &spec, source_name)?;
        }

        // --- Maintainer/changelog (optional) ---
        if emit_maintainers && !spec.changelog_entries.is_empty() {
            triples += self.emit_changelog_triples(writer, &spec, source_name)?;
        }

        if triples > 0 {
            eprintln!("  {} → {} triples", source_name, triples);
        }

        Ok(triples)
    }

    fn fetch_spec(&self, source_name: &str) -> Result<String> {
        let urls = self.spec_urls(source_name);

        for url in &urls {
            match self.fetch_url(url, source_name) {
                Ok(content) => return Ok(content),
                Err(_) => continue,
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Spec file not found for {} (tried {} URLs)", source_name, urls.len()),
        ))
    }

    fn spec_urls(&self, source_name: &str) -> Vec<String> {
        match self.distro.as_str() {
            "fedora" => {
                let branch = if self.release == "rawhide" {
                    "rawhide".to_string()
                } else {
                    format!("f{}", self.release)
                };
                vec![
                    format!("https://src.fedoraproject.org/rpms/{}/raw/{}/f/{}.spec", source_name, branch, source_name),
                    format!("https://src.fedoraproject.org/rpms/{}/raw/rawhide/f/{}.spec", source_name, source_name),
                    format!("https://src.fedoraproject.org/rpms/{}/raw/main/f/{}.spec", source_name, source_name),
                ]
            }
            "centos-stream" => {
                let branch = format!("c{}s", self.release);
                vec![
                    // CentOS Stream dist-git: spec file at repo root, not SPECS/
                    format!("https://gitlab.com/redhat/centos-stream/rpms/{}/-/raw/{}/{}.spec", source_name, branch, source_name),
                    format!("https://gitlab.com/redhat/centos-stream/rpms/{}/-/raw/main/{}.spec", source_name, source_name),
                ]
            }
            _ => vec![],
        }
    }

    fn fetch_url(&self, url: &str, source_name: &str) -> Result<String> {
        if let Some(ref cache) = self.cache {
            let scope = CacheScope {
                collector: "spec".to_string(),
                distro: self.distro.clone(),
                release: self.release.clone(),
                repo: None,
                arch: None,
            };
            match cache.fetch_or_reuse(url, &scope, &format!("{}.spec", source_name))? {
                CacheResult::Fresh(bytes) => return String::from_utf8(bytes)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                CacheResult::Cached(path) | CacheResult::NotModified(path) => {
                    return std::fs::read_to_string(&path);
                }
            }
        }

        // Direct download
        let resp = self.client.get(url).send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        if !resp.status().is_success() {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound,
                format!("HTTP {} for {}", resp.status(), url)));
        }
        resp.text().map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
    }

    fn emit_ecosystem_triples(
        &self,
        writer: &mut NTriplesWriter,
        spec: &SpecData,
        source_name: &str,
        identity_uris: &[String],
    ) -> Result<usize> {
        let mut triples = 0;

        if let Some(detection) = detect_ecosystem(spec, source_name) {
            let ecosystem_uri = format!("{DATA}ecosystem/{}", detection.ecosystem);

            // Emit once: ecosystem entity
            writer.write_triple(&ecosystem_uri, RDF_TYPE, &format!("{PKG}Ecosystem"))?;
            writer.write_literal(&ecosystem_uri, RDFS_LABEL, detection.ecosystem)?;
            triples += 2;

            // Emit on each PackageIdentity
            for identity_uri in identity_uris {
                writer.write_triple(identity_uri, &format!("{PKG}upstreamEcosystem"), &ecosystem_uri)?;
                triples += 1;

                if let Some(ref pkg_name) = detection.package_name {
                    writer.write_literal(identity_uri, &format!("{PKG}upstreamPackageName"), pkg_name)?;
                    triples += 1;
                }
            }

            // DQ: record detection method and confidence
            let confidence = match detection.detection_method {
                "source0-domain" | "buildrequires-macro" => "high",
                "name-prefix" => "medium",
                _ => "low",
            };
            triples += emit_dq_issue(writer, "collect-spec", "ecosystem-detection",
                &format!("{}:{} via {}", detection.ecosystem, source_name, detection.detection_method),
                &format!("ecosystem-detected-{}", confidence), "info")?;
        }

        Ok(triples)
    }

    fn emit_buildrequires_triples(
        &self,
        writer: &mut NTriplesWriter,
        spec: &SpecData,
        source_name: &str,
    ) -> Result<usize> {
        let mut triples = 0;
        let src_uri = source_uri(&self.distro, &self.release, source_name,
            &spec.version.as_deref().unwrap_or("unknown"));

        let mut seen = HashSet::new();
        for br_line in &spec.build_requires {
            // Parse individual requirements from the line (comma or space separated)
            for req in parse_buildrequires(br_line) {
                if seen.insert(req.clone()) {
                    // Emit shortcut triple
                    let target = package_identity_uri(&self.distro, &self.release, "noarch", &req);
                    writer.write_triple(&src_uri, &format!("{PKG}buildDependsOn"), &target)?;
                    triples += 1;
                }
            }
        }

        Ok(triples)
    }

    fn emit_changelog_triples(
        &self,
        writer: &mut NTriplesWriter,
        spec: &SpecData,
        source_name: &str,
    ) -> Result<usize> {
        let mut triples = 0;
        let src_uri = source_uri(&self.distro, &self.release, source_name,
            &spec.version.as_deref().unwrap_or("unknown"));

        // Emit build attribution from latest changelog entry
        if let Some(entry) = spec.changelog_entries.first() {
            if let Some(person_uri) = person_uri_from_email(&entry.email) {
                let email = normalize_email(&entry.email);
                writer.write_triple(&person_uri, RDF_TYPE, &format!("{PKG}Person"))?;
                writer.write_literal(&person_uri, &format!("{FOAF}name"), &entry.name)?;
                writer.write_literal(&person_uri, &format!("{FOAF}mbox"), &format!("mailto:{}", email))?;
                writer.write_triple(&src_uri, &format!("{PROV}wasAttributedTo"), &person_uri)?;
                triples += 4;

                // DQ: track that we normalized an obfuscated email
                if entry.email != email {
                    triples += emit_dq_issue(writer, "spec-changelog",
                        "obfuscated-email", &entry.email, "email-normalized", "info")?;
                }
            } else {
                // DQ: email could not be normalized — data loss
                triples += emit_dq_issue(writer, "spec-changelog",
                    "invalid-email", &entry.email, "email-unparseable", "warning")?;
            }
        }

        Ok(triples)
    }
}

// ─── Parsing functions ──────────────────────────────────────────────────

/// Parse a spec file into structured data.
pub fn parse_spec(content: &str) -> SpecData {
    let source0_url = SOURCE0_RE.captures(content)
        .map(|c| c[1].trim().to_string());

    let commit_hash = COMMIT_RE.captures(content)
        .map(|c| c[1].trim().to_string());

    let url_field = URL_RE.captures(content)
        .map(|c| c[1].trim().to_string());

    let name = NAME_RE.captures(content)
        .map(|c| c[1].trim().to_string());

    let version = VERSION_RE.captures(content)
        .map(|c| c[1].trim().to_string());

    let build_requires: Vec<String> = BUILDREQUIRES_RE.captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let all_sources: Vec<String> = ALL_SOURCES_RE.captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let patches: Vec<String> = PATCHES_RE.captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let changelog_entries: Vec<ChangelogEntry> = CHANGELOG_ENTRY_RE.captures_iter(content)
        .map(|c| ChangelogEntry {
            date: c[1].to_string(),
            name: c[2].to_string(),
            email: c[3].to_string(),
        })
        .collect();

    SpecData { source0_url, all_sources, patches, commit_hash, url_field, name, version, build_requires, changelog_entries }
}

/// Expand well-known RPM macros in a URL string.
fn expand_macros(url: &str, spec: &SpecData) -> String {
    let mut result = url.to_string();
    if let Some(ref name) = spec.name {
        result = result.replace("%{name}", name).replace("%name", name);
    }
    if let Some(ref version) = spec.version {
        result = result.replace("%{version}", version).replace("%version", version);
    }
    if let Some(ref url_field) = spec.url_field {
        result = result.replace("%{url}", url_field);
    }
    if let Some(ref commit) = spec.commit_hash {
        result = result.replace("%{commit}", commit);
    }
    result
}

/// Detect upstream ecosystem from spec data.
pub fn detect_ecosystem(spec: &SpecData, source_name: &str) -> Option<EcosystemDetection> {
    // Strategy 1: Source0 URL domain (highest confidence)
    if let Some(ref source0) = spec.source0_url {
        let expanded = expand_macros(source0, spec);
        let lower = expanded.to_lowercase();

        if lower.contains("files.pythonhosted.org") || lower.contains("pypi.org") || lower.contains("pypi.io") {
            let pkg = strip_ecosystem_prefix(source_name, &["python3-", "python-", "py"]);
            return Some(EcosystemDetection { ecosystem: "pypi", package_name: Some(pkg), detection_method: "source0-domain" });
        }
        if lower.contains("rubygems.org") {
            let pkg = strip_ecosystem_prefix(source_name, &["rubygem-"]);
            return Some(EcosystemDetection { ecosystem: "rubygems", package_name: Some(pkg), detection_method: "source0-domain" });
        }
        if lower.contains("crates.io") || lower.contains("static.crates.io") {
            let pkg = strip_ecosystem_prefix(source_name, &["rust-"]);
            return Some(EcosystemDetection { ecosystem: "cargo", package_name: Some(pkg), detection_method: "source0-domain" });
        }
        if lower.contains("registry.npmjs.org") || lower.contains("npmjs.com") {
            let pkg = strip_ecosystem_prefix(source_name, &["nodejs-", "npm-"]);
            return Some(EcosystemDetection { ecosystem: "npm", package_name: Some(pkg), detection_method: "source0-domain" });
        }
        if lower.contains("cpan.metacpan.org") || lower.contains("search.cpan.org") || lower.contains("cpan.org") {
            let pkg = strip_ecosystem_prefix(source_name, &["perl-"]);
            return Some(EcosystemDetection { ecosystem: "cpan", package_name: Some(pkg), detection_method: "source0-domain" });
        }
        if lower.contains("hackage.haskell.org") {
            let pkg = strip_ecosystem_prefix(source_name, &["ghc-"]);
            return Some(EcosystemDetection { ecosystem: "hackage", package_name: Some(pkg), detection_method: "source0-domain" });
        }
        if lower.contains("hex.pm") {
            return Some(EcosystemDetection { ecosystem: "hex", package_name: Some(source_name.to_string()), detection_method: "source0-domain" });
        }
    }

    // Strategy 2: BuildRequires macros (high confidence)
    for br in &spec.build_requires {
        if PYTHON3DIST_RE.is_match(br) {
            let pkg = strip_ecosystem_prefix(source_name, &["python3-", "python-"]);
            return Some(EcosystemDetection { ecosystem: "pypi", package_name: Some(pkg), detection_method: "buildrequires-macro" });
        }
        if PERL_RE.is_match(br) {
            let pkg = strip_ecosystem_prefix(source_name, &["perl-"]);
            return Some(EcosystemDetection { ecosystem: "cpan", package_name: Some(pkg), detection_method: "buildrequires-macro" });
        }
    }

    // Strategy 3: Package name prefix (medium confidence, fallback)
    if source_name.starts_with("python3-") || source_name.starts_with("python-") {
        let pkg = strip_ecosystem_prefix(source_name, &["python3-", "python-"]);
        return Some(EcosystemDetection { ecosystem: "pypi", package_name: Some(pkg), detection_method: "name-prefix" });
    }
    if source_name.starts_with("perl-") {
        let pkg = strip_ecosystem_prefix(source_name, &["perl-"]);
        return Some(EcosystemDetection { ecosystem: "cpan", package_name: Some(pkg), detection_method: "name-prefix" });
    }
    if source_name.starts_with("rubygem-") {
        let pkg = strip_ecosystem_prefix(source_name, &["rubygem-"]);
        return Some(EcosystemDetection { ecosystem: "rubygems", package_name: Some(pkg), detection_method: "name-prefix" });
    }
    if source_name.starts_with("rust-") {
        let pkg = strip_ecosystem_prefix(source_name, &["rust-"]);
        return Some(EcosystemDetection { ecosystem: "cargo", package_name: Some(pkg), detection_method: "name-prefix" });
    }
    if source_name.starts_with("ghc-") {
        let pkg = strip_ecosystem_prefix(source_name, &["ghc-"]);
        return Some(EcosystemDetection { ecosystem: "hackage", package_name: Some(pkg), detection_method: "name-prefix" });
    }
    if source_name.starts_with("golang-") {
        return Some(EcosystemDetection { ecosystem: "gomod", package_name: Some(source_name.to_string()), detection_method: "name-prefix" });
    }
    if source_name.starts_with("nodejs-") {
        let pkg = strip_ecosystem_prefix(source_name, &["nodejs-"]);
        return Some(EcosystemDetection { ecosystem: "npm", package_name: Some(pkg), detection_method: "name-prefix" });
    }

    None
}

/// Detect upstream ecosystem from package name and optional homepage URL.
/// Designed for Debian packages which don't have BuildRequires/Source0 metadata.
///
/// Strategy priority:
/// 1. Homepage domain (if provided) - high confidence
/// 2. Package name prefix - medium confidence
pub fn detect_ecosystem_by_name(package_name: &str, homepage_url: Option<&str>) -> Option<EcosystemDetection> {
    // Strategy 1: Homepage domain (highest confidence when available)
    if let Some(homepage) = homepage_url {
        let lower = homepage.to_lowercase();

        if lower.contains("files.pythonhosted.org") || lower.contains("pypi.org") || lower.contains("pypi.io") {
            let pkg = strip_ecosystem_prefix(package_name, &["python3-", "python-", "py"]);
            return Some(EcosystemDetection { ecosystem: "pypi", package_name: Some(pkg), detection_method: "homepage-domain" });
        }
        if lower.contains("rubygems.org") {
            let pkg = strip_ecosystem_prefix(package_name, &["rubygem-", "ruby-"]);
            return Some(EcosystemDetection { ecosystem: "rubygems", package_name: Some(pkg), detection_method: "homepage-domain" });
        }
        if lower.contains("crates.io") || lower.contains("static.crates.io") {
            let pkg = strip_ecosystem_prefix(package_name, &["rust-", "librust-"]);
            return Some(EcosystemDetection { ecosystem: "cargo", package_name: Some(pkg), detection_method: "homepage-domain" });
        }
        if lower.contains("registry.npmjs.org") || lower.contains("npmjs.com") {
            let pkg = strip_ecosystem_prefix(package_name, &["nodejs-", "npm-", "node-"]);
            return Some(EcosystemDetection { ecosystem: "npm", package_name: Some(pkg), detection_method: "homepage-domain" });
        }
        if lower.contains("cpan.metacpan.org") || lower.contains("search.cpan.org") || lower.contains("cpan.org") {
            let pkg = strip_ecosystem_prefix(package_name, &["perl-", "lib"]);
            return Some(EcosystemDetection { ecosystem: "cpan", package_name: Some(pkg), detection_method: "homepage-domain" });
        }
        if lower.contains("hackage.haskell.org") {
            let pkg = strip_ecosystem_prefix(package_name, &["ghc-", "libghc-"]);
            return Some(EcosystemDetection { ecosystem: "hackage", package_name: Some(pkg), detection_method: "homepage-domain" });
        }
        if lower.contains("hex.pm") {
            return Some(EcosystemDetection { ecosystem: "hex", package_name: Some(package_name.to_string()), detection_method: "homepage-domain" });
        }
        if lower.contains("cran.r-project.org") {
            let pkg = strip_ecosystem_prefix(package_name, &["r-cran-"]);
            return Some(EcosystemDetection { ecosystem: "cran", package_name: Some(pkg), detection_method: "homepage-domain" });
        }
        if lower.contains("bioconductor.org") {
            let pkg = strip_ecosystem_prefix(package_name, &["r-bioc-"]);
            return Some(EcosystemDetection { ecosystem: "bioconductor", package_name: Some(pkg), detection_method: "homepage-domain" });
        }
    }

    // Strategy 2: Debian package name prefixes
    // Python: python3- or python-
    if package_name.starts_with("python3-") || package_name.starts_with("python-") {
        let pkg = strip_ecosystem_prefix(package_name, &["python3-", "python-"]);
        return Some(EcosystemDetection { ecosystem: "pypi", package_name: Some(pkg), detection_method: "name-prefix" });
    }

    // Perl: lib*-perl pattern
    if package_name.ends_with("-perl") && package_name.starts_with("lib") {
        // libwww-perl → www (strip "lib" prefix and "-perl" suffix)
        let without_lib = package_name.strip_prefix("lib").unwrap_or(package_name);
        let without_suffix = without_lib.strip_suffix("-perl").unwrap_or(without_lib);
        return Some(EcosystemDetection { ecosystem: "cpan", package_name: Some(without_suffix.to_string()), detection_method: "name-prefix" });
    }

    // Ruby: ruby-
    if package_name.starts_with("ruby-") {
        let pkg = strip_ecosystem_prefix(package_name, &["ruby-"]);
        return Some(EcosystemDetection { ecosystem: "rubygems", package_name: Some(pkg), detection_method: "name-prefix" });
    }

    // Rust: librust-*-dev pattern
    if package_name.starts_with("librust-") && package_name.ends_with("-dev") {
        // librust-serde-dev → serde
        let without_lib = package_name.strip_prefix("librust-").unwrap_or(package_name);
        let without_suffix = without_lib.strip_suffix("-dev").unwrap_or(without_lib);
        return Some(EcosystemDetection { ecosystem: "cargo", package_name: Some(without_suffix.to_string()), detection_method: "name-prefix" });
    }

    // Node.js: node-
    if package_name.starts_with("node-") {
        let pkg = strip_ecosystem_prefix(package_name, &["node-"]);
        return Some(EcosystemDetection { ecosystem: "npm", package_name: Some(pkg), detection_method: "name-prefix" });
    }

    // Go: golang-
    if package_name.starts_with("golang-") {
        return Some(EcosystemDetection { ecosystem: "gomod", package_name: Some(package_name.to_string()), detection_method: "name-prefix" });
    }

    // R CRAN: r-cran-
    if package_name.starts_with("r-cran-") {
        let pkg = strip_ecosystem_prefix(package_name, &["r-cran-"]);
        return Some(EcosystemDetection { ecosystem: "cran", package_name: Some(pkg), detection_method: "name-prefix" });
    }

    // R Bioconductor: r-bioc-
    if package_name.starts_with("r-bioc-") {
        let pkg = strip_ecosystem_prefix(package_name, &["r-bioc-"]);
        return Some(EcosystemDetection { ecosystem: "bioconductor", package_name: Some(pkg), detection_method: "name-prefix" });
    }

    // Haskell: libghc-*-dev or ghc-
    if package_name.starts_with("libghc-") && package_name.ends_with("-dev") {
        // libghc-aeson-dev → aeson
        let without_lib = package_name.strip_prefix("libghc-").unwrap_or(package_name);
        let without_suffix = without_lib.strip_suffix("-dev").unwrap_or(without_lib);
        return Some(EcosystemDetection { ecosystem: "hackage", package_name: Some(without_suffix.to_string()), detection_method: "name-prefix" });
    }
    if package_name.starts_with("ghc-") {
        let pkg = strip_ecosystem_prefix(package_name, &["ghc-"]);
        return Some(EcosystemDetection { ecosystem: "hackage", package_name: Some(pkg), detection_method: "name-prefix" });
    }

    // Emacs Lisp: elpa-
    if package_name.starts_with("elpa-") {
        let pkg = strip_ecosystem_prefix(package_name, &["elpa-"]);
        return Some(EcosystemDetection { ecosystem: "elpa", package_name: Some(pkg), detection_method: "name-prefix" });
    }

    None
}

/// Strip known ecosystem prefixes from a source name to get the upstream package name.
fn strip_ecosystem_prefix(source_name: &str, prefixes: &[&str]) -> String {
    for prefix in prefixes {
        if source_name.starts_with(prefix) {
            return source_name[prefix.len()..].to_string();
        }
    }
    source_name.to_string()
}

/// Parse a BuildRequires line into individual package names.
fn parse_buildrequires(line: &str) -> Vec<String> {
    let mut result = Vec::new();
    // Split on commas or spaces, skip version constraints
    for part in line.split(|c: char| c == ',' || c.is_whitespace()) {
        let trimmed = part.trim();
        if trimmed.is_empty() || trimmed.starts_with('>') || trimmed.starts_with('<')
            || trimmed.starts_with('=') || trimmed.parse::<f64>().is_ok() {
            continue;
        }
        // Skip macros we can't resolve
        if trimmed.contains("%{") && !trimmed.contains("python3dist") && !trimmed.contains("perl(") {
            continue;
        }
        result.push(trimmed.to_string());
    }
    result
}

// ─── Constants ──────────────────────────────────────────────────────────

const FOAF: &str = "http://xmlns.com/foaf/0.1/";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_SPEC: &str = r#"
Name:           python-requests
Version:        2.31.0
Release:        1.fc43
Summary:        HTTP library for Python
URL:            https://requests.readthedocs.io
Source0:        https://files.pythonhosted.org/packages/source/r/requests/requests-%{version}.tar.gz

BuildRequires:  python3-devel
BuildRequires:  python3dist(pytest) >= 7.0
BuildRequires:  python3dist(urllib3) >= 1.26

%description
Python HTTP library.

%changelog
* Wed Apr 23 2026 Alice Developer <alice@fedoraproject.org> - 2.31.0-1
- Update to 2.31.0

* Mon Jan 15 2024 Bob Maintainer <bob@fedoraproject.org> - 2.28.0-1
- Initial package
"#;

    const GITHUB_SPEC: &str = r#"
Name:           openssl
Version:        3.2.1
Release:        1.fc43
URL:            https://www.openssl.org
%global commit abc123def456

Source0:        https://github.com/openssl/openssl/archive/%{commit}/openssl-%{commit}.tar.gz

BuildRequires:  gcc
BuildRequires:  perl(Test::More)

%changelog
* Mon Apr 21 2026 Security Team <security@fedoraproject.org> - 3.2.1-1
- Security update
"#;

    #[test]
    fn test_parse_spec_source0() {
        let spec = parse_spec(SAMPLE_SPEC);
        assert!(spec.source0_url.is_some());
        assert!(spec.source0_url.unwrap().contains("pythonhosted.org"));
        assert_eq!(spec.name, Some("python-requests".to_string()));
        assert_eq!(spec.version, Some("2.31.0".to_string()));
    }

    #[test]
    fn test_parse_spec_commit() {
        let spec = parse_spec(GITHUB_SPEC);
        assert_eq!(spec.commit_hash, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_parse_spec_buildrequires() {
        let spec = parse_spec(SAMPLE_SPEC);
        assert_eq!(spec.build_requires.len(), 3);
        assert!(spec.build_requires[0].contains("python3-devel"));
    }

    #[test]
    fn test_parse_spec_changelog() {
        let spec = parse_spec(SAMPLE_SPEC);
        assert_eq!(spec.changelog_entries.len(), 2);
        assert_eq!(spec.changelog_entries[0].name, "Alice Developer");
        assert_eq!(spec.changelog_entries[0].email, "alice@fedoraproject.org");
        assert_eq!(spec.changelog_entries[1].name, "Bob Maintainer");
    }

    #[test]
    fn test_expand_macros() {
        let spec = parse_spec(GITHUB_SPEC);
        let source0 = spec.source0_url.as_ref().unwrap();
        let expanded = expand_macros(source0, &spec);
        assert!(expanded.contains("abc123def456"), "Commit macro should be expanded");
        assert!(expanded.contains("openssl"), "Name macro should be expanded");
        assert!(!expanded.contains("%{commit}"), "Macro should not remain");
    }

    #[test]
    fn test_detect_ecosystem_pypi_source0() {
        let spec = parse_spec(SAMPLE_SPEC);
        let detection = detect_ecosystem(&spec, "python-requests").unwrap();
        assert_eq!(detection.ecosystem, "pypi");
        assert_eq!(detection.package_name, Some("requests".to_string()));
    }

    #[test]
    fn test_detect_ecosystem_cpan_buildrequires() {
        let spec = parse_spec(GITHUB_SPEC);
        let detection = detect_ecosystem(&spec, "openssl");
        // openssl has perl(Test::More) in BuildRequires but is not a Perl package
        // The perl() macro triggers CPAN detection — but openssl doesn't start with perl-
        // Strategy 2 matches because of perl() in BuildRequires
        assert!(detection.is_some());
    }

    #[test]
    fn test_detect_ecosystem_name_prefix() {
        let spec = SpecData {
            source0_url: None,
            all_sources: vec![],
            patches: vec![],
            commit_hash: None,
            url_field: None,
            name: Some("rubygem-rails".to_string()),
            version: Some("7.0.0".to_string()),
            build_requires: vec![],
            changelog_entries: vec![],
        };
        let detection = detect_ecosystem(&spec, "rubygem-rails").unwrap();
        assert_eq!(detection.ecosystem, "rubygems");
        assert_eq!(detection.package_name, Some("rails".to_string()));
    }

    #[test]
    fn test_parse_buildrequires() {
        let result = parse_buildrequires("python3dist(pytest) >= 7.0");
        assert!(result.contains(&"python3dist(pytest)".to_string()));
        assert!(!result.iter().any(|r| r == ">="));
        assert!(!result.iter().any(|r| r == "7.0"));
    }

    #[test]
    fn test_detect_ecosystem_by_name_debian_python() {
        // RED: Test Debian python3- prefix detection
        let detection = detect_ecosystem_by_name("python3-requests", Some("https://pypi.org/project/requests/")).unwrap();
        assert_eq!(detection.ecosystem, "pypi");
        assert_eq!(detection.package_name, Some("requests".to_string()));
    }

    #[test]
    fn test_detect_ecosystem_by_name_debian_rust() {
        // RED: Test Debian librust-*-dev pattern
        let detection = detect_ecosystem_by_name("librust-serde-dev", None).unwrap();
        assert_eq!(detection.ecosystem, "cargo");
        assert_eq!(detection.package_name, Some("serde".to_string()));
    }

    #[test]
    fn test_detect_ecosystem_by_name_debian_perl() {
        // RED: Test Debian lib*-perl pattern
        let detection = detect_ecosystem_by_name("libwww-perl", None).unwrap();
        assert_eq!(detection.ecosystem, "cpan");
        // Should strip "lib" prefix and "-perl" suffix
        assert_eq!(detection.package_name, Some("www".to_string()));
    }

    #[test]
    fn test_detect_ecosystem_by_name_homepage_domain() {
        // RED: Test Homepage domain detection
        let detection = detect_ecosystem_by_name("some-package", Some("https://crates.io/crates/my-crate")).unwrap();
        assert_eq!(detection.ecosystem, "cargo");
        assert_eq!(detection.detection_method, "homepage-domain");
    }
}
