//! Vendor security advisory enricher — RHSA (Red Hat) and DSA (Debian).
//!
//! Fetches advisories from vendor APIs and emits SecurityAdvisory triples
//! with CVE cross-references.

use crate::cache::FileCache;
use crate::enricher::rate_limit;
use crate::http_transport::HttpTransport;
use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

/// If more than this fraction of per-CVE RHSA detail fetches fail in a
/// single run, abort rather than upload a graph that silently dropped
/// advisoryForPackage links the previous run had. See design doc §3.1.
const RHSA_DETAIL_FAILURE_THRESHOLD: f64 = 0.05;

pub struct AdvisoryEnricher {
    transport: HttpTransport,
    cache: Option<FileCache>,
    advisory_type: AdvisoryType,
    days_back: u32,
    sparql: SparqlClient,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AdvisoryType {
    Rhsa,
    Dsa,
}

impl AdvisoryEnricher {
    pub fn new(
        advisory_type: AdvisoryType,
        days_back: u32,
        cache_dir: Option<&str>,
        endpoint: &str,
        auth: SparqlAuth,
        backend: SparqlBackend,
    ) -> Self {
        let client = crate::enricher::default_http_client();
        let sparql = make_sparql_client(endpoint, &auth, backend);

        let cache = cache_dir.map(|dir| {
            FileCache::new(dir, "advisory", 168, None) // 1 week TTL
                .expect("Failed to create cache")
        });

        Self {
            transport: HttpTransport::new(),
            cache,
            advisory_type,
            days_back,
            sparql,
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

        let (advisories, triples) = match self.advisory_type {
            AdvisoryType::Rhsa => self.enrich_rhsa(&mut writer)?,
            AdvisoryType::Dsa => self.enrich_dsa(&mut writer)?,
        };

        writer.flush()?;
        Ok((advisories, triples))
    }

    fn enrich_rhsa(&self, writer: &mut NTriplesWriter) -> Result<(usize, usize)> {
        let mut total_advisories = 0;
        let mut total_triples = 0;
        let mut total_cves = 0;
        let mut detail_fetch_failures = 0;
        let mut page = 1;

        loop {
            let url = format!(
                "https://access.redhat.com/hydra/rest/securitydata/cve.json?page={}&per_page=100&after={}",
                page,
                self.days_back_date()
            );

            let cache_key = format!("rhsa-page-{}-days-{}", page, self.days_back);
            let data = match self.cached_get(&cache_key) {
                Some(d) => d,
                None => {
                    // Distinguish a genuine API/outage failure from legitimate
                    // end-of-pagination. End-of-pagination is a 200 with an empty
                    // array (handled below); a failed fetch -- after the
                    // transport has exhausted its retries -- is an error and must
                    // not be swallowed into a truncated "successful" result that
                    // could feed a drop-and-replace load. Matches enrich_dsa.
                    let resp = self
                        .transport
                        .get_with(&url, &[("Accept", "application/json")], None)
                        .map_err(|e| {
                            std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!("RHSA API: {}", e),
                            )
                        })?;

                    let data: serde_json::Value =
                        serde_json::from_slice(&resp.bytes).map_err(|e| {
                            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                        })?;

                    self.cache_put(&cache_key, &data);
                    data
                }
            };

            let entries = match data.as_array() {
                Some(arr) if !arr.is_empty() => arr,
                _ => break,
            };

            for entry in entries {
                let cve_id = match entry.get("CVE").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => continue,
                };
                total_cves += 1;

                let severity = entry.get("severity").and_then(|v| v.as_str());
                let public_date = entry.get("public_date").and_then(|v| v.as_str());

                match self.fetch_cve_detail(cve_id) {
                    Ok(affected_releases) => {
                        let (advisories, triples) = self.emit_rhsa_advisories_for_cve(
                            writer,
                            cve_id,
                            severity,
                            public_date,
                            &affected_releases,
                        )?;
                        total_advisories += advisories;
                        total_triples += triples;
                    }
                    Err(e) => {
                        eprintln!("  RHSA detail fetch failed for {}: {}", cve_id, e);
                        detail_fetch_failures += 1;
                    }
                }

                rate_limit(Duration::from_millis(200));
            }

            page += 1;
            rate_limit(Duration::from_millis(500));
        }

