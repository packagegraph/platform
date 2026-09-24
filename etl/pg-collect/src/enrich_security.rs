//! OSV API security enricher.
//!
//! Queries the store for package names by ecosystem, asks OSV.dev which
//! vulnerabilities affect them, and emits one record per distinct
//! vulnerability. Complementary to the bulk OSV collector (osv.rs), which
//! downloads ecosystem ZIPs from GCS.
//!
//! ## Why this is not one request per package (#59)
//!
//! It used to be, and it could not finish. After 8 hours the run was 45,100
//! packages into the FIRST of eleven ecosystems, at ~94 packages/minute;
//! Debian alone is 464,223 packages in the corpus, so that ecosystem needed
//! ~82 hours on its own. No timeout makes that complete.
//!
//! Almost all of that work was redundant:
//!
//!   - the OSV query is keyed by `{name, ecosystem}` and carries no version,
//!     but the driving SPARQL returned one row per (package, VERSION) -- so
//!     every version of `curl` issued a byte-identical request;
//!   - `emit_vulnerability_triples` is a function of the vulnerability alone,
//!     so each of those duplicates re-emitted an identical block of triples
//!     into the output file;
//!   - `security.sh` never passed `--cache-dir`, so the FileCache was always
//!     None and nothing was reused within a run, let alone across runs.
//!
//! So: distinct names, asked in batches through OSV's `querybatch` endpoint,
//! reduced to the distinct vulnerability IDs they matched, and one detail
//! fetch and one emission per ID. The output is the same set of triples with
//! the duplication removed.

use crate::cache::FileCache;
use crate::http_transport::HttpTransport;
use crate::ntriples::NTriplesWriter;
use crate::osv::{emit_vulnerability_triples, OsvVulnerability};
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use std::fs::File;
use std::io::Result;

const OSV_API: &str = "https://api.osv.dev";

pub struct SecurityEnricher {
    sparql: SparqlClient,
    transport: HttpTransport,
    cache: Option<FileCache>,
    ecosystem: String,
    osv_api_base: String,
    pub graph_uri: Option<String>,
}

impl SecurityEnricher {
    pub fn new(
        endpoint: &str,
        ecosystem: &str,
        cache_dir: Option<&str>,
        auth: SparqlAuth,
        backend: SparqlBackend,
    ) -> Self {
        let sparql = make_sparql_client(endpoint, &auth, backend);
        // One namespace for every ecosystem, not one per ecosystem: the
        // cached artefact is now an OSV vulnerability record keyed by its own
        // ID, and that record is the same whoever asked for it. A CVE reached
        // from both `deb` and `rpm` is fetched once (#59).
        let cache = cache_dir.map(|dir| {
            FileCache::new(dir, "security-osv", 24, None).expect("Failed to create cache")
        });

        Self {
            sparql,
            transport: HttpTransport::new(),
            cache,
            ecosystem: ecosystem.to_string(),
            osv_api_base: OSV_API.to_string(),
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        // Map ecosystem name to RDF type — accepts both packaging system names (preferred)
        // and legacy distro names for backward compatibility
        let rdf_type = match self.ecosystem.as_str() {
            // Packaging system names (preferred)
            "deb" => "https://purl.org/packagegraph/ontology/deb#BinaryPackage",
            "apk" => "https://purl.org/packagegraph/ontology/apk#ApkPackage",
            "rpm" => "https://purl.org/packagegraph/ontology/rpm#BinaryRPM",
            "npm" => "https://purl.org/packagegraph/ontology/npm#NpmPackage",
            "pypi" => "https://purl.org/packagegraph/ontology/pypi#PythonPackage",
            "cargo" => "https://purl.org/packagegraph/ontology/cargo#Crate",
            "gomod" => "https://purl.org/packagegraph/ontology/gomod#GoModule",
            "maven" => "https://purl.org/packagegraph/ontology/maven#MavenArtifact",
            // Legacy distro names (backward compat)
            "debian" => "https://purl.org/packagegraph/ontology/deb#BinaryPackage",
            "alpine" => "https://purl.org/packagegraph/ontology/apk#ApkPackage",
            "fedora" | "rhel" | "centos" | "opensuse" => "https://purl.org/packagegraph/ontology/rpm#BinaryRPM",
            _ => return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Unsupported ecosystem: {}. Use packaging system name: deb, apk, rpm, npm, pypi, cargo, gomod, maven", self.ecosystem),
            )),
        };

