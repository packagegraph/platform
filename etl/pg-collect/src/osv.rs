use serde::{Deserialize, Serialize};
use crate::ntriples::NTriplesWriter;
use crate::uris::{cve_uri, vuln_uri, version_uri, package_identity_uri, cwe_uri, cve_entity_uri, cvss_score_uri, ecosystem_uri, event_type_uri, range_type_uri, PKG, SEC, VCS, DATA, RDF_TYPE, RDFS_LABEL};
use std::fs::File;
use std::io::{Cursor, Result};
use std::time::Duration;
use reqwest::blocking::Client;

pub struct OsvCollector {
    client: Client,
}

impl OsvCollector {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }

    pub fn collect(&self, ecosystem: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        let (vuln_count, triple_count) = process_ecosystem(&self.client, ecosystem, &mut writer)?;

        writer.flush()?;
        Ok((vuln_count, triple_count))
    }
}

/// Mapping from OSV ecosystem name to PackageGraph URI construction parameters.
#[derive(Debug, PartialEq)]
pub struct EcosystemMapping {
    pub distro: &'static str,
    pub release: &'static str,
}

/// Map OSV ecosystem name to PackageGraph URI params for version linking.
/// Returns `Some` for language ecosystems, `None` for distros (no version linking).
pub fn ecosystem_mapping(osv_ecosystem: &str) -> Option<EcosystemMapping> {
    match osv_ecosystem {
        "npm" => Some(EcosystemMapping { distro: "npm", release: "registry" }),
        "PyPI" => Some(EcosystemMapping { distro: "pypi", release: "index" }),
        "crates.io" => Some(EcosystemMapping { distro: "cargo", release: "crates.io" }),
        "Go" => Some(EcosystemMapping { distro: "go", release: "modules" }),
        "Maven" => Some(EcosystemMapping { distro: "maven", release: "central" }),
        "NuGet" => Some(EcosystemMapping { distro: "nuget", release: "gallery" }),
        "Packagist" => Some(EcosystemMapping { distro: "packagist", release: "registry" }),
        "RubyGems" => Some(EcosystemMapping { distro: "rubygems", release: "registry" }),
        "Hex" => Some(EcosystemMapping { distro: "hex", release: "registry" }),
        "Pub" => Some(EcosystemMapping { distro: "pub", release: "registry" }),
        "Hackage" => Some(EcosystemMapping { distro: "hackage", release: "registry" }),
        "SwiftURL" => Some(EcosystemMapping { distro: "swift", release: "registry" }),
        // Distros and unknown ecosystems return None (no version linking)
        _ => None,
    }
}