        if exceeds_failure_threshold(detail_fetch_failures, total_cves) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "RHSA per-CVE detail fetch failure rate ({}/{}) exceeds the {:.0}% threshold -- \
                     aborting rather than publishing a graph missing advisoryForPackage links the previous run had",
                    detail_fetch_failures,
                    total_cves,
                    RHSA_DETAIL_FAILURE_THRESHOLD * 100.0,
                ),
            ));
        }

        Ok((total_advisories, total_triples))
    }

    /// Fetch a single CVE's detail response and return its
    /// `affected_release` entries (empty if the field is absent — a CVE
    /// can legitimately have no RHEL-family affected releases at all).
    fn fetch_cve_detail(&self, cve_id: &str) -> Result<Vec<serde_json::Value>> {
        let cache_key = format!("rhsa-detail-{}", cve_id);
        let data = match self.cached_get(&cache_key) {
            Some(d) => d,
            None => {
                let url = format!(
                    "https://access.redhat.com/hydra/rest/securitydata/cve/{}.json",
                    cve_id
                );
                let resp = self
                    .transport
                    .get_with(&url, &[("Accept", "application/json")], None)
                    .map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("RHSA detail API: {} for {}", e, cve_id),
                        )
                    })?;

                let data: serde_json::Value = serde_json::from_slice(&resp.bytes)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                self.cache_put(&cache_key, &data);
                data
            }
        };

        Ok(data
            .get("affected_release")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Emit one `SecurityAdvisory` node per distinct RHSA identifier found
    /// across `affected_releases` (design doc §3.5: identity is per-RHSA,
    /// not per-CVE — a single CVE can be addressed by multiple advisories,
    /// e.g. across RHEL minor releases). `severity`/`public_date` come from
    /// the CVE-level bulk-list entry (Red Hat's public severity rating is
    /// per-CVE, not per-advisory) and are attached to every RHSA node this
    /// CVE produces.
    fn emit_rhsa_advisories_for_cve(
        &self,
        writer: &mut NTriplesWriter,
        cve_id: &str,
        severity: Option<&str>,
        public_date: Option<&str>,
        affected_releases: &[serde_json::Value],
    ) -> Result<(usize, usize)> {
        let mut by_rhsa: std::collections::BTreeMap<String, Vec<&serde_json::Value>> =
            Default::default();
        for rel in affected_releases {
            if let Some(rhsa_id) = rel.get("advisory").and_then(|v| v.as_str()) {
                by_rhsa.entry(rhsa_id.to_string()).or_default().push(rel);
            }
        }

        let mut advisory_count = 0;
        let mut triples = 0;

        for (rhsa_id, releases) in &by_rhsa {
            // RHSA-2024:1234 -> advisory/rhsa/RHSA-2024-1234 (':' isn't
            // valid inside an IRI path segment here).
            let advisory_uri = format!("{DATA}advisory/rhsa/{}", rhsa_id.replace(':', "-"));

            writer.write_triple(&advisory_uri, RDF_TYPE, &format!("{SEC}SecurityAdvisory"))?;
            writer.write_literal(&advisory_uri, &format!("{SEC}advisoryId"), rhsa_id)?;
            writer.write_triple(
                &advisory_uri,
                &format!("{SEC}advisoryType"),
                &advisory_category_uri("security"),
            )?;
            triples += 3;

            if let Some(sev) = severity {
                if let Some(sev_uri) = severity_concept_uri(sev) {
                    writer.write_triple(
                        &advisory_uri,
                        &format!("{SEC}advisorySeverity"),
                        &sev_uri,
                    )?;
                    triples += 1;
                }
            }

            if let Some(date) = public_date {
                writer.write_literal(&advisory_uri, &format!("{SEC}advisoryDate"), date)?;
                triples += 1;
            }

            let cve_entity = cve_entity_uri(cve_id);
            writer.write_triple(
                &advisory_uri,
                &format!("{SEC}addressesVulnerability"),
                &cve_entity,
            )?;
            triples += 1;

            advisory_count += 1;

            for rel in releases {
                let product_name = rel.get("product_name").and_then(|v| v.as_str());
                let package = rel.get("package").and_then(|v| v.as_str());
                let (Some(product_name), Some(package)) = (product_name, package) else {
                    continue;
                };

                let Some(major) = extract_rhel_major_version(product_name) else {
                    continue;
                };
                let Some((name, epoch, version, release)) = parse_nvr(package) else {
                    continue;
                };

                for graph in target_graphs_for_major_version(major) {
                    match self.resolve_package(&graph, &name, &version, &release, &epoch) {
                        Ok(pkg_uris) => {
                            for pkg_uri in pkg_uris {
                                writer.write_triple(
                                    &advisory_uri,
                                    &format!("{SEC}advisoryForPackage"),
                                    &pkg_uri,
                                )?;
                                triples += 1;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "  advisoryForPackage resolution failed for {} in {}: {}",
                                package, graph, e
                            );
                        }
                    }
                }
            }
        }

        Ok((advisory_count, triples))
    }

    /// Resolve a parsed NVR to concrete Package URIs (one per arch build)
    /// in one graph. An empty result is expected and silent (§3.3): the
    /// current collected snapshot may have already moved past, or not yet
    /// reached, this exact build.
    fn resolve_package(
        &self,
        graph_uri: &str,
        name: &str,
        version: &str,
        release: &str,
        epoch: &str,
    ) -> Result<Vec<String>> {
        let query = build_nvr_match_query(graph_uri, name, version, release, epoch);
        let rows = self.sparql.query(&query)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.get("pkg").cloned())
            .collect())
    }

    fn enrich_dsa(&self, writer: &mut NTriplesWriter) -> Result<(usize, usize)> {
        let url = "https://security-tracker.debian.org/tracker/data/json";

        let cache_key = "dsa-tracker-full";
        let data = match self.cached_get(cache_key) {
            Some(d) => d,
            None => {
                let resp = self.transport.get(url, None).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, format!("DSA tracker: {}", e))
                })?;

                let data: serde_json::Value = serde_json::from_slice(&resp.bytes)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                self.cache_put(cache_key, &data);
                data
            }
        };

        let mut total_advisories = 0;
        let mut total_triples = 0;

        // Debian tracker JSON: { "package_name": { "CVE-XXXX-YYYY": { ... } } }
        if let Some(packages) = data.as_object() {
            for (pkg_name, cves) in packages {
                if let Some(cves_obj) = cves.as_object() {
                    for (cve_id, cve_data) in cves_obj {
                        if !cve_id.starts_with("CVE-") {
                            continue;
                        }

                        let advisory_uri = format!("{DATA}advisory/dsa/{}", cve_id);
                        let mut triples = 0;

                        writer.write_triple(
                            &advisory_uri,
                            RDF_TYPE,
                            &format!("{SEC}SecurityAdvisory"),
                        )?;
                        writer.write_literal(&advisory_uri, &format!("{SEC}advisoryId"), cve_id)?;
                        writer.write_triple(
                            &advisory_uri,
                            &format!("{SEC}advisoryType"),
                            &advisory_category_uri("security"),
                        )?;
                        triples += 3;

                        if let Some(severity) = cve_data.get("urgency").and_then(|v| v.as_str()) {
                            if let Some(sev_uri) = severity_concept_uri(severity) {
                                writer.write_triple(
                                    &advisory_uri,
                                    &format!("{SEC}advisorySeverity"),
                                    &sev_uri,
                                )?;
                                triples += 1;
                            }
                        }

                        // Link to CVE entity (shared with OSV collector)
                        let cve_entity = cve_entity_uri(cve_id);
                        writer.write_triple(
                            &advisory_uri,
                            &format!("{SEC}addressesVulnerability"),
                            &cve_entity,
                        )?;
                        triples += 1;

                        // NOTE: advisoryForPackage intentionally NOT emitted here.
                        // Per SD-7, the target must be a concrete pkg:Package with
                        // partOfRelease context. The Debian tracker only provides source
                        // package names, which resolve to PackageIdentity URIs. Emitting
                        // advisoryForPackage requires version-aware package resolution.

                        total_advisories += 1;
                        total_triples += triples;
                    }
                }
            }
        }

        Ok((total_advisories, total_triples))
    }

    /// Confirmed live 2026-09-11: an earlier version of this rounded to
    /// `{year}-01-01` regardless of `days_back`, so every RHSA run fetched
    /// everything published since January 1st of the current year instead
    /// of an actual rolling window -- a `--days-back 3` run took 90
    /// minutes and pulled 28k advisories rather than the handful a 3-day
    /// window should produce.
    fn days_back_date(&self) -> String {
        let past = chrono::Utc::now() - chrono::Duration::days(self.days_back as i64);
        past.format("%Y-%m-%d").to_string()
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

/// Whether a run's per-CVE detail-fetch failure count crosses
/// `RHSA_DETAIL_FAILURE_THRESHOLD` (design doc §3.1). Zero attempted CVEs
/// never trips this -- an empty run isn't a degraded run.
fn exceeds_failure_threshold(failures: usize, total: usize) -> bool {
    if total == 0 {
        return false;
    }
    (failures as f64 / total as f64) > RHSA_DETAIL_FAILURE_THRESHOLD
}

/// Extract the bare major RHEL version from a `product_name` like
/// "Red Hat Enterprise Linux 9". Anything else -- EUS variants, module
/// streams, legacy "(v. 6 for 64-bit)"-style strings, minor-versioned
/// names -- is not guessed at; only exactly "Red Hat Enterprise Linux
/// <digits>" with nothing trailing is accepted (design doc §3.2).
fn extract_rhel_major_version(product_name: &str) -> Option<u32> {
    let rest = product_name.strip_prefix("Red Hat Enterprise Linux ")?;
    if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
        rest.parse().ok()
    } else {
        None
    }
}

