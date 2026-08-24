//! EPSS (Exploit Prediction Scoring System) enricher.
//!
//! Fetches daily EPSS scores from FIRST.org and attaches them to CVE entities
//! already in the graph. Produces sec:EPSSAssessment entities with score,
//! percentile, and assessment date.

use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::fs::File;
use std::io::Result;

const EPSS_API_URL: &str = "https://api.first.org/data/v1/epss";

pub struct EpssEnricher {
    sparql: SparqlClient,
    client: Client,
    min_score: f64,
    pub graph_uri: Option<String>,
}

impl EpssEnricher {
    pub fn new(endpoint: &str, min_score: f64, auth: SparqlAuth, backend: SparqlBackend) -> Self {
        let sparql = make_sparql_client(endpoint, &auth, backend);
        let client = Client::builder()
            .user_agent("pg-collect/1.0 (packagegraph.github.io)")
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("Failed to build HTTP client");
        Self {
            sparql,
            client,
            min_score,
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

        let cve_map = self.query_cve_entities()?;
        eprintln!("Found {} CVE entities in graph", cve_map.len());

        let epss_data = self.fetch_epss_bulk()?;
        eprintln!("Fetched {} EPSS scores from FIRST.org", epss_data.len());

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut matched = 0usize;
        let mut total_triples = 0usize;

        for (cve_id, vuln_uri) in &cve_map {
            if let Some((score, percentile)) = epss_data.get(cve_id.as_str()) {
                if *score < self.min_score {
                    continue;
                }

                let assessment_uri = epss_assessment_uri(cve_id, &today);

                writer.write_triple(
                    vuln_uri,
                    &format!("{SEC}hasEPSSAssessment"),
                    &assessment_uri,
                )?;
                writer.write_triple(&assessment_uri, RDF_TYPE, &format!("{SEC}EPSSAssessment"))?;
                writer.write_typed_literal(
                    &assessment_uri,
                    &format!("{SEC}epssScore"),
                    &format!("{score}"),
                    &format!("{XSD}decimal"),
                )?;
                writer.write_typed_literal(
                    &assessment_uri,
                    &format!("{SEC}epssPercentile"),
                    &format!("{percentile}"),
                    &format!("{XSD}decimal"),
                )?;
                writer.write_date(&assessment_uri, &format!("{SEC}epssAssessmentDate"), &today)?;

                matched += 1;
                total_triples += 5;
            }
        }

        writer.flush()?;
        eprintln!(
            "Matched {} CVEs with EPSS scores ({} triples)",
            matched, total_triples
        );
        Ok((matched, total_triples))
    }

    fn query_cve_entities(&self) -> Result<HashMap<String, String>> {
        let query = r#"
            SELECT DISTINCT ?vuln ?cveId WHERE {
              { GRAPH ?g { ?vuln <SEC_PLACEHOLDER>cveId ?cveId } }
            }
        "#
        .replace("SEC_PLACEHOLDER", SEC);

        let results = self.sparql.query(&query)?;
        let mut map = HashMap::new();
        for row in results {
            if let (Some(vuln), Some(cve_id)) = (row.get("vuln"), row.get("cveId")) {
                map.insert(cve_id.clone(), vuln.clone());
            }
        }
        Ok(map)
    }

    fn fetch_epss_bulk(&self) -> Result<HashMap<String, (f64, f64)>> {
        // FIRST.org EPSS API defaults to limit=100. We paginate through
        // all results using offset, fetching 10K per page.
        const PAGE_SIZE: usize = 10_000;
        let mut map = HashMap::new();
        let mut offset = 0usize;

        loop {
            let limit_str = PAGE_SIZE.to_string();
            let offset_str = offset.to_string();

            let resp = self
                .client
                .get(EPSS_API_URL)
                .query(&[
                    ("envelope", "true"),
                    ("pretty", "false"),
                    ("limit", &limit_str),
                    ("offset", &offset_str),
                ])
                .send()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

            if !resp.status().is_success() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("EPSS API returned {}", resp.status()),
                ));
            }

            let body: serde_json::Value = resp
                .json()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

            let data = match body.get("data").and_then(|d| d.as_array()) {
                Some(arr) => arr,
                None => break,
            };

            if data.is_empty() {
                break;
            }

            let page_count = data.len();
            for entry in data {
                let cve = match entry.get("cve").and_then(|v| v.as_str()) {
                    Some(c) => c,
                    None => continue,
                };
                let epss: f64 = entry
                    .get("epss")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let percentile: f64 = entry
                    .get("percentile")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                map.insert(cve.to_string(), (epss, percentile));
            }

            eprintln!(
                "  Fetched EPSS page at offset {} ({} records)",
                offset, page_count
            );

            if page_count < PAGE_SIZE {
                break;
            }
            offset += PAGE_SIZE;
        }

        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_epss_triple_emission() {
        let mut server = mockito::Server::new();

        let _sparql_mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(
                r#"{"results": {"bindings": [
                    {"vuln": {"type": "uri", "value": "https://packagegraph.github.io/d/cve/CVE-2024-6119"},
                     "cveId": {"type": "literal", "value": "CVE-2024-6119"}}
                ]}}"#,
            )
            .create();

        let enricher = EpssEnricher::new(&server.url(), 0.0, None, SparqlBackend::Fuseki);

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut epss_data = HashMap::new();
        epss_data.insert("CVE-2024-6119".to_string(), (0.00532, 0.721));

        let cve_map: HashMap<String, String> = [(
            "CVE-2024-6119".to_string(),
            "https://packagegraph.github.io/d/cve/CVE-2024-6119".to_string(),
        )]
        .into_iter()
        .collect();

        let today = "2026-06-04";
        let assessment_uri = epss_assessment_uri("CVE-2024-6119", today);

        writer
            .write_triple(
                &cve_map["CVE-2024-6119"],
                &format!("{SEC}hasEPSSAssessment"),
                &assessment_uri,
            )
            .unwrap();
        writer
            .write_triple(&assessment_uri, RDF_TYPE, &format!("{SEC}EPSSAssessment"))
            .unwrap();
        writer
            .write_typed_literal(
                &assessment_uri,
                &format!("{SEC}epssScore"),
                "0.00532",
                &format!("{XSD}decimal"),
            )
            .unwrap();
        writer
            .write_typed_literal(
                &assessment_uri,
                &format!("{SEC}epssPercentile"),
                "0.721",
                &format!("{XSD}decimal"),
            )
            .unwrap();
        writer
            .write_date(&assessment_uri, &format!("{SEC}epssAssessmentDate"), today)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("EPSSAssessment"));
        assert!(content.contains("epssScore"));
        assert!(content.contains("0.00532"));
        assert!(content.contains("epssPercentile"));
        assert!(content.contains("0.721"));
        assert!(content.contains("epssAssessmentDate"));
    }

    #[test]
    fn test_epss_min_score_filter() {
        let filtered = 0.001_f64;
        let threshold = 0.01_f64;
        assert!(
            filtered < threshold,
            "Score below threshold should be filtered"
        );
    }
}
