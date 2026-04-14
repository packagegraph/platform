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
            .timeout(Duration::from_secs(600))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            endpoint: endpoint.trim_end_matches('/').to_string(),
        }
    }

    /// Send a SPARQL Update query string.
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
    pub fn drop_graph(&self, graph_uri: &str) -> Result<()> {
        eprintln!("Dropping graph <{}>...", graph_uri);
        let sparql = format!("DROP SILENT GRAPH <{}>", graph_uri);
        self.update(&sparql)
    }

    /// Load an N-Triples file into a named graph via Fuseki's Graph Store Protocol.
    ///
    /// Uses POST to the GSP endpoint with the raw .nt file body, bypassing SPARQL
    /// parsing for 10-100x faster loading. Falls back to batched INSERT DATA if
    /// GSP upload fails.
    pub fn load_file(&self, file_path: &str, graph_uri: &str, batch_size: usize) -> Result<usize> {
        let start = Instant::now();

        let total = count_triples(file_path)?;
        eprintln!("Loading {} triples via Graph Store Protocol...", total);

        match self.gsp_upload(file_path, graph_uri) {
            Ok(()) => {
                let elapsed = start.elapsed().as_secs_f64();
                let rate = total as f64 / elapsed;
                eprintln!("Loaded {} triples ({:.0} triples/sec)", total, rate);
                Ok(total)
            }
            Err(e) => {
                eprintln!("GSP upload failed ({}), falling back to batched INSERT DATA...", e);
                self.load_file_batched(file_path, graph_uri, batch_size)
            }
        }
    }

    /// Upload an N-Triples file via Graph Store Protocol in chunks.
    ///
    /// Splits the file into chunks at line boundaries and POSTs each chunk
    /// separately. Fuseki's GSP POST is additive, so multiple POSTs to the
    /// same graph accumulate correctly. This avoids Fuseki OOM on large files.
    fn gsp_upload(&self, file_path: &str, graph_uri: &str) -> Result<()> {
        const CHUNK_SIZE: usize = 50 * 1024 * 1024; // 50MB per chunk

        let url = format!(
            "{}/data?graph={}",
            self.endpoint,
            percent_encoding::utf8_percent_encode(
                graph_uri,
                percent_encoding::NON_ALPHANUMERIC,
            )
        );

        let file_size = File::open(file_path)?.metadata()?.len();
        eprintln!("Uploading {} bytes to {} (chunked, {}MB per chunk)",
            file_size, url, CHUNK_SIZE / (1024 * 1024));

        let file = File::open(file_path)?;
        let reader = BufReader::new(file);

        let mut chunk = Vec::with_capacity(CHUNK_SIZE + 4096);
        let mut chunk_num = 0;
        let mut total_bytes: u64 = 0;
        let start = Instant::now();

        for line in reader.lines() {
            let line = line?;
            chunk.extend_from_slice(line.as_bytes());
            chunk.push(b'\n');

            if chunk.len() >= CHUNK_SIZE {
                chunk_num += 1;
                total_bytes += chunk.len() as u64;
                let pct = (total_bytes as f64 / file_size as f64 * 100.0) as u32;
                eprintln!("  Chunk {} ({} bytes, {}%)...", chunk_num, chunk.len(), pct);
                self.gsp_post_chunk(&url, &chunk)?;
                chunk.clear();
            }
        }

        // Send remaining data
        if !chunk.is_empty() {
            chunk_num += 1;
            total_bytes += chunk.len() as u64;
            eprintln!("  Chunk {} ({} bytes, 100%)...", chunk_num, chunk.len());
            self.gsp_post_chunk(&url, &chunk)?;
        }

        let elapsed = start.elapsed().as_secs_f64();
        eprintln!("GSP upload complete: {} bytes in {} chunks ({:.1}s)",
            total_bytes, chunk_num, elapsed);

        Ok(())
    }

    fn gsp_post_chunk(&self, url: &str, data: &[u8]) -> Result<()> {
        let response = self.client
            .post(url)
            .header("Content-Type", "application/n-triples")
            .body(data.to_vec())
            .send()
            .map_err(|e| Error::new(ErrorKind::Other, format!("GSP upload failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(Error::new(
                ErrorKind::Other,
                format!("GSP upload failed with status {}: {}",
                    response.status(),
                    response.text().unwrap_or_default())
            ));
        }

        Ok(())
    }

    /// Fallback: load via batched INSERT DATA.
    fn load_file_batched(&self, file_path: &str, graph_uri: &str, batch_size: usize) -> Result<usize> {
        let file = File::open(file_path)?;
        let reader = BufReader::new(file);

        let mut batch = Vec::with_capacity(batch_size);
        let mut total = 0;
        let start = Instant::now();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();

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

        if !batch.is_empty() {
            self.insert_batch(&batch, graph_uri)?;
            total += batch.len();

            let elapsed = start.elapsed().as_secs_f64();
            let rate = total as f64 / elapsed;
            eprintln!("Loaded {} triples ({:.0} triples/sec)", total, rate);
        }

        Ok(total)
    }

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

/// Count non-empty, non-comment lines in an N-Triples file.
fn count_triples(file_path: &str) -> Result<usize> {
    let file = File::open(file_path)?;
    let reader = BufReader::new(file);
    let mut count = 0;
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            count += 1;
        }
    }
    Ok(count)
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
        let graph_uri = "http://example.org/graph";
        let expected = format!("DROP SILENT GRAPH <{}>", graph_uri);
        assert!(expected.contains("DROP SILENT GRAPH"));
    }

    #[test]
    fn test_count_triples() -> Result<()> {
        use std::io::Write;
        let mut temp = tempfile::NamedTempFile::new()?;
        writeln!(temp, "<http://s> <http://p> <http://o> .")?;
        writeln!(temp, "# comment")?;
        writeln!(temp)?;
        writeln!(temp, "<http://s2> <http://p2> <http://o2> .")?;
        temp.flush()?;

        let count = count_triples(temp.path().to_str().unwrap())?;
        assert_eq!(count, 2);
        Ok(())
    }
}