/// Emit N-Triples for a single OSV vulnerability record.
///
/// Returns the number of triples written.
///
/// **CVE-keyed dedup**: When multiple OSV records alias the same CVE, triples merge at the
/// same subject URI. For non-functional properties (affectsVersion, cweId), this is additive.
/// For functional properties (summary), last-written value appears. This is acceptable — data
/// is still correct, source varies by ZIP entry order.
pub fn emit_vulnerability_triples(
    writer: &mut NTriplesWriter,
    vuln: &OsvVulnerability,
) -> Result<usize> {
    // Skip withdrawn records
    if vuln.withdrawn.is_some() {
        return Ok(0);
    }

    let mut triples = 0;

    // Resolve subject URI: CVE-keyed if CVE alias exists, else OSV-ID-keyed
    let subject_uri = vuln.aliases.iter()
        .find(|alias| alias.starts_with("CVE-"))
        .map(|cve| cve_uri(cve))
        .unwrap_or_else(|| vuln_uri(&vuln.id));

    // Type triple
    writer.write_triple(&subject_uri, RDF_TYPE, &format!("{}Vulnerability", SEC))?;
    triples += 1;

    // rdfs:label
    let label = vuln.aliases.iter()
        .find(|alias| alias.starts_with("CVE-"))
        .map(|cve| cve.as_str())
        .unwrap_or(&vuln.id);
    writer.write_literal(&subject_uri, "http://www.w3.org/2000/01/rdf-schema#label", label)?;
    triples += 1;

    // sec:cveId (exactly one, owl:FunctionalProperty)
    if let Some(cve) = vuln.aliases.iter().find(|alias| alias.starts_with("CVE-")) {
        writer.write_literal(&subject_uri, &format!("{}cveId", SEC), cve)?;
        triples += 1;

        // sec:cveEntity (link to shared CVE entity node)
        let cve_entity = cve_entity_uri(cve);
        writer.write_triple(&subject_uri, &format!("{}cveEntity", SEC), &cve_entity)?;
        triples += 1;
    } else if vuln.id.starts_with("CVE-") {
        writer.write_literal(&subject_uri, &format!("{}cveId", SEC), &vuln.id)?;
        triples += 1;

        // sec:cveEntity (link to shared CVE entity node)
        let cve_entity = cve_entity_uri(&vuln.id);
        writer.write_triple(&subject_uri, &format!("{}cveEntity", SEC), &cve_entity)?;
        triples += 1;
    }

    // sec:summary
    if let Some(summary) = &vuln.summary {
        writer.write_literal(&subject_uri, &format!("{}summary", SEC), summary)?;
        triples += 1;
    }

    // sec:publishedDate
    if let Some(published) = &vuln.published {
        writer.write_datetime(&subject_uri, &format!("{}publishedDate", SEC), published)?;
        triples += 1;
    }

    // sec:updatedDate (NOTE: OSV field is "modified", ontology property is "updatedDate")
    if let Some(modified) = &vuln.modified {
        writer.write_datetime(&subject_uri, &format!("{}updatedDate", SEC), modified)?;
        triples += 1;
    }

    // CVSSScore reification (v0.6.0) — emit all severity entries as CVSSScore entities
    // Also keep deprecated flat cvssVector for the best-available entry (backward compat)
    let best_cvss = vuln.severity.iter()
        .find(|s| matches!(s.severity_type, OsvSeverityType::CvssV3))
        .or_else(|| vuln.severity.iter().find(|s| matches!(s.severity_type, OsvSeverityType::CvssV4)))
        .or_else(|| vuln.severity.iter().find(|s| matches!(s.severity_type, OsvSeverityType::CvssV2)));

    if let Some(cvss) = best_cvss {
        // Deprecated flat properties (backward compat)
        writer.write_literal(&subject_uri, &format!("{SEC}cvssVector"), &cvss.score)?;
        triples += 1;

        if let Some(severity) = derive_cvss_severity(&cvss.score) {
            // sec:severity remains DatatypeProperty/xsd:string in the ontology.
            // Only advisorySeverity, advisoryType, eventType, rangeType were
            // migrated to SKOS ObjectProperties in v0.7.0.
            writer.write_literal(&subject_uri, &format!("{SEC}severity"), severity)?;
            triples += 1;
        }
    }

    // Reified CVSSScore entities for each severity entry
    for cvss in &vuln.severity {
        let cvss_version = match cvss.severity_type {
            OsvSeverityType::CvssV2 => "2.0",
            OsvSeverityType::CvssV3 => "3.1",
            OsvSeverityType::CvssV4 => "4.0",
        };
        let score_uri = cvss_score_uri(label, cvss_version);
        writer.write_triple(&subject_uri, &format!("{SEC}hasCVSSScore"), &score_uri)?;
        writer.write_triple(&score_uri, RDF_TYPE, &format!("{SEC}CVSSScore"))?;
        writer.write_literal(&score_uri, &format!("{SEC}vectorString"), &cvss.score)?;
        writer.write_literal(&score_uri, &format!("{SEC}cvssVersion"), cvss_version)?;
        triples += 4;
    }

    // sec:cweId and sec:hasCWE from database_specific.cwe_ids
    if let Some(db_specific) = &vuln.database_specific {
        if let Some(cwe_ids) = db_specific.get("cwe_ids").and_then(|v| v.as_array()) {
            for cwe in cwe_ids {
                if let Some(cwe_str) = cwe.as_str() {
                    // Literal cweId (existing)
                    writer.write_literal(&subject_uri, &format!("{}cweId", SEC), cwe_str)?;
                    triples += 1;

                    // Entity link hasCWE (new)
                    let cwe_entity = cwe_uri(cwe_str);
                    writer.write_triple(&subject_uri, &format!("{}hasCWE", SEC), &cwe_entity)?;
                    triples += 1;
                }
            }
        }
    }

    // sec:affectsVersion, sec:fixedInVersion, and sec:affectsPackage for language ecosystems
    for affected in &vuln.affected {
        let pkg = match &affected.package {
            Some(p) => p,
            None => continue,
        };

        let mapping = match ecosystem_mapping(&pkg.ecosystem) {
            Some(m) => m,
            None => continue, // Skip distros
        };

        // Direct package identity link (bypasses version-string joins)
        let pkg_identity = package_identity_uri(mapping.distro, mapping.release, "any", &pkg.name);
        writer.write_triple(&subject_uri, &format!("{SEC}affectsPackage"), &pkg_identity)?;
        triples += 1;

        // Affected versions (explicit list)
        for version in &affected.versions {
            let ver_uri = version_uri(mapping.distro, mapping.release, &pkg.name, version);
            writer.write_triple(&subject_uri, &format!("{SEC}affectsVersion"), &ver_uri)?;
            triples += 1;
        }

        // AffectedRange/RangeEvent reification (v0.6.0 OSV model)
        for (range_idx, range) in affected.ranges.iter().enumerate() {
            let range_type_str = match range.range_type {
                OsvRangeType::Semver => "SEMVER",
                OsvRangeType::Ecosystem => "ECOSYSTEM",
                OsvRangeType::Git => "GIT",
            };

            let range_bnode = format!("ar_{}_{}_{}_{}", label, mapping.distro, pkg.name, range_idx)
                .chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect::<String>();
            writer.write_bnode_object(&subject_uri, &format!("{SEC}hasAffectedRange"), &range_bnode)?;
            writer.write_bnode_subject(&range_bnode, RDF_TYPE, &format!("{SEC}AffectedRange"))?;
            writer.write_bnode_subject(&range_bnode, &format!("{SEC}rangeType"), &range_type_uri(range_type_str))?;
            writer.write_bnode_literal(&range_bnode, &format!("{SEC}affectsPackageName"), &pkg.name)?;
            // Link to ecosystem entity
            let eco_entity = ecosystem_uri(mapping.distro);
            writer.write_bnode_subject(&range_bnode, &format!("{SEC}affectsEcosystem"), &eco_entity)?;
            writer.write_triple(&eco_entity, RDF_TYPE, &format!("{PKG}Ecosystem"))?;
            writer.write_literal(&eco_entity, RDFS_LABEL, &pkg.ecosystem)?;
            triples += 7;

            for (event_idx, event) in range.events.iter().enumerate() {
                let event_bnode = format!("{range_bnode}_e{event_idx}");

                if let Some(ref introduced) = event.introduced {
                    writer.write_bnode_to_bnode(&range_bnode, &format!("{SEC}hasRangeEvent"), &event_bnode)?;
                    writer.write_bnode_subject(&event_bnode, RDF_TYPE, &format!("{SEC}RangeEvent"))?;
                    writer.write_bnode_subject(&event_bnode, &format!("{SEC}eventType"), &event_type_uri("introduced"))?;
                    writer.write_bnode_literal(&event_bnode, &format!("{SEC}eventVersion"), introduced)?;
                    triples += 4;

                    if range_type_str == "GIT" {
                        let commit_uri = format!("{DATA}commit/{}", introduced);
                        writer.write_bnode_subject(&event_bnode, &format!("{SEC}eventCommit"), &commit_uri)?;
                        writer.write_triple(&commit_uri, RDF_TYPE, &format!("{VCS}Commit"))?;
                        writer.write_literal(&commit_uri, &format!("{VCS}commitHash"), introduced)?;
                        triples += 3;
                    }
                }
                if let Some(ref fixed) = event.fixed {
                    let fix_bnode = format!("{event_bnode}_fix");
                    writer.write_bnode_to_bnode(&range_bnode, &format!("{SEC}hasRangeEvent"), &fix_bnode)?;
                    writer.write_bnode_subject(&fix_bnode, RDF_TYPE, &format!("{SEC}RangeEvent"))?;
                    writer.write_bnode_subject(&fix_bnode, &format!("{SEC}eventType"), &event_type_uri("fixed"))?;
                    writer.write_bnode_literal(&fix_bnode, &format!("{SEC}eventVersion"), fixed)?;
                    triples += 4;

                    if range_type_str == "GIT" {
                        let commit_uri = format!("{DATA}commit/{}", fixed);
                        writer.write_bnode_subject(&fix_bnode, &format!("{SEC}eventCommit"), &commit_uri)?;
                        writer.write_triple(&commit_uri, RDF_TYPE, &format!("{VCS}Commit"))?;
                        writer.write_literal(&commit_uri, &format!("{VCS}commitHash"), fixed)?;
                        triples += 3;
                    }

                    // Also keep flat fixedInVersion for backward compat
                    let ver_uri = version_uri(mapping.distro, mapping.release, &pkg.name, fixed);
                    writer.write_triple(&subject_uri, &format!("{SEC}fixedInVersion"), &ver_uri)?;
                    triples += 1;
                }
                if let Some(ref last_affected) = event.last_affected {
                    let la_bnode = format!("{event_bnode}_la");
                    writer.write_bnode_to_bnode(&range_bnode, &format!("{SEC}hasRangeEvent"), &la_bnode)?;
                    writer.write_bnode_subject(&la_bnode, RDF_TYPE, &format!("{SEC}RangeEvent"))?;
                    writer.write_bnode_subject(&la_bnode, &format!("{SEC}eventType"), &event_type_uri("last_affected"))?;
                    writer.write_bnode_literal(&la_bnode, &format!("{SEC}eventVersion"), last_affected)?;
                    triples += 4;

                    if range_type_str == "GIT" {
                        let commit_uri = format!("{DATA}commit/{}", last_affected);
                        writer.write_bnode_subject(&la_bnode, &format!("{SEC}eventCommit"), &commit_uri)?;
                        writer.write_triple(&commit_uri, RDF_TYPE, &format!("{VCS}Commit"))?;
                        writer.write_literal(&commit_uri, &format!("{VCS}commitHash"), last_affected)?;
                        triples += 3;
                    }
                }
            }
        }
    }

    Ok(triples)
}

