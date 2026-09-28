//! The reverse-dependency enricher must be able to decline a question it
//! cannot answer.
//!
//! An RPM requirement is a capability token; the collector mints a
//! `PackageIdentity` from it by name, and nothing resolves that token to the
//! package that provides it. A count attached to such a node measures string
//! equality between a requirement and an invented identity. Measured on the
//! live endpoint, 86.4% of almalinux/9's dependency targets and 92.0% of
//! rhel/9's are nodes no package claims to be.
//!
//! The guard is prepared, not deployed: the default still counts them, and
//! says so.

use pg_collect::enrich_revdeps::RevdepsEnricher;
use pg_collect::sparql::{SparqlAuth, SparqlBackend};

/// Returns the server, the mock that answers the counting query, and a decoy
/// that matches only a query carrying the resolution requirement. Asserting
/// the decoy's hit count is how "the filter is absent" gets tested: mockito
/// has no negative matcher.
fn server_asserting() -> (mockito::ServerGuard, mockito::Mock, mockito::Mock) {
    let mut server = mockito::Server::new();

    // The ontology gate and the support assessment both answer from here.
    server
        .mock("POST", "/sparql")
        .match_body(mockito::Matcher::Regex("reverseDependencyCount".to_string()))
        .with_status(200)
        .with_header("content-type", "application/sparql-results+json")
        .with_body(r#"{"results": {"bindings": [{"p": {"type":"uri","value":"http://www.w3.org/2002/07/owl#DatatypeProperty"}}]}}"#)
        .create();

    server
        .mock("POST", "/sparql")
        .match_body(mockito::Matcher::Regex("unresolved_count".to_string()))
        .with_status(200)
        .with_header("content-type", "application/sparql-results+json")
        .with_body(
            r#"{"results": {"bindings": [
                 {"total": {"type":"literal","value":"3928"},
                  "unresolved_count": {"type":"literal","value":"3395"}}
               ]}}"#,
        )
        .create();

    let resolved_only = server
        .mock("POST", "/sparql")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::Regex("revDepCount".to_string()),
            mockito::Matcher::Regex("anyVersion".to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/sparql-results+json")
        .with_body(r#"{"results": {"bindings": []}}"#)
        .create();

    let counting = server
        .mock("POST", "/sparql")
        .match_body(mockito::Matcher::Regex("revDepCount".to_string()))
        .with_status(200)
        .with_header("content-type", "application/sparql-results+json")
        .with_body(
            r#"{"results": {"bindings": [
                 {"targetIdentity": {"type":"uri","value":"https://packagegraph.github.io/d/pkg/almalinux/9/glibc"},
                  "revDepCount": {"type":"literal","value":"42"}}
               ]}}"#,
        )
        .expect_at_least(1)
        .create();

    (server, counting, resolved_only)
}

#[test]
fn the_default_still_counts_unresolved_targets_so_nothing_is_deployed_by_accident() {
    let (server, counting, resolved_only) = server_asserting();
    let out = tempfile::NamedTempFile::new().unwrap();

    let enricher = RevdepsEnricher::new(
        &server.url(),
        Some("https://packagegraph.github.io/graph/almalinux/9"),
        SparqlAuth::default(),
        SparqlBackend::Fuseki,
    );
    let (identities, triples) = enricher
        .enrich(out.path().to_str().unwrap())
        .expect("enrich failed");

    assert_eq!(identities, 1);
    assert_eq!(triples, 1);
    counting.assert();
    resolved_only.expect(0).assert();
}

#[test]
fn opting_in_asks_the_endpoint_only_for_identities_a_package_claims_to_be() {
    let (server, _counting, resolved_only) = server_asserting();
    let out = tempfile::NamedTempFile::new().unwrap();

    let enricher = RevdepsEnricher::new(
        &server.url(),
        Some("https://packagegraph.github.io/graph/almalinux/9"),
        SparqlAuth::default(),
        SparqlBackend::Fuseki,
    )
    .withholding_unresolved(true);

    enricher
        .enrich(out.path().to_str().unwrap())
        .expect("enrich failed");

    // The decoy is the only mock carrying the resolution requirement, so a
    // regression that drops it fails this assertion rather than quietly
    // counting invented identities again.
    resolved_only.assert();
}

#[test]
fn an_unresolved_identity_gets_no_triple_rather_than_a_zero() {
    // The endpoint returns nothing once the resolution requirement is on, so
    // the manufactured identity must be absent from the output entirely.
    let mut server = mockito::Server::new();
    server
        .mock("POST", "/sparql")
        .match_body(mockito::Matcher::Regex("reverseDependencyCount".to_string()))
        .with_status(200)
        .with_header("content-type", "application/sparql-results+json")
        .with_body(r#"{"results": {"bindings": [{"p": {"type":"uri","value":"http://www.w3.org/2002/07/owl#DatatypeProperty"}}]}}"#)
        .create();
    server
        .mock("POST", "/sparql")
        .with_status(200)
        .with_header("content-type", "application/sparql-results+json")
        .with_body(r#"{"results": {"bindings": []}}"#)
        .create();

    let out = tempfile::NamedTempFile::new().unwrap();
    let enricher = RevdepsEnricher::new(
        &server.url(),
        Some("https://packagegraph.github.io/graph/almalinux/9"),
        SparqlAuth::default(),
        SparqlBackend::Fuseki,
    )
    .withholding_unresolved(true);

    let (identities, triples) = enricher
        .enrich(out.path().to_str().unwrap())
        .expect("enrich failed");

    assert_eq!(identities, 0);
    assert_eq!(triples, 0);
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        !written.contains("reverseDependencyCount"),
        "an unanswerable question must produce no assertion, not a zero: {written}"
    );
}
