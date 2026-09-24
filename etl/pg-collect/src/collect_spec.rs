//! RPM spec file collector — fetches spec files from dist-git and extracts
//! upstream repository URLs, commit hashes, and ecosystem correlations.
//!
//! Supports Fedora (src.fedoraproject.org) and CentOS Stream (gitlab.com/redhat)
//! dist-git instances. Source0 URLs are routed through forge.rs for repository
//! extraction. Ecosystem correlation uses Source0 domain matching and
//! BuildRequires macro detection.

use crate::fetch_error::FetchError;
use crate::forge::{emit_dq_issue, emit_forge_triples, extract_forge_url_with_field};
use crate::http_transport::HttpTransport;
use crate::ntriples::NTriplesWriter;
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::io::{Result, Write};

/// Bump this for any change to this stage's emitted triples, including
/// changes to shared serialization or ontology helpers it calls. A stale
/// checkpoint fragment is indistinguishable from a correct one -- there is
/// no automatic detection. Emission changes and a bump belong in the same
/// patch.
pub const SPEC_SCHEMA_VERSION: &str = "spec-v1";

/// Regex for extracting Source0 URL from spec file.
static SOURCE0_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?im)^Source0?\s*:\s*(.+)$").unwrap());

/// Regex for extracting all Source* entries from spec file.
static ALL_SOURCES_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?im)^Source\d*\s*:\s*(.+)$").unwrap());

/// Regex for extracting all Patch* entries from spec file.
static PATCHES_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?im)^Patch\d*\s*:\s*(.+)$").unwrap());

/// Regex for extracting %global commit or %global githash macros.
static COMMIT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^%(?:global|define)\s+(?:commit|githash|gitcommit)\s+([0-9a-f]{7,40})")
        .unwrap()
});

/// Regex for extracting URL: field from spec header.
static URL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?im)^URL\s*:\s*(.+)$").unwrap());

/// Regex for extracting Name: field.
static NAME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?im)^Name\s*:\s*(.+)$").unwrap());

/// Regex for extracting Version: field.
static VERSION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?im)^Version\s*:\s*(.+)$").unwrap());

/// Regex for BuildRequires lines.
static BUILDREQUIRES_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?im)^BuildRequires\s*:\s*(.+)$").unwrap());

/// Regex for python3dist(X) macro.
static PYTHON3DIST_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"python3dist\(([^)]+)\)").unwrap());

/// Regex for perl(Module::Name) macro.
static PERL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"perl\(([^)]+)\)").unwrap());

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

/// Outcome of trying every candidate spec URL for one SRPM.
///
/// The distinction matters for checkpointing: `NotFound` is a deterministic
/// answer about this SRPM and may be checkpointed, while `RetryableFailure`
/// is operationally inconclusive and must not be.
pub enum SpecFetchResult {
    Found(String),
    /// Every candidate URL returned a definitive 404.
    NotFound,
    /// At least one candidate failed inconclusively (transport error, 5xx,
    /// malformed) and none succeeded.
    RetryableFailure(String),
}

/// Whether a derived item may be checkpointed.
///
/// A named type rather than a `bool`: the design's rule is an *explicit typed
/// outcome*, and `(usize, bool)` makes an inverted argument silently compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cacheability {
    Complete,
    Retryable,
}

/// What one candidate URL produced.
pub enum CandidateOutcome {
    Found(String),
    /// A definitive 404.
    Missing,
    /// Anything else: transport error, 5xx, malformed body.
    Inconclusive(String),
}

/// Reduce per-candidate outcomes to one answer for this SRPM.
///
/// A definitive `NotFound` requires EVERY candidate to have 404'd: one
/// inconclusive candidate means we cannot claim this SRPM has no spec, which
/// is exactly the distinction checkpointing depends on.
pub fn aggregate_candidates(
    source_name: &str,
    url_count: usize,
    outcomes: Vec<CandidateOutcome>,
) -> SpecFetchResult {
    let mut inconclusive: Option<String> = None;
    for o in outcomes {
        match o {
            CandidateOutcome::Found(c) => return SpecFetchResult::Found(c),
            CandidateOutcome::Missing => continue,
            CandidateOutcome::Inconclusive(d) => inconclusive = Some(d),
        }
    }
    match inconclusive {
        Some(detail) => SpecFetchResult::RetryableFailure(format!(
            "spec fetch inconclusive for {} across {} URLs: {}",
            source_name, url_count, detail
        )),
        None => SpecFetchResult::NotFound,
    }
}