        let names = self.sparql.query_package_names_by_type(rdf_type)?;
        eprintln!(
            "Found {} distinct {} package names to check for vulnerabilities",
            names.len(),
            self.ecosystem
        );

        let (ids, truncated) = self.vulnerability_ids_for(&names)?;
        eprintln!(
            "OSV matched {} distinct vulnerabilities across {} names",
            ids.len(),
            names.len()
        );
        if truncated > 0 {
            eprintln!(
                "WARNING: {} of {} names had more matches than OSV returned in one page -- \
                 this enricher is no longer seeing everything for them",
                truncated,
                names.len()
            );
        }

        let mut total_triples = 0;
        let mut fetched = 0;
        for id in &ids {
            fetched += 1;
            if fetched % 100 == 0 {
                eprintln!(
                    "Progress: {} of {} vulnerabilities fetched",
                    fetched,
                    ids.len()
                );
            }
            match self.fetch_vulnerability(id) {
                Ok(Some(vuln)) => total_triples += emit_vulnerability_triples(&mut writer, &vuln)?,
                // A record OSV named but will not serve is not an error here;
                // the bulk collector covers the same ground from the ZIPs.
                Ok(None) => {}
                Err(e) => eprintln!("  Error fetching {}: {}", id, e),
            }
        }

        writer.flush()?;
        // The first number is what was CHECKED, which is names, not versions:
        // a caller comparing it against a package count will find it smaller
        // for the reason in this module's header.
        Ok((names.len(), total_triples))
    }

    /// Every distinct vulnerability ID OSV reports for any of `names`, and
    /// the number of names whose answer OSV had to truncate.
    ///
    /// Batched: OSV's `querybatch` takes many package queries per request and
    /// answers with IDs only, which is all that is needed to decide what to
    /// fetch. One request per `BATCH` names replaces one per name.
    fn vulnerability_ids_for(&self, names: &[String]) -> Result<(Vec<String>, usize)> {
        const BATCH: usize = 1000;
        let mut ids: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut truncated = 0;

        for (chunk_no, chunk) in names.chunks(BATCH).enumerate() {
            if chunk_no > 0 && chunk_no % 10 == 0 {
                eprintln!(
                    "Progress: {} of {} names queried",
                    chunk_no * BATCH,
                    names.len()
                );
            }
            let (batch_ids, batch_truncated) = self.query_batch(chunk)?;
            truncated += batch_truncated;
            for id in batch_ids {
                if seen.insert(id.clone()) {
                    ids.push(id);
                }
            }
        }
        Ok((ids, truncated))
    }

    /// One `querybatch` request. Returns the IDs it named, in order, and how
    /// many of its queries OSV answered with only a first page.
    ///
    /// A batch that fails outright yields nothing rather than aborting the
    /// run: the bulk OSV collector covers the same ground, and losing an
    /// ecosystem to one bad request is the failure mode this rewrite exists
    /// to remove.
    fn query_batch(&self, names: &[String]) -> Result<(Vec<String>, usize)> {
        let payload = serde_json::json!({
            "queries": names
                .iter()
                .map(|name| serde_json::json!({
                    "package": {"name": name, "ecosystem": self.osv_ecosystem_name()}
                }))
                .collect::<Vec<_>>(),
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let url = format!("{}/v1/querybatch", self.osv_api_base);
        let resp = match self
            .transport
            .post(&url, &[("Content-Type", "application/json")], body)
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "  Warning: OSV batch of {} names failed: {}",
                    names.len(),
                    e
                );
                return Ok((Vec::new(), 0));
            }
        };

        let data: serde_json::Value = serde_json::from_slice(&resp.bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let mut ids = Vec::new();
        let mut truncated = 0;
        for result in data
            .get("results")
            .and_then(|r| r.as_array())
            .map(|a| a.as_slice())
            .unwrap_or_default()
        {
            // A per-query page token means OSV had more matches than it
            // returned. Counted rather than silently dropped -- the bulk
            // collector is the backstop, but a growing count here means this
            // enricher is no longer seeing everything.
            if result.get("next_page_token").is_some() {
                truncated += 1;
            }
            for vuln in result
                .get("vulns")
                .and_then(|v| v.as_array())
                .map(|a| a.as_slice())
                .unwrap_or_default()
            {
                if let Some(id) = vuln.get("id").and_then(|i| i.as_str()) {
                    ids.push(id.to_string());
                }
            }
        }
        Ok((ids, truncated))
    }

    /// One vulnerability record, from the cache when it is there.
    ///
    /// Keyed by ID alone, not by ecosystem and package: the record is the
    /// same whoever asked for it, and the same ID is reached from many
    /// packages and many ecosystems.
    fn fetch_vulnerability(&self, id: &str) -> Result<Option<OsvVulnerability>> {
        let cache_key = format!("osv-vuln-{id}");
        if let Some(v) = self.cached_get(&cache_key) {
            return Ok(Some(v));
        }

        let url = format!("{}/v1/vulns/{id}", self.osv_api_base);
        let resp = match self.transport.get(&url, None) {
            Ok(r) => r,
            // A record OSV named but will not serve is not an error.
            Err(_) => return Ok(None),
        };

        let vuln: OsvVulnerability = serde_json::from_slice(&resp.bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        self.cache_put(&cache_key, &serde_json::to_value(&vuln).unwrap());
        Ok(Some(vuln))
    }

    fn osv_ecosystem_name(&self) -> &str {
        match self.ecosystem.as_str() {
            "debian" => "Debian",
            "alpine" => "Alpine",
            "npm" => "npm",
            "pypi" => "PyPI",
            "cargo" => "crates.io",
            "gomod" => "Go",
            "maven" => "Maven",
            _ => &self.ecosystem,
        }
    }

    fn cached_get(&self, key: &str) -> Option<OsvVulnerability> {
        let val = self.cache.as_ref()?.get(key)?;
        serde_json::from_value(val).ok()
    }

    fn cache_put(&self, key: &str, data: &serde_json::Value) {
        if let Some(ref cache) = self.cache {
            cache.put(key, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_osv_ecosystem_mapping() {
        let enricher = SecurityEnricher::new(
            "http://localhost:3030/test",
            "debian",
            None,
            None,
            SparqlBackend::Fuseki,
        );
        assert_eq!(enricher.osv_ecosystem_name(), "Debian");

        let enricher2 = SecurityEnricher::new(
            "http://localhost:3030/test",
            "pypi",
            None,
            None,
            SparqlBackend::Fuseki,
        );
        assert_eq!(enricher2.osv_ecosystem_name(), "PyPI");
    }

    #[test]
    fn test_maven_ecosystem_mapping() {
        let enricher = SecurityEnricher::new(
            "http://localhost:3030/test",
            "maven",
            None,
            None,
            SparqlBackend::Fuseki,
        );
        assert_eq!(enricher.osv_ecosystem_name(), "Maven");
    }

    /// An enricher pointed at a mockito server for both SPARQL and OSV.
    fn enricher_against(server: &mockito::Server, ecosystem: &str) -> SecurityEnricher {
        SecurityEnricher {
            sparql: SparqlClient::new(&server.url()),
            transport: HttpTransport::new(),
            cache: None,
            ecosystem: ecosystem.to_string(),
            osv_api_base: server.url(),
            graph_uri: None,
        }
    }

    fn names_binding(names: &[&str]) -> String {
        let bindings: Vec<String> = names
            .iter()
            .map(|n| format!(r#"{{"name": {{"value": "{n}"}}}}"#))
            .collect();
        format!(r#"{{"results": {{"bindings": [{}]}}}}"#, bindings.join(","))
    }

    fn run(enricher: &SecurityEnricher) -> ((usize, usize), String) {
        let temp_file = NamedTempFile::new().unwrap();
        let counts = enricher
            .enrich(temp_file.path().to_str().unwrap())
            .expect("enrich should not fail");
        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        (counts, content)
    }

    /// The shape #59 is about: three packages, one advisory between them.
    /// The old code made three OSV queries and emitted the advisory three
    /// times. One batch request, one detail fetch, one emission.
    #[test]
    fn one_advisory_shared_by_three_names_is_fetched_and_emitted_once() {
        let mut server = mockito::Server::new();
        let _sparql = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(names_binding(&["curl", "openssl", "zlib"]))
            .create();
        let batch = server
            .mock("POST", "/v1/querybatch")
            // One request carrying all three names, with the ecosystem
            // translated to OSV's spelling.
            .match_body(mockito::Matcher::Regex(
                r#"curl[\s\S]*Debian[\s\S]*openssl[\s\S]*zlib"#.to_string(),
            ))
            .with_status(200)
            .with_body(
                r#"{"results": [
                     {"vulns": [{"id": "OSV-1", "modified": "2026-01-01T00:00:00Z"}]},
                     {"vulns": [{"id": "OSV-1", "modified": "2026-01-01T00:00:00Z"}]},
                     {"vulns": [{"id": "OSV-1", "modified": "2026-01-01T00:00:00Z"}]}
                   ]}"#,
            )
            .expect(1)
            .create();
        let detail = server
            .mock("GET", "/v1/vulns/OSV-1")
            .with_status(200)
            .with_body(r#"{"id": "OSV-1", "aliases": ["CVE-2026-0001"], "summary": "bad"}"#)
            .expect(1)
            .create();

        let ((checked, triples), content) = run(&enricher_against(&server, "debian"));

        batch.assert();
        detail.assert();
        assert_eq!(checked, 3, "all three names were checked");
        assert!(triples > 0, "the advisory should have been emitted");
        assert_eq!(
            content
                .lines()
                .filter(|line| line.contains("security#Vulnerability>"))
                .count(),
            1,
            "the advisory should appear once, not once per name that reached it:\n{content}"
        );
    }

    /// Losing one batch must not lose the ecosystem. The bulk OSV collector
    /// covers the same ground, so a refused request is a gap, not a failure.
    #[test]
    fn a_refused_batch_does_not_abort_the_run() {
        let mut server = mockito::Server::new();
        let _sparql = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(names_binding(&["curl"]))
            .create();
        // 400, not 500: a client error is not retried, so the test does not
        // sit through the transport's backoff schedule.
        let _batch = server
            .mock("POST", "/v1/querybatch")
            .with_status(400)
            .with_body("no")
            .create();

        let ((checked, triples), _) = run(&enricher_against(&server, "debian"));

        assert_eq!(checked, 1);
        assert_eq!(triples, 0, "nothing to emit, but the run still completed");
    }

    /// An advisory OSV names but will not serve is a gap, not a failure,
    /// for the same reason.
    #[test]
    fn an_unservable_advisory_does_not_abort_the_run() {
        let mut server = mockito::Server::new();
        let _sparql = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(names_binding(&["curl"]))
            .create();
        let _batch = server
            .mock("POST", "/v1/querybatch")
            .with_status(200)
            .with_body(r#"{"results": [{"vulns": [{"id": "OSV-GONE"}]}]}"#)
            .create();
        let _detail = server
            .mock("GET", "/v1/vulns/OSV-GONE")
            .with_status(404)
            .create();

        let ((checked, triples), _) = run(&enricher_against(&server, "debian"));

        assert_eq!(checked, 1);
        assert_eq!(triples, 0);
    }

    /// A page token means OSV had more matches than it returned. Counted, so
    /// the operator sees a number rather than silence.
    #[test]
    fn a_truncated_answer_is_counted_not_swallowed() {
        let mut server = mockito::Server::new();
        let _batch = server
            .mock("POST", "/v1/querybatch")
            .with_status(200)
            .with_body(
                r#"{"results": [
                     {"vulns": [{"id": "OSV-1"}], "next_page_token": "more"},
                     {"vulns": [{"id": "OSV-2"}]}
                   ]}"#,
            )
            .create();

        let enricher = enricher_against(&server, "debian");
        let (ids, truncated) = enricher
            .vulnerability_ids_for(&["curl".to_string(), "openssl".to_string()])
            .unwrap();

        assert_eq!(ids, vec!["OSV-1".to_string(), "OSV-2".to_string()]);
        assert_eq!(truncated, 1, "one of the two answers was a partial page");
    }

    #[test]
    fn test_unsupported_ecosystem() {
        let enricher = SecurityEnricher::new(
            "http://localhost:3030/test",
            "unsupported",
            None,
            None,
            SparqlBackend::Fuseki,
        );
        let temp_file = NamedTempFile::new().unwrap();
        let result = enricher.enrich(temp_file.path().to_str().unwrap());

        assert!(result.is_err(), "Should reject unsupported ecosystem");
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unsupported ecosystem"));
    }
}
