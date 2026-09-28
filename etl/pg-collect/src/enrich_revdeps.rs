//! Reverse dependency count materializer.
//!
//! Queries Fuseki for dependency relationships and materializes
//! met:reverseDependencyCount on PackageIdentity entities for
//! efficient criticality queries.
//!
//! **Gated on ontology:** Requires `met:reverseDependencyCount` to be declared
//! in the metrics ontology. The enricher will refuse to run until the property
//! exists in the target graph. Once the ontology term lands, remove the gate
//! check in `enrich()`.

use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use std::fs::File;
use std::io::Result;

pub struct RevdepsEnricher {
    sparql: SparqlClient,
    graph: Option<String>,
    pub graph_uri: Option<String>,
    /// Skip identities no package claims to be, instead of counting the
    /// dependents that happened to name them. Off by default: turning it on
    /// changes published metrics, which is a decision, not a bug fix.
    withhold_unresolved: bool,
}

impl RevdepsEnricher {
    pub fn new(
        endpoint: &str,
        graph: Option<&str>,
        auth: SparqlAuth,
        backend: SparqlBackend,
    ) -> Self {
        let sparql = make_sparql_client(endpoint, &auth, backend);
        Self {
            sparql,
            graph: graph.map(|s| s.to_string()),
            graph_uri: None,
            withhold_unresolved: false,
        }
    }

    /// Withhold counts for dependency targets that resolve to nothing.
    ///
    /// An RPM requirement is a capability token, and the collector mints a
    /// `PackageIdentity` from it by name. Nothing resolves that token to the
    /// package that actually provides it -- glibc provides `rtld(GNU_HASH)`
    /// under a different name entirely -- so a count attached to such a node
    /// measures string equality. With this on, those nodes get no triple at
    /// all, which reads downstream as "not measured" rather than "measured
    /// as zero".
    pub fn withholding_unresolved(mut self, withhold: bool) -> Self {
        self.withhold_unresolved = withhold;
        self
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        self.check_ontology_property()?;

        // Reported whether or not the guard is enforcing, so the run log says
        // what the numbers are worth before anyone decides to act on it.
        match crate::rpm_contract_guard::assess_reverse_dependency_support(
            &self.sparql,
            self.graph.as_deref(),
        ) {
            Ok(support) => {
                eprintln!("{}", support.summary());
                if !support.is_complete() && !self.withhold_unresolved {
                    eprintln!(
                        "  Counting them anyway: pass --withhold-unresolved-targets \
                         to omit identities no package claims to be."
                    );
                }
            }
            Err(e) => eprintln!("  Warning: could not assess target resolution: {e}"),
        }

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        let counts = self.query_reverse_dep_counts()?;
        eprintln!(
            "Computed reverse dependency counts for {} identities",
            counts.len()
        );

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

    fn check_ontology_property(&self) -> Result<()> {
        let query = format!(
            "SELECT ?p WHERE {{ <{MET}reverseDependencyCount> a ?p }} LIMIT 1",
            MET = MET,
        );
        let results = self.sparql.query(&query)?;
        if results.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "met:reverseDependencyCount is not declared in the ontology. \
                 Add it to metrics.ttl and load into Fuseki before running this enricher.",
            ));
        }
        Ok(())
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
        // A real identity is one some version claims to be. Without this, a
        // requirement token that matched no package still collects a count.
        let resolved_only = if self.withhold_unresolved {
            format!("?anyVersion <{PKG}isVersionOf> ?targetIdentity .")
        } else {
            String::new()
        };

        let query = format!(
            r#"SELECT ?targetIdentity (COUNT(DISTINCT ?depIdentity) AS ?revDepCount)
            WHERE {{
              {graph_clause}
                ?dependent <{PKG}directlyDependsOn> ?targetIdentity .
                ?dependent <{PKG}isVersionOf> ?depIdentity .
                {resolved_only}
              {close}
              ?targetIdentity a <{PKG}PackageIdentity> .
            }}
            GROUP BY ?targetIdentity
            HAVING (COUNT(DISTINCT ?depIdentity) > 0)
            ORDER BY DESC(?revDepCount)"#,
            PKG = PKG,
            graph_clause = graph_clause,
            close = close,
            resolved_only = resolved_only,
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

        let enricher = RevdepsEnricher::new(&server.url(), None, None, SparqlBackend::Fuseki);
        assert!(enricher.graph.is_none());
    }

    #[test]
    fn test_revdeps_with_graph_scope() {
        let enricher = RevdepsEnricher::new(
            "http://localhost:3030",
            Some("https://example.org/graph"),
            None,
            SparqlBackend::Fuseki,
        );
        assert_eq!(
            enricher.graph,
            Some("https://example.org/graph".to_string())
        );
    }
}
