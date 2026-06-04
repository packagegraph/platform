//! Blast radius materializer.
//!
//! Computes blast radius for vulnerabilities using the formula:
//!   blast_radius = log10(reverse_dependency_count) * cvss_base_score
//!
//! Depends on: met:reverseDependencyCount (from enrich-revdeps) and
//! sec:hasCVSSScore/sec:baseScore (from enrich-nvd or enrich-security).
//!
//! **Gated on ontology:** Requires `sec:blastRadius` to be declared in the
//! security ontology. The enricher will refuse to run until the property
//! exists in the target graph.

use crate::ntriples::NTriplesWriter;
use crate::sparql::SparqlClient;
use crate::uris::*;
use std::fs::File;
use std::io::Result;

pub struct BlastRadiusEnricher {
    sparql: SparqlClient,
}

impl BlastRadiusEnricher {
    pub fn new(endpoint: &str) -> Self {
        let sparql = SparqlClient::new(endpoint);
        Self { sparql }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        self.check_ontology_property()?;

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        let vulns = self.query_vuln_data()?;
        eprintln!("Found {} vulnerabilities with both CVSS and revdep data", vulns.len());

        let mut total_triples = 0usize;
        for (vuln_uri, blast_radius) in &vulns {
            let rounded = blast_radius.round().max(0.0) as i64;
            writer.write_integer(
                vuln_uri,
                &format!("{SEC}blastRadius"),
                rounded,
            )?;
            total_triples += 1;
        }

        writer.flush()?;
        eprintln!("Wrote {} blast radius scores", total_triples);
        Ok((vulns.len(), total_triples))
    }

    fn check_ontology_property(&self) -> Result<()> {
        let query = format!(
            "SELECT ?p WHERE {{ <{SEC}blastRadius> a ?p }} LIMIT 1",
            SEC = SEC,
        );
        let results = self.sparql.query(&query)?;
        if results.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "sec:blastRadius is not declared in the ontology. \
                 Add it to security.ttl and load into Fuseki before running this enricher.",
            ));
        }
        Ok(())
    }

    fn query_vuln_data(&self) -> Result<Vec<(String, f64)>> {
        let query = format!(
            r#"SELECT ?vuln ?cvssScore ?revDepCount
            WHERE {{
              ?vuln <{SEC}affectsPackage> ?identity ;
                    <{SEC}hasCVSSScore>/<{SEC}baseScore> ?cvssScore .
              ?identity <{MET}reverseDependencyCount> ?revDepCount .
              FILTER(?revDepCount > 0)
            }}"#,
            SEC = SEC,
            MET = MET,
        );

        let results = self.sparql.query(&query)?;
        let mut vulns = Vec::new();
        for row in results {
            if let (Some(vuln), Some(cvss_str), Some(revdep_str)) = (
                row.get("vuln"),
                row.get("cvssScore"),
                row.get("revDepCount"),
            ) {
                if let (Ok(cvss), Ok(revdeps)) =
                    (cvss_str.parse::<f64>(), revdep_str.parse::<f64>())
                {
                    if revdeps > 0.0 && cvss > 0.0 {
                        let blast_radius = revdeps.log10() * cvss;
                        vulns.push((vuln.clone(), blast_radius));
                    }
                }
            }
        }
        Ok(vulns)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_blast_radius_formula() {
        let revdeps: f64 = 4521.0;
        let cvss: f64 = 7.5;
        let blast = revdeps.log10() * cvss;
        assert!((blast - 27.4).abs() < 0.1);
    }

    #[test]
    fn test_blast_radius_zero_revdeps() {
        let revdeps: f64 = 0.0;
        assert!(revdeps <= 0.0, "Zero revdeps should be filtered");
    }

    #[test]
    fn test_blast_radius_single_revdep() {
        let revdeps: f64 = 1.0;
        let cvss: f64 = 10.0;
        let blast = revdeps.log10() * cvss;
        assert!((blast - 0.0).abs() < 0.001, "log10(1) = 0, so blast = 0");
    }
}
