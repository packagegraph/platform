//! Whether a named graph's dependency edges can support a reverse-dependency
//! metric.
//!
//! `met:reverseDependencyCount` counts, for each `pkg:PackageIdentity`, how
//! many packages point at it with `pkg:directlyDependsOn`, and
//! `met:blastRadius` is derived from those counts. Neither is a provider
//! resolver. For an RPM graph the edge is minted from a requirement token by
//! name, so the target is an identity the collector invented rather than a
//! package the requirement actually resolves to. A capability can be provided
//! by a package of an entirely different name -- glibc provides
//! `rtld(GNU_HASH)` -- so the count answers a question about string equality,
//! not about dependency.
//!
//! The signal is structural and needs no new ontology terms: a real identity
//! is the object of `pkg:isVersionOf` from at least one version node, because
//! some package in the graph claims to *be* it. A manufactured requirement
//! target has no such claim. Counting dependency targets with no version
//! behind them measures exactly the unresolved edges.
//!
//! **This guard is prepared, not deployed.** `enrich()` consults it only when
//! a caller opts in; the default is to behave as before and report what the
//! guard would have done. Turning it on changes published metrics for every
//! affected graph and is a separate decision.

use crate::sparql::SparqlClient;
use crate::uris::*;
use std::io::Result;

/// What a graph's dependency edges can and cannot support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverseDependencySupport {
    pub graph: Option<String>,
    /// `directlyDependsOn` targets that no version node claims to be.
    pub unresolved_targets: u64,
    /// `directlyDependsOn` targets in total.
    pub total_targets: u64,
}

impl ReverseDependencySupport {
    /// Whether every dependency target in this graph resolves to a package.
    ///
    /// The remedy is per identity, not per graph: an identity no package
    /// claims to be should receive no count at all, rather than a count of
    /// the dependents that happened to name it. Measured on the live
    /// endpoint, the share varies by an order of magnitude between
    /// ecosystems -- almalinux/9 86.4%, rhel/9 92.0%, debian/trixie 18.5%,
    /// alpine/v3.20 1.0% -- so this is a fact about the graph, not a
    /// threshold anyone should tune.
    pub fn is_complete(&self) -> bool {
        self.unresolved_targets == 0
    }

    /// Share of dependency targets that no package claims to be.
    pub fn unresolved_share(&self) -> f64 {
        if self.total_targets == 0 {
            return 0.0;
        }
        self.unresolved_targets as f64 / self.total_targets as f64
    }

    /// A line for the run log, said the same way whether or not the guard is
    /// enforcing. "Unavailable" is not zero: zero is a measurement.
    pub fn summary(&self) -> String {
        let scope = match &self.graph {
            Some(g) => format!("<{g}>"),
            None => "the default union".to_string(),
        };
        if self.is_complete() {
            format!(
                "{scope}: every one of {} dependency targets resolves to a \
                 package; reverse-dependency counts are supported",
                self.total_targets
            )
        } else {
            format!(
                "{scope}: {} of {} dependency targets ({:.1}%) are name-minted \
                 identities no package claims to be. Counts for those \
                 identities are UNAVAILABLE, not zero.",
                self.unresolved_targets,
                self.total_targets,
                self.unresolved_share() * 100.0
            )
        }
    }
}

/// Measure how many of a graph's dependency targets resolve to a real package.
pub fn assess_reverse_dependency_support(
    sparql: &SparqlClient,
    graph: Option<&str>,
) -> Result<ReverseDependencySupport> {
    let (open, close) = match graph {
        Some(g) => (format!("GRAPH <{g}> {{"), "}"),
        None => ("{ GRAPH ?g {".to_string(), "} }"),
    };

    // One query, two subselects, so both numbers describe the same snapshot.
    // `FILTER NOT EXISTS` sits inside the GRAPH block: a version node in some
    // other graph does not make this graph's edge resolved.
    let query = format!(
        "SELECT ?total ?unresolved_count WHERE {{\n\
           {{ SELECT (COUNT(DISTINCT ?target) AS ?total) WHERE {{\n\
               {open}\n\
                 ?dependent <{PKG}directlyDependsOn> ?target .\n\
               {close}\n\
             }} }}\n\
           {{ SELECT (COUNT(DISTINCT ?target) AS ?unresolved_count) WHERE {{\n\
               {open}\n\
                 ?dependent <{PKG}directlyDependsOn> ?target .\n\
                 FILTER NOT EXISTS {{ ?version <{PKG}isVersionOf> ?target . }}\n\
               {close}\n\
             }} }}\n\
         }}",
        open = open,
        close = close,
        PKG = PKG,
    );

    let rows = sparql.query(&query)?;
    let read = |key: &str| -> u64 {
        rows.first()
            .and_then(|r| r.get(key))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };

    Ok(ReverseDependencySupport {
        graph: graph.map(str::to_string),
        total_targets: read("total"),
        unresolved_targets: read("unresolved_count"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn support(total: u64, unresolved: u64) -> ReverseDependencySupport {
        ReverseDependencySupport {
            graph: Some("https://packagegraph.github.io/graph/almalinux/9".to_string()),
            total_targets: total,
            unresolved_targets: unresolved,
        }
    }

    #[test]
    fn a_graph_whose_targets_all_resolve_supports_the_metric() {
        let s = support(1200, 0);
        assert!(s.is_complete());
        assert!(s.summary().contains("supported"));
    }

    #[test]
    fn an_unresolved_target_is_reported_as_unavailable_not_as_zero() {
        // Emitting 0 says "nothing depends on this", which is a measurement.
        // The truth is that the question cannot be answered for that node.
        let s = support(3928, 3395);
        assert!(!s.is_complete());
        assert!(s.summary().contains("UNAVAILABLE"));
        assert!(s.summary().contains("not zero"));
        assert!((s.unresolved_share() - 0.864).abs() < 0.001);
    }

    #[test]
    fn the_assessment_asks_the_endpoint_for_unresolved_targets() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("POST", "/sparql")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("directlyDependsOn".to_string()),
                mockito::Matcher::Regex("isVersionOf".to_string()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(
                r#"{"results": {"bindings": [
                     {"total": {"type":"literal","value":"3611932"},
                      "unresolved_count": {"type":"literal","value":"1545695"}}
                   ]}}"#,
            )
            .expect_at_least(1)
            .create();

        let client = crate::sparql::SparqlClient::new(&server.url());
        let assessed = assess_reverse_dependency_support(
            &client,
            Some("https://packagegraph.github.io/graph/almalinux/9"),
        )
        .unwrap();

        assert_eq!(assessed.total_targets, 3_611_932);
        assert_eq!(assessed.unresolved_targets, 1_545_695);
        assert!(!assessed.is_complete());
    }
}