/// Replace `url`'s scheme and authority with `base`, keeping its path.
///
/// Each distro's dist-git layout differs, so the override names only the
/// origin and the per-distro path shape is preserved. Used by
/// `PG_COLLECT_DIST_GIT_BASE` to point spec fetches at a mirror, and by the
/// wrapper rehearsal to serve specs from a local fixture rather than reaching
/// src.fedoraproject.org.
fn rewrite_origin(url: &str, base: &str) -> String {
    let path = url
        .find("://")
        .and_then(|i| url[i + 3..].find('/').map(|j| &url[i + 3 + j..]))
        .unwrap_or("/");
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// A parsed %changelog entry.
pub struct ChangelogEntry {
    pub date: String,
    pub name: String,
    pub email: String,
}

/// Spec file collector for Fedora/CentOS Stream dist-git.
pub struct SpecCollector {
    transport: HttpTransport,
    distro: String,
    release: String,
    cache: Option<SourceCache>,
    /// Origin to fetch specs from instead of the distro's public dist-git.
    dist_git_base: Option<String>,
}

impl SpecCollector {
    pub fn new(distro: &str, release: &str, cache_dir: Option<&str>) -> Result<Self> {
        let cache = match cache_dir {
            Some(dir) => Some(SourceCache::new(dir, "spec")?),
            None => None,
        };

        Ok(Self {
            transport: HttpTransport::new(),
            distro: distro.to_string(),
            release: release.to_string(),
            cache,
            // Not part of the checkpoint context: the override names where a
            // spec is fetched from, not which spec. Pointing at a mirror of
            // the same dist-git must not invalidate fragments.
            dist_git_base: std::env::var("PG_COLLECT_DIST_GIT_BASE")
                .ok()
                .filter(|v| !v.is_empty()),
        })
    }

    /// Collect spec file data for a set of SRPM source names.
    /// identity_map maps source name → list of PackageIdentity URIs for upstream linking.
    pub fn collect<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        srpm_names: &HashSet<String>,
        srpm_identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
    ) -> Result<(usize, usize)> {
        self.collect_checkpointed(
            writer, srpm_names, srpm_identity_map, existing_ecosystem_pkgs,
            emit_buildrequires, emit_maintainers,
            &crate::output_cache::OutputCache::disabled(),
        )
        .map(|(specs, triples, _report)| (specs, triples))
    }

    /// As `collect`, but replays per-item fragments from `checkpoint` so an
    /// interrupted run resumes instead of restarting.
    #[allow(clippy::too_many_arguments)]
    pub fn collect_checkpointed<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        srpm_names: &HashSet<String>,
        srpm_identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
        checkpoint: &crate::output_cache::OutputCache,
    ) -> Result<(usize, usize, crate::stage_report::StageReport)> {
        use crate::output_cache::{CachedOutput, CanonicalContext, ComputeOutcome};

        let mut total_specs = 0;
        let mut total_triples = 0;
        // Spec derivation is an explicitly optional stage: a dist-git that
        // will not answer costs its packages' spec-derived triples, not the
        // run. What the published graph must carry is which of the two
        // happened (#70).
        let mut report = crate::stage_report::StageReport::new("spec", false);
        // Set by the closure below on a checkpoint MISS. Read after
        // `get_or_compute` rather than from `checkpoint.stats()`, so the
        // count is right even when checkpointing is disabled -- a run whose
        // checkpoint setup failed still publishes a graph.
        let inconclusive = std::cell::Cell::new(false);

        // Sorted, not HashSet order. Two processes iterate a HashSet in
        // different randomized orders, which would make "a replayed run is
        // byte-identical" untestable and, worse, produce gratuitously
        // different .nt files between runs. Sorting makes byte identity a
        // real contract.
        let mut ordered: Vec<&String> = srpm_names.iter().collect();
        ordered.sort();

        for (idx, name) in ordered.into_iter().enumerate() {
            let identities = srpm_identity_map.get(name).cloned().unwrap_or_default();
            let ctx = CanonicalContext::new()
                .field("distro", &self.distro)
                .field("release", &self.release)
                .list("identities", &identities)
                .flag("in_existing_ecosystem", existing_ecosystem_pkgs.contains(name))
                .flag("emit_buildrequires", emit_buildrequires)
                .flag("emit_maintainers", emit_maintainers);

            report.attempted += 1;
            inconclusive.set(false);
            let result = checkpoint.get_or_compute(name, &ctx, || {
                // Derive into a scratch writer with no graph term, so the
                // fragment is canonical N-Triples and graph selection stays
                // at replay time.
                let mut scratch = NTriplesWriter::new(Vec::<u8>::new());
                let (logical, cacheability) = self.process_spec_classified(
                    &mut scratch, name, srpm_identity_map, existing_ecosystem_pkgs,
                    emit_buildrequires, emit_maintainers,
                )?;
                let out = CachedOutput {
                    logical_triples: logical,
                    skipped_invalid_iri: scratch.skipped_invalid_iri,
                    auto_inverses: scratch.auto_inverses,
                    text: scratch.into_string()?,
                };
                inconclusive.set(matches!(cacheability, Cacheability::Retryable));
                Ok(match cacheability {
                    Cacheability::Complete => ComputeOutcome::Complete(out),
                    Cacheability::Retryable => ComputeOutcome::Retryable(out),
                })
            });

            match result {
                Ok(out) => {
                    for line in out.text.lines() {
                        writer.write_raw_line(line)?;
                    }
                    writer.skipped_invalid_iri += out.skipped_invalid_iri;
                    writer.auto_inverses += out.auto_inverses;
                    if inconclusive.get() {
                        report.retryable += 1;
                    } else {
                        // A definitive "no spec for this package" is a
                        // conclusive answer, so a zero-triple item completed.
                        report.completed += 1;
                    }
                    if out.logical_triples > 0 {
                        total_specs += 1;
                        total_triples += out.logical_triples;
                    }
                }
                Err(e) => {
                    // Previously printed and then forgotten.
                    report.failed += 1;
                    eprintln!("  {} → error: {}", name, e);
                }
            }

            if (idx + 1) % 100 == 0 {
                eprintln!(
                    "Processed {}/{} spec files ({} triples)",
                    idx + 1,
                    srpm_names.len(),
                    total_triples
                );
            }
        }

        let s = checkpoint.stats();
        eprintln!(
            "Spec collection complete: {} specs, {} triples \
             (checkpoint: {} hits, {} misses, {} retryable, {} write-fail, {} integrity-fail)",
            total_specs, total_triples,
            s.hits, s.misses, s.retryable, s.write_failures, s.integrity_failures
        );
        Ok((total_specs, total_triples, report))
    }

    /// Derivation plus an explicit cacheability verdict.
    ///
    /// Cacheability is NOT "did we emit a DQ issue" -- a successful ecosystem
    /// detection deliberately emits one (see the DQ call after detection in
    /// this file). Only an inconclusive *fetch* is non-cacheable; a definitive
    /// 404 is a real answer about this SRPM and may be checkpointed.
    #[allow(clippy::too_many_arguments)]
    fn process_spec_classified<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        source_name: &str,
        identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
    ) -> Result<(usize, Cacheability)> {
        let content = match self.fetch_spec(source_name) {
            SpecFetchResult::Found(c) => c,
            SpecFetchResult::NotFound => {
                eprintln!("  {} → no spec in dist-git", source_name);
                let t = emit_dq_issue(
                    writer, "collect-spec", "spec-file", source_name,
                    "spec-fetch-failed", "warning",
                )?;
                return Ok((t, Cacheability::Complete)); // definitive answer
            }
            SpecFetchResult::RetryableFailure(detail) => {
                eprintln!("  {} → spec fetch inconclusive: {}", source_name, detail);
                let t = emit_dq_issue(
                    writer, "collect-spec", "spec-file", source_name,
                    "spec-fetch-failed", "warning",
                )?;
                return Ok((t, Cacheability::Retryable));
            }
        };
        let triples = self.process_spec_with_content(
            writer, source_name, &content, identity_map,
            existing_ecosystem_pkgs, emit_buildrequires, emit_maintainers,
        )?;
        Ok((triples, Cacheability::Complete))
    }

    /// Derivation only -- the caller has already fetched the spec. Split out so
    /// the checkpointed path can classify the fetch and derive from its result
    /// without issuing a second request.
    #[allow(clippy::too_many_arguments)]
    fn process_spec_with_content<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        source_name: &str,
        spec_content: &str,
        identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
    ) -> Result<usize> {
        let spec = parse_spec(spec_content);
        let mut triples = 0;

        // Get identity URIs for this SRPM
        let identity_uris = identity_map.get(source_name).cloned().unwrap_or_default();

        // --- Source0 → upstream repository ---
        if let Some(ref source0) = spec.source0_url {
            // Expand simple macros
            let expanded = expand_macros(source0, &spec);

            // DQ: check for unexpanded macros
            if expanded.contains("%{") || expanded.contains("%(") {
                triples += emit_dq_issue(
                    writer,
                    "collect-spec",
                    "source0",
                    &expanded,
                    "unexpanded-macro",
                    "info",
                )?;
            }

            if let Some(extraction) = extract_forge_url_with_field(&expanded, "source0") {
                let r_uri = repo_uri(&extraction.repo_url);

                // Emit upstreamRepository on each PackageIdentity
                for identity_uri in &identity_uris {
                    writer.write_triple(
                        identity_uri,
                        &format!("{PKG}upstreamRepository"),
                        &r_uri,
                    )?;
                    triples += 1;
                }

                // Emit forge triples for the repository
                triples += emit_forge_triples(writer, &r_uri, &extraction.repo_url)?;
            } else if !expanded.contains("%{") {
                // DQ: Source0 present but not a recognizable forge URL
                triples += emit_dq_issue(
                    writer,
                    "collect-spec",
                    "source0",
                    &expanded,
                    "no-forge-match",
                    "info",
                )?;
            }
        } else {
            // DQ: no Source0 in spec file
            triples += emit_dq_issue(
                writer,
                "collect-spec",
                "source0",
                source_name,
                "missing-source0",
                "info",
            )?;
        }

        // --- All Source*: entries → rpm:hasSpecSource ---
        let source_uri = source_uri(
            &self.distro,
            &self.release,
            source_name,
            &spec.version.as_deref().unwrap_or("unknown"),
        );
        for source_url in &spec.all_sources {
            writer.write_literal(&source_uri, &format!("{RPM}hasSpecSource"), source_url)?;
            triples += 1;
        }

        // --- All Patch*: entries → rpm:hasPatch → rpm:Patch entities ---
        for (idx, patch_url) in spec.patches.iter().enumerate() {
            // Extract patch filename from URL (may contain macros)
            let patch_name = patch_url.split('/').last().unwrap_or(patch_url);

            let patch_uri = format!(
                "{DATA}patch/{}/{}/{}/{}-{}",
                crate::uris::encode(&self.distro),
                crate::uris::encode(&self.release),
                crate::uris::encode(source_name),
                idx,
                crate::uris::encode(patch_name)
            );

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

    fn fetch_spec(&self, source_name: &str) -> SpecFetchResult {
        let urls = self.spec_urls(source_name);
        // Stop at the first success, exactly as before. Eagerly mapping every
        // candidate would triple spec-fetch traffic (three URLs per SRPM,
        // tens of thousands of SRPMs per run).
        let mut outcomes = Vec::with_capacity(urls.len());
        for url in &urls {
            let outcome = match self.fetch_url(url, source_name) {
                Ok(content) => CandidateOutcome::Found(content),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => CandidateOutcome::Missing,
                Err(e) => CandidateOutcome::Inconclusive(format!("{} ({})", url, e)),
            };
            let done = matches!(outcome, CandidateOutcome::Found(_));
            outcomes.push(outcome);
            if done {
                break;
            }
        }
        aggregate_candidates(source_name, urls.len(), outcomes)
    }

    fn spec_urls(&self, source_name: &str) -> Vec<String> {
        let urls = self.public_spec_urls(source_name);
        // An unsupported distro yields no candidates, and the override must
        // not invent any: that is what keeps offline tests offline.
        match &self.dist_git_base {
            Some(base) => urls.iter().map(|u| rewrite_origin(u, base)).collect(),
            None => urls,
        }
    }

    fn public_spec_urls(&self, source_name: &str) -> Vec<String> {
        match self.distro.as_str() {
            "fedora" => {
                let branch = if self.release == "rawhide" {
                    "rawhide".to_string()
                } else {
                    format!("f{}", self.release)
                };
                vec![
                    format!(
                        "https://src.fedoraproject.org/rpms/{}/raw/{}/f/{}.spec",
                        source_name, branch, source_name
                    ),
                    format!(
                        "https://src.fedoraproject.org/rpms/{}/raw/rawhide/f/{}.spec",
                        source_name, source_name
                    ),
                    format!(
                        "https://src.fedoraproject.org/rpms/{}/raw/main/f/{}.spec",
                        source_name, source_name
                    ),
                ]
            }
            "centos-stream" => {
                let branch = format!("c{}s", self.release);
                vec![
                    // CentOS Stream dist-git: spec file at repo root, not SPECS/
                    format!(
                        "https://gitlab.com/redhat/centos-stream/rpms/{}/-/raw/{}/{}.spec",
                        source_name, branch, source_name
                    ),
                    format!(
                        "https://gitlab.com/redhat/centos-stream/rpms/{}/-/raw/main/{}.spec",
                        source_name, source_name
                    ),
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
                CacheResult::Fresh(bytes) => {
                    return String::from_utf8(bytes)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                }
                CacheResult::Cached(path) | CacheResult::NotModified(path) => {
                    return std::fs::read_to_string(&path);
                }
            }
        }

        // Direct download. Only a real 404 maps to NotFound; every other
        // failure keeps its own kind so the caller can tell "this branch
        // has no spec" from "the server was unhappy".
        match self.transport.get(url, None) {
            Ok(resp) => String::from_utf8(resp.bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(FetchError::NotFound { .. }) => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no spec at {}", url),
            )),
            Err(e) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("{} for {}", e, url),
            )),
        }
    }

    fn emit_ecosystem_triples<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
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
                writer.write_triple(
                    identity_uri,
                    &format!("{PKG}upstreamEcosystem"),
                    &ecosystem_uri,
                )?;
                triples += 1;

                if let Some(ref pkg_name) = detection.package_name {
                    writer.write_literal(
                        identity_uri,
                        &format!("{PKG}upstreamPackageName"),
                        pkg_name,
                    )?;
                    triples += 1;
                }
            }

            // DQ: record detection method and confidence
            let confidence = match detection.detection_method {
                "source0-domain" | "buildrequires-macro" => "high",
                "name-prefix" => "medium",
                _ => "low",
            };
            triples += emit_dq_issue(
                writer,
                "collect-spec",
                "ecosystem-detection",
                &format!(
                    "{}:{} via {}",
                    detection.ecosystem, source_name, detection.detection_method
                ),
                &format!("ecosystem-detected-{}", confidence),
                "info",
            )?;
        }

        Ok(triples)
    }

    fn emit_buildrequires_triples<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        spec: &SpecData,
        source_name: &str,
    ) -> Result<usize> {
        let mut triples = 0;
        let src_uri = source_uri(
            &self.distro,
            &self.release,
            source_name,
            &spec.version.as_deref().unwrap_or("unknown"),
        );

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

    fn emit_changelog_triples<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        spec: &SpecData,
        source_name: &str,
    ) -> Result<usize> {
        let mut triples = 0;
        let src_uri = source_uri(
            &self.distro,
            &self.release,
            source_name,
            &spec.version.as_deref().unwrap_or("unknown"),
        );

        // Emit build attribution from latest changelog entry
        if let Some(entry) = spec.changelog_entries.first() {
            if let Some(person_uri) = person_uri_from_email(&entry.email) {
                let email = normalize_email(&entry.email);
                writer.write_triple(&person_uri, RDF_TYPE, &format!("{PKG}Person"))?;
                writer.write_literal(&person_uri, &format!("{FOAF}name"), &entry.name)?;
                writer.write_literal(
                    &person_uri,
                    &format!("{FOAF}mbox"),
                    &format!("mailto:{}", email),
                )?;
                writer.write_triple(&src_uri, &format!("{PROV}wasAttributedTo"), &person_uri)?;
                triples += 4;

                // DQ: track that we normalized an obfuscated email
                if entry.email != email {
                    triples += emit_dq_issue(
                        writer,
                        "spec-changelog",
                        "obfuscated-email",
                        &entry.email,
                        "email-normalized",
                        "info",
                    )?;
                }
            } else {
                // DQ: email could not be normalized — data loss
                triples += emit_dq_issue(
                    writer,
                    "spec-changelog",
                    "invalid-email",
                    &entry.email,
                    "email-unparseable",
                    "warning",
                )?;
            }
        }

        Ok(triples)
    }
}

