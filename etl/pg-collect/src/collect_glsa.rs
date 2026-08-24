//! GLSA advisory collector — Gentoo security advisories via XML feed.
//!
//! Fetches GLSA (Gentoo Linux Security Advisories) from security.gentoo.org,
//! parses XML advisory metadata, and extracts affected package atoms with
//! version constraints for SPARQL-based resolution.
//!
//! NOTE: sec:advisoryForPackage links are over-inclusive in v1. The collector
//! emits links to ALL packages matching the affected atom (category/name),
//! without filtering by Portage PVR version comparison. This is by design —
//! precise filtering requires implementing Portage version semantics. The
//! structured sec:hasAffectedRange data preserves the version constraint for
//! SPARQL-level filtering. See plan for follow-on PVR comparison task.

use crate::cache::FileCache;
use crate::enricher::rate_limit;
use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use once_cell::sync::Lazy;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use regex::Regex;
use reqwest::blocking::Client;
use serde_json;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

/// CVE identifier regex: CVE-YYYY-NNNNN
static CVE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"CVE-\d{4}-\d{4,}").unwrap());

/// GLSA advisory collector with SPARQL-based atom resolution.
pub struct GlsaCollector {
    client: Client,
    sparql: SparqlClient,
    since: Option<String>,
    cache: Option<FileCache>,
    pub graph_uri: Option<String>,
}

impl GlsaCollector {
    /// Create a new GLSA collector.
    ///
    /// - `endpoint`: Fuseki SPARQL endpoint URL for atom→package resolution
    /// - `since`: Optional date filter (ISO format YYYY-MM-DD) — only GLSAs after this date
    /// - `cache_dir`: Optional cache directory for GLSA XML files
    pub fn new(
        endpoint: &str,
        since: Option<String>,
        cache_dir: Option<&str>,
        auth: SparqlAuth,
        backend: SparqlBackend,
    ) -> Result<Self> {
        let client = crate::enricher::default_http_client();

        let cache = cache_dir
            .map(|dir| FileCache::new(dir, "glsa", 168, None))
            .transpose()?;

        Ok(Self {
            client,
            sparql: make_sparql_client(endpoint, &auth, backend),
            since,
            cache,
            graph_uri: None,
        })
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    /// Collect GLSA advisories and emit N-Triples.
    ///
    /// Returns (advisories_count, triples_count).
    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        // Fetch RSS index to get GLSA IDs
        let glsa_ids = self.fetch_glsa_index()?;
        eprintln!("Found {} GLSA advisories in index", glsa_ids.len());

        let mut total_advisories = 0;
        let mut total_triples = 0;
        let mut total_resolved_packages = 0;
        let mut unresolved_atoms = 0;

        for (idx, glsa_id) in glsa_ids.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Processing {}/{}...", idx + 1, glsa_ids.len());
            }

            // Fetch and parse individual GLSA XML
            let xml = match self.fetch_glsa_xml(glsa_id) {
                Ok(xml) => xml,
                Err(e) => {
                    eprintln!("  Warning: Failed to fetch GLSA {}: {}", glsa_id, e);
                    continue;
                }
            };
            let advisory = match GlsaAdvisory::from_xml(&xml) {
                Some(adv) => adv,
                None => {
                    eprintln!("  Warning: Failed to parse GLSA {}", glsa_id);
                    continue;
                }
            };

            // Apply --since date filter if specified
            if let Some(ref cutoff) = self.since {
                if &advisory.date < cutoff {
                    continue; // Skip GLSAs announced before cutoff
                }
            }

            let advisory_uri = format!("{DATA}advisory/gentoo/{}", advisory.id);
            let mut advisory_triples = 0;

            // Advisory entity
            writer.write_triple(&advisory_uri, RDF_TYPE, &format!("{SEC}SecurityAdvisory"))?;
            writer.write_literal(&advisory_uri, &format!("{SEC}advisoryId"), &advisory.id)?;
            writer.write_triple(
                &advisory_uri,
                &format!("{SEC}advisoryType"),
                &advisory_category_uri("security"),
            )?;
            advisory_triples += 3;