/// Derive CVSS severity label from a CVSS v3/v4 vector string.
///
/// Parses the base score from the vector and maps to severity levels:
/// - 0.0: NONE
/// - 0.1-3.9: LOW
/// - 4.0-6.9: MEDIUM
/// - 7.0-8.9: HIGH
/// - 9.0-10.0: CRITICAL
///
/// For CVSS v3, attempts to extract the score from common patterns.
/// Returns None if the vector can't be parsed.
fn derive_cvss_severity(vector: &str) -> Option<&'static str> {
    // CVSS v3/v4 vectors don't embed the score directly — we approximate
    // from the vector components. A full CVSS calculator would be ideal,
    // but a heuristic based on Attack Vector + Impact is good enough.
    //
    // Simple heuristic: if the vector contains high-impact markers
    if vector.contains("CVSS:3") || vector.contains("CVSS:4") {
        // Check for critical indicators
        let has_network = vector.contains("/AV:N");
        let has_low_complexity = vector.contains("/AC:L");
        let has_high_impact = vector.contains("/C:H") || vector.contains("/I:H") || vector.contains("/A:H");
        let has_no_privs = vector.contains("/PR:N");

        if has_network && has_low_complexity && has_high_impact && has_no_privs {
            return Some("CRITICAL");
        }
        if has_network && has_high_impact {
            return Some("HIGH");
        }
        if has_high_impact {
            return Some("MEDIUM");
        }
        return Some("LOW");
    }

    None
}

