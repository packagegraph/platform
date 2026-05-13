//! Deriver for computing pkg:lastReleaseDate from ecosystem-specific build timestamps.
//!
//! This deriver queries Fuseki for ecosystem timestamps (rpm:buildTime, apk:buildDate),
//! computes the MAX timestamp per release-scoped PackageIdentity, and emits
//! pkg:lastReleaseDate as N-Triples to a dedicated derived graph.

use crate::ntriples::NTriplesWriter;
use crate::sparql::SparqlClient;
use crate::uris::PKG;
use chrono::DateTime;
use std::collections::HashMap;
use std::fs::File;
use std::io::Result;

/// Detailed breakdown of derivation metrics per ecosystem.
#[derive(Debug, Default)]
pub struct EcosystemBreakdown {
    pub rpm: usize,
    pub alpine: usize,
    pub unsupported: usize,
}

/// Report from a derive() run showing coverage metrics.
#[derive(Debug)]
pub struct DeriveReport {
    pub total_identities: usize,
    pub derived: usize,
    pub unsupported: usize,
    pub triples: usize,
    pub by_ecosystem: EcosystemBreakdown,
}

/// Deriver that computes pkg:lastReleaseDate from ecosystem build timestamps.
pub struct ReleaseDeriver {
    sparql: SparqlClient,
}

impl ReleaseDeriver {
    pub fn new(endpoint: &str) -> Self {
        Self {
            sparql: SparqlClient::new(endpoint),
        }
    }

    /// Derive pkg:lastReleaseDate for all PackageIdentities in the specified graphs.
    ///
    /// Collects timestamps from all graphs first, deduplicates by identity URI,
    /// takes global MAX per identity, then emits. This prevents duplicate triples
    /// when the same PackageIdentity appears in multiple graphs (e.g., base + updates).
    pub fn derive(&self, output_path: &str, graphs: &[String]) -> Result<DeriveReport> {
        let mut report = DeriveReport {
            total_identities: 0,
            derived: 0,
            unsupported: 0,
            triples: 0,
            by_ecosystem: EcosystemBreakdown::default(),
        };

        // Accumulate (identity_uri -> max_date_string) across all graphs
        let mut identity_dates: HashMap<String, String> = HashMap::new();
        let mut unsupported_count = 0;

        for graph_uri in graphs {
            eprintln!("Processing graph: {}", graph_uri);
            let ecosystem = detect_ecosystem(graph_uri);

            match ecosystem {
                Ecosystem::Rpm => {
                    let dates = self.collect_rpm_dates(graph_uri)?;
                    eprintln!("  RPM: {} identities found", dates.len());
                    report.by_ecosystem.rpm += dates.len();
                    for (identity, date) in dates {
                        identity_dates.entry(identity)
                            .and_modify(|existing| {
                                if date > *existing {
                                    *existing = date.clone();
                                }
                            })
                            .or_insert(date);
                    }
                }
                Ecosystem::Alpine => {
                    let dates = self.collect_alpine_dates(graph_uri)?;
                    eprintln!("  Alpine: {} identities found", dates.len());
                    report.by_ecosystem.alpine += dates.len();
                    for (identity, date) in dates {
                        identity_dates.entry(identity)
                            .and_modify(|existing| {
                                if date > *existing {
                                    *existing = date.clone();
                                }
                            })
                            .or_insert(date);
                    }
                }
                Ecosystem::Unsupported => {
                    let count = self.count_identities(graph_uri)?;
                    eprintln!("  Unsupported ecosystem: {} identities skipped", count);
                    unsupported_count += count;
                }
            }
        }

        // Emit deduplicated results
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        for (identity_uri, date_str) in &identity_dates {
            writer.write_date(identity_uri, &format!("{PKG}lastReleaseDate"), date_str)?;
        }

        writer.flush()?;

        report.total_identities = identity_dates.len() + unsupported_count;
        report.derived = identity_dates.len();
        report.unsupported = unsupported_count;
        report.triples = identity_dates.len();

        eprintln!();
        eprintln!("=== Derivation Summary ===");
        eprintln!("Total identities: {}", report.total_identities);
        eprintln!("Derived: {}", report.derived);
        eprintln!("Unsupported: {}", report.unsupported);
        eprintln!("Triples emitted: {}", report.triples);
        eprintln!("By ecosystem: RPM={}, Alpine={}, Unsupported={}",
                  report.by_ecosystem.rpm,
                  report.by_ecosystem.alpine,
                  report.by_ecosystem.unsupported);

        Ok(report)
    }