            // Publication date as xsd:dateTime (YYYY-MM-DD → YYYY-MM-DDT00:00:00Z)
            let datetime = format!("{}T00:00:00Z", advisory.date);
            writer.write_datetime(&advisory_uri, &format!("{SEC}advisoryDate"), &datetime)?;
            advisory_triples += 1;

            // Severity
            if let Some(ref sev) = advisory.severity {
                if let Some(sev_uri) = severity_concept_uri(sev) {
                    writer.write_triple(
                        &advisory_uri,
                        &format!("{SEC}advisorySeverity"),
                        &sev_uri,
                    )?;
                    advisory_triples += 1;
                }
            }

            // CVE cross-references (emit first — affected ranges attach to these)
            for cve_id in &advisory.cves {
                let cve_uri = cve_entity_uri(cve_id);
                writer.write_triple(
                    &advisory_uri,
                    &format!("{SEC}addressesVulnerability"),
                    &cve_uri,
                )?;
                advisory_triples += 1;

                // Emit affected-range for this CVE (per affected package)
                for (pkg_idx, affected_pkg) in advisory.affected_packages.iter().enumerate() {
                    let range_triples = self.emit_affected_range(
                        &mut writer,
                        &cve_uri, // Attach to vulnerability, not advisory
                        &advisory.id,
                        affected_pkg,
                        pkg_idx,
                    )?;
                    advisory_triples += range_triples;
                }
            }

            // Resolve atoms to packages and emit advisoryForPackage
            for affected_pkg in &advisory.affected_packages {
                // Resolve atom to concrete packages via SPARQL
                match self.resolve_atom_to_packages(&affected_pkg.atom) {
                    Ok(packages) if !packages.is_empty() => {
                        // Emit advisoryForPackage for each matched package
                        for pkg_uri in &packages {
                            writer.write_triple(
                                &advisory_uri,
                                &format!("{SEC}advisoryForPackage"),
                                pkg_uri,
                            )?;
                            advisory_triples += 1;
                            total_resolved_packages += 1;
                        }
                    }
                    Ok(_) => {
                        eprintln!(
                            "  Warning: Atom {} has no matching packages in graph",
                            affected_pkg.atom
                        );
                        unresolved_atoms += 1;
                    }
                    Err(e) => {
                        eprintln!(
                            "  Warning: SPARQL query failed for atom {}: {}",
                            affected_pkg.atom, e
                        );
                        unresolved_atoms += 1;
                    }
                }
            }

            total_advisories += 1;
            total_triples += advisory_triples;

