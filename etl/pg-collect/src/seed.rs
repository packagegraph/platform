//! Seed generator — queries Fuseki for distinct package names in a graph.
//!
//! Usage: pg-collect seed --endpoint <fuseki-url> --graph <graph-uri> -o <output-file>
//!
//! Queries for all distinct core:packageName values in the specified graph,
//! writes them to a text file (one per line, sorted, deduplicated).

use crate::sparql::SparqlClient;
use std::fs::File;
use std::io::{Result, Write};

/// Discover package names for a specific upstream ecosystem from Fuseki.
///
/// Queries for `upstreamPackageName` where `upstreamEcosystem` matches the given
/// ecosystem name (handles both old string literals and new v0.6.0 Ecosystem URIs).
pub fn discover_by_ecosystem(endpoint: &str, ecosystem: &str) -> Result<Vec<String>> {
    let client = SparqlClient::new(endpoint);
    let sparql = format!(
        "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
         SELECT DISTINCT ?name WHERE {{\n\
           GRAPH ?g {{\n\
             ?pkg pkg:upstreamPackageName ?name .\n\
             ?pkg pkg:upstreamEcosystem ?eco .\n\
             FILTER(STR(?eco) = \"{ecosystem}\" || CONTAINS(STR(?eco), \"ecosystem/{ecosystem}\"))\n\
           }}\n\
         }} ORDER BY ?name"
    );

    eprintln!("Querying Fuseki for {} upstream package names...", ecosystem);
    let bindings = client.query(&sparql)?;

    let names: Vec<String> = bindings
        .into_iter()
        .filter_map(|b| b.get("name").cloned())
        .collect();

    eprintln!("Found {} distinct {} package names", names.len(), ecosystem);
    Ok(names)
}

pub fn generate_seed(
    endpoint: &str,
    graph_uri: &str,
    output_path: &str,
) -> Result<()> {
    let client = SparqlClient::new(endpoint);

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

    #[test]
    fn test_generate_seed_with_mockito() {
        let mut server = mockito::Server::new();
        let mock = server.mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(r#"{
                "results": {
                    "bindings": [
                        {"name": {"type": "literal", "value": "bash"}},
                        {"name": {"type": "literal", "value": "curl"}},
                        {"name": {"type": "literal", "value": "git"}}
                    ]
                }
            }"#)
            .create();

        let output_path = "/tmp/test-seed-output.txt";
        generate_seed(
            &server.url(),
            "https://packagegraph.github.io/graph/test",
            output_path
        ).unwrap();

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
        let mock = server.mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let output_path = "/tmp/test-seed-empty.txt";
        generate_seed(
            &server.url(),
            "https://packagegraph.github.io/graph/empty",
            output_path
        ).unwrap();

        mock.assert();

        // Verify empty output file
        let content = fs::read_to_string(output_path).unwrap();
        assert_eq!(content, "");

        // Cleanup
        fs::remove_file(output_path).ok();
    }
}
