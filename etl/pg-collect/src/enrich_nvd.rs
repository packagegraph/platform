//! NVD CVE metadata enricher — canonical CVE publication dates, CVSS scores, and CWE mappings.
//!
//! Queries Fuseki for advisory-linked CVE IDs, paginates through NVD 2.0 REST API,
//! and emits sec:publishedDate, sec:hasCVSSScore, and sec:hasCWE triples for matched CVEs.

use crate::ntriples::{escape_literal, NTriplesWriter};
use crate::sparql::SparqlClient;
use crate::uris::*;
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Result, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const NVD_API_BASE: &str = "https://services.nvd.nist.gov/rest/json/cves/2.0";

pub struct NvdEnricher {
    client: Client,
    sparql: SparqlClient,
    api_key: Option<String>,
    cache_dir: Option<PathBuf>,
}

impl NvdEnricher {
    pub fn new(endpoint: &str, api_key: Option<String>, cache_dir: Option<&str>) -> Result<Self> {
        let client = crate::enricher::default_http_client();

        let cache_path = if let Some(dir) = cache_dir {
            let p = Path::new(dir).join("nvd");
            fs::create_dir_all(&p)?;
            Some(p)
        } else {
            None
        };

        Ok(Self {
            client,
            sparql: SparqlClient::new(endpoint),
            api_key,
            cache_dir: cache_path,
        })
    }

    /// Fetch NVD META file and extract SHA256 hash.
    fn fetch_meta_sha256(&self, feed_name: &str) -> Option<String> {
        let url = format!(
            "https://nvd.nist.gov/feeds/json/cve/2.0/nvdcve-2.0-{}.meta",
            feed_name
        );
        let resp = self.client.get(&url).send().ok()?;
        if !resp.status().is_success() { return None; }
        let text = resp.text().ok()?;
        // META format: key:value lines. Find sha256:XXXX
        for line in text.lines() {
            if let Some(hash) = line.strip_prefix("sha256:") {
                return Some(hash.trim().to_uppercase());
            }
        }
        None
    }

    /// Get cached feed bytes or download fresh. Returns compressed bytes.
    fn get_or_download_feed(&self, feed_name: &str) -> Result<Vec<u8>> {
        let url = format!(
            "https://nvd.nist.gov/feeds/json/cve/2.0/nvdcve-2.0-{}.json.gz",
            feed_name
        );

        // If cache is enabled, check META SHA256 against cached copy
        if let Some(ref cache_dir) = self.cache_dir {
            let cached_gz = cache_dir.join(format!("nvdcve-2.0-{}.json.gz", feed_name));
            let cached_sha = cache_dir.join(format!("nvdcve-2.0-{}.sha256", feed_name));

            // Fetch META to get current SHA256
            if let Some(remote_sha) = self.fetch_meta_sha256(feed_name) {
                // Check if cached SHA matches
                if cached_gz.exists() && cached_sha.exists() {
                    if let Ok(local_sha) = fs::read_to_string(&cached_sha) {
                        if local_sha.trim() == remote_sha {
                            eprintln!("  Cache hit (SHA256 match) — using cached {}", feed_name);
                            return fs::read(&cached_gz);
                        }
                    }
                }

                // Cache miss or SHA mismatch — download
                eprintln!("  Downloading {} ({})...", feed_name, url);
                let resp = self.client.get(&url).send()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
                if !resp.status().is_success() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("NVD feed {} returned {}", feed_name, resp.status()),
                    ));
                }
                let bytes = resp.bytes()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
                    .to_vec();

                // Save to cache
                let mut f = File::create(&cached_gz)?;
                f.write_all(&bytes)?;
                fs::write(&cached_sha, &remote_sha)?;
                eprintln!("  Cached {} ({} bytes)", feed_name, bytes.len());

