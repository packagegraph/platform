//! Seed generator — queries Fuseki for distinct package names in a graph.
//!
//! Usage: pg-collect seed --endpoint <fuseki-url> --graph <graph-uri> -o <output-file>
//!
//! Queries for all distinct core:packageName values in the specified graph,
//! writes them to a text file (one per line, sorted, deduplicated).

use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend};
use std::fs::File;
use std::io::{Result, Write};

/// The graph pattern binding a package's upstream names in one ecosystem.
///
/// Shared by the seed query and the ambiguity count below, so the two cannot
/// drift apart about what "in this ecosystem" means -- a count taken over a
/// different population would not describe what the seed dropped.
/// Handles both old string literals and v0.6.0 Ecosystem URIs.
fn ecosystem_pattern(ecosystem: &str) -> String {
    format!(
        "?pkg pkg:upstreamPackageName ?name .\n\
         ?pkg pkg:upstreamEcosystem ?eco .\n\
         FILTER(STR(?eco) = \"{ecosystem}\" || CONTAINS(STR(?eco), \"ecosystem/{ecosystem}\"))"
    )
}

/// A package whose Provides named a SECOND upstream ecosystem. `?eco` and
/// `?pkg` come from `ecosystem_pattern`.
///
/// `upstreamEcosystem` and `upstreamPackageName` are independently
/// multi-valued on one subject, so a package in two ecosystems joins as a
/// cross-product: a crate name is handed to the PyPI collector, which either
/// 404s or collects an unrelated project and attaches it here (#46). RDF
/// stores the two as unordered sets with no pairing, so nothing in the data
/// can say which name came from which ecosystem. Until a model that pairs
/// them exists, the only correct answer is to not guess.
const SECOND_ECOSYSTEM: &str = "?pkg pkg:upstreamEcosystem ?other .\n\
                                FILTER(?other != ?eco)";

const PREFIX: &str = "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>";

/// Upstream names in `ecosystem`, from packages that name only that one.
fn ecosystem_seed_query(ecosystem: &str) -> String {
    format!(
        "{PREFIX}\nSELECT DISTINCT ?name WHERE {{\n\
           GRAPH ?g {{\n{pattern}\n\
             FILTER NOT EXISTS {{\n{second}\n}}\n\
           }}\n\
         }} ORDER BY ?name",
        pattern = ecosystem_pattern(ecosystem),
        second = SECOND_ECOSYSTEM,
    )
}

/// How many packages the seed query above declined to answer for. The exact
/// complement of it, so the number describes what was dropped and nothing else.
fn ecosystem_ambiguity_query(ecosystem: &str) -> String {
    format!(
        "{PREFIX}\nSELECT (COUNT(DISTINCT ?pkg) AS ?ambiguous) WHERE {{\n\
           GRAPH ?g {{\n{pattern}\n\
             FILTER EXISTS {{\n{second}\n}}\n\
           }}\n\
         }}",
        pattern = ecosystem_pattern(ecosystem),
        second = SECOND_ECOSYSTEM,
    )
}

/// Read the count out of an aggregate result. A store that answers with no
/// rows, or with something that is not a number, has told us nothing -- which
/// is not the same as telling us zero.
fn ambiguous_count(bindings: &[std::collections::HashMap<String, String>]) -> Option<u64> {
    bindings.first()?.get("ambiguous")?.parse().ok()
}

/// Discover package names for a specific upstream ecosystem from Fuseki.
///
/// Packages whose Provides named more than one upstream ecosystem are left
/// out: see `SECOND_ECOSYSTEM`. They are counted and reported, because
/// dropping them silently is the same shape of failure as seeding them wrongly.
pub fn discover_by_ecosystem(
    endpoint: &str,
    ecosystem: &str,
    auth: &SparqlAuth,
    backend: SparqlBackend,
) -> Result<Vec<String>> {
    let client = make_sparql_client(endpoint, auth, backend);

    eprintln!(
        "Querying Fuseki for {} upstream package names...",
        ecosystem
    );
    let bindings = client.query(&ecosystem_seed_query(ecosystem))?;

    let names: Vec<String> = bindings
        .into_iter()
        .filter_map(|b| b.get("name").cloned())
        .collect();

    eprintln!("Found {} distinct {} package names", names.len(), ecosystem);

    // Advisory: a failure here must not lose a seed list that is already in
    // hand and already correct.
    match client.query(&ecosystem_ambiguity_query(ecosystem)) {
        Ok(rows) => match ambiguous_count(&rows) {
            Some(n) if n > 0 => eprintln!(
                "Skipped {n} package(s) in more than one upstream ecosystem: \
                 nothing in the graph says which of their upstream names is a {ecosystem} one"
            ),
            Some(_) => {}
            None => eprintln!("Warning: ambiguous-package count returned no usable answer"),
        },
        Err(e) => eprintln!("Warning: could not count ambiguous packages: {e}"),
    }

    Ok(names)
}