/// Map a supported RHEL major version to the graphs its RHSA-fixed NVRs
/// should be matched against: RHEL itself, plus its two true downstream
/// rebuilds (AlmaLinux, Rocky), which preserve upstream NVRs exactly.
/// CentOS Stream is deliberately excluded -- it tracks ahead of RHEL, not
/// a downstream rebuild, so exact-NVR matching against it is unreliable
/// (design doc §1 non-goals). Any other major version returns no targets.
fn target_graphs_for_major_version(major: u32) -> Vec<String> {
    match major {
        9 | 10 => vec![
            format!("https://packagegraph.github.io/graph/rhel/{major}"),
            format!("https://packagegraph.github.io/graph/almalinux/{major}"),
            format!("https://packagegraph.github.io/graph/rocky/{major}"),
        ],
        _ => vec![],
    }
}

/// Parse an NVRA string (`name-epoch:version-release.arch`, e.g.
/// `kernel-0:5.14.0-427.13.1.el9_4.x86_64`) into its five components.
/// Red Hat's per-CVE `affected_release[].package` field is an NVR
/// (`name-epoch:version-release`), *not* an NVRA -- confirmed live against
/// the real securitydata API (e.g. `glibc-0:2.34-168.el9_6.19`): one
/// source build produces multiple arch binaries, and the advisory tracks
/// the build, not any one arch of it. An earlier version of this parser
/// assumed a trailing `.arch` and silently matched zero packages ever,
/// because it misparsed the last `.`-delimited segment of the release
/// string (e.g. "19") as if it were an architecture.
///
/// Returns `None` for anything that doesn't have this exact shape --
/// notably a missing epoch (no ':') -- rather than defaulting (design doc
/// §3.2). Name may itself contain '-'; version may not (per RPM's own
/// field constraints), which is what makes this unambiguous: epoch is
/// always the digits between the *last* '-' before the ':' and the ':'
/// itself, and version is always the first '-'-delimited segment of what
/// remains after the ':' (release is everything after that, and may
/// itself contain '.' and '_').
fn parse_nvr(nvr: &str) -> Option<(String, String, String, String)> {
    let colon_pos = nvr.find(':')?;
    let before_colon = &nvr[..colon_pos];
    let dash_before_epoch = before_colon.rfind('-')?;
    let name = &before_colon[..dash_before_epoch];
    let epoch = &before_colon[dash_before_epoch + 1..];
    let version_release = &nvr[colon_pos + 1..];
    let (version, release) = version_release.split_once('-')?;

    if name.is_empty() || epoch.is_empty() || version.is_empty() || release.is_empty() {
        return None;
    }

    Some((
        name.to_string(),
        epoch.to_string(),
        version.to_string(),
        release.to_string(),
    ))
}