                return Ok(bytes);
            }
        }

        // No cache — direct download
        let resp = match self.client.get(&url).send() {
            Ok(r) => r,
            Err(e) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to fetch {}: {}", feed_name, e),
                ));
            }
        };
        if !resp.status().is_success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("NVD feed {} returned {}", feed_name, resp.status()),
            ));
        }
        let bytes = resp.bytes()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
            .to_vec();
        Ok(bytes)
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        // Discover advisory-linked CVE IDs via SPARQL
        eprintln!("Discovering advisory-linked CVE IDs...");
        let advisory_linked_cves = self.discover_advisory_linked_cves()?;
        eprintln!("Found {} advisory-linked CVE entities", advisory_linked_cves.len());

        // Download NVD JSON 2.0 per-year feeds (no rate limiting, no API key needed)
        let mut total_nvd_cves = 0;
        let mut matched_cves = 0;
        let mut total_triples = 0;
        let mut with_published_date = 0;
        let mut with_cvss = 0;
        let mut with_cwe = 0;

        // Year range: CVE-2002 through current year + "Recent" and "Modified"
        let current_year = 2026; // Could derive from system time
        let mut feed_names: Vec<String> = (2002..=current_year).map(|y| y.to_string()).collect();
        feed_names.push("Recent".to_string());
        feed_names.push("Modified".to_string());

        // Track emitted CVEs to avoid double-counting across feeds (Recent/Modified overlap with year feeds)
        let mut emitted_cve_ids: HashSet<String> = HashSet::new();

        for feed_name in &feed_names {
            eprintln!("Processing NVD feed {}...", feed_name);

            let compressed = match self.get_or_download_feed(feed_name) {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!("  Warning: {} — skipping", e);
                    continue;
                }
            };

            eprintln!("  {} bytes, decompressing...", compressed.len());

            // Decompress gzip
            let mut decoder = GzDecoder::new(&compressed[..]);
            let mut decompressed = Vec::new();
            if let Err(e) = decoder.read_to_end(&mut decompressed) {
                eprintln!("  Warning: Decompression error for {}: {} — skipping", feed_name, e);
                continue;
            }

            // Parse JSON from bytes (avoids UTF-8 validation overhead of String)
            let nvd_response: NvdResponse = match serde_json::from_slice(&decompressed) {
                Ok(resp) => resp,
                Err(e) => {
                    eprintln!("  Warning: JSON parse error for {}: {} — skipping", feed_name, e);
                    continue;
                }
            };

            drop(decompressed); // Free memory before processing

            let mut feed_matched = 0;
            for vuln_wrapper in &nvd_response.vulnerabilities {
                let cve = &vuln_wrapper.cve;
                total_nvd_cves += 1;

                // Skip if already emitted (CVEs can appear in Recent/Modified AND their year feed)
                if emitted_cve_ids.contains(&cve.id) {
                    continue;
                }

                if advisory_linked_cves.contains(&cve.id) {
                    let emitted = self.emit_cve_triples(&mut writer, cve)?;
                    emitted_cve_ids.insert(cve.id.clone());
                    matched_cves += 1;
                    feed_matched += 1;
                    total_triples += emitted.triples;
                    if emitted.has_published_date { with_published_date += 1; }
                    if emitted.has_cvss { with_cvss += 1; }
                    if emitted.has_cwe { with_cwe += 1; }
                }
            }

            eprintln!("  {} CVEs in feed, {} advisory-linked matches", nvd_response.total_results, feed_matched);
        }

        writer.flush()?;

        eprintln!();
        eprintln!("Processed {} total NVD CVEs across {} feeds", total_nvd_cves, feed_names.len());
        eprintln!("Matched {} advisory-linked CVEs", matched_cves);
        eprintln!("  With publishedDate: {}", with_published_date);
        eprintln!("  With CVSS: {}", with_cvss);
        eprintln!("  With CWE: {}", with_cwe);

        Ok((matched_cves, total_triples))
    }

    /// Enrich CVEs using NVD REST API (per-CVE incremental mode).
    ///
    /// Discovers advisory-linked CVEs missing from the specified graph,
    /// fetches each from NVD API, and inserts triples via SPARQL UPDATE.
    ///
    /// Returns (matched_cves, total_triples).
    pub fn enrich_api(&self, graph_uri: &str) -> Result<(usize, usize)> {
        eprintln!("Discovering missing CVEs from {}...", graph_uri);
        let missing_cve_ids = self.discover_missing_cves(graph_uri)?;

        eprintln!("Found {} CVEs missing from {}", missing_cve_ids.len(), graph_uri);

        if missing_cve_ids.is_empty() {
            eprintln!("No gaps found — graph is up to date");
            return Ok((0, 0));
        }

        if missing_cve_ids.len() > 5000 {
            eprintln!("WARNING: Large gap set ({} CVEs) — consider using feed mode for bulk refresh", missing_cve_ids.len());
        }

        let total_missing = missing_cve_ids.len();
        let mut batch: Vec<String> = Vec::new();
        let mut matched_cves = 0;
        let mut total_triples = 0;
        let mut with_published_date = 0;
        let mut with_cvss = 0;
        let mut with_cwe = 0;

        for (idx, cve_id) in missing_cve_ids.iter().enumerate() {
            match self.fetch_cve_from_api(cve_id) {
                Ok(Some(cve)) => {
                    let (lines, stats) = format_cve_ntriples(&cve);
                    batch.extend(lines);
                    matched_cves += 1;
                    total_triples += stats.triples;
                    if stats.has_published_date { with_published_date += 1; }
                    if stats.has_cvss { with_cvss += 1; }
                    if stats.has_cwe { with_cwe += 1; }

                    // Flush batch when reaching 500 lines
                    if batch.len() >= 500 {
                        eprintln!("  Inserting batch ({} triples)...", batch.len());
                        self.sparql.insert_batch(&batch, graph_uri)?;
                        batch.clear();
                    }

                    // Progress reporting every 50 CVEs
                    if (idx + 1) % 50 == 0 {
                        eprintln!("  Processed {}/{} CVEs ({} triples inserted)", idx + 1, total_missing, total_triples);
                    }
                }
                Ok(None) => {
                    eprintln!("  Warning: {} not found in NVD — skipping", cve_id);
                }
                Err(e) => {
                    eprintln!("  Warning: Failed to fetch {}: {} — skipping", cve_id, e);
                }
            }
        }

        // Flush remaining batch
        if !batch.is_empty() {
            eprintln!("  Inserting final batch ({} triples)...", batch.len());
            self.sparql.insert_batch(&batch, graph_uri)?;
        }

        eprintln!();
        eprintln!("API mode enrichment complete:");
        eprintln!("  Matched {} CVEs from NVD", matched_cves);
        eprintln!("  Total triples: {}", total_triples);
        eprintln!("    With publishedDate: {}", with_published_date);
        eprintln!("    With CVSS: {}", with_cvss);
        eprintln!("    With CWE: {}", with_cwe);

        Ok((matched_cves, total_triples))
    }

    /// Discover all CVE entities linked from advisory graphs via SPARQL.
    fn discover_advisory_linked_cves(&self) -> Result<HashSet<String>> {
        let sparql = format!(
            r#"PREFIX sec: <{SEC}>
SELECT DISTINCT ?vuln WHERE {{
  GRAPH ?g {{ ?adv sec:addressesVulnerability ?vuln }}
}}"#
        );

        let bindings = self.sparql.query(&sparql)?;
        let cve_uris: HashSet<String> = bindings.into_iter()
            .filter_map(|b| b.get("vuln").cloned())
            .collect();

        // Extract CVE IDs from URIs (d/cve/CVE-YYYY-NNNNN → CVE-YYYY-NNNNN)
        let cve_ids: HashSet<String> = cve_uris.iter()
            .filter_map(|uri| uri.rsplit('/').next().map(|s| s.to_string()))
            .collect();

        Ok(cve_ids)
    }

    /// Discover CVE entities that exist in advisory graphs but are incompletely enriched.
    ///
    /// Returns a Vec of CVE IDs (e.g., "CVE-2025-1234") that are linked from advisories
    /// but are missing ANY of: sec:publishedDate, sec:hasCVSSScore, or sec:hasCWE in the NVD graph.
    ///
    /// This ensures API mode can backfill partial data (e.g., a CVE that has publishedDate
    /// but is missing CVSS/CWE metadata will be re-enriched).
    fn discover_missing_cves(&self, graph_uri: &str) -> Result<Vec<String>> {
        let sparql = format!(
            r#"PREFIX sec: <{SEC}>
SELECT DISTINCT ?vuln WHERE {{
  GRAPH ?g {{ ?adv sec:addressesVulnerability ?vuln }}
  FILTER NOT EXISTS {{
    GRAPH <{graph_uri}> {{
      ?vuln sec:publishedDate ?date .
      ?vuln sec:hasCVSSScore ?cvss .
      ?vuln sec:hasCWE ?cwe .
    }}
  }}
}}"#
        );

        let bindings = self.sparql.query(&sparql)?;
        let cve_uris: Vec<String> = bindings.into_iter()
            .filter_map(|b| b.get("vuln").cloned())
            .collect();

        // Extract CVE IDs from URIs (d/cve/CVE-YYYY-NNNNN → CVE-YYYY-NNNNN)
        let cve_ids: Vec<String> = cve_uris.iter()
            .filter_map(|uri| uri.rsplit('/').next().map(|s| s.to_string()))
            .collect();

        Ok(cve_ids)
    }

    /// Fetch a single CVE from the NVD REST API.
    ///
    /// Returns None if the CVE doesn't exist (totalResults=0).
    /// Applies rate limiting (700ms without API key, 100ms with key).
    /// Retries on 429/403/503 with exponential backoff.
    fn fetch_cve_from_api(&self, cve_id: &str) -> Result<Option<NvdCve>> {
        self.fetch_cve_from_api_with_base(NVD_API_BASE, cve_id)
    }

    /// Internal helper for fetch_cve_from_api with configurable base URL (for testing).
    fn fetch_cve_from_api_with_base(&self, base_url: &str, cve_id: &str) -> Result<Option<NvdCve>> {
        // Rate limiting: pre-delay to prevent burst at start
        // NVD uses a rolling 30-second window: 5 req/30s = 6s/req, 50 req/30s = 600ms/req
        let delay = match self.api_key {
            Some(_) => Duration::from_millis(650),  // 50 req/30s with key
            None => Duration::from_millis(6500),    // 5 req/30s without key
        };
        std::thread::sleep(delay);

        let url = format!("{}?cveId={}", base_url, cve_id);
        let max_retries = 3;

        for attempt in 0..=max_retries {
            let mut request = self.client.get(&url);

            // Add API key header if present
            if let Some(ref key) = self.api_key {
                request = request.header("apiKey", key);
            }

            match request.send() {
                Ok(response) if response.status().is_success() => {
                    let nvd_response: NvdResponse = response.json()
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to parse NVD response: {}", e)))?;

                    // If totalResults=0, CVE doesn't exist
                    if nvd_response.total_results == 0 || nvd_response.vulnerabilities.is_empty() {
                        return Ok(None);
                    }

                    return Ok(Some(nvd_response.vulnerabilities[0].cve.clone()));
                }
                Ok(response) if (response.status() == 429 || response.status() == 403 || response.status() == 503) && attempt < max_retries => {
                    let backoff = Duration::from_secs(2 * (1 << attempt)); // 2s, 4s, 8s
                    eprintln!("    NVD API {} for {}, retrying in {}s...", response.status(), cve_id, backoff.as_secs());
                    std::thread::sleep(backoff);
                }
                Ok(response) if response.status() == 404 => {
                    // CVE not found in NVD
                    return Ok(None);
                }
                Ok(response) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("NVD API request failed for {}: {}", cve_id, response.status()),
                    ));
                }
                Err(e) if attempt < max_retries => {
                    let backoff = Duration::from_secs(2 * (1 << attempt));
                    eprintln!("    NVD API error for {}: {}, retrying in {}s...", cve_id, e, backoff.as_secs());
                    std::thread::sleep(backoff);
                }
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("NVD API request failed for {}: {}", cve_id, e),
                    ));
                }
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("NVD API request failed for {} after {} retries", cve_id, max_retries),
        ))
    }

    /// Emit triples for a single CVE. Returns emission stats.
    fn emit_cve_triples(&self, writer: &mut NTriplesWriter, cve: &NvdCve) -> Result<CveEmissionStats> {
        let (lines, stats) = format_cve_ntriples(cve);

        for line in lines {
            writer.write_raw_line(&line)?;
        }

        Ok(stats)
    }
}