            rate_limit(Duration::from_secs(1)); // 1 request/second
        }

        writer.flush()?;

        eprintln!();
        eprintln!("Collected {} advisories", total_advisories);
        eprintln!("Resolved {} package links", total_resolved_packages);
        eprintln!("Unresolved atoms: {}", unresolved_atoms);

        Ok((total_advisories, total_triples))
    }

    /// Fetch RSS index and extract GLSA IDs.
    fn fetch_glsa_index(&self) -> Result<Vec<String>> {
        let url = "https://security.gentoo.org/glsa/feed.rss";
        let cache_key = "glsa-index-rss";

        let xml = match self.cached_get(cache_key) {
            Some(data) => data,
            None => {
                let resp =
                    self.client.get(url).send().map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    })?;

                if !resp.status().is_success() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("GLSA RSS index returned {}", resp.status()),
                    ));
                }

                let xml = resp
                    .text()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                self.cache_put(cache_key, &xml);
                xml
            }
        };

        // Extract GLSA IDs from RSS <link> elements
        let mut reader = Reader::from_str(&xml);
        reader.config_mut().trim_text(true);

        let mut glsa_ids = Vec::new();
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) if e.name().as_ref() == b"link" => {
                    if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                        let link = text.unescape().unwrap_or_default().to_string();
                        // Extract GLSA ID from URL like "https://security.gentoo.org/glsa/202601-02"
                        // Skip non-advisory links (channel link, etc.)
                        if let Some(id_part) = link.split('/').last() {
                            if id_part.contains('-')
                                && id_part.chars().next().map_or(false, |c| c.is_ascii_digit())
                            {
                                glsa_ids.push(format!("GLSA-{}", id_part));
                            }
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("RSS parse error: {}", e),
                    ));
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(glsa_ids)
    }

    /// Fetch individual GLSA XML document.
    fn fetch_glsa_xml(&self, glsa_id: &str) -> Result<String> {
        // Strip "GLSA-" prefix for URL
        let id_part = glsa_id.strip_prefix("GLSA-").unwrap_or(glsa_id);
        let url = format!("https://security.gentoo.org/glsa/{}.xml", id_part);
        let cache_key = format!("glsa-{}", id_part);

        match self.cached_get(&cache_key) {
            Some(data) => Ok(data),
            None => {
                let resp =
                    self.client.get(&url).send().map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    })?;

                if !resp.status().is_success() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("GLSA {} returned {}", glsa_id, resp.status()),
                    ));
                }

                let xml = resp
                    .text()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                self.cache_put(&cache_key, &xml);
                Ok(xml)
            }
        }
    }

    /// Resolve Portage atom to concrete package URIs via SPARQL.
    ///
    /// Returns ALL versioned packages matching the atom (category/name).
    /// v1 does NOT filter by PVR version constraints — over-inclusive by design.
    fn resolve_atom_to_packages(&self, atom: &str) -> Result<Vec<String>> {
        let graph_uri = "https://packagegraph.github.io/graph/gentoo";

        let sparql = format!(
            r#"PREFIX pkg: <{PKG}>
SELECT ?pkg WHERE {{
  GRAPH <{graph_uri}> {{
    ?pkg a pkg:SourcePackage ;
         pkg:packageName "{atom}" .
  }}
}}"#
        );

        let bindings = self.sparql.query(&sparql)?;
        Ok(bindings
            .into_iter()
            .filter_map(|b| b.get("pkg").cloned())
            .collect())
    }

    /// Emit affected-range blank nodes for a GLSA affected package.
    ///
    /// Follows the OSV pattern (osv.rs:215-267): hasAffectedRange → AffectedRange
    /// with hasRangeEvent → RangeEvent (introduced/fixed).
    ///
    /// NOTE: sec:hasAffectedRange attaches to sec:Vulnerability, not sec:SecurityAdvisory
    /// per the ontology (security.ttl:206). The vulnerability_uri parameter is the CVE
    /// entity that this range describes.
    ///
    /// For GLSA, we model:
    /// - introduced: version "0" (all prior versions)
    /// - fixed: first unaffected version from unaffected_ranges
    fn emit_affected_range(
        &self,
        writer: &mut NTriplesWriter,
        vulnerability_uri: &str,
        advisory_id: &str,
        affected_pkg: &GlsaAffectedPackage,
        _pkg_idx: usize,
    ) -> Result<usize> {
        let mut triples = 0;

        // Create deterministic blank node ID for the range
        let range_input = format!("{}_{}", advisory_id, affected_pkg.atom);
        let range_bnode = bnode_id("ar", &range_input);

        // Attach to vulnerability, not advisory (per ontology security.ttl:206)
        writer.write_bnode_object(
            vulnerability_uri,
            &format!("{SEC}hasAffectedRange"),
            &range_bnode,
        )?;
        writer.write_bnode_subject(&range_bnode, RDF_TYPE, &format!("{SEC}AffectedRange"))?;
        writer.write_bnode_subject(
            &range_bnode,
            &format!("{SEC}rangeType"),
            &range_type_uri("ECOSYSTEM"),
        )?;
        triples += 3;

        // Introduced event: version "0" (all prior versions)
        let intro_bnode = format!("{}_intro", range_bnode);
        writer.write_bnode_to_bnode(&range_bnode, &format!("{SEC}hasRangeEvent"), &intro_bnode)?;
        writer.write_bnode_subject(&intro_bnode, RDF_TYPE, &format!("{SEC}RangeEvent"))?;
        writer.write_bnode_subject(
            &intro_bnode,
            &format!("{SEC}eventType"),
            &event_type_uri("introduced"),
        )?;
        writer.write_bnode_literal(&intro_bnode, &format!("{SEC}eventVersion"), "0")?;
        triples += 4;

        // Fixed events: from unaffected ranges
        for (idx, (range_op, version)) in affected_pkg.unaffected_ranges.iter().enumerate() {
            let fix_bnode = format!("{}_fix{}", range_bnode, idx);
            writer.write_bnode_to_bnode(
                &range_bnode,
                &format!("{SEC}hasRangeEvent"),
                &fix_bnode,
            )?;
            writer.write_bnode_subject(&fix_bnode, RDF_TYPE, &format!("{SEC}RangeEvent"))?;
            writer.write_bnode_subject(
                &fix_bnode,
                &format!("{SEC}eventType"),
                &event_type_uri("fixed"),
            )?;
            writer.write_bnode_literal(&fix_bnode, &format!("{SEC}eventVersion"), version)?;
            triples += 4;
        }

        Ok(triples)
    }

    fn cached_get(&self, key: &str) -> Option<String> {
        self.cache
            .as_ref()?
            .get(key)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
    }

    fn cache_put(&self, key: &str, data: &str) {
        if let Some(ref cache) = self.cache {
            cache.put(key, &serde_json::Value::String(data.to_string()));
        }
    }
}

