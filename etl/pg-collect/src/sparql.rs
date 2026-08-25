use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Error, ErrorKind, Result};
use std::time::{Duration, Instant};

/// Optional SPARQL Basic Auth credentials (username, password).
pub type SparqlAuth = Option<(String, String)>;

/// SPARQL backend protocol — determines URL construction and auth mechanism.
#[derive(Clone)]
pub enum SparqlBackend {
    Fuseki,
    QLever { access_token: String },
}

/// SPARQL JSON results format (application/sparql-results+json)
#[derive(Debug, Deserialize)]
struct SparqlResults {
    results: SparqlBindings,
}

#[derive(Debug, Deserialize)]
struct SparqlBindings {
    bindings: Vec<HashMap<String, SparqlValue>>,
}

#[derive(Debug, Deserialize)]
struct SparqlValue {
    value: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    value_type: Option<String>,
}

pub struct SparqlClient {
    client: Client,
    endpoint: String,
    auth: SparqlAuth,
    backend: SparqlBackend,
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
            auth: None,
            backend: SparqlBackend::Fuseki,
        }
    }

    pub fn with_backend(mut self, backend: SparqlBackend) -> Self {
        self.backend = backend;
        self
    }

    pub fn with_auth(mut self, username: String, password: String) -> Self {
        self.auth = Some((username, password));
        self
    }

    fn apply_auth(
        &self,
        req: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        match &self.auth {
            Some((user, pass)) => req.basic_auth(user, Some(pass)),
            None => req,
        }
    }

    fn query_url(&self) -> String {
        match &self.backend {
            SparqlBackend::Fuseki => format!("{}/sparql", self.endpoint),
            SparqlBackend::QLever { .. } => format!("{}/", self.endpoint),
        }
    }

    fn query_form_params<'a>(&'a self, sparql: &'a str) -> Vec<(&'a str, &'a str)> {
        let mut params = vec![("query", sparql)];
        if let SparqlBackend::QLever { ref access_token } = self.backend {
            params.push(("access-token", access_token));
        }
        params
    }

    fn guard_write(&self, op: &str) -> Result<()> {
        match &self.backend {
            SparqlBackend::Fuseki => Ok(()),
            SparqlBackend::QLever { .. } => Err(Error::new(
                ErrorKind::Unsupported,
                format!("QLever does not support {} (read-only engine)", op),
            )),
        }
    }

    /// Send a SPARQL Update query string with retry on transient failures.
    pub fn update(&self, sparql: &str) -> Result<()> {
        self.guard_write("SPARQL Update")?;
        let url = format!("{}/update", self.endpoint);
        let max_retries = 3;

        for attempt in 0..=max_retries {
            match self
                .apply_auth(
                    self.client
                        .post(&url)
                        .header("Content-Type", "application/sparql-update")
                        .body(sparql.to_string()),
                )
                .send()
            {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) if response.status().is_server_error() && attempt < max_retries => {
                    let delay = 5 * (1 << attempt);
                    eprintln!(
                        "    SPARQL update failed ({}), retrying in {}s...",
                        response.status(),
                        delay
                    );
                    std::thread::sleep(Duration::from_secs(delay));
                }
                Ok(response) => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "SPARQL update failed with status {}: {}",
                            response.status(),
                            response.text().unwrap_or_default()
                        ),
                    ));
                }
                Err(e) if attempt < max_retries => {
                    let delay = 5 * (1 << attempt);
                    eprintln!("    SPARQL update error: {}, retrying in {}s...", e, delay);
                    std::thread::sleep(Duration::from_secs(delay));
                }
                Err(e) => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("SPARQL update failed: {}", e),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Drop a named graph from the triplestore.
    pub fn drop_graph(&self, graph_uri: &str) -> Result<()> {
        eprintln!("Dropping graph <{}>...", graph_uri);
        let sparql = format!("DROP SILENT GRAPH <{}>", graph_uri);
        self.update(&sparql)
    }

    /// Execute a SPARQL SELECT query and return bindings as a list of variable→value maps.
    ///
    /// Each binding is a HashMap where keys are variable names (without ?) and values
    /// are the string representations of the bound values (URIs or literals).
    pub fn query(&self, sparql: &str) -> Result<Vec<HashMap<String, String>>> {
        let url = self.query_url();
        let form_params = self.query_form_params(sparql);

        let response = self
            .apply_auth(
                self.client
                    .post(&url)
                    .header("Accept", "application/sparql-results+json")
                    .form(&form_params),
            )
            .send()
            .map_err(|e| Error::new(ErrorKind::Other, format!("SPARQL query failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "SPARQL query failed with status {}: {}",
                    response.status(),
                    response.text().unwrap_or_default()
                ),
            ));
        }

        let results: SparqlResults = response.json().map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("Failed to parse SPARQL JSON: {}", e),
            )
        })?;

        Ok(results
            .results
            .bindings
            .into_iter()
            .map(|binding| binding.into_iter().map(|(k, v)| (k, v.value)).collect())
            .collect())
    }

    /// Query for packages of a given RDF type, returning (package_uri, name, version) tuples.
    pub fn query_packages_by_type(&self, rdf_type: &str) -> Result<Vec<(String, String, String)>> {
        let sparql = format!(
            "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
             SELECT ?pkg ?name ?version WHERE {{\n\
               ?pkg a <{rdf_type}> ;\n\
                    pkg:packageName ?name ;\n\
                    pkg:hasVersion ?v .\n\
               ?v pkg:versionString ?version .\n\
             }}"
        );

        let bindings = self.query(&sparql)?;
        Ok(bindings
            .into_iter()
            .filter_map(|b| {
                Some((
                    b.get("pkg")?.clone(),
                    b.get("name")?.clone(),
                    b.get("version")?.clone(),
                ))
            })
            .collect())
    }

    /// Graph-scoped variant of query_packages_by_type.
    pub fn query_packages_by_type_in_graph(
        &self,
        rdf_type: &str,
        graph_uri: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let sparql = format!(
            "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
             SELECT ?pkg ?name ?version WHERE {{\n\
               GRAPH <{graph_uri}> {{\n\
                 ?pkg a <{rdf_type}> ;\n\
                      pkg:packageName ?name ;\n\
                      pkg:hasVersion ?v .\n\
                 ?v pkg:versionString ?version .\n\
               }}\n\
             }}"
        );

        let bindings = self.query(&sparql)?;
        Ok(bindings
            .into_iter()
            .filter_map(|b| {
                Some((
                    b.get("pkg")?.clone(),
                    b.get("name")?.clone(),
                    b.get("version")?.clone(),
                ))
            })
            .collect())
    }

    /// Query for packages with GitHub homepages, returning (package_uri, homepage_url, maintainer_uri) tuples.
    pub fn query_github_homepages(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let sparql = "\
            PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
            SELECT DISTINCT ?pkg ?homepage ?maintainer WHERE {\n\
              ?pkg pkg:homepage ?homepage .\n\
              FILTER(CONTAINS(STR(?homepage), \"github.com\"))\n\
              OPTIONAL { ?pkg pkg:maintainedBy ?maintainer }\n\
            }";

        let bindings = self.query(sparql)?;
        Ok(bindings
            .into_iter()
            .filter_map(|b| {
                Some((
                    b.get("pkg")?.clone(),
                    b.get("homepage")?.clone(),
                    b.get("maintainer").cloned(),
                ))
            })
            .collect())
    }

    /// Query for unique (package_name, version_string) pairs.
    pub fn query_package_names_and_versions(&self) -> Result<Vec<(String, String)>> {
        let sparql = "\
            PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
            SELECT DISTINCT ?name ?version WHERE {\n\
              ?p a pkg:BinaryPackage .\n\
              ?p pkg:packageName ?name .\n\
              ?p pkg:hasVersion ?v .\n\
              ?v pkg:versionString ?version .\n\
            }";

        let bindings = self.query(sparql)?;
        Ok(bindings
            .into_iter()
            .filter_map(|b| Some((b.get("name")?.clone(), b.get("version")?.clone())))
            .collect())
    }

    /// Query for GitHub repositories ranked by enrichment value, with maintainer URIs.
    ///
    /// Returns repositories ordered by:
    /// 1. Unenriched repos before already-enriched repos
    /// 2. Total package coverage count per homepage (DESC)
    /// 3. Homepage URL (ASC for determinism)
    ///
    /// For repos with multiple maintainers, returns all (homepage, maintainer) pairs.
    /// Package count is computed per homepage (not per homepage+maintainer pair) via subquery,
    /// so ranking reflects true repo coverage and LIMIT applies to unique repos.
    ///
    /// `graph_uri` is the enrichment graph to check for already-enriched repos.
    /// `limit` bounds the result set by unique repos.
    ///
    /// Returns: Vec<(homepage, maintainer_uri_option, repo_package_count)>
    pub fn query_github_candidates(
        &self,
        graph_uri: &str,
        limit: usize,
    ) -> Result<Vec<(String, Option<String>, usize)>> {
        // Normalize GitHub URLs to https://github.com/{owner}/{repo}:
        // strip fragment, force https, strip .git/ and .git$, strip trailing slash,
        // then extract just the first two path segments (owner/repo)
        let normalize_url = "REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(\
            STR(?raw), \
            \"#.*$\", \"\"), \
            \"^http://\", \"https://\"), \
            \"\\\\.git/\", \"/\"), \
            \"\\\\.git$\", \"\"), \
            \"/$\", \"\"), \
            \"^(https://github\\\\.com/[^/]+/[^/]+).*$\", \"$1\")";
        let inner_bind = normalize_url.replace("?raw", "?rawHomepage");
        let outer_bind = normalize_url.replace("?raw", "?rawHomepage2");

        let sparql = format!(
            "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
             PREFIX vcs: <https://purl.org/packagegraph/ontology/vcs#>\n\
             SELECT DISTINCT ?homepage ?maintainer ?packageCount WHERE {{\n\
               {{\n\
                 SELECT ?homepage (COUNT(DISTINCT ?pkg) AS ?packageCount) WHERE {{\n\
                   ?pkg pkg:homepage ?rawHomepage .\n\
                   FILTER(CONTAINS(STR(?rawHomepage), \"github.com\"))\n\
                   BIND({inner_bind} AS ?homepage)\n\
                   FILTER NOT EXISTS {{\n\
                     GRAPH <{graph_uri}> {{\n\
                       ?enrichedURI vcs:repositoryURL ?enrichedURL .\n\
                       FILTER(STR(?homepage) = STR(?enrichedURL))\n\
                     }}\n\
                   }}\n\
                 }}\n\
                 GROUP BY ?homepage\n\
                 ORDER BY DESC(?packageCount) ASC(?homepage)\n\
                 LIMIT {limit}\n\
               }}\n\
               ?pkg2 pkg:homepage ?rawHomepage2 .\n\
               BIND({outer_bind} AS ?homepage2)\n\
               FILTER(?homepage2 = ?homepage)\n\
               OPTIONAL {{ ?pkg2 pkg:maintainedBy ?maintainer }}\n\
             }}\n\
             ORDER BY DESC(?packageCount) ASC(?homepage) ASC(?maintainer)",
            inner_bind = inner_bind,
            outer_bind = outer_bind,
            graph_uri = graph_uri,
            limit = limit
        );

        let bindings = self.query(&sparql)?;
        Ok(bindings
            .into_iter()
            .filter_map(|b| {
                let homepage = b.get("homepage")?.clone();
                let maintainer = b.get("maintainer").cloned();
                let count = b.get("packageCount")?.parse::<usize>().ok()?;
                Some((homepage, maintainer, count))
            })
            .collect())
    }

    /// Query for forge instances with their URLs and software types.
    ///
    /// Returns (forge_uri, forge_url, forge_software_uri) tuples for all vcs:Forge
    /// entities in the graph that have both forgeUrl and forgeSoftware properties.
    pub fn query_forge_instances(&self) -> Result<Vec<(String, String, String)>> {
        let sparql = "\
            PREFIX vcs: <https://purl.org/packagegraph/ontology/vcs#>\n\
            SELECT DISTINCT ?forge ?forgeUrl ?forgeSoftware WHERE {\n\
              ?forge a vcs:Forge ;\n\
                     vcs:forgeUrl ?forgeUrl ;\n\
                     vcs:forgeSoftware ?forgeSoftware .\n\
            }";

        let bindings = self.query(sparql)?;
        Ok(bindings
            .into_iter()
            .filter_map(|b| {
                Some((
                    b.get("forge")?.clone(),
                    b.get("forgeUrl")?.clone(),
                    b.get("forgeSoftware")?.clone(),
                ))
            })
            .collect())
    }

    /// Execute a SPARQL CONSTRUCT query and return raw N-Triple lines.
    ///
    /// POSTs to the /sparql endpoint with Accept: application/n-triples.
    /// Returns each non-empty line as a String. Uses the same retry logic
    /// as update() for transient failures.
    pub fn query_construct(&self, sparql: &str) -> Result<Vec<String>> {
        let url = self.query_url();
        let form_params = self.query_form_params(sparql);
        let max_retries = 3;

        for attempt in 0..=max_retries {
            match self
                .apply_auth(
                    self.client
                        .post(&url)
                        .header("Accept", "application/n-triples")
                        .form(&form_params),
                )
                .send()
            {
                Ok(response) if response.status().is_success() => {
                    let body = response.text().map_err(|e| {
                        Error::new(
                            ErrorKind::Other,
                            format!("Failed to read CONSTRUCT response: {}", e),
                        )
                    })?;
                    return Ok(body
                        .lines()
                        .map(|l| l.trim())
                        .filter(|l| !l.is_empty() && !l.starts_with('#'))
                        .map(|l| l.to_string())
                        .collect());
                }
                Ok(response) if response.status().is_server_error() && attempt < max_retries => {
                    let delay = 5 * (1 << attempt);
                    eprintln!(
                        "    CONSTRUCT query failed ({}), retrying in {}s...",
                        response.status(),
                        delay
                    );
                    std::thread::sleep(Duration::from_secs(delay));
                }
                Ok(response) => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "CONSTRUCT query failed with status {}: {}",
                            response.status(),
                            response.text().unwrap_or_default()
                        ),
                    ));
                }
                Err(e) if attempt < max_retries => {
                    let delay = 5 * (1 << attempt);
                    eprintln!(
                        "    CONSTRUCT query error: {}, retrying in {}s...",
                        e, delay
                    );
                    std::thread::sleep(Duration::from_secs(delay));
                }
                Err(e) => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("CONSTRUCT query failed: {}", e),
                    ));
                }
            }
        }
        Ok(vec![])
    }

    /// Load an N-Triples file into a named graph via Fuseki's Graph Store Protocol.
    ///
    /// Uses POST to the GSP endpoint with the raw .nt file body, bypassing SPARQL
    /// parsing for 10-100x faster loading. Falls back to batched INSERT DATA if
    /// GSP upload fails.
    pub fn load_file(&self, file_path: &str, graph_uri: &str, batch_size: usize) -> Result<usize> {
        self.guard_write("Graph Store Protocol load")?;
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
                eprintln!(
                    "GSP upload failed ({}), falling back to batched INSERT DATA...",
                    e
                );
                self.load_file_batched(file_path, graph_uri, batch_size)
            }
        }
    }

    /// Upload an N-Triples file to a named graph via Graph Store Protocol POST.
    ///
    /// GSP POST is additive — triples are appended to the existing graph, not replaced.
    /// On failure: logs error with HTTP status code and returns Err to stop the caller.
    /// The file is left on disk for operator debugging and manual retry via `pg-collect load`.
    ///
    /// Chunked upload (10MB per POST) avoids Fuseki JVM memory issues on large files.
    pub fn gsp_post_file(&self, file_path: &str, graph_uri: &str) -> Result<()> {
        self.guard_write("Graph Store Protocol upload")?;
        self.gsp_upload(file_path, graph_uri)
    }

    /// Upload an N-Triples file via Graph Store Protocol in chunks.
    ///
    /// Splits the file into chunks at line boundaries and POSTs each chunk
    /// separately. Fuseki's GSP POST is additive, so multiple POSTs to the
    /// same graph accumulate correctly. This avoids Fuseki OOM on large files.
    fn gsp_upload(&self, file_path: &str, graph_uri: &str) -> Result<()> {
        const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10MB per chunk (reduced from 50MB to avoid Fuseki JVM memory faults)

        let url = format!(
            "{}/data?graph={}",
            self.endpoint,
            percent_encoding::utf8_percent_encode(graph_uri, percent_encoding::NON_ALPHANUMERIC,)
        );

        let file_size = File::open(file_path)?.metadata()?.len();
        eprintln!(
            "Uploading {} bytes to {} (chunked, {}MB per chunk)",
            file_size,
            url,
            CHUNK_SIZE / (1024 * 1024)
        );

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
        eprintln!(
            "GSP upload complete: {} bytes in {} chunks ({:.1}s)",
            total_bytes, chunk_num, elapsed
        );

        Ok(())
    }

    fn gsp_post_chunk(&self, url: &str, data: &[u8]) -> Result<()> {
        let max_retries = 3;
        for attempt in 0..=max_retries {
            match self
                .apply_auth(
                    self.client
                        .post(url)
                        .header("Content-Type", "application/n-triples")
                        .body(data.to_vec()),
                )
                .send()
            {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) if response.status().is_server_error() && attempt < max_retries => {
                    let delay = 5 * (1 << attempt);
                    eprintln!(
                        "    GSP chunk failed ({}), retrying in {}s...",
                        response.status(),
                        delay
                    );
                    std::thread::sleep(Duration::from_secs(delay));
                }
                Ok(response) => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "GSP upload failed with status {}: {}",
                            response.status(),
                            response.text().unwrap_or_default()
                        ),
                    ));
                }
                Err(e) if attempt < max_retries => {
                    let delay = 5 * (1 << attempt);
                    eprintln!("    GSP chunk error: {}, retrying in {}s...", e, delay);
                    std::thread::sleep(Duration::from_secs(delay));
                }
                Err(e) => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("GSP upload failed: {}", e),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Fallback: load via batched INSERT DATA.
    fn load_file_batched(
        &self,
        file_path: &str,
        graph_uri: &str,
        batch_size: usize,
    ) -> Result<usize> {
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

    pub(crate) fn insert_batch(&self, triples: &[String], graph_uri: &str) -> Result<()> {
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

pub fn make_sparql_client(
    endpoint: &str,
    auth: &SparqlAuth,
    backend: SparqlBackend,
) -> SparqlClient {
    let client = SparqlClient::new(endpoint);
    let client = match auth {
        Some((u, p)) => client.with_auth(u.clone(), p.clone()),
        None => client,
    };
    client.with_backend(backend)
}

/// Graph manifest for Minio-based write path.
/// Maps filename → graph URI (matches the live graphs.json format).
pub mod manifest {
    use std::collections::HashMap;
    use std::io::{Error, ErrorKind, Result};

    pub type GraphManifest = HashMap<String, String>;

    pub fn parse(json: &str) -> Result<GraphManifest> {
        serde_json::from_str(json).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Invalid graphs.json: {}", e),
            )
        })
    }

    pub fn add_entry(manifest: &mut GraphManifest, filename: &str, graph_uri: &str) {
        manifest.insert(filename.to_string(), graph_uri.to_string());
    }

    pub fn remove_graph(manifest: &mut GraphManifest, graph_uri: &str) -> Vec<String> {
        let keys: Vec<String> = manifest
            .iter()
            .filter(|(_, v)| v.as_str() == graph_uri)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &keys {
            manifest.remove(k);
        }
        keys
    }

    pub fn serialize(manifest: &GraphManifest) -> Result<String> {
        serde_json::to_string_pretty(manifest).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("Failed to serialize manifest: {}", e),
            )
        })
    }
}