/// Format CVE metadata as N-Triple lines (standalone function for both feed and API modes).
///
/// Returns (Vec<String> of complete N-Triple lines, CveEmissionStats).
/// Each line is a complete triple ending with " ." like:
/// - `<s> <p> <o> .`
/// - `<s> <p> "lit" .`
/// - `<s> <p> "val"^^<dt> .`
fn format_cve_ntriples(cve: &NvdCve) -> (Vec<String>, CveEmissionStats) {
    let subject = cve_entity_uri(&cve.id);
    let mut lines = Vec::new();
    let mut stats = CveEmissionStats::default();

    // Type assertion
    lines.push(format!("<{subject}> <{RDF_TYPE}> <{SEC}Vulnerability> ."));
    let cve_id_escaped = escape_literal(&cve.id);
    lines.push(format!("<{subject}> <{SEC}cveId> \"{cve_id_escaped}\" ."));
    stats.triples += 2;

    // Published date
    if let Some(ref published) = cve.published {
        // Truncate milliseconds: "2025-02-21T13:15:11.687" → "2025-02-21T13:15:11Z"
        let truncated = published.split('.').next().unwrap_or(published).to_string() + "Z";
        let truncated_escaped = escape_literal(&truncated);
        lines.push(format!(
            "<{subject}> <{SEC}publishedDate> \"{truncated_escaped}\"^^<{XSD}dateTime> ."
        ));
        stats.triples += 1;
        stats.has_published_date = true;
    }

    // CVSS scores (emit all available versions as separate CVSSScore entities)
    if let Some(ref metrics) = cve.metrics {
        let mut emitted_cvss = false;
        let mut best_severity: Option<String> = None;

        // Priority order: v3.1, v4.0, v2
        for (cvss_metrics, version_label) in [
            (metrics.cvss_metric_v31.as_ref(), "3.1"),
            (metrics.cvss_metric_v40.as_ref(), "4.0"),
            (metrics.cvss_metric_v2.as_ref(), "2.0"),
        ] {
            if let Some(metrics_vec) = cvss_metrics {
                for cvss_metric in metrics_vec {
                    let score_uri = cvss_score_uri(&cve.id, version_label);
                    lines.push(format!("<{subject}> <{SEC}hasCVSSScore> <{score_uri}> ."));
                    lines.push(format!("<{score_uri}> <{RDF_TYPE}> <{SEC}CVSSScore> ."));

                    let vector_escaped = escape_literal(&cvss_metric.cvss_data.vector_string);
                    lines.push(format!("<{score_uri}> <{SEC}vectorString> \"{vector_escaped}\" ."));
                    lines.push(format!("<{score_uri}> <{SEC}cvssVersion> \"{version_label}\" ."));

                    // baseScore as decimal
                    let base_score_str = format!("{:.1}", cvss_metric.cvss_data.base_score);
                    lines.push(format!(
                        "<{score_uri}> <{SEC}baseScore> \"{base_score_str}\"^^<{XSD}decimal> ."
                    ));

                    stats.triples += 5;
                    emitted_cvss = true;

                    // Capture best severity for flat property
                    if best_severity.is_none() {
                        if let Some(ref sev) = cvss_metric.cvss_data.base_severity {
                            best_severity = Some(sev.clone());
                        }
                    }
                }
            }
        }

        // Emit flat severity for backward compat (from best-available CVSS)
        if let Some(sev) = best_severity {
            let sev_escaped = escape_literal(&sev);
            lines.push(format!("<{subject}> <{SEC}severity> \"{sev_escaped}\" ."));
            stats.triples += 1;
        }

        if emitted_cvss {
            stats.has_cvss = true;
        }
    }

    // CWE mappings
    if let Some(ref weaknesses) = cve.weaknesses {
        let mut emitted_cwe = false;
        for weakness in weaknesses {
            for desc in &weakness.description {
                if desc.lang == "en" {
                    let cwe_val = &desc.value;
                    // Filter NVD placeholders
                    if !cwe_val.starts_with("NVD-CWE-") {
                        let cwe_val_escaped = escape_literal(cwe_val);
                        lines.push(format!("<{subject}> <{SEC}cweId> \"{cwe_val_escaped}\" ."));
                        let cwe_entity = cwe_uri(cwe_val);
                        lines.push(format!("<{subject}> <{SEC}hasCWE> <{cwe_entity}> ."));
                        stats.triples += 2;
                        emitted_cwe = true;
                    }
                }
            }
        }
        if emitted_cwe {
            stats.has_cwe = true;
        }
    }

    (lines, stats)
}

