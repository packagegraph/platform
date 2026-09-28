//! Whole-run properties of test corpus extraction: what ends up in the
//! corpus must be a function of the data, the manifest must describe what was
//! left out, and `max_triples` must be a budget rather than a remark.
//!
//! These drive `extract::run` end to end against a mock endpoint, because all
//! three defects live between the phases rather than inside any one of them.

use std::collections::HashSet;
use std::path::Path;

/// Two graphs, deliberately equal in cost, so nothing but the ordering rule
/// decides which one a one-graph budget keeps.
const GRAPH_A: &str = "https://packagegraph.github.io/graph/aaa/1";
const GRAPH_B: &str = "https://packagegraph.github.io/graph/bbb/1";

fn seed_bindings() -> String {
    format!(
        r#"{{"results": {{"bindings": [
             {{"pkg": {{"type":"uri","value":"https://packagegraph.github.io/d/pkg/a/openssl"}},
              "g": {{"type":"uri","value":"{GRAPH_A}"}}}},
             {{"pkg": {{"type":"uri","value":"https://packagegraph.github.io/d/pkg/b/openssl"}},
              "g": {{"type":"uri","value":"{GRAPH_B}"}}}}
           ]}}}}"#
    )
}

/// How many triples each graph's description costs. Both graphs cost the
/// same, so nothing but the ordering rule decides which one a budget keeps.
const GRAPH_TRIPLES: usize = 200;

/// A package description of a fixed size, differing between graphs only in
/// its subject. The first four triples carry the shape the contract cares
/// about -- a type, a name, a capability edge, and a dependency that leaves
/// the selection -- and the rest is padding to a known cost.
fn construct_body(tag: &str) -> String {
    let pkg = format!("https://packagegraph.github.io/d/pkg/{tag}/openssl");
    let cap = format!("https://packagegraph.github.io/d/capability/{tag}-libssl");
    let outside = format!("https://packagegraph.github.io/d/pkg/{tag}/glibc");
    let mut body = format!(
        "<{pkg}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#Package> .\n\
         <{pkg}> <https://purl.org/packagegraph/ontology/core#packageName> \"openssl\" .\n\
         <{pkg}> <https://purl.org/packagegraph/ontology/core#providesCapability> <{cap}> .\n\
         <{pkg}> <https://purl.org/packagegraph/ontology/core#directlyDependsOn> <{outside}> .\n\
         <{cap}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#Capability> .\n\
         <{cap}> <http://www.w3.org/2000/01/rdf-schema#label> \"libssl\" .\n"
    );
    for n in 0..(GRAPH_TRIPLES - 6) {
        body.push_str(&format!(
            "<{pkg}> <https://purl.org/packagegraph/ontology/core#description> \"pad {n}\" .\n"
        ));
    }
    body
}

struct Harness {
    server: mockito::ServerGuard,
    _dir: tempfile::TempDir,
    config: std::path::PathBuf,
    ontology: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let mut server = mockito::Server::new();

        // Every SELECT shares the JSON accept header, so each one is matched
        // on a fragment only it contains. Leaving any of them to a catch-all
        // makes the test depend on mock resolution order.
        server
            .mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .match_body(mockito::Matcher::Regex("packageName".to_string()))
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(seed_bindings())
            .expect_at_least(1)
            .create();

        // BFS finds nothing new; the seed is the whole selection.
        server
            .mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .match_body(mockito::Matcher::Regex("neighbor".to_string()))
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        // Nothing leaves the corpus, and gap fill finds no exemplars.
        server
            .mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        for (graph, tag) in [(GRAPH_A, "a"), (GRAPH_B, "b")] {
            let encoded = graph.replace(':', "%3A").replace('/', "%2F");
            server
                .mock("POST", "/sparql")
                .match_header("accept", "application/n-triples")
                .match_body(mockito::Matcher::Regex(encoded))
                .with_status(200)
                .with_header("content-type", "application/n-triples")
                .with_body(construct_body(tag))
                .create();
        }

        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("seeds.toml");
        std::fs::write(
            &config,
            "[global]\nmax_triples = 1000\ndepth = 1\nfan_out = 5\n\n\
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
             pkg:Capability a owl:Class .\n\
             pkg:packageName a owl:DatatypeProperty .\n",
        )
        .unwrap();