/// Parsed GLSA advisory.
#[derive(Debug, Clone, PartialEq)]
pub struct GlsaAdvisory {
    /// Advisory ID (e.g., "GLSA-202601-02")
    pub id: String,
    /// Announcement date (YYYY-MM-DD format from XML)
    pub date: String,
    /// Severity from impact/@type (e.g., "high")
    pub severity: Option<String>,
    /// Affected packages with version constraints
    pub affected_packages: Vec<GlsaAffectedPackage>,
    /// CVE IDs extracted from reference URIs
    pub cves: Vec<String>,
}

/// Affected package with version constraints.
#[derive(Debug, Clone, PartialEq)]
pub struct GlsaAffectedPackage {
    /// Portage atom (category/name, e.g., "app-editors/vim")
    pub atom: String,
    /// Vulnerable ranges: (operator, version) pairs (e.g., ("lt", "9.1.1652"))
    pub vulnerable_ranges: Vec<(String, String)>,
    /// Unaffected ranges: (operator, version) pairs (e.g., ("ge", "9.1.1652"))
    pub unaffected_ranges: Vec<(String, String)>,
}

impl GlsaAdvisory {
    /// Parse a GLSA XML document and extract advisory metadata.
    ///
    /// Returns None if required fields are missing.
    pub fn from_xml(xml: &str) -> Option<Self> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut id: Option<String> = None;
        let mut date: Option<String> = None;
        let mut severity: Option<String> = None;
        let mut affected_packages: Vec<GlsaAffectedPackage> = Vec::new();
        let mut cves: Vec<String> = Vec::new();