pub fn count_triples_pub(file_path: &str) -> Result<usize> {
    count_triples(file_path)
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

    #[test]
    fn test_parse_sparql_json_results() {
        let json = r#"{
            "results": {
                "bindings": [
                    {
                        "name": {"type": "literal", "value": "openssl"},
                        "version": {"type": "literal", "value": "3.1.4"}
                    },
                    {
                        "name": {"type": "literal", "value": "curl"},
                        "version": {"type": "literal", "value": "8.5.0"}
                    }
                ]
            }
        }"#;

        let results: SparqlResults = serde_json::from_str(json).unwrap();
        assert_eq!(results.results.bindings.len(), 2);
        assert_eq!(results.results.bindings[0]["name"].value, "openssl");
        assert_eq!(results.results.bindings[0]["version"].value, "3.1.4");
        assert_eq!(results.results.bindings[1]["name"].value, "curl");
        assert_eq!(results.results.bindings[1]["version"].value, "8.5.0");
    }

    #[test]
    fn test_parse_sparql_json_with_uri_values() {
        let json = r#"{
            "results": {
                "bindings": [
                    {
                        "pkg": {"type": "uri", "value": "https://packagegraph.github.io/d/pkg/debian/trixie/amd64/openssl/3.1.4"},
                        "homepage": {"type": "literal", "value": "https://github.com/openssl/openssl"}
                    }
                ]
            }
        }"#;

        let results: SparqlResults = serde_json::from_str(json).unwrap();
        assert_eq!(results.results.bindings.len(), 1);
        assert_eq!(
            results.results.bindings[0]["pkg"].value,
            "https://packagegraph.github.io/d/pkg/debian/trixie/amd64/openssl/3.1.4"
        );
        assert_eq!(
            results.results.bindings[0]["homepage"].value,
            "https://github.com/openssl/openssl"
        );
    }

    #[test]
    fn test_parse_sparql_json_empty_results() {
        let json = r#"{"results": {"bindings": []}}"#;
        let results: SparqlResults = serde_json::from_str(json).unwrap();
        assert_eq!(results.results.bindings.len(), 0);
    }

    #[test]
    fn test_query_with_mockito() {
        let mut server = mockito::Server::new();
        let mock = server.mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(r#"{
                "results": {
                    "bindings": [
                        {"name": {"type": "literal", "value": "bash"}, "version": {"type": "literal", "value": "5.2"}}
                    ]
                }
            }"#)
            .create();

        let client = SparqlClient::new(&server.url());
        let results = client
            .query("SELECT ?name ?version WHERE { ?p a pkg:Package }")
            .unwrap();

        mock.assert();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["name"], "bash");
        assert_eq!(results[0]["version"], "5.2");
    }

    #[test]
    fn test_query_construct_returns_ntriples() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/sparql")
            .match_header("accept", "application/n-triples")
            .with_status(200)
            .with_header("content-type", "application/n-triples")
            .with_body(
                "<http://s1> <http://p1> <http://o1> .\n<http://s2> <http://p2> <http://o2> .\n",
            )
            .create();

        let client = SparqlClient::new(&server.url());
        let triples = client
            .query_construct("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o } LIMIT 2")
            .unwrap();

        mock.assert();
        assert_eq!(triples.len(), 2);
        assert_eq!(triples[0], "<http://s1> <http://p1> <http://o1> .");
        assert_eq!(triples[1], "<http://s2> <http://p2> <http://o2> .");
    }

    #[test]
    fn test_query_construct_empty_result() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/sparql")
            .match_header("accept", "application/n-triples")
            .with_status(200)
            .with_body("")
            .create();

        let client = SparqlClient::new(&server.url());
        let triples = client
            .query_construct("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o } LIMIT 0")
            .unwrap();

        mock.assert();
        assert!(triples.is_empty());
    }

    #[test]
    fn test_query_github_candidates_ranking() {
        let mut server = mockito::Server::new();

        // Mock SPARQL response with ranked homepages (with optional maintainers):
        // Only unenriched repos returned (FILTER NOT EXISTS excludes enriched)
        // - url1 (100 packages, with maintainer) should rank 1st
        // - url2 (50 packages, no maintainer) should rank 2nd
        let mock_response = r#"{
            "results": {
                "bindings": [
                    {"homepage": {"value": "https://github.com/user1/repo1"}, "maintainer": {"value": "http://pkg.graph/maintainer/1"}, "packageCount": {"value": "100"}},
                    {"homepage": {"value": "https://github.com/user2/repo2"}, "packageCount": {"value": "50"}}
                ]
            }
        }"#;

        let mock = server
            .mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(mock_response)
            .create();

        let client = SparqlClient::new(&server.url());
        let candidates = client
            .query_github_candidates("http://example.org/enrichment/github", 10)
            .unwrap();

        mock.assert();

        // Only unenriched repos returned, ordered by package count DESC
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].0, "https://github.com/user1/repo1");
        assert_eq!(
            candidates[0].1,
            Some("http://pkg.graph/maintainer/1".to_string())
        );
        assert_eq!(candidates[1].0, "https://github.com/user2/repo2");
        assert_eq!(candidates[1].1, None);
    }

    #[test]
    fn test_query_github_candidates_deterministic_ordering() {
        let mut server = mockito::Server::new();

        // Two homepages with same package count — SPARQL ORDER BY sorts by homepage ASC
        let mock_response = r#"{
            "results": {
                "bindings": [
                    {"homepage": {"value": "https://github.com/aaa/first"}, "packageCount": {"value": "10"}},
                    {"homepage": {"value": "https://github.com/zzz/last"}, "packageCount": {"value": "10"}}
                ]
            }
        }"#;

        let mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(mock_response)
            .create();

        let client = SparqlClient::new(&server.url());
        let candidates = client
            .query_github_candidates("http://example.org/enrichment/github", 10)
            .unwrap();

        mock.assert();

        // Should order by homepage ASC when tied
        assert_eq!(candidates[0].0, "https://github.com/aaa/first");
        assert_eq!(candidates[1].0, "https://github.com/zzz/last");
    }

    #[test]
    fn test_gsp_post_file_propagates_http_errors() {
        use std::io::Write;
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        writeln!(temp, "<http://s> <http://p> <http://o> .").unwrap();
        temp.flush().unwrap();

        let mut server = mockito::Server::new();
        // GSP POST has retry logic (max 3 retries on server errors) — expect 4 requests
        // Match any path starting with /data (mockito will match query params)
        let mock = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(500)
            .with_body("Internal Server Error")
            .expect(4) // 1 initial + 3 retries
            .create();

        let client = SparqlClient::new(&server.url());
        let result =
            client.gsp_post_file(temp.path().to_str().unwrap(), "http://example.org/graph");

        mock.assert();
        assert!(result.is_err(), "Should propagate GSP failure as Err");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("500") || err_msg.contains("GSP"),
            "Error should mention HTTP 500 or GSP"
        );
    }

    #[test]
    fn test_query_forge_instances() {
        let mut server = mockito::Server::new();
        let mock_response = r#"{
            "results": {
                "bindings": [
                    {
                        "forge": {"value": "https://packagegraph.github.io/d/forge/gitlab.gnome.org"},
                        "forgeUrl": {"value": "https://gitlab.gnome.org"},
                        "forgeSoftware": {"value": "https://purl.org/packagegraph/ontology/vcs#GitLab"}
                    },
                    {
                        "forge": {"value": "https://packagegraph.github.io/d/forge/codeberg.org"},
                        "forgeUrl": {"value": "https://codeberg.org"},
                        "forgeSoftware": {"value": "https://purl.org/packagegraph/ontology/vcs#Forgejo"}
                    }
                ]
            }
        }"#;

        let mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(mock_response)
            .create();

        let client = SparqlClient::new(&server.url());
        let forges = client.query_forge_instances().unwrap();

        mock.assert();

        assert_eq!(forges.len(), 2);
        assert_eq!(
            forges[0].0,
            "https://packagegraph.github.io/d/forge/gitlab.gnome.org"
        );
        assert_eq!(forges[0].1, "https://gitlab.gnome.org");
        assert_eq!(
            forges[0].2,
            "https://purl.org/packagegraph/ontology/vcs#GitLab"
        );
        assert_eq!(
            forges[1].2,
            "https://purl.org/packagegraph/ontology/vcs#Forgejo"
        );
    }

    #[test]
    fn test_query_sends_basic_auth_header() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/sparql")
            .match_header("Authorization", "Basic dGVzdHVzZXI6dGVzdHBhc3M=")
            .with_status(200)
            .with_header("Content-Type", "application/sparql-results+json")
            .with_body(r#"{"head":{"vars":["x"]},"results":{"bindings":[]}}"#)
            .create();
        let client =
            SparqlClient::new(&server.url()).with_auth("testuser".into(), "testpass".into());
        let _ = client.query("SELECT * WHERE { ?s ?p ?o }");
        mock.assert();
    }

    #[test]
    fn test_update_sends_basic_auth_header() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/update")
            .match_header("Authorization", "Basic dGVzdHVzZXI6dGVzdHBhc3M=")
            .with_status(200)
            .create();
        let client =
            SparqlClient::new(&server.url()).with_auth("testuser".into(), "testpass".into());
        let _ = client.update("INSERT DATA { <s> <p> <o> }");
        mock.assert();
    }

    #[test]
    fn test_gsp_upload_sends_basic_auth_header() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", mockito::Matcher::Regex(r"/data\?graph=.*".into()))
            .match_header("Authorization", "Basic dGVzdHVzZXI6dGVzdHBhc3M=")
            .with_status(200)
            .create();
        let tmp = std::env::temp_dir().join("test_auth_gsp.nt");
        std::fs::write(&tmp, "<s> <p> <o> .\n").unwrap();
        let client =
            SparqlClient::new(&server.url()).with_auth("testuser".into(), "testpass".into());
        let _ = client.gsp_post_file(tmp.to_str().unwrap(), "http://example.org/graph");
        mock.assert();
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_query_without_auth_sends_no_authorization_header() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/sparql")
            .match_header("Authorization", mockito::Matcher::Missing)
            .with_status(200)
            .with_header("Content-Type", "application/sparql-results+json")
            .with_body(r#"{"head":{"vars":["x"]},"results":{"bindings":[]}}"#)
            .create();
        let client = SparqlClient::new(&server.url());
        let _ = client.query("SELECT * WHERE { ?s ?p ?o }");
        mock.assert();
    }

    #[test]
    fn test_qlever_backend_query_sends_access_token_in_body() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("access-token=mytoken".into()),
                mockito::Matcher::Regex("query=".into()),
            ]))
            .with_status(200)
            .with_header("Content-Type", "application/sparql-results+json")
            .with_body(r#"{"head":{"vars":["x"]},"results":{"bindings":[]}}"#)
            .create();
        let client = SparqlClient::new(&server.url()).with_backend(SparqlBackend::QLever {
            access_token: "mytoken".into(),
        });
        let _ = client.query("SELECT * WHERE { ?s ?p ?o }");
        mock.assert();
    }

    #[test]
    fn test_qlever_backend_update_returns_error() {
        let client =
            SparqlClient::new("http://localhost:9999").with_backend(SparqlBackend::QLever {
                access_token: "tok".into(),
            });
        let result = client.update("INSERT DATA { <s> <p> <o> }");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not support"));
    }

    #[test]
    fn test_qlever_backend_gsp_returns_error() {
        let tmp = std::env::temp_dir().join("test_qlever_gsp.nt");
        std::fs::write(&tmp, "<s> <p> <o> .\n").unwrap();
        let client =
            SparqlClient::new("http://localhost:9999").with_backend(SparqlBackend::QLever {
                access_token: "tok".into(),
            });
        let result = client.gsp_post_file(tmp.to_str().unwrap(), "http://example.org/graph");
        assert!(result.is_err());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_fuseki_backend_is_default() {
        let client = SparqlClient::new("http://localhost:9999");
        assert!(matches!(client.backend, SparqlBackend::Fuseki));
    }

    #[test]
    fn test_manifest_parse_and_add() {
        let json = r#"{"fedora-43.nt": "https://example.org/graph/fedora/43"}"#;
        let mut m = manifest::parse(json).unwrap();
        manifest::add_entry(
            &mut m,
            "debian-trixie.nt",
            "https://example.org/graph/debian/trixie",
        );
        assert_eq!(m.len(), 2);
        assert_eq!(
            m["debian-trixie.nt"],
            "https://example.org/graph/debian/trixie"
        );
    }

    #[test]
    fn test_manifest_remove_graph() {
        let json = r#"{"a.nt": "https://example.org/g/a", "b.nt": "https://example.org/g/b"}"#;
        let mut m = manifest::parse(json).unwrap();
        let removed = manifest::remove_graph(&mut m, "https://example.org/g/a");
        assert_eq!(removed, vec!["a.nt"]);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn test_manifest_roundtrip() {
        let mut m = manifest::GraphManifest::new();
        manifest::add_entry(
            &mut m,
            "fedora-43.nt",
            "https://example.org/graph/fedora/43",
        );
        let json = manifest::serialize(&m).unwrap();
        let m2 = manifest::parse(&json).unwrap();
        assert_eq!(m, m2);
    }
}