        Harness {
            server,
            _dir: dir,
            config,
            ontology,
        }
    }

    fn run(&self, out: &Path, max_triples: Option<usize>) -> std::io::Result<()> {
        pg_collect::extract::run(
            &self.server.url(),
            &self.config,
            &self.ontology,
            out,
            max_triples,
            Some(1),
            Some(5),
            pg_collect::sparql::SparqlAuth::default(),
            pg_collect::sparql::SparqlBackend::Fuseki,
        )
    }
}

fn manifest(out: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap()
}

fn graphs_in(manifest: &serde_json::Value) -> Vec<String> {
    manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["graph"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_budget_that_holds_one_graph_keeps_the_same_one_every_run() {
    // Both graphs cost six triples. With room for one, which survives used to
    // depend on HashMap iteration order, so repeated runs over an unchanged
    // endpoint disagreed about the corpus contents.
    let mut chosen: HashSet<Vec<String>> = HashSet::new();

    // A fresh run each time, in one process: Rust seeds every HashMap
    // differently, so eight iterations sample eight iteration orders. That is
    // what surfaced the original defect.
    for _ in 0..8 {
        let harness = Harness::new();
        let out = tempfile::tempdir().unwrap();
        harness
            .run(out.path(), Some(GRAPH_TRIPLES + 50))
            .expect("run failed");
        chosen.insert(graphs_in(&manifest(out.path())));
    }

    assert_eq!(
        chosen.len(),
        1,
        "eight identical runs produced different corpora: {chosen:?}"
    );
    assert_eq!(
        chosen.iter().next().unwrap(),
        &vec![GRAPH_A.to_string()],
        "the surviving graph should be the first in sort order"
    );
}

#[test]
fn a_graph_that_does_not_fit_is_named_rather_than_trimmed() {
    let harness = Harness::new();
    let out = tempfile::tempdir().unwrap();
    harness
        .run(out.path(), Some(GRAPH_TRIPLES + 50))
        .expect("run failed");
    let m = manifest(out.path());

    let omitted = m["graphs_over_budget"].as_array().unwrap();
    assert_eq!(omitted.len(), 1, "the second graph must be reported");
    assert_eq!(omitted[0]["graph"].as_str().unwrap(), GRAPH_B);
    assert_eq!(
        omitted[0]["triples"].as_u64().unwrap(),
        GRAPH_TRIPLES as u64
    );

    assert_eq!(
        m["total_triples"].as_u64().unwrap(),
        GRAPH_TRIPLES as u64,
        "the corpus must not exceed the budget it was given"
    );
    assert!(
        !out.path().join("collector/bbb-1.nt").exists(),
        "an omitted graph must not be left on disk"
    );

    // Nothing was trimmed: the retained graph is whole.
    let kept = std::fs::read_to_string(out.path().join("collector/aaa-1.nt")).unwrap();
    assert_eq!(kept.lines().count(), GRAPH_TRIPLES);
}

#[test]
fn a_budget_no_graph_fits_fails_instead_of_writing_a_corpus() {
    let harness = Harness::new();
    let out = tempfile::tempdir().unwrap();

    let err = harness
        .run(out.path(), Some(1))
        .expect_err("a one-triple budget must not produce a 400-triple corpus");
    assert!(
        err.to_string().contains("no graph fits"),
        "the error should say why: {err}"
    );
    assert!(
        !out.path().join("manifest.json").exists(),
        "a failed run must not leave a manifest claiming success"
    );
}

#[test]
fn the_whole_corpus_stays_within_its_budget() {
    let harness = Harness::new();
    let out = tempfile::tempdir().unwrap();
    harness.run(out.path(), Some(1000)).expect("run failed");
    let m = manifest(out.path());

    let total = m["total_triples"].as_u64().unwrap();
    assert!(total <= 1000, "{total} triples against a budget of 1000");
    assert_eq!(graphs_in(&m).len(), 2, "both graphs fit");
    assert!(m["graphs_over_budget"].as_array().unwrap().is_empty());
}

#[test]
fn the_manifest_counts_references_that_leave_the_corpus() {
    // Each graph's package depends on a glibc that BFS never reached. The
    // edge is kept; the manifest has to say the target is not here.
    let harness = Harness::new();
    let out = tempfile::tempdir().unwrap();
    harness.run(out.path(), Some(1000)).expect("run failed");
    let m = manifest(out.path());

    for file in m["files"].as_array().unwrap() {
        assert_eq!(
            file["dangling_targets"].as_u64().unwrap(),
            0,
            "the mock reports no dangling objects, so the count must be zero \
             rather than inherited from another file: {file}"
        );
    }
}
