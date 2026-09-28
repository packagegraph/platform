//! End-to-end check that extraction preserves an RPM capability record whole.
//!
//! `tests/fixtures/rpm-declaration-extract.nt` is what the endpoint holds for
//! one selected package: its type and name, three capabilities it provides,
//! one dependency edge that leaves the selection, and a deliberately
//! unlabelled capability. Extraction must hand all of it back.
//!
//! The record that matters most here is the type assertion. `sh:targetClass`
//! is the only way a shape acquires focus nodes, so a corpus whose
//! `rdf:type` triples were stripped satisfies every class-targeted shape over
//! zero nodes. Shapes that reach their targets another way -- `sh:targetNode`,
//! `sh:targetSubjectsOf` -- are not affected by this.
//! The unlabelled capability is in the fixture to make that concrete: it
//! violates `pkg:CapabilityShape`'s `sh:minCount 1` on `rdfs:label` at the
//! source, and it has to keep violating it after extraction rather than
//! disappearing from the shape's target set.
//!
//! These assertions are structural: they check the two facts a SHACL engine
//! would use -- the node is still typed, and it still has no label. Running
//! the shapes themselves belongs to the corpus audit script, which has an RDF
//! toolchain; this test guards the extractor.

use std::collections::HashSet;

const PKG: &str = "https://purl.org/packagegraph/ontology/core#";
const DATA: &str = "https://packagegraph.github.io/d/";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";

const OPENSSL: &str = "https://packagegraph.github.io/d/pkg/almalinux/9/x86_64/openssl";
const GLIBC: &str = "https://packagegraph.github.io/d/pkg/almalinux/9/x86_64/glibc";
const UNLABELLED: &str = "https://packagegraph.github.io/d/capability/unlabelled";

fn fixture() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rpm-declaration-extract.nt"
    ))
    .expect("fixture is missing")
}

/// The selection an outgoing BFS reaches from `openssl`: the package and every
/// capability it provides. `glibc` is deliberately left out, so the dependency
/// edge points outside the corpus.
fn selection() -> HashSet<String> {
    fixture()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            l.split_once('>')
                .expect("subject")
                .0
                .trim_start_matches('<')
        })
        .filter(|s| *s != GLIBC)
        .map(str::to_string)
        .collect()
}

fn extract(fixture_body: &str, dangling: &[&str]) -> (Vec<String>, usize) {
    let mut server = mockito::Server::new();
    let _construct = server
        .mock("POST", "/sparql")
        .match_header("accept", "application/n-triples")
        .with_status(200)
        .with_header("content-type", "application/n-triples")
        .with_body(fixture_body)
        .expect_at_least(1)
        .create();
    let bindings: Vec<String> = dangling
        .iter()
        .map(|u| format!(r#"{{"o": {{"type": "uri", "value": "{u}"}}}}"#))
        .collect();
    let _objects = server
        .mock("POST", "/sparql")
        .match_header("accept", "application/sparql-results+json")
        .with_status(200)
        .with_header("content-type", "application/sparql-results+json")
        .with_body(format!(
            r#"{{"results": {{"bindings": [{}]}}}}"#,
            bindings.join(",")
        ))
        .create();

    let client = pg_collect::sparql::SparqlClient::new(&server.url());
    let (triples, report) = pg_collect::extract::extract_triples_reporting(
        &client,
        "https://packagegraph.github.io/graph/almalinux/9",
        &selection(),
    )
    .expect("extraction failed");
    (triples, report.dangling_targets)
}

fn triple(subject: &str, predicate: &str, object: &str) -> String {
    format!("<{subject}> <{predicate}> <{object}> .")
}

fn literal(subject: &str, predicate: &str, value: &str) -> String {
    format!("<{subject}> <{predicate}> \"{value}\" .")
}

#[test]
fn a_capability_record_survives_extraction_whole() {
    let (kept, _) = extract(&fixture(), &[GLIBC]);

    let required = [
        triple(OPENSSL, RDF_TYPE, &format!("{PKG}Package")),
        literal(OPENSSL, &format!("{PKG}packageName"), "openssl"),
        triple(
            OPENSSL,
            &format!("{PKG}providesCapability"),
            &format!("{DATA}capability/libssl.so.3%28%29%2864bit%29"),
        ),
        triple(
            &format!("{DATA}capability/libssl.so.3%28%29%2864bit%29"),
            RDF_TYPE,
            &format!("{PKG}Capability"),
        ),
        literal(
            &format!("{DATA}capability/libssl.so.3%28%29%2864bit%29"),
            &format!("{PKG}capabilityName"),
            "libssl.so.3()(64bit)",
        ),
        literal(
            &format!("{DATA}capability/libssl.so.3%28%29%2864bit%29"),
            RDFS_LABEL,
            "libssl.so.3()(64bit)",
        ),
    ];

    for expected in &required {
        assert!(
            kept.contains(expected),
            "extraction dropped a triple the contract requires:\n  {expected}"
        );
    }
}

#[test]
fn the_extracted_corpus_still_gives_capability_shape_something_to_target() {
    // Nonzero targets, not merely a passing mock: count the typed capability
    // nodes the way a SHACL engine selects them.
    let (kept, _) = extract(&fixture(), &[GLIBC]);

    let capability_type = format!("<{RDF_TYPE}> <{PKG}Capability> .");
    let targets = kept
        .iter()
        .filter(|t| t.ends_with(&capability_type))
        .count();
    assert_eq!(
        targets, 3,
        "CapabilityShape must keep all three focus nodes; with none it \
         conforms vacuously"
    );
}

#[test]
fn an_unlabelled_capability_still_violates_its_shape_after_extraction() {
    let source = fixture();
    let (kept, _) = extract(&source, &[GLIBC]);

    let has_label = |corpus: &str| {
        corpus
            .lines()
            .any(|l| l.starts_with(&format!("<{UNLABELLED}>")) && l.contains(RDFS_LABEL))
    };
    assert!(!has_label(&source), "the fixture must start out violating");
    assert!(
        !has_label(&kept.join("\n")),
        "extraction must not invent the label that makes the violation vanish"
    );
    assert!(
        kept.contains(&triple(UNLABELLED, RDF_TYPE, &format!("{PKG}Capability"))),
        "and it must stay typed, or the shape has nothing to fail on"
    );
}

#[test]
fn a_dependency_leaving_the_selection_is_reported_not_deleted() {
    let (kept, dangling) = extract(&fixture(), &[GLIBC]);

    assert!(
        kept.contains(&triple(OPENSSL, &format!("{PKG}directlyDependsOn"), GLIBC)),
        "the edge is a fact about openssl; dropping it rewrites the package"
    );
    assert_eq!(
        dangling, 1,
        "the corpus must declare that it points at a record it does not carry"
    );
}
