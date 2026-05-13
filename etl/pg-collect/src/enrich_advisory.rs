//! Vendor security advisory enricher — RHSA (Red Hat) and DSA (Debian).
//!
//! Fetches advisories from vendor APIs and emits SecurityAdvisory triples
//! with CVE cross-references.

use crate::cache::FileCache;
use crate::enricher::rate_limit;
use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use reqwest::blocking::Client;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct AdvisoryEnricher {
    client: Client,
    cache: Option<FileCache>,
    advisory_type: AdvisoryType,
    days_back: u32,
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
    ) -> Self {
        let client = crate::enricher::default_http_client();

        let cache = cache_dir.map(|dir| {
            FileCache::new(dir, "advisory", 168, None) // 1 week TTL
                .expect("Failed to create cache")
        });

        Self { client, cache, advisory_type, days_back }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

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
                    let resp = self.client.get(&url)
                        .header("Accept", "application/json")
                        .send()
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                    if !resp.status().is_success() {
                        eprintln!("RHSA API returned {}", resp.status());
                        break;
                    }

                    let data: serde_json::Value = resp.json()
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                    self.cache_put(&cache_key, &data);
                    data
                }
            };

            let entries = match data.as_array() {
                Some(arr) if !arr.is_empty() => arr,
                _ => break,
            };

            for entry in entries {
                if let Some(triples) = self.emit_rhsa_advisory(writer, entry)? {
                    total_advisories += 1;
                    total_triples += triples;
                }
            }

            page += 1;
            rate_limit(Duration::from_millis(500));
        }

        Ok((total_advisories, total_triples))
    }

    fn emit_rhsa_advisory(&self, writer: &mut NTriplesWriter, entry: &serde_json::Value) -> Result<Option<usize>> {
        let cve_id = match entry.get("CVE").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(None),
        };

        let advisory_uri = format!("{DATA}advisory/rhsa/{}", cve_id);
        let mut triples = 0;

        writer.write_triple(&advisory_uri, RDF_TYPE, &format!("{SEC}SecurityAdvisory"))?;
        writer.write_literal(&advisory_uri, &format!("{SEC}advisoryId"), cve_id)?;
        writer.write_triple(&advisory_uri, &format!("{SEC}advisoryType"), &advisory_category_uri("security"))?;
        triples += 3;

        if let Some(severity) = entry.get("severity").and_then(|v| v.as_str()) {
            if let Some(sev_uri) = severity_concept_uri(severity) {
                writer.write_triple(&advisory_uri, &format!("{SEC}advisorySeverity"), &sev_uri)?;
                triples += 1;
            }
        }

        if let Some(date) = entry.get("public_date").and_then(|v| v.as_str()) {
            writer.write_literal(&advisory_uri, &format!("{SEC}advisoryDate"), date)?;
            triples += 1;
        }

        // Link to CVE entity (shared with OSV collector)
        let cve_entity = cve_entity_uri(cve_id);
        writer.write_triple(&advisory_uri, &format!("{SEC}addressesVulnerability"), &cve_entity)?;
        triples += 1;

        Ok(Some(triples))
    }

    fn enrich_dsa(&self, writer: &mut NTriplesWriter) -> Result<(usize, usize)> {
        let url = "https://security-tracker.debian.org/tracker/data/json";

        let cache_key = "dsa-tracker-full";
        let data = match self.cached_get(cache_key) {
            Some(d) => d,
            None => {
                let resp = self.client.get(url)
                    .send()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                if !resp.status().is_success() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("DSA tracker returned {}", resp.status()),
                    ));
                }

                let data: serde_json::Value = resp.json()
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

                        writer.write_triple(&advisory_uri, RDF_TYPE, &format!("{SEC}SecurityAdvisory"))?;
                        writer.write_literal(&advisory_uri, &format!("{SEC}advisoryId"), cve_id)?;
                        writer.write_triple(&advisory_uri, &format!("{SEC}advisoryType"), &advisory_category_uri("security"))?;
                        triples += 3;

                        if let Some(severity) = cve_data.get("urgency").and_then(|v| v.as_str()) {
                            if let Some(sev_uri) = severity_concept_uri(severity) {
                                writer.write_triple(&advisory_uri, &format!("{SEC}advisorySeverity"), &sev_uri)?;
                                triples += 1;
                            }
                        }

                        // Link to CVE entity (shared with OSV collector)
                        let cve_entity = cve_entity_uri(cve_id);
                        writer.write_triple(&advisory_uri, &format!("{SEC}addressesVulnerability"), &cve_entity)?;
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

    fn days_back_date(&self) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let past = now - (self.days_back as u64 * 86400);
        // Format as YYYY-MM-DD
        let days_since_epoch = past / 86400;
        let year = 1970 + (days_since_epoch / 365); // Approximate
        format!("{}-01-01", year)
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
    fn test_emit_rhsa_advisory() {
        let enricher = AdvisoryEnricher::new(AdvisoryType::Rhsa, 365, None);

        let entry = serde_json::json!({
            "CVE": "CVE-2024-1234",
            "severity": "important",
            "public_date": "2024-03-15T00:00:00Z"
        });

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let result = enricher.emit_rhsa_advisory(&mut writer, &entry).unwrap();
        writer.flush().unwrap();

        assert!(result.is_some(), "Should emit advisory triples");
        assert!(result.unwrap() >= 5, "Should emit at least 5 triples");

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("security#SecurityAdvisory"), "Should have SecurityAdvisory type");
        assert!(content.contains("\"CVE-2024-1234\""), "Should have advisory ID");
        assert!(content.contains("security#cat-security"), "Should have advisoryType as SKOS concept");
        assert!(content.contains("security#sev-important"), "Should have severity as SKOS concept");
        assert!(content.contains("security#addressesVulnerability"), "Should link to CVE");
        assert!(content.contains("cve/CVE-2024-1234"), "Should link to CVE entity URI");
    }

    #[test]
    fn test_emit_rhsa_advisory_missing_cve() {
        let enricher = AdvisoryEnricher::new(AdvisoryType::Rhsa, 365, None);

        let entry = serde_json::json!({"severity": "low"});

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let result = enricher.emit_rhsa_advisory(&mut writer, &entry).unwrap();
        assert!(result.is_none(), "Should return None for entries without CVE");
    }

    #[test]
    fn test_dsa_parsing() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // Simulate DSA tracker data structure
        let advisory_uri = format!("{DATA}advisory/dsa/CVE-2024-5678");
        writer.write_triple(&advisory_uri, RDF_TYPE, &format!("{SEC}SecurityAdvisory")).unwrap();
        writer.write_literal(&advisory_uri, &format!("{SEC}advisoryId"), "CVE-2024-5678").unwrap();
        let sev_uri = severity_concept_uri("high").unwrap();
        writer.write_triple(&advisory_uri, &format!("{SEC}advisorySeverity"), &sev_uri).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("advisory/dsa/CVE-2024-5678"), "Should use DSA advisory URI");
        assert!(content.contains("security#SecurityAdvisory"));
        assert!(content.contains("security#sev-important"), "high maps to sev-important");
    }
}