#[derive(Default)]
struct CveEmissionStats {
    triples: usize,
    has_published_date: bool,
    has_cvss: bool,
    has_cwe: bool,
}

/// NVD 2.0 API response wrapper.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvdResponse {
    total_results: usize,
    vulnerabilities: Vec<NvdVulnerability>,
}

/// NVD vulnerability wrapper.
#[derive(Debug, Deserialize)]
struct NvdVulnerability {
    cve: NvdCve,
}

/// NVD CVE metadata.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvdCve {
    id: String,
    published: Option<String>,
    last_modified: Option<String>,
    metrics: Option<NvdMetrics>,
    weaknesses: Option<Vec<NvdWeakness>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvdMetrics {
    cvss_metric_v31: Option<Vec<NvdCvssMetric>>,
    cvss_metric_v40: Option<Vec<NvdCvssMetric>>,
    cvss_metric_v2: Option<Vec<NvdCvssMetric>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvdCvssMetric {
    cvss_data: NvdCvssData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvdCvssData {
    version: String,
    vector_string: String,
    base_score: f64,
    base_severity: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct NvdWeakness {
    description: Vec<NvdCweDescription>,
}

#[derive(Debug, Clone, Deserialize)]
struct NvdCweDescription {
    lang: String,
    value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_NVD_JSON: &str = r#"{
  "totalResults": 1,
  "vulnerabilities": [
    {
      "cve": {
        "id": "CVE-2025-26794",
        "published": "2025-02-21T13:15:11.687",
        "lastModified": "2025-12-18T19:16:22.593",
        "metrics": {
          "cvssMetricV31": [{
            "cvssData": {
              "version": "3.1",
              "vectorString": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H",
              "baseScore": 7.5,
              "baseSeverity": "HIGH"
            }
          }]
        },
        "weaknesses": [{
          "description": [{ "lang": "en", "value": "CWE-89" }]
        }]
      }
    }
  ]
}"#;

    #[test]
    fn test_parse_nvd_response() {
        let response: NvdResponse = serde_json::from_str(SAMPLE_NVD_JSON)
            .expect("Should parse NVD JSON");

        assert_eq!(response.total_results, 1);
        assert_eq!(response.vulnerabilities.len(), 1);

        let cve = &response.vulnerabilities[0].cve;
        assert_eq!(cve.id, "CVE-2025-26794");
        assert_eq!(cve.published.as_ref().unwrap(), "2025-02-21T13:15:11.687");

        let metrics = cve.metrics.as_ref().unwrap();
        let cvss_v31 = metrics.cvss_metric_v31.as_ref().unwrap();
        assert_eq!(cvss_v31.len(), 1);
        assert_eq!(cvss_v31[0].cvss_data.version, "3.1");
        assert_eq!(cvss_v31[0].cvss_data.vector_string, "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H");
        assert_eq!(cvss_v31[0].cvss_data.base_score, 7.5);

        let weaknesses = cve.weaknesses.as_ref().unwrap();
        assert_eq!(weaknesses[0].description[0].value, "CWE-89");
    }

    #[test]
    fn test_truncate_published_date() {
        let nvd_date = "2025-02-21T13:15:11.687";
        let truncated = nvd_date.split('.').next().unwrap().to_string() + "Z";
        assert_eq!(truncated, "2025-02-21T13:15:11Z");
    }

    #[test]
    fn test_filter_nvd_cwe_placeholders() {
        let cwe_values = vec!["CWE-89", "NVD-CWE-noinfo", "CWE-79", "NVD-CWE-Other"];
        let filtered: Vec<&str> = cwe_values.iter()
            .filter(|s| !s.starts_with("NVD-CWE-"))
            .copied()
            .collect();
        assert_eq!(filtered, vec!["CWE-89", "CWE-79"]);
    }

    #[test]
    fn test_cve_triple_emission() {
        use tempfile::NamedTempFile;
        use std::io::Read;

        let tmp = NamedTempFile::new().unwrap();
        let file = tmp.reopen().unwrap();
        let mut writer = NTriplesWriter::new(file);

        let nvd_cve = NvdCve {
            id: "CVE-2025-26794".to_string(),
            published: Some("2025-02-21T13:15:11.687".to_string()),
            last_modified: Some("2025-12-18T19:16:22.593".to_string()),
            metrics: Some(NvdMetrics {
                cvss_metric_v31: Some(vec![NvdCvssMetric {
                    cvss_data: NvdCvssData {
                        version: "3.1".to_string(),
                        vector_string: "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H".to_string(),
                        base_score: 7.5,
                        base_severity: Some("HIGH".to_string()),
                    },
                }]),
                cvss_metric_v40: None,
                cvss_metric_v2: None,
            }),
            weaknesses: Some(vec![NvdWeakness {
                description: vec![NvdCweDescription {
                    lang: "en".to_string(),
                    value: "CWE-89".to_string(),
                }],
            }]),
        };

        let enricher = NvdEnricher {
            client: Client::builder().timeout(Duration::from_secs(10)).build().unwrap(),
            sparql: SparqlClient::new("http://localhost:3030/packagegraph"),
            api_key: None,
            cache_dir: None,
        };

        let stats = enricher.emit_cve_triples(&mut writer, &nvd_cve).unwrap();
        writer.flush().unwrap();

        assert!(stats.has_published_date);
        assert!(stats.has_cvss);
        assert!(stats.has_cwe);
        assert!(stats.triples > 0);

        let mut content = String::new();
        tmp.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("security#Vulnerability"));
        assert!(content.contains("CVE-2025-26794"));
        assert!(content.contains("security#publishedDate"));
        assert!(content.contains("2025-02-21T13:15:11Z"), "Should truncate milliseconds");
        assert!(content.contains("security#hasCVSSScore"));
        assert!(content.contains("security#CVSSScore"));
        assert!(content.contains("CVSS:3.1/AV:N/AC:L"));
        assert!(content.contains("security#cvssVersion"));
        assert!(content.contains("security#hasCWE"));
        assert!(content.contains("cwe.mitre.org/data/definitions/89"));
        assert!(content.contains("security#severity"));
        assert!(content.contains("HIGH"));
    }

    #[test]
    fn test_format_cve_ntriples_returns_correct_lines() {
        let nvd_cve = NvdCve {
            id: "CVE-2025-26794".to_string(),
            published: Some("2025-02-21T13:15:11.687".to_string()),
            last_modified: Some("2025-12-18T19:16:22.593".to_string()),
            metrics: Some(NvdMetrics {
                cvss_metric_v31: Some(vec![NvdCvssMetric {
                    cvss_data: NvdCvssData {
                        version: "3.1".to_string(),
                        vector_string: "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H".to_string(),
                        base_score: 7.5,
                        base_severity: Some("HIGH".to_string()),
                    },
                }]),
                cvss_metric_v40: None,
                cvss_metric_v2: None,
            }),
            weaknesses: Some(vec![NvdWeakness {
                description: vec![NvdCweDescription {
                    lang: "en".to_string(),
                    value: "CWE-89".to_string(),
                }],
            }]),
        };

        let (lines, stats) = format_cve_ntriples(&nvd_cve);

        // Verify stats
        assert!(stats.has_published_date);
        assert!(stats.has_cvss);
        assert!(stats.has_cwe);
        assert!(stats.triples > 0);

        // Join lines and verify content
        let content = lines.join("\n");
        assert!(content.contains("security#Vulnerability"));
        assert!(content.contains("CVE-2025-26794"));
        assert!(content.contains("security#publishedDate"));
        assert!(content.contains("2025-02-21T13:15:11Z"), "Should truncate milliseconds");
        assert!(content.contains("security#hasCVSSScore"));
        assert!(content.contains("security#CVSSScore"));
        assert!(content.contains("CVSS:3.1/AV:N/AC:L"));
        assert!(content.contains("security#cvssVersion"));
        assert!(content.contains("security#hasCWE"));
        assert!(content.contains("cwe.mitre.org/data/definitions/89"));
        assert!(content.contains("security#severity"));
        assert!(content.contains("HIGH"));

        // Verify lines end with " ."
        for line in &lines {
            assert!(line.ends_with(" ."), "Line should end with ' .': {}", line);
        }
    }

    #[test]
    fn test_discover_missing_cves() {
        let mut server = mockito::Server::new();
        let mock = server.mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(r#"{
                "results": {
                    "bindings": [
                        {"vuln": {"type": "uri", "value": "https://packagegraph.github.io/d/cve/CVE-2025-1234"}},
                        {"vuln": {"type": "uri", "value": "https://packagegraph.github.io/d/cve/CVE-2025-5678"}},
                        {"vuln": {"type": "uri", "value": "https://packagegraph.github.io/d/cve/CVE-2024-9999"}}
                    ]
                }
            }"#)
            .create();

        let enricher = NvdEnricher {
            client: Client::builder().timeout(Duration::from_secs(10)).build().unwrap(),
            sparql: SparqlClient::new(&server.url()),
            api_key: None,
            cache_dir: None,
        };

        let missing = enricher.discover_missing_cves("https://packagegraph.github.io/graph/cve/nvd").unwrap();

        mock.assert();
        assert_eq!(missing.len(), 3);
        assert!(missing.contains(&"CVE-2025-1234".to_string()));
        assert!(missing.contains(&"CVE-2025-5678".to_string()));
        assert!(missing.contains(&"CVE-2024-9999".to_string()));
    }

    #[test]
    fn test_discover_missing_cves_empty() {
        let mut server = mockito::Server::new();
        let mock = server.mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let enricher = NvdEnricher {
            client: Client::builder().timeout(Duration::from_secs(10)).build().unwrap(),
            sparql: SparqlClient::new(&server.url()),
            api_key: None,
            cache_dir: None,
        };

        let missing = enricher.discover_missing_cves("https://packagegraph.github.io/graph/cve/nvd").unwrap();

        mock.assert();
        assert_eq!(missing.len(), 0);
    }

    #[test]
    fn test_fetch_cve_from_api_success() {
        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/")
            .match_query(mockito::Matcher::UrlEncoded("cveId".into(), "CVE-2025-1234".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{
                "totalResults": 1,
                "vulnerabilities": [{
                    "cve": {
                        "id": "CVE-2025-1234",
                        "published": "2025-01-15T10:00:00.000",
                        "metrics": {
                            "cvssMetricV31": [{
                                "cvssData": {
                                    "version": "3.1",
                                    "vectorString": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N",
                                    "baseScore": 7.5,
                                    "baseSeverity": "HIGH"
                                }
                            }]
                        }
                    }
                }]
            }"#)
            .create();

        let enricher = NvdEnricher {
            client: Client::builder().timeout(Duration::from_secs(10)).build().unwrap(),
            sparql: SparqlClient::new("http://localhost:3030/test"),
            api_key: None,
            cache_dir: None,
        };

        // Override NVD API base to point to mock server
        let cve = enricher.fetch_cve_from_api_with_base(&server.url(), "CVE-2025-1234").unwrap();

        mock.assert();
        assert!(cve.is_some());
        let cve_data = cve.unwrap();
        assert_eq!(cve_data.id, "CVE-2025-1234");
        assert_eq!(cve_data.published.as_ref().unwrap(), "2025-01-15T10:00:00.000");
    }

    #[test]
    fn test_fetch_cve_from_api_not_found() {
        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/")
            .match_query(mockito::Matcher::UrlEncoded("cveId".into(), "CVE-NONEXISTENT".into()))
            .with_status(200)
            .with_body(r#"{"totalResults": 0, "vulnerabilities": []}"#)
            .create();

        let enricher = NvdEnricher {
            client: Client::builder().timeout(Duration::from_secs(10)).build().unwrap(),
            sparql: SparqlClient::new("http://localhost:3030/test"),
            api_key: None,
            cache_dir: None,
        };

        let cve = enricher.fetch_cve_from_api_with_base(&server.url(), "CVE-NONEXISTENT").unwrap();

        mock.assert();
        assert!(cve.is_none());
    }

    #[test]
    fn test_enrich_api_zero_gaps() {
        let mut server = mockito::Server::new();
        // Gap discovery returns empty
        let mock_sparql = server.mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let enricher = NvdEnricher {
            client: Client::builder().timeout(Duration::from_secs(10)).build().unwrap(),
            sparql: SparqlClient::new(&server.url()),
            api_key: None,
            cache_dir: None,
        };

        let (matched, triples) = enricher.enrich_api("https://packagegraph.github.io/graph/cve/nvd").unwrap();

        mock_sparql.assert();
        assert_eq!(matched, 0);
        assert_eq!(triples, 0);
    }
}