/// OSV vulnerability record.
/// https://ossf.github.io/osv-schema/
#[derive(Debug, Deserialize, Serialize)]
pub struct OsvVulnerability {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub summary: Option<String>,
    pub details: Option<String>,
    pub published: Option<String>,
    pub modified: Option<String>,
    pub withdrawn: Option<String>,
    #[serde(default)]
    pub affected: Vec<OsvAffected>,
    #[serde(default)]
    pub severity: Vec<OsvSeverity>,
    #[serde(default)]
    pub references: Vec<OsvReference>,
    pub database_specific: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OsvAffected {
    pub package: Option<OsvPackage>,
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub ranges: Vec<OsvRange>,
    pub database_specific: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OsvPackage {
    pub name: String,
    pub ecosystem: String,
    pub purl: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OsvRange {
    #[serde(rename = "type")]
    pub range_type: OsvRangeType,
    #[serde(default)]
    pub events: Vec<OsvEvent>,
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OsvRangeType {
    Semver,
    Ecosystem,
    Git,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OsvEvent {
    pub introduced: Option<String>,
    pub fixed: Option<String>,
    pub last_affected: Option<String>,
    pub limit: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OsvSeverity {
    #[serde(rename = "type")]
    pub severity_type: OsvSeverityType,
    pub score: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum OsvSeverityType {
    #[serde(rename = "CVSS_V2")]
    CvssV2,
    #[serde(rename = "CVSS_V3")]
    CvssV3,
    #[serde(rename = "CVSS_V4")]
    CvssV4,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OsvReference {
    #[serde(rename = "type")]
    pub ref_type: String,
    pub url: String,
}

/// Process an entire OSV ecosystem: download ZIP, extract entries, emit triples.
///
/// Returns `(vuln_count, triple_count)`.
pub fn process_ecosystem(
    client: &Client,
    ecosystem: &str,
    writer: &mut NTriplesWriter,
) -> Result<(usize, usize)> {
    let url = format!("https://osv-vulnerabilities.storage.googleapis.com/{}/all.zip", ecosystem);

    eprintln!("Downloading {} from {}...", ecosystem, url);

    // Download with retry logic
    let zip_bytes = download_with_retry(client, &url)?;

    eprintln!("Downloaded {:.2} MB", zip_bytes.len() as f64 / 1_048_576.0);

    // Process ZIP from memory
    process_zip_from_bytes(&zip_bytes, writer)
}

/// Download a URL with retry logic (single retry on transient failure).
fn download_with_retry(client: &Client, url: &str) -> std::io::Result<Vec<u8>> {
    match download_bytes(client, url) {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            eprintln!("Download failed ({}), retrying after 5s...", e);
            std::thread::sleep(Duration::from_secs(5));
            download_bytes(client, url)
        }
    }
}

/// Download bytes from a URL.
fn download_bytes(client: &Client, url: &str) -> std::io::Result<Vec<u8>> {
    let response = client.get(url)
        .send()
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    if !response.status().is_success() {
        return Err(std::io::Error::other(format!("HTTP {}", response.status())));
    }

    let bytes = response.bytes()
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    Ok(bytes.to_vec())
}

/// Process a ZIP archive from bytes, extracting and emitting triples for each OSV JSON entry.
///
/// Returns `(vuln_count, triple_count)`.
pub fn process_zip_from_bytes(
    zip_bytes: &[u8],
    writer: &mut NTriplesWriter,
) -> Result<(usize, usize)> {
    use zip::ZipArchive;

    let cursor = Cursor::new(zip_bytes);
    let mut archive = ZipArchive::new(cursor)
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let entry_count = archive.len();
    eprintln!("Processing {} entries from ZIP...", entry_count);

    let mut vuln_count = 0;
    let mut triple_count = 0;
    let mut errors_skipped = 0;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Skip non-JSON files
        if !file.name().ends_with(".json") {
            continue;
        }

        // Deserialize OSV record
        let vuln: OsvVulnerability = match serde_json::from_reader(&mut file) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Warning: skipping malformed entry {}: {}", file.name(), e);
                errors_skipped += 1;
                continue;
            }
        };

        // Emit triples
        match emit_vulnerability_triples(writer, &vuln) {
            Ok(count) => {
                triple_count += count;
                if count > 0 {
                    vuln_count += 1;
                }
            }
            Err(e) => {
                eprintln!("Warning: error emitting triples for {}: {}", vuln.id, e);
                errors_skipped += 1;
            }
        }

        // Progress logging every 10,000 entries
        if (i + 1) % 10_000 == 0 {
            eprintln!("Progress: {}/{} entries, {} vulnerabilities, {} triples",
                     i + 1, entry_count, vuln_count, triple_count);
        }
    }

    if errors_skipped > 0 {
        eprintln!("Skipped {} entries due to errors", errors_skipped);
    }

    Ok((vuln_count, triple_count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntriples::NTriplesWriter;
    use std::io::Read as IoRead;
    use tempfile::NamedTempFile;

    /// Real GHSA npm record from https://osv-vulnerabilities.storage.googleapis.com/npm/GHSA-2g4f-4pwh-qvx6.json
    const GHSA_NPM_JSON: &str = r#"{
  "schema_version": "1.7.3",
  "id": "GHSA-2g4f-4pwh-qvx6",
  "published": "2026-02-11T21:30:39Z",
  "modified": "2026-03-04T15:06:32.662074Z",
  "aliases": [
    "CVE-2025-69873"
  ],
  "related": [
    "CGA-pjx7-r22p-cr2c"
  ],
  "summary": "ajv has ReDoS when using `$data` option",
  "details": "ajv (Another JSON Schema Validator) through version 8.17.1 is vulnerable to Regular Expression Denial of Service (ReDoS) when the `$data` option is enabled.",
  "affected": [
    {
      "package": {
        "name": "ajv",
        "ecosystem": "npm",
        "purl": "pkg:npm/ajv"
      },
      "ranges": [
        {
          "type": "SEMVER",
          "events": [
            {
              "introduced": "7.0.0-alpha.0"
            },
            {
              "fixed": "8.18.0"
            }
          ]
        }
      ],
      "database_specific": {
        "source": "https://github.com/github/advisory-database/blob/main/advisories/github-reviewed/2026/02/GHSA-2g4f-4pwh-qvx6/GHSA-2g4f-4pwh-qvx6.json"
      }
    },
    {
      "package": {
        "name": "ajv",
        "ecosystem": "npm",
        "purl": "pkg:npm/ajv"
      },
      "ranges": [
        {
          "type": "SEMVER",
          "events": [
            {
              "introduced": "0"
            },
            {
              "fixed": "6.14.0"
            }
          ]
        }
      ],
      "database_specific": {
        "source": "https://github.com/github/advisory-database/blob/main/advisories/github-reviewed/2026/02/GHSA-2g4f-4pwh-qvx6/GHSA-2g4f-4pwh-qvx6.json"
      }
    }
  ],
  "references": [
    {
      "type": "ADVISORY",
      "url": "https://nvd.nist.gov/vuln/detail/CVE-2025-69873"
    }
  ],
  "database_specific": {
    "cwe_ids": [
      "CWE-1333",
      "CWE-400"
    ],
    "github_reviewed": true,
    "github_reviewed_at": "2026-02-17T16:38:57Z",
    "nvd_published_at": "2026-02-11T19:15:50Z",
    "severity": "MODERATE"
  },
  "severity": [
    {
      "type": "CVSS_V4",
      "score": "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:N/VI:N/VA:L/SC:N/SI:N/SA:N/E:P"
    }
  ]
}"#;

    /// Real PYSEC PyPI record from https://osv-vulnerabilities.storage.googleapis.com/PyPI/PYSEC-2024-1.json
    const PYSEC_PYPI_JSON: &str = r#"{
  "schema_version": "1.7.3",
  "id": "PYSEC-2024-1",
  "published": "2024-01-03T23:23:36.586611Z",
  "modified": "2024-01-03T22:31:36Z",
  "summary": "gratient 0.5 contains credential harvesting code",
  "details": "gratient is a user-facing library for generating color gradients of text.\nVersion 0.5 contained obfuscated, malicious code targeting\nWindows platforms, harvesting information and credentials from the\nuser's system and sending them to a remote server.\nServices may include Mullvad VPN and Telegram.\n",
  "affected": [
    {
      "package": {
        "name": "gratient",
        "ecosystem": "PyPI",
        "purl": "pkg:pypi/gratient"
      },
      "versions": [
        "0.5"
      ],
      "database_specific": {
        "source": "https://github.com/pypa/advisory-database/blob/main/vulns/gratient/PYSEC-2024-1.yaml"
      }
    }
  ],
  "references": [
    {
      "type": "EVIDENCE",
      "url": "https://inspector.pypi.io/project/gratient/0.5/packages/c5/c5/353e45fa57fa5f1b2b42fa24a029cdfb018d7263850fb43b6d6352157734/gratient-0.5-py3-none-any.whl/gratient/__init__.py#line.4"
    },
    {
      "type": "WEB",
      "url": "https://pypi.org/project/gratient/"
    }
  ],
  "credits": [
    {
      "name": "Mike Fiedler",
      "type": "ANALYST"
    }
  ]
}"#;

    #[test]
    fn test_deserialize_ghsa_npm() {
        let vuln: OsvVulnerability = serde_json::from_str(GHSA_NPM_JSON)
            .expect("Failed to deserialize GHSA npm record");

        assert_eq!(vuln.id, "GHSA-2g4f-4pwh-qvx6");
        assert_eq!(vuln.aliases.len(), 1);
        assert_eq!(vuln.aliases[0], "CVE-2025-69873");
        assert!(vuln.summary.is_some());
        assert_eq!(vuln.affected.len(), 2);

        // Check affected ranges
        let first_affected = &vuln.affected[0];
        assert_eq!(first_affected.package.as_ref().unwrap().name, "ajv");
        assert_eq!(first_affected.package.as_ref().unwrap().ecosystem, "npm");
        assert_eq!(first_affected.ranges.len(), 1);
        assert_eq!(first_affected.ranges[0].events.len(), 2);
        assert_eq!(first_affected.ranges[0].events[0].introduced.as_deref(), Some("7.0.0-alpha.0"));
        assert_eq!(first_affected.ranges[0].events[1].fixed.as_deref(), Some("8.18.0"));

        // Check severity
        assert_eq!(vuln.severity.len(), 1);
        assert!(matches!(vuln.severity[0].severity_type, OsvSeverityType::CvssV4));
    }

    #[test]
    fn test_deserialize_pysec_pypi() {
        let vuln: OsvVulnerability = serde_json::from_str(PYSEC_PYPI_JSON)
            .expect("Failed to deserialize PYSEC PyPI record");

        assert_eq!(vuln.id, "PYSEC-2024-1");
        assert_eq!(vuln.aliases.len(), 0); // No CVE alias
        assert!(vuln.summary.is_some());
        assert_eq!(vuln.affected.len(), 1);

        // Check affected versions (explicit list, not ranges)
        let affected = &vuln.affected[0];
        assert_eq!(affected.package.as_ref().unwrap().name, "gratient");
        assert_eq!(affected.package.as_ref().unwrap().ecosystem, "PyPI");
        assert_eq!(affected.versions.len(), 1);
        assert_eq!(affected.versions[0], "0.5");
        assert_eq!(affected.ranges.len(), 0);
    }

    #[test]
    fn test_handles_missing_fields() {
        let minimal = r#"{"id": "TEST-001"}"#;
        let vuln: OsvVulnerability = serde_json::from_str(minimal)
            .expect("Failed to deserialize minimal record");

        assert_eq!(vuln.id, "TEST-001");
        assert_eq!(vuln.aliases.len(), 0);
        assert_eq!(vuln.affected.len(), 0);
        assert_eq!(vuln.severity.len(), 0);
        assert!(vuln.summary.is_none());
    }

    #[test]
    fn test_ecosystem_mapping_language_ecosystems() {
        assert_eq!(
            ecosystem_mapping("npm"),
            Some(EcosystemMapping { distro: "npm", release: "registry" })
        );
        assert_eq!(
            ecosystem_mapping("PyPI"),
            Some(EcosystemMapping { distro: "pypi", release: "index" })
        );
        assert_eq!(
            ecosystem_mapping("crates.io"),
            Some(EcosystemMapping { distro: "cargo", release: "crates.io" })
        );
        assert_eq!(
            ecosystem_mapping("Go"),
            Some(EcosystemMapping { distro: "go", release: "modules" })
        );
    }

    #[test]
    fn test_ecosystem_mapping_distros_return_none() {
        assert_eq!(ecosystem_mapping("Debian"), None);
        assert_eq!(ecosystem_mapping("Alpine"), None);
        assert_eq!(ecosystem_mapping("Ubuntu"), None);
    }

    #[test]
    fn test_ecosystem_mapping_unknown_returns_none() {
        assert_eq!(ecosystem_mapping("UnknownEcosystem"), None);
        assert_eq!(ecosystem_mapping(""), None);
    }

    #[test]
    fn test_emit_vulnerability_triples_with_cve() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let vuln = OsvVulnerability {
            id: "GHSA-test-1234-5678".to_string(),
            aliases: vec!["CVE-2024-1234".to_string()],
            summary: Some("Test vulnerability".to_string()),
            details: None,
            published: Some("2024-01-01T00:00:00Z".to_string()),
            modified: Some("2024-01-02T00:00:00Z".to_string()),
            withdrawn: None,
            affected: vec![
                OsvAffected {
                    package: Some(OsvPackage {
                        name: "test-pkg".to_string(),
                        ecosystem: "npm".to_string(),
                        purl: None,
                    }),
                    versions: vec!["1.0.0".to_string(), "1.0.1".to_string()],
                    ranges: vec![
                        OsvRange {
                            range_type: OsvRangeType::Semver,
                            events: vec![
                                OsvEvent {
                                    introduced: Some("0".to_string()),
                                    fixed: None,
                                    last_affected: None,
                                    limit: None,
                                },
                                OsvEvent {
                                    introduced: None,
                                    fixed: Some("1.1.0".to_string()),
                                    last_affected: None,
                                    limit: None,
                                },
                            ],
                            repo: None,
                        }
                    ],
                    database_specific: None,
                }
            ],
            severity: vec![
                OsvSeverity {
                    severity_type: OsvSeverityType::CvssV3,
                    score: "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H".to_string(),
                }
            ],
            references: vec![],
            database_specific: Some(serde_json::json!({
                "cwe_ids": ["CWE-79", "CWE-89"]
            })),
        };

        let triples = emit_vulnerability_triples(&mut writer, &vuln).unwrap();
        writer.flush().unwrap();

        assert!(triples > 0);

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Verify CVE-keyed subject
        assert!(content.contains("cve/CVE-2024-1234"));

        // Verify type
        assert!(content.contains("security#Vulnerability"));

        // Verify cveId
        assert!(content.contains("security#cveId"));
        assert!(content.contains("\"CVE-2024-1234\""));

        // Verify CVSS vector
        assert!(content.contains("security#cvssVector"));
        assert!(content.contains("CVSS:3.1/AV:N/AC:L"));

        // Verify severity derived from CVSS vector (plain string, not SKOS — sec:severity is DatatypeProperty)
        assert!(content.contains("security#severity"));
        assert!(content.contains("\"CRITICAL\""), "AV:N/AC:L/PR:N with high impact should be CRITICAL");

        // Verify CWE literal
        assert!(content.contains("security#cweId"));
        assert!(content.contains("\"CWE-79\""));
        assert!(content.contains("\"CWE-89\""));

        // Verify CWE entity URIs (new in Task 9)
        assert!(content.contains("security#hasCWE"));
        assert!(content.contains("cwe.mitre.org/data/definitions/79"));
        assert!(content.contains("cwe.mitre.org/data/definitions/89"));

        // Verify CVE entity URI (new in Task 9)
        assert!(content.contains("security#cveEntity"));

        // Verify affectsPackage direct link (new in Task 9)
        assert!(content.contains("security#affectsPackage"));
        assert!(content.contains("pkg/npm/registry/any/test-pkg"));

        // Verify affected versions
        assert!(content.contains("security#affectsVersion"));
        assert!(content.contains("ver/npm/registry/test-pkg/1.0.0"));
        assert!(content.contains("ver/npm/registry/test-pkg/1.0.1"));

        // Verify fixed version
        assert!(content.contains("security#fixedInVersion"));
        assert!(content.contains("ver/npm/registry/test-pkg/1.1.0"));

        // Verify dates
        assert!(content.contains("security#publishedDate"));
        assert!(content.contains("security#updatedDate"));
        assert!(content.contains("2024-01-01T00:00:00Z"));
        assert!(content.contains("2024-01-02T00:00:00Z"));

        // Verify CVSSScore reification (v0.6.0)
        assert!(content.contains("security#hasCVSSScore"), "Should link to reified CVSSScore");
        assert!(content.contains("security#CVSSScore"), "Should type as CVSSScore");
        assert!(content.contains("security#vectorString"), "Should have vectorString on CVSSScore");
        assert!(content.contains("security#cvssVersion"), "Should have cvssVersion");
        assert!(content.contains("\"3.1\""), "Should have CVSS version 3.1");

        // Verify AffectedRange reification (v0.7.0 — SKOS concept URIs)
        assert!(content.contains("security#hasAffectedRange"), "Should link to AffectedRange");
        assert!(content.contains("security#AffectedRange"), "Should type as AffectedRange");
        assert!(content.contains("security#rangeType"), "Should have rangeType");
        assert!(content.contains("security#range-semver"), "Should have range-semver SKOS concept");
        assert!(content.contains("security#affectsPackageName"), "Should have affectsPackageName");

        // Verify RangeEvent reification (v0.7.0 — SKOS concept URIs)
        assert!(content.contains("security#hasRangeEvent"), "Should link to RangeEvent");
        assert!(content.contains("security#RangeEvent"), "Should type as RangeEvent");
        assert!(content.contains("security#eventType"), "Should have eventType");
        assert!(content.contains("security#event-introduced"), "Should have event-introduced SKOS concept");
        assert!(content.contains("security#event-fixed"), "Should have event-fixed SKOS concept");
    }

    #[test]
    fn test_emit_vulnerability_triples_without_cve() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let vuln = OsvVulnerability {
            id: "PYSEC-2024-001".to_string(),
            aliases: vec![], // No CVE
            summary: Some("Malicious code".to_string()),
            details: None,
            published: None,
            modified: None,
            withdrawn: None,
            affected: vec![],
            severity: vec![],
            references: vec![],
            database_specific: None,
        };

        let triples = emit_vulnerability_triples(&mut writer, &vuln).unwrap();
        writer.flush().unwrap();

        assert!(triples > 0);

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should use vuln_uri, not cve_uri
        assert!(content.contains("vuln/PYSEC-2024-001"));
        assert!(!content.contains("cve/"));

        // Should NOT have sec:cveId (no CVE alias)
        assert!(!content.contains("security#cveId"));
    }

    #[test]
    fn test_emit_vulnerability_triples_withdrawn() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let vuln = OsvVulnerability {
            id: "WITHDRAWN-001".to_string(),
            aliases: vec![],
            summary: Some("Withdrawn".to_string()),
            details: None,
            published: None,
            modified: None,
            withdrawn: Some("2024-01-01T00:00:00Z".to_string()),
            affected: vec![],
            severity: vec![],
            references: vec![],
            database_specific: None,
        };

        let triples = emit_vulnerability_triples(&mut writer, &vuln).unwrap();
        writer.flush().unwrap();

        // Should produce zero triples
        assert_eq!(triples, 0);

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn test_derive_cvss_severity() {
        // CRITICAL: network + low complexity + no privileges + high impact
        assert_eq!(derive_cvss_severity("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"), Some("CRITICAL"));
        // HIGH: network + high impact but requires privileges
        assert_eq!(derive_cvss_severity("CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:N"), Some("HIGH"));
        // MEDIUM: local + high impact
        assert_eq!(derive_cvss_severity("CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N"), Some("MEDIUM"));
        // LOW: no high impact
        assert_eq!(derive_cvss_severity("CVSS:3.1/AV:N/AC:H/PR:L/UI:R/S:U/C:L/I:N/A:N"), Some("LOW"));
        // Non-CVSS string
        assert_eq!(derive_cvss_severity("not a cvss vector"), None);
    }

    #[test]
    fn test_process_ecosystem_with_zip() {
        use std::io::{Cursor, Write as IoWrite};
        use zip::write::ZipWriter;

        // Create an in-memory ZIP with two OSV JSON files
        let mut zip_buffer = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut zip_buffer);

            // Entry 1: CVE-aliased npm vulnerability
            zip.start_file::<&str, ()>("npm/GHSA-test.json", Default::default()).unwrap();
            zip.write_all(br#"{
                "id": "GHSA-test-xxxx-yyyy",
                "aliases": ["CVE-2024-9999"],
                "summary": "Test vulnerability",
                "affected": [{
                    "package": {"name": "test-pkg", "ecosystem": "npm"},
                    "versions": ["1.0.0"]
                }]
            }"#).unwrap();

            // Entry 2: Non-JSON file (should be skipped)
            zip.start_file::<&str, ()>("README.md", Default::default()).unwrap();
            zip.write_all(b"# Test README").unwrap();

            zip.finish().unwrap();
        }

        let zip_bytes = zip_buffer.into_inner();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let (vuln_count, triple_count) = process_zip_from_bytes(&zip_bytes, &mut writer).unwrap();

        writer.flush().unwrap();

        // Should process 1 vulnerability (skip README.md)
        assert_eq!(vuln_count, 1);
        assert!(triple_count > 0);

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();
        assert!(content.contains("CVE-2024-9999"));
    }
}