// ─── Parsing functions ──────────────────────────────────────────────────

/// Parse a spec file into structured data.
pub fn parse_spec(content: &str) -> SpecData {
    let source0_url = SOURCE0_RE
        .captures(content)
        .map(|c| c[1].trim().to_string());

    let commit_hash = COMMIT_RE.captures(content).map(|c| c[1].trim().to_string());

    let url_field = URL_RE.captures(content).map(|c| c[1].trim().to_string());

    let name = NAME_RE.captures(content).map(|c| c[1].trim().to_string());

    let version = VERSION_RE
        .captures(content)
        .map(|c| c[1].trim().to_string());

    let build_requires: Vec<String> = BUILDREQUIRES_RE
        .captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let all_sources: Vec<String> = ALL_SOURCES_RE
        .captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let patches: Vec<String> = PATCHES_RE
        .captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let changelog_entries: Vec<ChangelogEntry> = CHANGELOG_ENTRY_RE
        .captures_iter(content)
        .map(|c| ChangelogEntry {
            date: c[1].to_string(),
            name: c[2].to_string(),
            email: c[3].to_string(),
        })
        .collect();

    SpecData {
        source0_url,
        all_sources,
        patches,
        commit_hash,
        url_field,
        name,
        version,
        build_requires,
        changelog_entries,
    }
}