/// Escape a string for embedding in a SPARQL string literal.
fn escape_sparql_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build the SPARQL query that resolves a parsed NVR to concrete Package
/// URIs in one graph (design doc §3.3). The advisory data has no arch, so
/// this can't match `pkg:versionString` (`"{version}-{release}.{arch}"`)
/// exactly -- instead it matches every arch build of that NVR via a
/// `STRSTARTS` prefix filter on `"{version}-{release}."`, which is
/// unambiguous because RPM's Version field can't contain '-' (so the
/// prefix always lands exactly on the '.' before the arch segment).
/// Epoch is matched via OPTIONAL + FILTER so a zero-epoch RPM -- which has
/// no `pkg:epoch` triple at all -- still matches, instead of being
/// wrongly excluded by a filter that unconditionally requires one.
fn build_nvr_match_query(
    graph_uri: &str,
    name: &str,
    version: &str,
    release: &str,
    epoch: &str,
) -> String {
    let version_release_prefix = escape_sparql_literal(&format!("{version}-{release}."));
    let name = escape_sparql_literal(name);

    let epoch_filter = if epoch == "0" {
        "!BOUND(?epoch)".to_string()
    } else {
        format!("?epoch = \"{}\"", escape_sparql_literal(epoch))
    };

    // Match the identity name on either predicate. The collectors now emit
    // pkg:identityName (correct: rdfs:domain pkg:PackageIdentity); they used
    // to emit pkg:packageName, whose domain is pkg:Package. Graphs are
    // recollected on independent schedules -- some weekly -- so for one full
    // cycle both forms are live in the corpus. Matching only the new one
    // would silently return zero rows for every not-yet-recollected graph,
    // which is exactly the failure mode this enricher cannot signal.
    //
    // Drop the UNION arm once every graph has been recollected; the
    // ontology-shape-check.py run for PackageIdentity.identityName reaching
    // 0 violations is the signal that it is safe.
    format!(
        r#"SELECT ?pkg WHERE {{
  GRAPH <{graph_uri}> {{
    {{ ?identity <{PKG}identityName> "{name}" }}
    UNION
    {{ ?identity <{PKG}packageName> "{name}" }}
    ?pkg <{PKG}isVersionOf> ?identity ;
         <{PKG}hasVersion> ?ver .
    ?ver <{PKG}versionString> ?versionStr .
    FILTER(STRSTARTS(?versionStr, "{version_release_prefix}"))
    OPTIONAL {{ ?ver <{PKG}epoch> ?epoch }}
    FILTER({epoch_filter})
  }}
}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    /// A deliberately-refused endpoint (connection error, not a hang) --
    /// the same pattern maven.rs's own tests use for offline-safe SPARQL/
    /// HTTP error simulation.
    const UNREACHABLE_ENDPOINT: &str = "http://127.0.0.1:1";

    fn test_enricher() -> AdvisoryEnricher {
        AdvisoryEnricher::new(
            AdvisoryType::Rhsa,
            365,
            None,
            UNREACHABLE_ENDPOINT,
            None,
            SparqlBackend::Fuseki,
        )
    }

    #[test]
    fn test_emit_rhsa_advisories_for_cve_single_rhsa() {
        let enricher = test_enricher();

        let affected_releases = vec![serde_json::json!({
            "product_name": "Red Hat Enterprise Linux 9",
            "package": "kernel-0:5.14.0-427.13.1.el9_4",
            "advisory": "RHSA-2024:1234",
        })];

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let (advisories, triples) = enricher
            .emit_rhsa_advisories_for_cve(
                &mut writer,
                "CVE-2024-1234",
                Some("important"),
                Some("2024-03-15T00:00:00Z"),
                &affected_releases,
            )
            .unwrap();
        writer.flush().unwrap();

        assert_eq!(
            advisories, 1,
            "One distinct RHSA should produce one advisory node"
        );
        assert!(triples >= 5, "Should emit at least 5 triples");

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("advisory/rhsa/RHSA-2024-1234"),
            "Subject must be RHSA-keyed, not CVE-keyed"
        );
        assert!(
            content.contains("security#SecurityAdvisory"),
            "Should have SecurityAdvisory type"
        );
        assert!(
            content.contains("\"RHSA-2024:1234\""),
            "advisoryId must be the RHSA identifier, not the CVE identifier"
        );
        assert!(
            !content.contains("\"CVE-2024-1234\""),
            "The CVE ID must appear only via addressesVulnerability's URI, never as a literal advisoryId"
        );
        assert!(
            content.contains("security#cat-security"),
            "Should have advisoryType as SKOS concept"
        );
        assert!(
            content.contains("security#sev-important"),
            "Should have severity as SKOS concept"
        );
        assert!(
            content.contains("security#addressesVulnerability"),
            "Should link to CVE"
        );
        assert!(
            content.contains("cve/CVE-2024-1234"),
            "Should link to CVE entity URI"
        );
        // Resolution against UNREACHABLE_ENDPOINT fails fast and is logged,
        // not propagated -- no advisoryForPackage triple should appear, but
        // the advisory node itself must still be fully emitted.
        assert!(!content.contains("security#advisoryForPackage"));
    }

    #[test]
    fn test_emit_rhsa_advisories_for_cve_two_distinct_rhsas() {
        // One CVE addressed by two different RHSAs (e.g. separate fixes
        // for RHEL 9 and RHEL 10) must produce two distinct advisory
        // subjects, not one CVE-keyed node absorbing both (design §3.5).
        let enricher = test_enricher();

        let affected_releases = vec![
            serde_json::json!({
                "product_name": "Red Hat Enterprise Linux 9",
                "package": "kernel-0:5.14.0-427.13.1.el9_4",
                "advisory": "RHSA-2024:1234",
            }),
            serde_json::json!({
                "product_name": "Red Hat Enterprise Linux 10",
                "package": "kernel-0:6.12.0-55.9.1.el10_0",
                "advisory": "RHSA-2024:5678",
            }),
        ];

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let (advisories, _triples) = enricher
            .emit_rhsa_advisories_for_cve(
                &mut writer,
                "CVE-2024-9999",
                Some("critical"),
                Some("2024-05-01T00:00:00Z"),
                &affected_releases,
            )
            .unwrap();
        writer.flush().unwrap();

        assert_eq!(
            advisories, 2,
            "Two distinct RHSAs must produce two advisory nodes"
        );

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("advisory/rhsa/RHSA-2024-1234"));
        assert!(content.contains("advisory/rhsa/RHSA-2024-5678"));
        // Both still address the same CVE.
        assert_eq!(content.matches("cve/CVE-2024-9999").count(), 2);
    }

    #[test]
    fn test_emit_rhsa_advisories_for_cve_no_affected_releases() {
        let enricher = test_enricher();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let (advisories, triples) = enricher
            .emit_rhsa_advisories_for_cve(&mut writer, "CVE-2024-0001", None, None, &[])
            .unwrap();

        assert_eq!(advisories, 0);
        assert_eq!(triples, 0);
    }

    #[test]
    fn test_extract_rhel_major_version() {
        assert_eq!(
            extract_rhel_major_version("Red Hat Enterprise Linux 9"),
            Some(9)
        );
        assert_eq!(
            extract_rhel_major_version("Red Hat Enterprise Linux 10"),
            Some(10)
        );
        // EUS, module streams, legacy formats, minor versions: skipped, not guessed at.
        assert_eq!(
            extract_rhel_major_version("Red Hat Enterprise Linux 9.4 Extended Update Support"),
            None
        );
        assert_eq!(
            extract_rhel_major_version("Red Hat Enterprise Linux Server (v. 6 for 64-bit)"),
            None
        );
        assert_eq!(extract_rhel_major_version("Fedora 43"), None);
    }

    #[test]
    fn test_target_graphs_for_major_version() {
        let graphs = target_graphs_for_major_version(9);
        assert_eq!(graphs.len(), 3);
        assert!(graphs.iter().any(|g| g.ends_with("graph/rhel/9")));
        assert!(graphs.iter().any(|g| g.ends_with("graph/almalinux/9")));
        assert!(graphs.iter().any(|g| g.ends_with("graph/rocky/9")));
        // CentOS Stream deliberately excluded (design §1 non-goals).
        assert!(!graphs.iter().any(|g| g.contains("centos-stream")));

        assert!(target_graphs_for_major_version(8).is_empty());
    }

    #[test]
    fn test_parse_nvr() {
        assert_eq!(
            parse_nvr("kernel-0:5.14.0-427.13.1.el9_4"),
            Some((
                "kernel".to_string(),
                "0".to_string(),
                "5.14.0".to_string(),
                "427.13.1.el9_4".to_string(),
            ))
        );
        // Real Red Hat securitydata API response shape (no arch).
        assert_eq!(
            parse_nvr("glibc-0:2.34-168.el9_6.19"),
            Some((
                "glibc".to_string(),
                "0".to_string(),
                "2.34".to_string(),
                "168.el9_6.19".to_string(),
            ))
        );
        // Name containing '-' must not be split at the wrong point.
        assert_eq!(
            parse_nvr("java-11-openjdk-0:11.0.21.0.9-1.el9"),
            Some((
                "java-11-openjdk".to_string(),
                "0".to_string(),
                "11.0.21.0.9".to_string(),
                "1.el9".to_string(),
            ))
        );
        // Non-zero epoch.
        assert_eq!(
            parse_nvr("foo-2:1.0-1.el9"),
            Some((
                "foo".to_string(),
                "2".to_string(),
                "1.0".to_string(),
                "1.el9".to_string()
            ))
        );
        // Missing epoch (no ':') -- must not default to "0", must skip.
        assert_eq!(parse_nvr("kernel-5.14.0-427.13.1.el9_4"), None);
        // No release separator at all.
        assert_eq!(parse_nvr("kernel-0:5.14.0"), None);
    }

    #[test]
    fn test_build_nvr_match_query_zero_epoch() {
        let query = build_nvr_match_query(
            "https://packagegraph.github.io/graph/almalinux/9",
            "kernel",
            "5.14.0",
            "427.13.1.el9_4",
            "0",
        );
        assert!(query.contains("STRSTARTS(?versionStr, \"5.14.0-427.13.1.el9_4.\")"));
        assert!(query.contains("!BOUND(?epoch)"));
        assert!(!query.contains("?epoch = "));
    }

    #[test]
    fn test_build_nvr_match_query_nonzero_epoch() {
        let query = build_nvr_match_query(
            "https://packagegraph.github.io/graph/rhel/9",
            "foo",
            "1.0",
            "1.el9",
            "2",
        );
        assert!(query.contains("STRSTARTS(?versionStr, \"1.0-1.el9.\")"));
        assert!(query.contains("?epoch = \"2\""));
        assert!(!query.contains("!BOUND"));
    }

    #[test]
    fn test_days_back_date_has_day_granularity() {
        // A 3-day and a 30-day window must land on different dates -- the
        // pre-fix version rounded both to the same "{year}-01-01".
        let short = AdvisoryEnricher::new(
            AdvisoryType::Rhsa,
            3,
            None,
            UNREACHABLE_ENDPOINT,
            None,
            SparqlBackend::Fuseki,
        );
        let long = AdvisoryEnricher::new(
            AdvisoryType::Rhsa,
            30,
            None,
            UNREACHABLE_ENDPOINT,
            None,
            SparqlBackend::Fuseki,
        );
        assert_ne!(short.days_back_date(), long.days_back_date());

        let expected = (chrono::Utc::now() - chrono::Duration::days(3))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(short.days_back_date(), expected);
    }

    #[test]
    fn test_exceeds_failure_threshold() {
        assert!(
            !exceeds_failure_threshold(0, 0),
            "empty run is not degraded"
        );
        assert!(
            !exceeds_failure_threshold(4, 100),
            "4% is under the 5% threshold"
        );
        assert!(
            exceeds_failure_threshold(6, 100),
            "6% exceeds the 5% threshold"
        );
        assert!(
            exceeds_failure_threshold(1, 1),
            "100% failure always exceeds threshold"
        );
    }

    #[test]
    fn test_dsa_parsing() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // Simulate DSA tracker data structure
        let advisory_uri = format!("{DATA}advisory/dsa/CVE-2024-5678");
        writer
            .write_triple(&advisory_uri, RDF_TYPE, &format!("{SEC}SecurityAdvisory"))
            .unwrap();
        writer
            .write_literal(&advisory_uri, &format!("{SEC}advisoryId"), "CVE-2024-5678")
            .unwrap();
        let sev_uri = severity_concept_uri("high").unwrap();
        writer
            .write_triple(&advisory_uri, &format!("{SEC}advisorySeverity"), &sev_uri)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("advisory/dsa/CVE-2024-5678"),
            "Should use DSA advisory URI"
        );
        assert!(content.contains("security#SecurityAdvisory"));
        assert!(
            content.contains("security#sev-important"),
            "high maps to sev-important"
        );
    }
}
