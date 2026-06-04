//! Reverse dependency count materializer.
//!
//! Queries Fuseki for dependency relationships and materializes
//! met:reverseDependencyCount on PackageIdentity entities for
//! efficient criticality queries.

use crate::ntriples::NTriplesWriter;
use crate::sparql::SparqlClient;
use crate::uris::*;
use std::fs::File;
use std::io::Result;

pub struct RevdepsEnricher {
    sparql: SparqlClient,
    graph: Option<String>,
}

impl RevdepsEnricher {
    pub fn new(endpoint: &str, graph: Option<&str>) -> Self {
        let sparql = SparqlClient::new(endpoint);
        Self {
            sparql,
            graph: graph.map(|s| s.to_string()),
        }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        let counts = self.query_reverse_dep_counts()?;
        eprintln!("Computed reverse dependency counts for {} identities", counts.len());

        let mut total_triples = 0usize;
        for (identity_uri, count) in &counts {
            writer.write_integer(
                identity_uri,
                &format!("{MET}reverseDependencyCount"),
                *count,
            )?;
            total_triples += 1;
        }

        writer.flush()?;
        eprintln!("Wrote {} triples", total_triples);
        Ok((counts.len(), total_triples))
    }

    fn query_reverse_dep_counts(&self) -> Result<Vec<(String, i64)>> {
        // Dependencies target PackageIdentity stubs directly via
        // pkg:directlyDependsOn. The dependent package links to its
        // own identity via pkg:isVersionOf. Count distinct dependents
        // per target identity.
        let graph_clause = match &self.graph {
            Some(g) => format!("GRAPH <{g}> {{"),
            None => "{ GRAPH ?g {".to_string(),
        };
        let close = match &self.graph {
            Some(_) => "}",
            None => "} }",
        };

        let query = format!(
            r#"SELECT ?targetIdentity (COUNT(DISTINCT ?depIdentity) AS ?revDepCount)
            WHERE {{
              {graph_clause}
                ?dependent <{PKG}directlyDependsOn> ?targetIdentity .
                ?dependent <{PKG}isVersionOf> ?depIdentity .
              {close}
              ?targetIdentity a <{PKG}PackageIdentity> .
            }}
            GROUP BY ?targetIdentity
            HAVING (COUNT(DISTINCT ?depIdentity) > 0)
            ORDER BY DESC(?revDepCount)"#,
            PKG = PKG,
            graph_clause = graph_clause,
            close = close,
        );

        let results = self.sparql.query(&query)?;
        let mut counts = Vec::new();
        for row in results {
            if let (Some(uri), Some(count_str)) =
                (row.get("targetIdentity"), row.get("revDepCount"))
            {
                if let Ok(count) = count_str.parse::<i64>() {
                    counts.push((uri.clone(), count));
                }
            }
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_revdeps_enricher_creation() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let enricher = RevdepsEnricher::new(&server.url(), None);
        assert!(enricher.graph.is_none());
    }

    #[test]
    fn test_revdeps_with_graph_scope() {
        let enricher = RevdepsEnricher::new("http://localhost:3030", Some("https://example.org/graph"));
        assert_eq!(
            enricher.graph,
            Some("https://example.org/graph".to_string())
        );
    }
}