pub fn generate_seed(
    endpoint: &str,
    graph_uri: &str,
    output_path: &str,
    auth: SparqlAuth,
    backend: SparqlBackend,
) -> Result<()> {
    let client = make_sparql_client(endpoint, &auth, backend);

    // Query for distinct package names in the specified graph
    let sparql = format!(
        "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
         SELECT DISTINCT ?name WHERE {{\n\
           GRAPH <{graph_uri}> {{\n\
             ?p pkg:packageName ?name .\n\
           }}\n\
         }} ORDER BY ?name"
    );

    eprintln!("Querying for package names in graph: {}", graph_uri);
    let bindings = client.query(&sparql)?;

    // Extract package names
    let names: Vec<String> = bindings
        .into_iter()
        .filter_map(|b| b.get("name").cloned())
        .collect();

    eprintln!("Found {} distinct package names", names.len());

    // Write to output file (one per line)
    let mut file = File::create(output_path)?;
    for name in &names {
        writeln!(file, "{}", name)?;
    }

    eprintln!("Wrote {} package names to {}", names.len(), output_path);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // --- a package in two ecosystems seeds neither (#46) ---

    const RESULTS: &str = "application/sparql-results+json";

    fn names_body(names: &[&str]) -> String {
        let rows: Vec<String> = names
            .iter()
            .map(|n| format!("{{\"name\": {{\"type\": \"literal\", \"value\": \"{n}\"}}}}"))
            .collect();
        format!("{{\"results\": {{\"bindings\": [{}]}}}}", rows.join(","))
    }

    fn count_body(n: u64) -> String {
        format!(
            "{{\"results\": {{\"bindings\": [{{\"ambiguous\": \
             {{\"type\": \"literal\", \"value\": \"{n}\"}}}}]}}}}"
        )
    }

    #[test]
    fn the_ambiguity_count_is_the_exact_complement_of_the_seed_query() {
        // If the two ever ask about different populations, the number
        // reported stops describing what the seed dropped, and a reader has
        // no way to tell. Same pattern, opposite guard, nothing else.
        let seed = ecosystem_seed_query("pypi");
        let count = ecosystem_ambiguity_query("pypi");
        let pattern = ecosystem_pattern("pypi");

        assert!(seed.contains(&pattern), "{seed}");
        assert!(count.contains(&pattern), "{count}");
        assert!(seed.contains(SECOND_ECOSYSTEM));
        assert!(count.contains(SECOND_ECOSYSTEM));

        assert!(seed.contains("FILTER NOT EXISTS"), "{seed}");
        assert!(count.contains("FILTER EXISTS"), "{count}");
        assert!(
            !count.contains("NOT EXISTS"),
            "the count must select the packages the seed rejected, not repeat it:\n{count}"
        );
    }

    #[test]
    fn the_ecosystem_pattern_still_matches_both_shapes_of_ecosystem_value() {
        // Pre-v0.6.0 graphs carry a literal; later ones carry an Ecosystem
        // URI. Losing either would empty a seed list without failing.
        let pattern = ecosystem_pattern("cargo");
        assert!(pattern.contains("STR(?eco) = \"cargo\""), "{pattern}");
        assert!(
            pattern.contains("CONTAINS(STR(?eco), \"ecosystem/cargo\")"),
            "{pattern}"
        );
    }

    #[test]
    fn an_aggregate_with_no_usable_answer_is_not_read_as_zero() {
        use std::collections::HashMap;
        assert_eq!(ambiguous_count(&[]), None, "no rows is not an answer");
        let row: HashMap<String, String> = [("other".to_string(), "3".to_string())]
            .into_iter()
            .collect();
        assert_eq!(
            ambiguous_count(&[row]),
            None,
            "wrong variable is not an answer"
        );
        let garbage: HashMap<String, String> = [("ambiguous".to_string(), "many".to_string())]
            .into_iter()
            .collect();
        assert_eq!(ambiguous_count(&[garbage]), None);
        let good: HashMap<String, String> = [("ambiguous".to_string(), "7".to_string())]
            .into_iter()
            .collect();
        assert_eq!(ambiguous_count(&[good]), Some(7));
    }

    #[test]
    fn the_seed_query_sent_to_the_store_excludes_ambiguous_packages() {
        // The exclusion happens in the store, so what this can prove is that
        // the store was asked for it. The mock only answers a request whose
        // body carries the guard; mock.assert() fails if none arrived.
        let mut server = mockito::Server::new();
        let seed = server
            .mock("POST", "/sparql")
            .match_body(mockito::Matcher::Regex(r"NOT\+EXISTS".to_string()))
            .with_status(200)
            .with_header("content-type", RESULTS)
            .with_body(names_body(&["requests", "urllib3"]))
            .create();
        let count = server
            .mock("POST", "/sparql")
            .match_body(mockito::Matcher::Regex(r"COUNT".to_string()))
            .with_status(200)
            .with_header("content-type", RESULTS)
            .with_body(count_body(4))
            .create();

        let names =
            discover_by_ecosystem(&server.url(), "pypi", &None, SparqlBackend::Fuseki).unwrap();

        seed.assert();
        count.assert();
        assert_eq!(names, vec!["requests".to_string(), "urllib3".to_string()]);
    }

    #[test]
    fn a_useless_ambiguity_answer_does_not_lose_the_seed_list() {
        // The count is advisory. Losing a correct seed list over a failed
        // side query would trade away the thing the guard exists to protect.
        let mut server = mockito::Server::new();
        server
            .mock("POST", "/sparql")
            .match_body(mockito::Matcher::Regex(r"NOT\+EXISTS".to_string()))
            .with_status(200)
            .with_header("content-type", RESULTS)
            .with_body(names_body(&["serde"]))
            .create();
        server
            .mock("POST", "/sparql")
            .match_body(mockito::Matcher::Regex(r"COUNT".to_string()))
            .with_status(200)
            .with_header("content-type", RESULTS)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let names =
            discover_by_ecosystem(&server.url(), "cargo", &None, SparqlBackend::Fuseki).unwrap();
        assert_eq!(names, vec!["serde".to_string()]);
    }

    #[test]
    fn test_generate_seed_with_mockito() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(
                r#"{
                "results": {
                    "bindings": [
                        {"name": {"type": "literal", "value": "bash"}},
                        {"name": {"type": "literal", "value": "curl"}},
                        {"name": {"type": "literal", "value": "git"}}
                    ]
                }
            }"#,
            )
            .create();

        let output_path = "/tmp/test-seed-output.txt";
        generate_seed(
            &server.url(),
            "https://packagegraph.github.io/graph/test",
            output_path,
            None,
            SparqlBackend::Fuseki,
        )
        .unwrap();

        mock.assert();

        // Verify output file contents
        let content = fs::read_to_string(output_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "bash");
        assert_eq!(lines[1], "curl");
        assert_eq!(lines[2], "git");

        // Cleanup
        fs::remove_file(output_path).ok();
    }

    #[test]
    fn test_generate_seed_empty_result() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let output_path = "/tmp/test-seed-empty.txt";
        generate_seed(
            &server.url(),
            "https://packagegraph.github.io/graph/empty",
            output_path,
            None,
            SparqlBackend::Fuseki,
        )
        .unwrap();

        mock.assert();

        // Verify empty output file
        let content = fs::read_to_string(output_path).unwrap();
        assert_eq!(content, "");

        // Cleanup
        fs::remove_file(output_path).ok();
    }
}
