//! Gap fill is part of the corpus, so it has to account for itself.
//!
//! It used to call the non-reporting wrappers and then write a hardcoded
//! `dangling_targets: 0` into the manifest, and its mini-BFS cuts never
//! reached the corpus-wide selection report. A corpus that pulled in an
//! exemplar with references leaving the selection, and truncated that
//! exemplar's neighbourhood, still claimed it had omitted nothing.

const MAIN_GRAPH: &str = "https://packagegraph.github.io/graph/aaa/1";
const EXEMPLAR_GRAPH: &str = "https://packagegraph.github.io/graph/zzz/1";
const MISSING_CLASS: &str = "https://purl.org/packagegraph/ontology/core#Distribution";

fn encoded(uri: &str) -> String {
    uri.replace(':', "%3A").replace('/', "%2F")
}

fn bindings(rows: &[String]) -> String {
    format!(r#"{{"results": {{"bindings": [{}]}}}}"#, rows.join(","))
}

fn json_mock(server: &mut mockito::ServerGuard, fragments: &[&str], body: String) {
    let matchers: Vec<mockito::Matcher> = fragments
        .iter()
        .map(|f| mockito::Matcher::Regex((*f).to_string()))
        .collect();
    server
        .mock("POST", "/sparql")
        .match_header("accept", "application/sparql-results+json")
        .match_body(mockito::Matcher::AllOf(matchers))
        .with_status(200)
        .with_header("content-type", "application/sparql-results+json")
        .with_body(body)
        .create();
}

#[test]
fn gap_fill_reports_its_own_cuts_and_dangling_references() {
    let mut server = mockito::Server::new();

    // Phase 1: one seed, one graph.
    json_mock(
        &mut server,
        &["packageName"],
        bindings(&[format!(
            r#"{{"pkg": {{"type":"uri","value":"https://packagegraph.github.io/d/pkg/a/openssl"}},
                "g": {{"type":"uri","value":"{MAIN_GRAPH}"}}}}"#
        )]),
    );

    // Phase 2: the main graph expands to nothing new.
    json_mock(
        &mut server,
        &[&encoded(MAIN_GRAPH), "neighbor"],
        bindings(&[]),
    );

    // Phase 4: the exemplar for the class the corpus is missing lives in a
    // different graph.
    json_mock(
        &mut server,
        &["Distribution", "ORDER"],
        bindings(&[format!(
            r#"{{"pkg": {{"type":"uri","value":"https://packagegraph.github.io/d/dist/zzz"}},
                "g": {{"type":"uri","value":"{EXEMPLAR_GRAPH}"}}}}"#
        )]),
    );

    // The exemplar's mini-BFS caps at fan_out 5, so three of its eight
    // neighbours are cut.
    let neighbours: Vec<String> = (0..8)
        .map(|n| {
            format!(
                r#"{{"seed": {{"type":"uri","value":"https://packagegraph.github.io/d/dist/zzz"}},
                    "predicate": {{"type":"uri","value":"https://purl.org/packagegraph/ontology/core#memberOfPackageSet"}},
                    "neighbor": {{"type":"uri","value":"https://packagegraph.github.io/d/pkg/z/p{n}"}}}}"#
            )
        })
        .collect();
    json_mock(
        &mut server,
        &[&encoded(EXEMPLAR_GRAPH), "neighbor"],
        bindings(&neighbours),
    );

    // One of the exemplar's objects is outside the selection.
    json_mock(
        &mut server,
        &[&encoded(EXEMPLAR_GRAPH), "DISTINCT"],
        bindings(&[
            r#"{"o": {"type":"uri","value":"https://packagegraph.github.io/d/pkg/z/outside"}}"#
                .to_string(),
        ]),
    );

    // Everything else that asks for bindings gets none.
    server
        .mock("POST", "/sparql")
        .match_header("accept", "application/sparql-results+json")
        .with_status(200)
        .with_header("content-type", "application/sparql-results+json")
        .with_body(bindings(&[]))
        .create();

    server
        .mock("POST", "/sparql")
        .match_header("accept", "application/n-triples")
        .match_body(mockito::Matcher::Regex(encoded(MAIN_GRAPH)))
        .with_status(200)
        .with_header("content-type", "application/n-triples")
        .with_body(
            "<https://packagegraph.github.io/d/pkg/a/openssl> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#Package> .\n",
        )
        .create();

    server
        .mock("POST", "/sparql")
        .match_header("accept", "application/n-triples")
        .with_status(200)
        .with_header("content-type", "application/n-triples")
        .with_body(
            "<https://packagegraph.github.io/d/dist/zzz> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#Distribution> .\n\
             <https://packagegraph.github.io/d/dist/zzz> <https://purl.org/packagegraph/ontology/core#memberOfPackageSet> <https://packagegraph.github.io/d/pkg/z/outside> .\n",
        )
        .create();

    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("seeds.toml");
    std::fs::write(
        &config,
        "[global]\nmax_triples = 10000\ndepth = 1\nfan_out = 5\n\n\
         [seeds.linux_distro]\npackages = [\"openssl\"]\n",
    )
    .unwrap();
    let ontology = dir.path().join("ontology");
    std::fs::create_dir_all(&ontology).unwrap();
    std::fs::write(
        ontology.join("core.ttl"),
        "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
         @prefix pkg: <https://purl.org/packagegraph/ontology/core#> .\n\
         pkg:Package a owl:Class .\n\
         pkg:Distribution a owl:Class .\n",
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();

    pg_collect::extract::run(
        &server.url(),
        &config,
        &ontology,
        out.path(),
        None,
        Some(1),
        Some(5),
        pg_collect::sparql::SparqlAuth::default(),
        pg_collect::sparql::SparqlBackend::Fuseki,
    )
    .expect("run failed");

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path().join("manifest.json")).unwrap())
            .unwrap();

    let gap = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["graph"] == "gap-fill")
        .expect("gap fill ran, so the manifest must carry its entry");

    assert_eq!(
        gap["dangling_targets"].as_u64().unwrap(),
        1,
        "the exemplar references a package the corpus does not carry; the \
         manifest reported {gap}"
    );
    assert_eq!(
        manifest["selection"]["fan_out_cuts"].as_u64().unwrap(),
        3,
        "gap fill capped eight neighbours at five, and those cuts belong in \
         the corpus-wide selection report"
    );
    assert!(
        manifest["selection"]["capped_pairs"].as_u64().unwrap() >= 1,
        "the truncated pair must be counted too"
    );

    // The class the corpus was missing is now covered, so gap fill is not
    // being asserted about an empty run.
    assert!(
        !manifest["classes_over_budget"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == MISSING_CLASS),
        "the budget was ample; the class should have been covered"
    );
}
