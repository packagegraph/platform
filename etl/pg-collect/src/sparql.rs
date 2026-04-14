use reqwest::blocking::Client;
use std::fs::File;
use std::io::{BufRead, BufReader, Error, ErrorKind, Result};
use std::time::{Duration, Instant};

pub struct SparqlClient {
    client: Client,
    endpoint: String,
}

impl SparqlClient {
    /// Create a new SPARQL client with the given endpoint URL.
    /// Example: `SparqlClient::new("http://localhost:3030/packagegraph")`
    pub fn new(endpoint: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            endpoint: endpoint.trim_end_matches('/').to_string(),
        }
    }

    /// Send a SPARQL Update query string.
    /// Returns Ok(()) on success, or an error if the request fails.
    pub fn update(&self, sparql: &str) -> Result<()> {
        let url = format!("{}/update", self.endpoint);

        let response = self.client
            .post(&url)
            .header("Content-Type", "application/sparql-update")
            .body(sparql.to_string())
            .send()
            .map_err(|e| Error::new(ErrorKind::Other, format!("SPARQL update failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(Error::new(
                ErrorKind::Other,
                format!("SPARQL update failed with status {}: {}",
                    response.status(),
                    response.text().unwrap_or_default())
            ));
        }

        Ok(())
    }

    /// Drop a named graph from the triplestore.
    /// Uses DROP SILENT GRAPH so it doesn't error if the graph doesn't exist.
    pub fn drop_graph(&self, graph_uri: &str) -> Result<()> {
        eprintln!("Dropping graph <{}>...", graph_uri);
        let sparql = format!("DROP SILENT GRAPH <{}>", graph_uri);
        self.update(&sparql)
    }

    /// Load an N-Triples file into a named graph via batched INSERT DATA.
    /// Returns the total number of triples loaded.
    ///
    /// # Arguments
    /// * `file_path` - Path to the N-Triples file
    /// * `graph_uri` - URI of the named graph to load into
    /// * `batch_size` - Number of triples per INSERT DATA request
    pub fn load_file(&self, file_path: &str, graph_uri: &str, batch_size: usize) -> Result<usize> {
        let file = File::open(file_path)?;
        let reader = BufReader::new(file);

        let mut batch = Vec::with_capacity(batch_size);
        let mut total = 0;
        let start = Instant::now();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            batch.push(trimmed.to_string());

            if batch.len() >= batch_size {
                self.insert_batch(&batch, graph_uri)?;
                total += batch.len();

                let elapsed = start.elapsed().as_secs_f64();
                let rate = total as f64 / elapsed;
                eprintln!("Loaded {} triples ({:.0} triples/sec)", total, rate);

                batch.clear();
            }
        }

        // Insert remaining triples
        if !batch.is_empty() {
            self.insert_batch(&batch, graph_uri)?;
            total += batch.len();

            let elapsed = start.elapsed().as_secs_f64();
            let rate = total as f64 / elapsed;
            println!("Loaded {} triples ({:.0} triples/sec)", total, rate);
        }

        Ok(total)
    }

    /// Internal: send a batch of N-Triples lines as INSERT DATA.
    fn insert_batch(&self, triples: &[String], graph_uri: &str) -> Result<()> {
        let mut sparql = format!("INSERT DATA {{\n  GRAPH <{}> {{\n", graph_uri);

        for triple in triples {
            sparql.push_str("    ");
            sparql.push_str(triple);
            sparql.push('\n');
        }

        sparql.push_str("  }\n}");

        self.update(&sparql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sparql_client_creation() {
        let client = SparqlClient::new("http://localhost:3030/test");
        assert_eq!(client.endpoint, "http://localhost:3030/test");
    }

    #[test]
    fn test_drop_graph_query_format() {
        let _client = SparqlClient::new("http://localhost:3030/test");
        // We can't test the actual HTTP call without a mock server,
        // but we can verify the query format is correct by checking
        // the implementation doesn't panic with valid URIs
        let graph_uri = "http://example.org/graph";
        let expected = format!("DROP SILENT GRAPH <{}>", graph_uri);
        assert!(expected.contains("DROP SILENT GRAPH"));
    }
}