    /// Collect lastReleaseDate candidates for RPM-based graphs using rpm:buildTime.
    fn collect_rpm_dates(&self, graph_uri: &str) -> Result<HashMap<String, String>> {
        let sparql = format!(
            r#"PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
PREFIX rpm: <https://purl.org/packagegraph/ontology/rpm#>

SELECT ?identity (MAX(?bt) AS ?maxBuildTime)
WHERE {{
  GRAPH <{graph_uri}> {{
    ?pkg pkg:isVersionOf ?identity .
    ?pkg rpm:buildTime ?bt .
  }}
}}
GROUP BY ?identity"#
        );

        let bindings = self.sparql.query(&sparql)?;
        let mut dates = HashMap::new();

        for binding in &bindings {
            if let (Some(identity_uri), Some(datetime_str)) = (
                binding.get("identity"),
                binding.get("maxBuildTime"),
            ) {
                // Extract date from dateTime (YYYY-MM-DD from YYYY-MM-DDTHH:MM:SSZ)
                if let Some(date_str) = datetime_to_date(datetime_str) {
                    dates.insert(identity_uri.clone(), date_str);
                }
            }
        }

        Ok(dates)
    }

    /// Collect lastReleaseDate candidates for Alpine graphs using apk:buildDate (unix epoch).
    fn collect_alpine_dates(&self, graph_uri: &str) -> Result<HashMap<String, String>> {
        let sparql = format!(
            r#"PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
PREFIX apk: <https://purl.org/packagegraph/ontology/apk#>

SELECT ?identity (MAX(?bd) AS ?maxBuildDate)
WHERE {{
  GRAPH <{graph_uri}> {{
    ?pkg pkg:isVersionOf ?identity .
    ?pkg apk:buildDate ?bd .
  }}
}}
GROUP BY ?identity"#
        );

        let bindings = self.sparql.query(&sparql)?;
        let mut dates = HashMap::new();

        for binding in &bindings {
            if let (Some(identity_uri), Some(epoch_str)) = (
                binding.get("identity"),
                binding.get("maxBuildDate"),
            ) {
                // Parse epoch as i64, convert to date
                if let Ok(epoch) = epoch_str.parse::<i64>() {
                    if let Some(date_str) = epoch_to_date(epoch) {
                        dates.insert(identity_uri.clone(), date_str);
                    }
                }
            }
        }

        Ok(dates)
    }

    /// Count PackageIdentities in a graph (for unsupported ecosystem reporting).
    fn count_identities(&self, graph_uri: &str) -> Result<usize> {
        let sparql = format!(
            r#"PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>

SELECT (COUNT(DISTINCT ?identity) AS ?count)
WHERE {{
  GRAPH <{graph_uri}> {{
    ?pkg pkg:isVersionOf ?identity .
  }}
}}"#
        );

        let bindings = self.sparql.query(&sparql)?;
        if let Some(binding) = bindings.first() {
            if let Some(count_str) = binding.get("count") {
                return Ok(count_str.parse::<usize>().unwrap_or(0));
            }
        }
        Ok(0)
    }
}