/// Expand well-known RPM macros in a URL string.
fn expand_macros(url: &str, spec: &SpecData) -> String {
    let mut result = url.to_string();
    if let Some(ref name) = spec.name {
        result = result.replace("%{name}", name).replace("%name", name);
    }
    if let Some(ref version) = spec.version {
        result = result
            .replace("%{version}", version)
            .replace("%version", version);
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
/// A `golang-` prefix establishes the ecosystem and nothing more.
///
/// A Go module path is slash-separated (`github.com/spf13/cobra`), and both
/// Fedora and Debian flatten `/` and `.` to `-` when they name the package.
/// That flattening is not reversible by string manipulation:
/// `github-spf13-cobra` could be `github.com/spf13/cobra` or
/// `github-spf13.com/cobra`. Stripping the prefix would only produce a
/// differently-unusable name, and `seed.rs` hands these straight to the Go
/// module collector (#42).
///
/// The import path is carried by evidence that actually knows it: a
/// `golang(...)` RPM Provides, read by `rpm::emit_ecosystem_triples`, and
/// Debian's `Go-Import-Path`, read by `collect_sources`. Where neither is
/// present the ecosystem is all we know, and recording only that is the
/// honest answer.
const GO_ECOSYSTEM_ONLY: EcosystemDetection = EcosystemDetection {
    ecosystem: "gomod",
    package_name: None,
    detection_method: "name-prefix",
};

pub fn detect_ecosystem(spec: &SpecData, source_name: &str) -> Option<EcosystemDetection> {
    // Strategy 1: Source0 URL domain (highest confidence)
    if let Some(ref source0) = spec.source0_url {
        let expanded = expand_macros(source0, spec);
        let lower = expanded.to_lowercase();

        if lower.contains("files.pythonhosted.org")
            || lower.contains("pypi.org")
            || lower.contains("pypi.io")
        {
            let pkg = strip_ecosystem_prefix(source_name, &["python3-", "python-"]);
            return Some(EcosystemDetection {
                ecosystem: "pypi",
                package_name: Some(pkg),
                detection_method: "source0-domain",
            });
        }
        if lower.contains("rubygems.org") {
            let pkg = strip_ecosystem_prefix(source_name, &["rubygem-"]);
            return Some(EcosystemDetection {
                ecosystem: "rubygems",
                package_name: Some(pkg),
                detection_method: "source0-domain",
            });
        }
        if lower.contains("crates.io") || lower.contains("static.crates.io") {
            let pkg = strip_ecosystem_prefix(source_name, &["rust-"]);
            return Some(EcosystemDetection {
                ecosystem: "cargo",
                package_name: Some(pkg),
                detection_method: "source0-domain",
            });
        }
        if lower.contains("registry.npmjs.org") || lower.contains("npmjs.com") {
            let pkg = strip_ecosystem_prefix(source_name, &["nodejs-", "npm-"]);
            return Some(EcosystemDetection {
                ecosystem: "npm",
                package_name: Some(pkg),
                detection_method: "source0-domain",
            });
        }
        if lower.contains("cpan.metacpan.org")
            || lower.contains("search.cpan.org")
            || lower.contains("cpan.org")
        {
            let pkg = strip_ecosystem_prefix(source_name, &["perl-"]);
            return Some(EcosystemDetection {
                ecosystem: "cpan",
                package_name: Some(pkg),
                detection_method: "source0-domain",
            });
        }
        if lower.contains("hackage.haskell.org") {
            let pkg = strip_ecosystem_prefix(source_name, &["ghc-"]);
            return Some(EcosystemDetection {
                ecosystem: "hackage",
                package_name: Some(pkg),
                detection_method: "source0-domain",
            });
        }
        if lower.contains("hex.pm") {
            return Some(EcosystemDetection {
                ecosystem: "hex",
                package_name: Some(source_name.to_string()),
                detection_method: "source0-domain",
            });
        }
    }

    // Strategy 2: BuildRequires macros (high confidence)
    for br in &spec.build_requires {
        if PYTHON3DIST_RE.is_match(br) {
            let pkg = strip_ecosystem_prefix(source_name, &["python3-", "python-"]);
            return Some(EcosystemDetection {
                ecosystem: "pypi",
                package_name: Some(pkg),
                detection_method: "buildrequires-macro",
            });
        }
        if PERL_RE.is_match(br) {
            let pkg = strip_ecosystem_prefix(source_name, &["perl-"]);
            return Some(EcosystemDetection {
                ecosystem: "cpan",
                package_name: Some(pkg),
                detection_method: "buildrequires-macro",
            });
        }
    }

    // Strategy 3: Package name prefix (medium confidence, fallback)
    if source_name.starts_with("python3-") || source_name.starts_with("python-") {
        let pkg = strip_ecosystem_prefix(source_name, &["python3-", "python-"]);
        return Some(EcosystemDetection {
            ecosystem: "pypi",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }
    if source_name.starts_with("perl-") {
        let pkg = strip_ecosystem_prefix(source_name, &["perl-"]);
        return Some(EcosystemDetection {
            ecosystem: "cpan",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }
    if source_name.starts_with("rubygem-") {
        let pkg = strip_ecosystem_prefix(source_name, &["rubygem-"]);
        return Some(EcosystemDetection {
            ecosystem: "rubygems",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }
    if source_name.starts_with("rust-") {
        let pkg = strip_ecosystem_prefix(source_name, &["rust-"]);
        return Some(EcosystemDetection {
            ecosystem: "cargo",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }
    if source_name.starts_with("ghc-") {
        let pkg = strip_ecosystem_prefix(source_name, &["ghc-"]);
        return Some(EcosystemDetection {
            ecosystem: "hackage",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }
    if source_name.starts_with("golang-") {
        return Some(GO_ECOSYSTEM_ONLY);
    }
    if source_name.starts_with("nodejs-") {
        let pkg = strip_ecosystem_prefix(source_name, &["nodejs-"]);
        return Some(EcosystemDetection {
            ecosystem: "npm",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }

    None
}

/// Detect upstream ecosystem from package name and optional homepage URL.
/// Designed for Debian packages which don't have BuildRequires/Source0 metadata.
///
/// Strategy priority:
/// 1. Homepage domain (if provided) - high confidence
/// 2. Package name prefix - medium confidence
pub fn detect_ecosystem_by_name(
    package_name: &str,
    homepage_url: Option<&str>,
) -> Option<EcosystemDetection> {
    // Strategy 1: Homepage domain (highest confidence when available)
    if let Some(homepage) = homepage_url {
        let lower = homepage.to_lowercase();

        if lower.contains("files.pythonhosted.org")
            || lower.contains("pypi.org")
            || lower.contains("pypi.io")
        {
            let pkg = strip_ecosystem_prefix(package_name, &["python3-", "python-"]);
            return Some(EcosystemDetection {
                ecosystem: "pypi",
                package_name: Some(pkg),
                detection_method: "homepage-domain",
            });
        }
        if lower.contains("rubygems.org") {
            let pkg = strip_ecosystem_prefix(package_name, &["rubygem-", "ruby-"]);
            return Some(EcosystemDetection {
                ecosystem: "rubygems",
                package_name: Some(pkg),
                detection_method: "homepage-domain",
            });
        }
        if lower.contains("crates.io") || lower.contains("static.crates.io") {
            let pkg = normalize_librust_crate_name(&strip_ecosystem_prefix(
                package_name,
                &["rust-", "librust-"],
            ));
            return Some(EcosystemDetection {
                ecosystem: "cargo",
                package_name: Some(pkg),
                detection_method: "homepage-domain",
            });
        }
        if lower.contains("registry.npmjs.org") || lower.contains("npmjs.com") {
            let pkg = strip_ecosystem_prefix(package_name, &["nodejs-", "npm-", "node-"]);
            return Some(EcosystemDetection {
                ecosystem: "npm",
                package_name: Some(pkg),
                detection_method: "homepage-domain",
            });
        }
        if lower.contains("cpan.metacpan.org")
            || lower.contains("search.cpan.org")
            || lower.contains("cpan.org")
        {
            let pkg = strip_perl_packaging(package_name);
            return Some(EcosystemDetection {
                ecosystem: "cpan",
                package_name: Some(pkg),
                detection_method: "homepage-domain",
            });
        }
        if lower.contains("hackage.haskell.org") {
            let pkg = strip_ecosystem_prefix(package_name, &["ghc-", "libghc-"]);
            return Some(EcosystemDetection {
                ecosystem: "hackage",
                package_name: Some(pkg),
                detection_method: "homepage-domain",
            });
        }
        if lower.contains("hex.pm") {
            return Some(EcosystemDetection {
                ecosystem: "hex",
                package_name: Some(package_name.to_string()),
                detection_method: "homepage-domain",
            });
        }
        if lower.contains("cran.r-project.org") {
            let pkg = strip_ecosystem_prefix(package_name, &["r-cran-"]);
            return Some(EcosystemDetection {
                ecosystem: "cran",
                package_name: Some(pkg),
                detection_method: "homepage-domain",
            });
        }
        if lower.contains("bioconductor.org") {
            let pkg = strip_ecosystem_prefix(package_name, &["r-bioc-"]);
            return Some(EcosystemDetection {
                ecosystem: "bioconductor",
                package_name: Some(pkg),
                detection_method: "homepage-domain",
            });
        }
    }

    // Strategy 2: Debian package name prefixes
    // Python: python3- or python-
    if package_name.starts_with("python3-") || package_name.starts_with("python-") {
        let pkg = strip_ecosystem_prefix(package_name, &["python3-", "python-"]);
        return Some(EcosystemDetection {
            ecosystem: "pypi",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }

    // Perl: lib*-perl pattern
    if package_name.ends_with("-perl") && package_name.starts_with("lib") {
        return Some(EcosystemDetection {
            ecosystem: "cpan",
            package_name: Some(strip_perl_packaging(package_name)),
            detection_method: "name-prefix",
        });
    }

    // Ruby: ruby-
    if package_name.starts_with("ruby-") {
        let pkg = strip_ecosystem_prefix(package_name, &["ruby-"]);
        return Some(EcosystemDetection {
            ecosystem: "rubygems",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }

    // Rust: librust-*-dev pattern
    if package_name.starts_with("librust-") && package_name.ends_with("-dev") {
        // librust-serde-dev → serde
        let without_lib = package_name
            .strip_prefix("librust-")
            .unwrap_or(package_name);
        let without_suffix = without_lib.strip_suffix("-dev").unwrap_or(without_lib);
        return Some(EcosystemDetection {
            ecosystem: "cargo",
            package_name: Some(normalize_librust_crate_name(without_suffix)),
            detection_method: "name-prefix",
        });
    }

    // Node.js: node-
    if package_name.starts_with("node-") {
        let pkg = strip_ecosystem_prefix(package_name, &["node-"]);
        return Some(EcosystemDetection {
            ecosystem: "npm",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }

    // Go: golang-
    if package_name.starts_with("golang-") {
        return Some(GO_ECOSYSTEM_ONLY);
    }

    // R CRAN: r-cran-
    if package_name.starts_with("r-cran-") {
        let pkg = strip_ecosystem_prefix(package_name, &["r-cran-"]);
        return Some(EcosystemDetection {
            ecosystem: "cran",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }

    // R Bioconductor: r-bioc-
    if package_name.starts_with("r-bioc-") {
        let pkg = strip_ecosystem_prefix(package_name, &["r-bioc-"]);
        return Some(EcosystemDetection {
            ecosystem: "bioconductor",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }

    // Haskell: libghc-*-dev or ghc-
    if package_name.starts_with("libghc-") && package_name.ends_with("-dev") {
        // libghc-aeson-dev → aeson
        let without_lib = package_name.strip_prefix("libghc-").unwrap_or(package_name);
        let without_suffix = without_lib.strip_suffix("-dev").unwrap_or(without_lib);
        return Some(EcosystemDetection {
            ecosystem: "hackage",
            package_name: Some(without_suffix.to_string()),
            detection_method: "name-prefix",
        });
    }
    if package_name.starts_with("ghc-") {
        let pkg = strip_ecosystem_prefix(package_name, &["ghc-"]);
        return Some(EcosystemDetection {
            ecosystem: "hackage",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }

    // Emacs Lisp: elpa-
    if package_name.starts_with("elpa-") {
        let pkg = strip_ecosystem_prefix(package_name, &["elpa-"]);
        return Some(EcosystemDetection {
            ecosystem: "elpa",
            package_name: Some(pkg),
            detection_method: "name-prefix",
        });
    }

    None
}

/// A packaging prefix has to end in a separator to be one.
///
/// `python3-` is a convention; `py` is two letters that a great many upstream
/// names begin with. Only the first can be stripped without reading the rest
/// of the name.
fn is_anchored_prefix(prefix: &str) -> bool {
    prefix.ends_with(['-', '_', '.'])
}

/// Strip a known packaging prefix from a distro package name, leaving the
/// upstream package name.
///
/// Unanchored prefixes are ignored. A bare `py` matched every name beginning
/// with those two letters, so `pytest` was emitted as `test` and `pyyaml` as
/// `yaml` (#40). That is not merely a wrong string: `seed.rs` reads
/// `upstreamPackageName` back to decide what the registry collectors fetch,
/// so a truncated name makes us collect whatever unrelated project happens to
/// own it and attach that project's provenance here.
///
/// The longest match wins, so a caller's list need not be ordered, and a
/// prefix that would consume the whole name is declined -- an empty upstream
/// name is never an improvement on the one we were given.
fn strip_ecosystem_prefix(source_name: &str, prefixes: &[&str]) -> String {
    debug_assert!(
        prefixes.iter().all(|prefix| is_anchored_prefix(prefix)),
        "unanchored ecosystem prefix in {prefixes:?} -- see #40"
    );
    prefixes
        .iter()
        .filter(|prefix| is_anchored_prefix(prefix))
        .filter(|prefix| source_name.starts_with(**prefix))
        .max_by_key(|prefix| prefix.len())
        .map(|prefix| &source_name[prefix.len()..])
        .filter(|upstream| !upstream.is_empty())
        .unwrap_or(source_name)
        .to_string()
}

/// Strip Perl packaging decoration, whichever distribution applied it.
///
/// Fedora names a CPAN distribution `perl-<Dist>`; Debian names it
/// `lib<dist>-perl`. Both tokens are packaging rather than part of the
/// distribution name, but only when the whole shape is present -- stripping a
/// bare `lib` from anything that merely starts with those letters is the #40
/// truncation again.
fn strip_perl_packaging(package_name: &str) -> String {
    if package_name.starts_with("lib") && package_name.ends_with("-perl") {
        let without_lib = package_name.strip_prefix("lib").unwrap_or(package_name);
        let without_suffix = without_lib.strip_suffix("-perl").unwrap_or(without_lib);
        if !without_suffix.is_empty() {
            return without_suffix.to_string();
        }
    }
    strip_ecosystem_prefix(package_name, &["perl-"])
}

/// Strip a `-dev` suffix and a `+<feature>` suffix from a Debian Rust
/// package name fragment, leaving only the real crates.io crate name.
/// Debian packages each enabled-feature combination of a crate as its own
/// binary package, named `librust-<crate>+<feature>-dev`; the crate name
/// is only the part before the first `+` -- a feature name can itself
/// contain `+` (e.g. "c++14"), so this splits on the first occurrence
/// only, never the crate name.
fn normalize_librust_crate_name(name: &str) -> String {
    let without_suffix = name.strip_suffix("-dev").unwrap_or(name);
    without_suffix
        .split_once('+')
        .map(|(crate_name, _feature)| crate_name)
        .unwrap_or(without_suffix)
        .to_string()
}

/// Parse a BuildRequires line into individual package names.
fn parse_buildrequires(line: &str) -> Vec<String> {
    let mut result = Vec::new();
    // Split on commas or spaces, skip version constraints
    for part in line.split(|c: char| c == ',' || c.is_whitespace()) {
        let trimmed = part.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('>')
            || trimmed.starts_with('<')
            || trimmed.starts_with('=')
            || trimmed.parse::<f64>().is_ok()
        {
            continue;
        }
        // Skip macros we can't resolve
        if trimmed.contains("%{") && !trimmed.contains("python3dist") && !trimmed.contains("perl(")
        {
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
        assert!(
            expanded.contains("abc123def456"),
            "Commit macro should be expanded"
        );
        assert!(
            expanded.contains("openssl"),
            "Name macro should be expanded"
        );
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
        let detection = detect_ecosystem_by_name(
            "python3-requests",
            Some("https://pypi.org/project/requests/"),
        )
        .unwrap();
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
    fn test_detect_ecosystem_by_name_debian_rust_feature_variant() {
        // Debian packages each enabled-feature combination of a crate as
        // its own binary package: librust-<crate>+<feature>-dev. The
        // upstream crate name must not include the +feature suffix.
        let detection =
            detect_ecosystem_by_name("librust-adler+compiler-builtins-dev", None).unwrap();
        assert_eq!(detection.ecosystem, "cargo");
        assert_eq!(detection.package_name, Some("adler".to_string()));
    }

    #[test]
    fn test_detect_ecosystem_by_name_debian_rust_feature_variant_with_plus_in_feature() {
        // A feature name can itself contain '+' (e.g. a C++ standard
        // version like "c++14") -- must split on the first '+' only.
        let detection =
            detect_ecosystem_by_name("librust-cxxbridge-flags+c++14-dev", None).unwrap();
        assert_eq!(detection.ecosystem, "cargo");
        assert_eq!(detection.package_name, Some("cxxbridge-flags".to_string()));
    }

    #[test]
    fn test_detect_ecosystem_by_name_debian_rust_feature_variant_via_homepage() {
        // The homepage-domain strategy (checked first, higher confidence)
        // has the same +feature suffix to strip.
        let detection = detect_ecosystem_by_name(
            "librust-adler+compiler-builtins-dev",
            Some("https://crates.io/crates/adler"),
        )
        .unwrap();
        assert_eq!(detection.ecosystem, "cargo");
        assert_eq!(detection.package_name, Some("adler".to_string()));
    }

    #[test]
    fn test_detect_ecosystem_by_name_debian_perl() {
        // RED: Test Debian lib*-perl pattern
        let detection = detect_ecosystem_by_name("libwww-perl", None).unwrap();
        assert_eq!(detection.ecosystem, "cpan");
        // Should strip "lib" prefix and "-perl" suffix
        assert_eq!(detection.package_name, Some("www".to_string()));
    }

    // --- packaging prefixes are stripped, upstream names are not (#40) ---
    //
    // `upstreamPackageName` is not only recorded: seed.rs reads it back to
    // decide what the registry collectors fetch. A name truncated here is a
    // lookup for a different project whose provenance we then attach to this
    // package, so these assert on the emitted name, not on the detection.

    fn pypi_spec() -> SpecData {
        SpecData {
            source0_url: Some("https://files.pythonhosted.org/packages/x.tar.gz".to_string()),
            all_sources: vec![],
            patches: vec![],
            commit_hash: None,
            url_field: None,
            name: None,
            version: None,
            build_requires: vec![],
            changelog_entries: vec![],
        }
    }

    /// A bare "py" prefix made every one of these a different project.
    const NOT_PY_PREFIXED: [&str; 5] = ["pytest", "pyyaml", "pygments", "pyparsing", "py3dns"];

    #[test]
    fn a_name_that_merely_starts_with_py_is_not_truncated_via_source0() {
        let spec = pypi_spec();
        for name in NOT_PY_PREFIXED {
            let detection = detect_ecosystem(&spec, name).unwrap();
            assert_eq!(detection.ecosystem, "pypi");
            assert_eq!(
                detection.package_name,
                Some(name.to_string()),
                "{name} was truncated"
            );
        }
    }

    #[test]
    fn a_name_that_merely_starts_with_py_is_not_truncated_via_homepage() {
        for name in NOT_PY_PREFIXED {
            let detection =
                detect_ecosystem_by_name(name, Some("https://pypi.org/project/x/")).unwrap();
            assert_eq!(
                detection.package_name,
                Some(name.to_string()),
                "{name} was truncated"
            );
        }
    }

    #[test]
    fn real_python_packaging_prefixes_are_still_stripped() {
        // The point of the fix is not to strip less, it is to strip only
        // what is packaging. If these regress the bare prefix was load-bearing.
        let spec = pypi_spec();
        for (packaged, upstream) in [
            ("python3-requests", "requests"),
            ("python-dateutil", "dateutil"),
        ] {
            assert_eq!(
                detect_ecosystem(&spec, packaged).unwrap().package_name,
                Some(upstream.to_string())
            );
            assert_eq!(
                detect_ecosystem_by_name(packaged, Some("https://pypi.org/project/x/"))
                    .unwrap()
                    .package_name,
                Some(upstream.to_string())
            );
        }
    }

    #[test]
    fn a_cpan_homepage_does_not_license_stripping_a_bare_lib() {
        // Anything whose homepage is on cpan.org took this branch, not only
        // packages carrying a Perl packaging prefix.
        let detection =
            detect_ecosystem_by_name("libreoffice", Some("https://metacpan.org/dist/x")).unwrap();
        assert_eq!(detection.ecosystem, "cpan");
        assert_eq!(detection.package_name, Some("libreoffice".to_string()));
    }

    #[test]
    fn both_cpan_strategies_agree_on_one_package() {
        // The homepage strategy is checked first and is meant to be the more
        // confident of the two. Two answers for one package is worse than
        // either answer.
        for name in ["libwww-perl", "libjson-perl", "perl-JSON"] {
            let by_homepage =
                detect_ecosystem_by_name(name, Some("https://metacpan.org/dist/x")).unwrap();
            let by_prefix = detect_ecosystem_by_name(name, None);
            assert_eq!(by_homepage.ecosystem, "cpan");
            if let Some(by_prefix) = by_prefix {
                assert_eq!(
                    by_homepage.package_name, by_prefix.package_name,
                    "{name} gets a different upstream name from each strategy"
                );
            }
        }
    }

    #[test]
    fn perl_packaging_is_stripped_from_both_distro_conventions() {
        assert_eq!(strip_perl_packaging("libjson-perl"), "json");
        assert_eq!(strip_perl_packaging("perl-JSON"), "JSON");
        assert_eq!(strip_perl_packaging("perl-Test-More"), "Test-More");
        // Nothing left over is not a name.
        assert_eq!(strip_perl_packaging("lib-perl"), "lib-perl");
        assert_eq!(strip_perl_packaging("perl-"), "perl-");
    }

    #[test]
    fn the_longest_matching_prefix_wins_regardless_of_order() {
        // Callers list prefixes by hand; first-match-wins made the order
        // load-bearing and silently wrong when it was got wrong.
        // Under first-match-wins this returns "red-dashboard", and nothing
        // says so except the registry lookup that later finds nothing.
        assert_eq!(
            strip_ecosystem_prefix("node-red-dashboard", &["node-", "node-red-"]),
            "dashboard"
        );
        assert_eq!(
            strip_ecosystem_prefix("node-red-dashboard", &["node-red-", "node-"]),
            "dashboard"
        );
    }

    #[test]
    fn a_prefix_that_would_consume_the_whole_name_is_declined() {
        assert_eq!(
            strip_ecosystem_prefix("python3-", &["python3-"]),
            "python3-"
        );
    }

    #[test]
    fn every_prefix_list_in_this_file_is_anchored_to_a_separator() {
        // The debug_assert only sees lists a test actually reaches. A list on
        // an unexercised branch is exactly where the next bare "py" would
        // hide, so check them where they are written. The needle is built by
        // concat! so this scan does not match its own source.
        let source = include_str!("collect_spec.rs");
        let needle = concat!("strip_ecosystem_prefix", "(");
        let mut checked = 0;
        for call in source.split(needle).skip(1) {
            let Some(open) = call.find("&[") else {
                continue;
            };
            let Some(close) = call[open..].find(']') else {
                continue;
            };
            for prefix in call[open..open + close].split('"').skip(1).step_by(2) {
                checked += 1;
                assert!(
                    is_anchored_prefix(prefix),
                    "unanchored prefix {prefix:?}: it truncates every name that \
                     merely begins with those letters (#40)"
                );
            }
        }
        assert!(
            checked >= 25,
            "the scan found only {checked} prefixes -- it has stopped matching \
             the call sites and now proves nothing"
        );
    }

    #[test]
    fn test_detect_ecosystem_by_name_homepage_domain() {
        // RED: Test Homepage domain detection
        let detection =
            detect_ecosystem_by_name("some-package", Some("https://crates.io/crates/my-crate"))
                .unwrap();
        assert_eq!(detection.ecosystem, "cargo");
        assert_eq!(detection.detection_method, "homepage-domain");
    }

    // --- fetch_spec outcome classification ---
    //
    // The aggregation rule, not the variants, is what carries the risk, so
    // these drive the pure reducer. Constructing the enum directly and
    // asserting on it would assert nothing about behavior.

    #[test]
    fn all_404s_aggregate_to_not_found() {
        let outcomes = vec![CandidateOutcome::Missing, CandidateOutcome::Missing];
        assert!(matches!(
            aggregate_candidates("pkg", 2, outcomes),
            SpecFetchResult::NotFound
        ));
    }

    #[test]
    fn one_inconclusive_candidate_poisons_the_aggregate() {
        // The case that matters: a 404 on one branch plus a transport error
        // on another is NOT evidence that this SRPM has no spec. Checkpointing
        // the NotFound would bake a transient blip into the corpus.
        let outcomes = vec![
            CandidateOutcome::Missing,
            CandidateOutcome::Inconclusive("connection reset".into()),
        ];
        assert!(matches!(
            aggregate_candidates("pkg", 2, outcomes),
            SpecFetchResult::RetryableFailure(_)
        ));
    }

    #[test]
    fn a_later_success_wins_over_an_earlier_failure() {
        let outcomes = vec![
            CandidateOutcome::Inconclusive("5xx".into()),
            CandidateOutcome::Found("Name: pkg".into()),
        ];
        match aggregate_candidates("pkg", 2, outcomes) {
            SpecFetchResult::Found(c) => assert_eq!(c, "Name: pkg"),
            _ => panic!("a successful fallback must win"),
        }
    }

    #[test]
    fn no_candidate_urls_is_not_found() {
        assert!(matches!(
            aggregate_candidates("pkg", 0, vec![]),
            SpecFetchResult::NotFound
        ));
    }

    #[test]
    fn rewrite_origin_keeps_the_path_and_replaces_the_host() {
        assert_eq!(
            rewrite_origin(
                "https://src.fedoraproject.org/rpms/zlib/raw/f44/f/zlib.spec",
                "http://127.0.0.1:8080"
            ),
            "http://127.0.0.1:8080/rpms/zlib/raw/f44/f/zlib.spec"
        );
        // A trailing slash on the base must not double up.
        assert_eq!(
            rewrite_origin("https://gitlab.com/a/b.spec", "http://h:1/"),
            "http://h:1/a/b.spec"
        );
    }

    // --- a golang- prefix is not a module path (#42) ---

    #[test]
    fn a_golang_prefix_names_the_ecosystem_and_not_the_module() {
        // Fedora and Debian both flatten "/" and "." to "-", and that is not
        // reversible: github-spf13-cobra could be github.com/spf13/cobra or
        // github-spf13.com/cobra. Stripping the prefix, as every neighbouring
        // branch does, would only produce a differently-unusable name.
        let spec = SpecData {
            source0_url: None,
            all_sources: vec![],
            patches: vec![],
            commit_hash: None,
            url_field: None,
            name: Some("golang-github-spf13-cobra".to_string()),
            version: Some("1.8.0".to_string()),
            build_requires: vec![],
            changelog_entries: vec![],
        };
        let by_spec = detect_ecosystem(&spec, "golang-github-spf13-cobra").unwrap();
        let by_name = detect_ecosystem_by_name("golang-github-spf13-cobra", None).unwrap();
        for detection in [by_spec, by_name] {
            assert_eq!(detection.ecosystem, "gomod");
            assert_eq!(
                detection.package_name, None,
                "a flattened distro name was recorded as a Go module path"
            );
        }
    }

    #[test]
    fn a_go_package_publishes_its_ecosystem_without_an_unusable_name() {
        // What reaches the graph, not what the detector returns: seed.rs
        // hands upstreamPackageName straight to the Go module collector, so
        // an unusable name there is a lookup that can only fail.
        let name = "golang-github-spf13-cobra";
        let mut server = mockito::Server::new();
        server
            .mock(
                "GET",
                format!("/rpms/{name}/raw/f44/f/{name}.spec").as_str(),
            )
            .with_status(200)
            .with_body(format!(
                "Name:           {name}\nVersion:        1.8.0\n%description\na package\n"
            ))
            .create();
        let collector = offline_spec_collector(server.url());

        let names: HashSet<String> = [name.to_string()].into_iter().collect();
        let identities: HashMap<String, Vec<String>> = [(
            name.to_string(),
            vec![format!("{DATA}identity/fedora/44/x86_64/{name}")],
        )]
        .into_iter()
        .collect();

        let mut w = crate::ntriples::NTriplesWriter::new(Vec::<u8>::new());
        collector
            .collect(&mut w, &names, &identities, &HashSet::new(), false, false)
            .unwrap();
        let body = w.into_string().unwrap();

        assert!(
            body.contains(&format!("upstreamEcosystem> <{DATA}ecosystem/gomod>")),
            "the ecosystem is still worth recording:\n{body}"
        );
        assert!(
            !body.contains("upstreamPackageName"),
            "a flattened distro name reached the graph as a Go module path:\n{body}"
        );
    }

    // --- the RPM path outranks the spec heuristics (#43) ---

    /// A spec whose Source0 domain makes the PyPI heuristic fire.
    fn pythonhosted_spec(name: &str) -> String {
        let upstream = name.strip_prefix("python-").unwrap();
        format!(
            "Name:           {name}\n\
             Version:        1.0\n\
             Source0:        https://files.pythonhosted.org/packages/source/{upstream}.tar.gz\n\
             %description\n\
             a package\n"
        )
    }

    fn offline_spec_collector(base: String) -> SpecCollector {
        SpecCollector {
            transport: HttpTransport::new(),
            distro: "fedora".to_string(),
            release: "44".to_string(),
            cache: None,
            dist_git_base: Some(base),
        }
    }

    #[test]
    fn a_source_package_whose_ecosystem_came_from_provides_is_not_re_derived() {
        // The guard has always been here; main.rs passed it a permanently
        // empty set, so it never fired (#43). Both collectors run in the same
        // pass for all nine RPM distros, and where both produced a result both
        // were emitted to the same subject with no provenance to rank them.
        let mut server = mockito::Server::new();
        for name in ["python-requests", "python-urllib3"] {
            server
                .mock(
                    "GET",
                    format!("/rpms/{name}/raw/f44/f/{name}.spec").as_str(),
                )
                .with_status(200)
                .with_body(pythonhosted_spec(name))
                .create();
        }
        let collector = offline_spec_collector(server.url());

        let names: HashSet<String> = ["python-requests", "python-urllib3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Only python-requests had a Provides: capability naming its upstream.
        let from_provides: HashSet<String> = ["python-requests".to_string()].into_iter().collect();

        // upstreamPackageName is emitted on each PackageIdentity of the
        // source package, so without this map the spec path writes nothing
        // and the test would pass by having nothing to suppress.
        let identities: HashMap<String, Vec<String>> = names
            .iter()
            .map(|n| {
                (
                    n.clone(),
                    vec![format!("{DATA}identity/fedora/44/x86_64/{n}")],
                )
            })
            .collect();

        let mut w = crate::ntriples::NTriplesWriter::new(Vec::<u8>::new());
        let (specs, _triples) = collector
            .collect(&mut w, &names, &identities, &from_provides, false, false)
            .unwrap();
        let body = w.into_string().unwrap();

        assert_eq!(specs, 2, "both specs must still be fetched and processed");
        assert!(
            !body.contains("upstreamPackageName> \"requests\""),
            "the spec heuristic re-derived an upstream name the capability \
             had already stated:\n{body}"
        );
        // Non-vacuous: the same heuristic on the same shape of spec does fire
        // for the package the RPM path said nothing about. Without this, an
        // assertion that "requests" is absent would pass if detection were
        // simply broken.
        assert!(
            body.contains("upstreamPackageName> \"urllib3\""),
            "the heuristic must still run where it is the only evidence:\n{body}"
        );
    }

    // --- spec-stage checkpointing ---

    #[test]
    fn spec_cache_path_carries_its_own_schema_version() {
        // Non-vacuous version check: assert the path actually contains THIS
        // stage's version segment, and that changing only the version changes
        // the path. Asserting merely that spec and koji paths differ would
        // pass even if both stages shared one constant, since the stage
        // segment already differs. enrich_koji.rs has the mirror of this.
        let d = tempfile::TempDir::new().unwrap();
        let ctx = crate::output_cache::CanonicalContext::new();
        let mine = crate::output_cache::OutputCache::new(
            d.path(), "20260912T010203Z-abcdef01", "spec", SPEC_SCHEMA_VERSION).unwrap();
        let bumped = crate::output_cache::OutputCache::new(
            d.path(), "20260912T010203Z-abcdef01", "spec", "spec-vNEXT").unwrap();
        let p = mine.entry_path("k", &ctx).unwrap();
        assert!(p.to_string_lossy().contains(SPEC_SCHEMA_VERSION), "got {p:?}");
        assert_ne!(p, bumped.entry_path("k", &ctx).unwrap(),
            "a version bump must invalidate existing checkpoints");
    }

    #[test]
    fn collect_checkpointed_replays_a_prepopulated_item_without_fetching() {
        // Drives the REAL loop, not OutputCache in isolation. Only this shape
        // can catch a wrong stage context, a fetch still happening on a hit,
        // lost counter propagation, or output not going through
        // write_raw_line.
        //
        // Distro "offline-test" is deliberately unsupported: spec_urls returns
        // no candidates for it, so this test can never issue a network
        // request. A hit replays the checkpoint; a miss would produce a
        // NotFound DQ fragment instead. Either way the assertions below
        // distinguish them deterministically, with no dependency on the live
        // Fedora service.
        use crate::output_cache::{CachedOutput, ComputeOutcome, OutputCache};
        let d = tempfile::TempDir::new().unwrap();
        let cache = OutputCache::new(
            d.path(), "20260912T010203Z-abcdef01", "spec", SPEC_SCHEMA_VERSION).unwrap();

        let c = SpecCollector::new("offline-test", "44", None).unwrap();
        let mut names = HashSet::new();
        names.insert("zlib".to_string());
        let identity_map: HashMap<String, Vec<String>> = HashMap::new();
        let existing: HashSet<String> = HashSet::new();

        // Pre-populate using the exact context the loop will compute, so the
        // lookup only hits if the stage builds its context identically.
        let ctx = crate::output_cache::CanonicalContext::new()
            .field("distro", "offline-test")
            .field("release", "44")
            .list("identities", &[])
            .flag("in_existing_ecosystem", false)
            .flag("emit_buildrequires", false)
            .flag("emit_maintainers", false);
        cache.get_or_compute("zlib", &ctx, || Ok(ComputeOutcome::Complete(CachedOutput {
            logical_triples: 4,
            skipped_invalid_iri: 1,
            auto_inverses: 2,
            text: "<s> <p> <o> .\n".into(),
        }))).unwrap();

        let mut w = crate::ntriples::NTriplesWriter::new(Vec::<u8>::new());
        let (specs, triples, report) = c.collect_checkpointed(
            &mut w, &names, &identity_map, &existing, false, false, &cache,
        ).unwrap();
        // A replayed item counts as completed, and its verdict comes from the
        // replay -- the closure that would set the retryable flag never runs.
        assert_eq!((report.attempted, report.completed, report.retryable), (1, 1, 0));
        // A NotFound DQ fragment would have non-zero triples but different
        // text, so the assertions below separate a hit from a silent miss.

        assert_eq!((specs, triples), (1, 4), "totals must come from the checkpoint");
        assert_eq!(w.skipped_invalid_iri, 1, "counters must propagate on replay");
        assert_eq!(w.auto_inverses, 2);
        assert_eq!(w.into_string().unwrap(), "<s> <p> <o> .\n",
            "replayed text must reach the output writer verbatim");
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 1, "only the pre-population miss");
    }

    #[test]
    fn a_different_context_does_not_hit_the_checkpoint() {
        // Guards the context fingerprint: flipping a declared input must miss.
        // Same offline-only distro, so the miss cannot reach the network.
        use crate::output_cache::{CachedOutput, ComputeOutcome, OutputCache};
        let d = tempfile::TempDir::new().unwrap();
        let cache = OutputCache::new(
            d.path(), "20260912T010203Z-abcdef01", "spec", SPEC_SCHEMA_VERSION).unwrap();
        let ctx = crate::output_cache::CanonicalContext::new()
            .field("distro", "offline-test").field("release", "44")
            .list("identities", &[])
            .flag("in_existing_ecosystem", false)
            .flag("emit_buildrequires", false)   // <- differs from the call below
            .flag("emit_maintainers", false);
        cache.get_or_compute("zlib", &ctx, || Ok(ComputeOutcome::Complete(CachedOutput {
            logical_triples: 4, skipped_invalid_iri: 0, auto_inverses: 0,
            text: "<s> <p> <o> .\n".into(),
        }))).unwrap();

        let c = SpecCollector::new("offline-test", "44", None).unwrap();
        let mut names = HashSet::new();
        names.insert("zlib".to_string());
        let mut w = crate::ntriples::NTriplesWriter::new(Vec::<u8>::new());
        // emit_buildrequires = true this time.
        let _ = c.collect_checkpointed(
            &mut w, &names, &HashMap::new(), &HashSet::new(), true, false, &cache,
        );
        assert_eq!(cache.stats().hits, 0, "a changed declared input must not hit");
    }

    #[test]
    fn fetch_spec_reports_an_unsupported_distro_as_not_found() {
        // spec_urls returns no candidates for an unknown distro, so this
        // exercises the shell end-to-end without touching the network.
        let c = SpecCollector::new("not-a-distro", "1", None).unwrap();
        assert!(matches!(
            c.fetch_spec("anything"),
            SpecFetchResult::NotFound
        ));
    }
}
