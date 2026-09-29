//! How many of a graph's dependency targets are identities no package claims.
//!
//! # What this measures, and what it does not
//!
//! A `pkg:PackageIdentity` is *claimed* when at least one version node points
//! at it with `pkg:isVersionOf` -- some package in the graph asserts that it
//! *is* that identity. An RPM dependency edge is minted from a requirement
//! token by name, so a requirement naming something no package in the graph
//! is produces an **orphan target**: an identity that exists only because
//! something referred to it.
//!
//! That is the whole of what this module reports. It is a structural
//! diagnostic and needs no new ontology terms.
//!
//! **It does not establish dependency resolution, and must not be read as
//! doing so.** Two independent reasons:
//!
//! 1. *Version constraints are not evaluated.* Against a graph holding the
//!    requirement `foo >= 2` and only `foo` version 1, every target here is
//!    claimed -- `foo`'s identity is claimed by `foo-1` -- so this reports no
//!    orphans while the requirement is unsatisfiable. An earlier version of
//!    this module called that result "every target resolved" and "counts
//!    supported". It was wrong.
//! 2. *Capabilities are not followed.* An RPM requirement is a capability
//!    token, and a capability may be provided by a package of an entirely
//!    different name: glibc provides `rtld(GNU_HASH)`, which 1,199 packages
//!    require. A claimed identity named after the token says nothing about
//!    the provider, and an orphan target may still be perfectly resolvable
//!    through `pkg:providesCapability`.
//!
//! So a zero here does not license `met:reverseDependencyCount`, and a
//! nonzero here is not proof a requirement is broken. Deciding what the
//! service should publish when a reverse-dependency count cannot be supported
//! is a separate question, tracked with the RPM contract work; resolving
//! capabilities and constraints would need a solver, which is out of scope.
//!
//! Nothing in this module is wired into any enricher. It reports.

use crate::sparql::SparqlClient;
use crate::uris::*;
use std::io::Result;

/// Orphan dependency targets in a graph, and the total they were drawn from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanTargetReport {
    pub graph: Option<String>,
    /// `directlyDependsOn` targets that no version node claims via
    /// `isVersionOf`.
    pub orphan_targets: u64,
    /// `directlyDependsOn` targets in total.
    pub total_targets: u64,
}

impl OrphanTargetReport {
    /// Whether every dependency target is claimed by some package.
    ///
    /// Named for what it checks. This is NOT `is_complete` or
    /// `supports_reverse_dependency_counts`: see the module docs for the
    /// `foo >= 2` counterexample that passes this and is still unresolvable.
    pub fn has_no_orphan_targets(&self) -> bool {
        self.orphan_targets == 0
    }

    /// Share of dependency targets that no package claims to be.
    ///
    /// Measured on the live endpoint, this varies by an order of magnitude
    /// between ecosystems -- almalinux/9 86.4%, rhel/9 92.0%, debian/trixie
    /// 18.5%, alpine/v3.20 1.0%. That is a fact about how each ecosystem
    /// names dependencies, not a threshold anyone should tune.
    pub fn orphan_share(&self) -> f64 {
        if self.total_targets == 0 {
            return 0.0;
        }
        self.orphan_targets as f64 / self.total_targets as f64
    }

    /// A line for the run log. Deliberately says nothing about resolution or
    /// about whether any metric is supported.
    pub fn summary(&self) -> String {
        let scope = match &self.graph {
            Some(g) => format!("<{g}>"),
            None => "the default union".to_string(),
        };
        if self.has_no_orphan_targets() {
            format!(
                "{scope}: all {} dependency targets are claimed by some \
                 package. This says nothing about whether their version \
                 constraints hold or their capabilities have providers.",
                self.total_targets
            )
        } else {
            format!(
                "{scope}: {} of {} dependency targets ({:.1}%) are \
                 name-minted identities no package claims to be. Orphan \
                 targets only -- not a resolution result.",
                self.orphan_targets,
                self.total_targets,
                self.orphan_share() * 100.0
            )
        }
    }
}

/// Count a graph's orphan dependency targets.
pub fn count_orphan_targets(
    sparql: &SparqlClient,
    graph: Option<&str>,
) -> Result<OrphanTargetReport> {
    let (open, close) = match graph {
        Some(g) => (format!("GRAPH <{g}> {{"), "}"),
        None => ("{ GRAPH ?g {".to_string(), "} }"),
    };

    // One query, two subselects, so both numbers describe the same snapshot.
    // `FILTER NOT EXISTS` sits inside the GRAPH block: a version node in some
    // other graph does not make this graph's edge claimed.
    let query = format!(
        "SELECT ?total ?orphan_count WHERE {{\n\
           {{ SELECT (COUNT(DISTINCT ?target) AS ?total) WHERE {{\n\
               {open}\n\
                 ?dependent <{PKG}directlyDependsOn> ?target .\n\
               {close}\n\
             }} }}\n\
           {{ SELECT (COUNT(DISTINCT ?target) AS ?orphan_count) WHERE {{\n\
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

    Ok(OrphanTargetReport {
        graph: graph.map(str::to_string),
        total_targets: read("total"),
        orphan_targets: read("orphan_count"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(total: u64, orphans: u64) -> OrphanTargetReport {
        OrphanTargetReport {
            graph: Some("https://packagegraph.github.io/graph/almalinux/9".to_string()),
            total_targets: total,
            orphan_targets: orphans,
        }
    }

    #[test]
    fn no_orphans_is_not_reported_as_resolution() {
        // The correction. An endpoint holding `foo >= 2` and only foo-1 has
        // no orphan targets and an unsatisfiable requirement, so the summary
        // must not claim anything about resolution or metric support.
        let r = report(1200, 0);
        assert!(r.has_no_orphan_targets());
        let summary = r.summary();
        assert!(
            summary.contains("says nothing about"),
            "summary must disclaim resolution, got {summary}"
        );
        for forbidden in ["resolve", "supported", "UNAVAILABLE"] {
            assert!(
                !summary.contains(forbidden),
                "summary must not say {forbidden:?}, got {summary}"
            );
        }
    }

    #[test]
    fn orphans_are_reported_as_orphans_and_nothing_more() {
        let r = report(3928, 3395);
        assert!(!r.has_no_orphan_targets());
        let summary = r.summary();
        assert!(summary.contains("no package claims to be"));
        assert!(summary.contains("not a resolution result"));
        assert!((r.orphan_share() - 0.864).abs() < 0.001);
    }

    #[test]
    fn an_empty_graph_reports_no_orphans_without_dividing_by_zero() {
        let r = report(0, 0);
        assert_eq!(0.0, r.orphan_share());
        assert!(r.has_no_orphan_targets());
    }

    #[test]
    fn the_query_asks_the_endpoint_for_both_numbers_at_once() {
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
                      "orphan_count": {"type":"literal","value":"1545695"}}
                   ]}}"#,
            )
            .expect_at_least(1)
            .create();

        let client = crate::sparql::SparqlClient::new(&server.url());
        let counted = count_orphan_targets(
            &client,
            Some("https://packagegraph.github.io/graph/almalinux/9"),
        )
        .unwrap();

        assert_eq!(counted.total_targets, 3_611_932);
        assert_eq!(counted.orphan_targets, 1_545_695);
        assert!(!counted.has_no_orphan_targets());
    }
}