/// Extract identity URIs from an N-Triples output file.
///
/// Reads the .nt file and extracts the subject URI from each triple.
/// Used by the --load upsert logic to delete stale triples before inserting new ones.
pub fn extract_identity_uris(nt_path: &str) -> Result<Vec<String>> {
    use std::io::{BufRead, BufReader};
    let file = File::open(nt_path)?;
    let reader = BufReader::new(file);
    let mut uris = Vec::new();
    for line in reader.lines() {
        let line = line?;
        // N-Triples format: <subject> <predicate> "value"^^<type> .
        if let Some(end) = line.find("> ") {
            if line.starts_with('<') {
                uris.push(line[1..end].to_string());
            }
        }
    }
    Ok(uris)
}

/// Ecosystem classification for graph URI routing.
#[derive(Debug, PartialEq)]
enum Ecosystem {
    Rpm,
    Alpine,
    Unsupported,
}

/// Detect ecosystem from graph URI pattern.
fn detect_ecosystem(graph_uri: &str) -> Ecosystem {
    let uri_lower = graph_uri.to_lowercase();
    if uri_lower.contains("fedora")
        || uri_lower.contains("rhel")
        || uri_lower.contains("centos")
        || uri_lower.contains("opensuse")
    {
        Ecosystem::Rpm
    } else if uri_lower.contains("alpine") {
        Ecosystem::Alpine
    } else {
        Ecosystem::Unsupported
    }
}

/// Convert xsd:dateTime string to xsd:date (extract YYYY-MM-DD prefix).
fn datetime_to_date(datetime_str: &str) -> Option<String> {
    // ISO 8601 format: "2024-04-18T16:00:00Z" → "2024-04-18"
    if datetime_str.len() >= 10 {
        Some(datetime_str[0..10].to_string())
    } else {
        None
    }
}

/// Convert unix epoch (seconds) to xsd:date format (YYYY-MM-DD).
fn epoch_to_date(epoch: i64) -> Option<String> {
    DateTime::from_timestamp(epoch, 0).map(|dt| dt.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: Integration tests that verify derive() against a live Fuseki instance
    // are performed manually during Task 5 validation. Mock-based SPARQL testing
    // would require substantial mockito setup and doesn't add value over the
    // manual end-to-end validation already performed.

    #[test]
    fn test_datetime_to_date() {
        assert_eq!(
            datetime_to_date("2024-04-18T16:00:00Z"),
            Some("2024-04-18".to_string())
        );
        assert_eq!(
            datetime_to_date("2026-01-15T08:30:45Z"),
            Some("2026-01-15".to_string())
        );
        assert_eq!(datetime_to_date(""), None);
        assert_eq!(datetime_to_date("2024"), None);
    }

    #[test]
    fn test_epoch_to_date() {
        // 1713456000 = 2024-04-18 16:00:00 UTC
        assert_eq!(epoch_to_date(1713456000), Some("2024-04-18".to_string()));

        // 0 = 1970-01-01
        assert_eq!(epoch_to_date(0), Some("1970-01-01".to_string()));

        // Negative epoch (before 1970) is valid — e.g., -1 = 1969-12-31
        assert_eq!(epoch_to_date(-1), Some("1969-12-31".to_string()));
    }

    #[test]
    fn test_detect_ecosystem() {
        assert_eq!(
            detect_ecosystem("https://packagegraph.github.io/graph/fedora/43"),
            Ecosystem::Rpm
        );
        assert_eq!(
            detect_ecosystem("https://packagegraph.github.io/graph/rhel/9"),
            Ecosystem::Rpm
        );
        assert_eq!(
            detect_ecosystem("https://packagegraph.github.io/graph/centos-stream/10"),
            Ecosystem::Rpm
        );
        assert_eq!(
            detect_ecosystem("https://packagegraph.github.io/graph/opensuse/tumbleweed"),
            Ecosystem::Rpm
        );
        assert_eq!(
            detect_ecosystem("https://packagegraph.github.io/graph/alpine/v3.20"),
            Ecosystem::Alpine
        );
        assert_eq!(
            detect_ecosystem("https://packagegraph.github.io/graph/debian/trixie"),
            Ecosystem::Unsupported
        );
    }
}