        let mut current_package: Option<String> = None;
        let mut current_vulnerable: Vec<(String, String)> = Vec::new();
        let mut current_unaffected: Vec<(String, String)> = Vec::new();

        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    let tag_name = e.name();
                    match tag_name.as_ref() {
                        b"glsa" => {
                            // Extract id attribute
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"id" {
                                    let id_val = String::from_utf8_lossy(&attr.value).to_string();
                                    id = Some(format!("GLSA-{}", id_val));
                                }
                            }
                        }
                        b"announced" => {
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                date = Some(text.unescape().ok()?.to_string());
                            }
                        }
                        b"impact" => {
                            // Extract type attribute
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"type" {
                                    severity =
                                        Some(String::from_utf8_lossy(&attr.value).to_string());
                                }
                            }
                        }
                        b"package" => {
                            // Save previous package if any
                            if let Some(atom) = current_package.take() {
                                affected_packages.push(GlsaAffectedPackage {
                                    atom,
                                    vulnerable_ranges: std::mem::take(&mut current_vulnerable),
                                    unaffected_ranges: std::mem::take(&mut current_unaffected),
                                });
                            }
                            // Extract name attribute
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"name" {
                                    current_package =
                                        Some(String::from_utf8_lossy(&attr.value).to_string());
                                }
                            }
                        }
                        b"vulnerable" => {
                            let range_op = e
                                .attributes()
                                .flatten()
                                .find(|a| a.key.as_ref() == b"range")
                                .map(|a| String::from_utf8_lossy(&a.value).to_string());
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                if let Some(op) = range_op {
                                    let version = text.unescape().ok()?.to_string();
                                    current_vulnerable.push((op, version));
                                }
                            }
                        }
                        b"unaffected" => {
                            let range_op = e
                                .attributes()
                                .flatten()
                                .find(|a| a.key.as_ref() == b"range")
                                .map(|a| String::from_utf8_lossy(&a.value).to_string());
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                if let Some(op) = range_op {
                                    let version = text.unescape().ok()?.to_string();
                                    current_unaffected.push((op, version));
                                }
                            }
                        }
                        b"uri" => {
                            // Extract CVE from link attribute and element text
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                let text_content = text.unescape().ok()?.to_string();
                                // Extract CVE IDs using regex
                                for cve_match in CVE_RE.find_iter(&text_content) {
                                    cves.push(cve_match.as_str().to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(e)) if e.name().as_ref() == b"affected" => {
                    // Save last package when </affected> closes
                    if let Some(atom) = current_package.take() {
                        affected_packages.push(GlsaAffectedPackage {
                            atom,
                            vulnerable_ranges: std::mem::take(&mut current_vulnerable),
                            unaffected_ranges: std::mem::take(&mut current_unaffected),
                        });
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => return None,
                _ => {}
            }
            buf.clear();
        }

        Some(GlsaAdvisory {
            id: id?,
            date: date?,
            severity,
            affected_packages,
            cves,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_GLSA_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE glsa SYSTEM "http://www.gentoo.org/dtd/glsa.dtd">
<glsa id="202601-02">
  <title>Vim, gVim: Multiple Vulnerabilities</title>
  <synopsis>Multiple vulnerabilities have been discovered in Vim and gVim.</synopsis>
  <product type="ebuild">gvim,vim,vim-core</product>
  <announced>2026-01-26</announced>
  <revised count="1">2026-01-26</revised>
  <bug>961498</bug>
  <access>local</access>
  <affected>
    <package name="app-editors/gvim" auto="yes" arch="*">
      <unaffected range="ge">9.1.1652</unaffected>
      <vulnerable range="lt">9.1.1652</vulnerable>
    </package>
    <package name="app-editors/vim" auto="yes" arch="*">
      <unaffected range="ge">9.1.1652</unaffected>
      <vulnerable range="lt">9.1.1652</vulnerable>
    </package>
  </affected>
  <impact type="high">
    <p>Multiple vulnerabilities...</p>
  </impact>
  <references>
    <uri link="https://nvd.nist.gov/vuln/detail/CVE-2025-53905">CVE-2025-53905</uri>
    <uri link="https://nvd.nist.gov/vuln/detail/CVE-2025-53906">CVE-2025-53906</uri>
  </references>
</glsa>
"#;

    #[test]
    fn test_parse_glsa_xml() {
        let advisory =
            GlsaAdvisory::from_xml(SAMPLE_GLSA_XML).expect("Should parse valid GLSA XML");

        assert_eq!(advisory.id, "GLSA-202601-02");
        assert_eq!(advisory.date, "2026-01-26");
        assert_eq!(advisory.severity, Some("high".to_string()));
        assert_eq!(advisory.cves, vec!["CVE-2025-53905", "CVE-2025-53906"]);
        assert_eq!(advisory.affected_packages.len(), 2);

        let vim_pkg = &advisory.affected_packages[0];
        assert_eq!(vim_pkg.atom, "app-editors/gvim");
        assert_eq!(
            vim_pkg.vulnerable_ranges,
            vec![("lt".to_string(), "9.1.1652".to_string())]
        );
        assert_eq!(
            vim_pkg.unaffected_ranges,
            vec![("ge".to_string(), "9.1.1652".to_string())]
        );
    }

    #[test]
    fn test_parse_multi_package_glsa() {
        let advisory =
            GlsaAdvisory::from_xml(SAMPLE_GLSA_XML).expect("Should parse multi-package GLSA");

        assert_eq!(advisory.affected_packages.len(), 2);
        assert_eq!(advisory.affected_packages[0].atom, "app-editors/gvim");
        assert_eq!(advisory.affected_packages[1].atom, "app-editors/vim");
    }

    #[test]
    fn test_cve_extraction_from_references() {
        let advisory = GlsaAdvisory::from_xml(SAMPLE_GLSA_XML).expect("Should parse GLSA");

        assert_eq!(advisory.cves.len(), 2);
        assert!(advisory.cves.contains(&"CVE-2025-53905".to_string()));
        assert!(advisory.cves.contains(&"CVE-2025-53906".to_string()));
    }

    #[test]
    fn test_affected_range_emission() {
        use std::io::Read;
        use tempfile::NamedTempFile;

        let tmp = NamedTempFile::new().unwrap();
        let file = tmp.reopen().unwrap();
        let mut writer = NTriplesWriter::new(file);

        let advisory_uri = "https://packagegraph.github.io/d/advisory/gentoo/GLSA-202601-02";
        let affected_pkg = GlsaAffectedPackage {
            atom: "app-editors/vim".to_string(),
            vulnerable_ranges: vec![("lt".to_string(), "9.1.1652".to_string())],
            unaffected_ranges: vec![("ge".to_string(), "9.1.1652".to_string())],
        };

        // Simulate collector's emit_affected_range call
        let range_input = format!("{}_{}", "GLSA-202601-02", affected_pkg.atom);
        let range_bnode = bnode_id("ar", &range_input);

        // NOTE: hasAffectedRange attaches to Vulnerability, not Advisory
        let cve_uri = "https://packagegraph.github.io/d/cve/CVE-2025-53905";
        writer
            .write_bnode_object(cve_uri, &format!("{SEC}hasAffectedRange"), &range_bnode)
            .unwrap();
        writer
            .write_bnode_subject(&range_bnode, RDF_TYPE, &format!("{SEC}AffectedRange"))
            .unwrap();
        writer
            .write_bnode_subject(
                &range_bnode,
                &format!("{SEC}rangeType"),
                &range_type_uri("ECOSYSTEM"),
            )
            .unwrap();

        // Introduced event
        let intro_bnode = format!("{}_intro", range_bnode);
        writer
            .write_bnode_to_bnode(&range_bnode, &format!("{SEC}hasRangeEvent"), &intro_bnode)
            .unwrap();
        writer
            .write_bnode_subject(&intro_bnode, RDF_TYPE, &format!("{SEC}RangeEvent"))
            .unwrap();
        writer
            .write_bnode_subject(
                &intro_bnode,
                &format!("{SEC}eventType"),
                &event_type_uri("introduced"),
            )
            .unwrap();
        writer
            .write_bnode_literal(&intro_bnode, &format!("{SEC}eventVersion"), "0")
            .unwrap();

        // Fixed event
        let fix_bnode = format!("{}_fix0", range_bnode);
        writer
            .write_bnode_to_bnode(&range_bnode, &format!("{SEC}hasRangeEvent"), &fix_bnode)
            .unwrap();
        writer
            .write_bnode_subject(&fix_bnode, RDF_TYPE, &format!("{SEC}RangeEvent"))
            .unwrap();
        writer
            .write_bnode_subject(
                &fix_bnode,
                &format!("{SEC}eventType"),
                &event_type_uri("fixed"),
            )
            .unwrap();
        writer
            .write_bnode_literal(&fix_bnode, &format!("{SEC}eventVersion"), "9.1.1652")
            .unwrap();

        writer.flush().unwrap();

        let mut content = String::new();
        tmp.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("hasAffectedRange"));
        assert!(content.contains("AffectedRange"));
        assert!(content.contains("rangeType"));
        assert!(content.contains("hasRangeEvent"));
        assert!(content.contains("RangeEvent"));
        assert!(content.contains("event-introduced"));
        assert!(content.contains("event-fixed"));
        assert!(content.contains("eventVersion"));
    }

    #[test]
    fn test_deterministic_blank_nodes() {
        // Same input should produce same blank node IDs
        let range_input1 = format!("{}_{}", "GLSA-202601-02", "app-editors/vim");
        let range_input2 = format!("{}_{}", "GLSA-202601-02", "app-editors/vim");

        let bnode1 = bnode_id("ar", &range_input1);
        let bnode2 = bnode_id("ar", &range_input2);

        assert_eq!(bnode1, bnode2, "Blank node IDs must be deterministic");
    }
}
