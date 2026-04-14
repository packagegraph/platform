# PackageGraph v0.5.0 Feature Buildout — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transform PackageGraph from a single-distro SPARQL endpoint into a multi-distribution package knowledge graph with security, VCS metadata, query catalog, and CI/CD — using named graphs via SPARQL Update for decade-scale operation.

**Architecture:** Each data source writes to its own named graph in Fuseki via SPARQL UPDATE. Collection uses the Rust pg-collect binary (extended with `load`/`drop` subcommands). Enrichment (security, VCS) uses Python for API orchestration but delegates SPARQL writes to pg-collect. Fuseki stays online during all writes.

**Tech Stack:** Rust (pg-collect), Python 3.12 (enrichers), Apache Jena Fuseki 5.3.0, Kubernetes/Kustomize, GitHub Actions, YASGUI (query catalog)

**Design Spec:** `docs/superpowers/specs/2026-04-13-v050-feature-buildout-design.md`

---

## File Structure

### New Files
- `etl/pg-collect/src/loader.rs` — SPARQL load/drop subcommands
- `etl/pg-collect/src/sparql.rs` — HTTP client for Fuseki SPARQL Update
- `etl/pg-collect/tests/loader_test.rs` — Integration tests for load/drop
- `etl/packagegraph/sparql_client.py` — Thin Python wrapper to query Fuseki SPARQL endpoint
- `etl/packagegraph/enrichers/security.py` — Refactored SecurityEnricher (queries Fuseki, writes .nt)
- `etl/packagegraph/enrichers/github.py` — Refactored GitHubEnricher (queries Fuseki, writes .nt)
- `etl/tests/test_sparql_client.py` — Tests for SPARQL client
- `etl/tests/test_enrichers/test_security.py` — Tests for refactored security enricher
- `etl/tests/test_enrichers/test_github.py` — Tests for refactored GitHub enricher
- `deploy/base/etl/job-template.yaml` — Base CronJob template for collection
- `deploy/overlays/dev/jobs/` — Per-distro CronJob patches (6 files)
- `deploy/base/secrets/github-token.yaml` — GitHub token secret template
- `fuseki/catalog.html` — YASGUI query catalog page
- `.github/workflows/ci.yml` — CI pipeline
- `.github/workflows/release.yml` — Release pipeline

### Modified Files
- `etl/pg-collect/Cargo.toml` — Add reqwest async features
- `etl/pg-collect/src/main.rs` — Add load/drop subcommands, fix RPM multi-repo
- `etl/pg-collect/src/lib.rs` — Export new modules
- `fuseki/config.ttl` — Add SPARQL Update endpoint
- `fuseki/Containerfile` — Copy catalog.html into webapp
- `deploy/base/kustomization.yaml` — Add new resources
- `deploy/overlays/dev/kustomization.yaml` — Add CronJob patches

---

### Task 1: pg-collect SPARQL Client Module (Rust)

**Files:**
- Create: `etl/pg-collect/src/sparql.rs`
- Modify: `etl/pg-collect/src/lib.rs`
- Modify: `etl/pg-collect/Cargo.toml`

- [ ] **Step 1: Add reqwest with blocking features to Cargo.toml**

reqwest is already a dependency but we need to ensure it has the features we need for SPARQL Update POST requests.

```toml
# In Cargo.toml [dependencies], reqwest already exists:
reqwest = { version = "0.12", features = ["blocking"] }
```

No change needed — blocking feature already present. Verify with:

```bash
cd etl/pg-collect && grep reqwest Cargo.toml
```

- [ ] **Step 2: Create sparql.rs with SparqlClient struct**

```rust
// etl/pg-collect/src/sparql.rs
use reqwest::blocking::Client;
use std::io::{BufRead, BufReader, Result};
use std::time::{Duration, Instant};

/// Client for Fuseki SPARQL Update operations.
pub struct SparqlClient {
    client: Client,
    endpoint: String,
}

impl SparqlClient {
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

    /// Send a SPARQL Update query.
    pub fn update(&self, sparql: &str) -> Result<()> {
        let url = format!("{}/update", self.endpoint);
        let response = self.client
            .post(&url)
            .header("Content-Type", "application/sparql-update")
            .body(sparql.to_string())
            .send()
            .map_err(|e| std::io::Error::other(format!("SPARQL Update failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(std::io::Error::other(
                format!("SPARQL Update returned {status}: {body}")
            ));
        }

        Ok(())
    }

    /// DROP a named graph.
    pub fn drop_graph(&self, graph_uri: &str) -> Result<()> {
        eprintln!("Dropping graph <{graph_uri}>...");
        self.update(&format!("DROP SILENT GRAPH <{graph_uri}>"))
    }

    /// Load an N-Triples file into a named graph via batched INSERT DATA.
    pub fn load_file(
        &self,
        file_path: &str,
        graph_uri: &str,
        batch_size: usize,
    ) -> Result<usize> {
        let file = std::fs::File::open(file_path)?;
        let reader = BufReader::new(file);

        let mut batch = Vec::with_capacity(batch_size);
        let mut total_triples = 0;
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
                total_triples += batch.len();
                let elapsed = start.elapsed().as_secs_f64();
                let rate = total_triples as f64 / elapsed;
                eprintln!(
                    "  Loaded {total_triples} triples ({rate:.0} triples/sec)"
                );
                batch.clear();
            }
        }

        // Flush remaining
        if !batch.is_empty() {
            self.insert_batch(&batch, graph_uri)?;
            total_triples += batch.len();
        }

        let elapsed = start.elapsed().as_secs_f64();
        eprintln!(
            "Load complete: {total_triples} triples in {elapsed:.1}s ({:.0} triples/sec)",
            total_triples as f64 / elapsed
        );

        Ok(total_triples)
    }

    fn insert_batch(&self, triples: &[String], graph_uri: &str) -> Result<()> {
        let body = triples.join("\n");
        let sparql = format!("INSERT DATA {{ GRAPH <{graph_uri}> {{\n{body}\n}} }}");
        self.update(&sparql)
    }
}
```

- [ ] **Step 3: Export sparql module from lib.rs**

Add to `etl/pg-collect/src/lib.rs`:

```rust
pub mod uris;
pub mod ntriples;
pub mod debian;
pub mod rpm;
pub mod sparql;
```

- [ ] **Step 4: Verify it compiles**

```bash
cd etl/pg-collect && cargo build 2>&1 | tail -5
```

Expected: `Compiling pg-collect ...` then `Finished`

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/sparql.rs etl/pg-collect/src/lib.rs
git commit -m "feat(pg-collect): add SPARQL Update client module"
```

---

### Task 2: pg-collect `load` and `drop` Subcommands

**Files:**
- Modify: `etl/pg-collect/src/main.rs`

- [ ] **Step 1: Add Load and Drop subcommands to CLI**

Add to the `Commands` enum in `main.rs`:

```rust
    /// Load N-Triples file into a Fuseki named graph via SPARQL Update
    Load {
        /// N-Triples file to load
        #[arg(required = true)]
        file: String,

        /// Named graph URI (e.g., https://packagegraph.github.io/graph/debian/trixie)
        #[arg(long, required = true)]
        graph: String,

        /// Fuseki SPARQL endpoint base URL (e.g., http://fuseki:3030/packagegraph)
        #[arg(long, required = true)]
        endpoint: String,

        /// Number of triples per INSERT DATA batch
        #[arg(long, default_value = "10000")]
        batch_size: usize,
    },

    /// Drop a named graph from Fuseki
    Drop {
        /// Named graph URI to drop
        #[arg(long, required = true)]
        graph: String,

        /// Fuseki SPARQL endpoint base URL
        #[arg(long, required = true)]
        endpoint: String,
    },
```

- [ ] **Step 2: Add match arms for Load and Drop**

Add to the `match cli.command` block in `main()`:

```rust
        Commands::Load {
            file,
            graph,
            endpoint,
            batch_size,
        } => {
            eprintln!("=== PackageGraph SPARQL Loader ===");
            eprintln!("File: {}", file);
            eprintln!("Graph: {}", graph);
            eprintln!("Endpoint: {}", endpoint);
            eprintln!("Batch size: {}", batch_size);
            eprintln!();

            let client = pg_collect::sparql::SparqlClient::new(&endpoint);
            client.load_file(&file, &graph, batch_size)
                .map(|count| (count, count))
        }

        Commands::Drop {
            graph,
            endpoint,
        } => {
            eprintln!("=== PackageGraph Graph Drop ===");
            eprintln!("Graph: {}", graph);
            eprintln!("Endpoint: {}", endpoint);
            eprintln!();

            let client = pg_collect::sparql::SparqlClient::new(&endpoint);
            client.drop_graph(&graph)
                .map(|_| (0, 0))
        }
```

- [ ] **Step 3: Verify it compiles and shows help**

```bash
cd etl/pg-collect && cargo build && ./target/debug/pg-collect load --help
```

Expected: Shows `--file`, `--graph`, `--endpoint`, `--batch-size` options

```bash
./target/debug/pg-collect drop --help
```

Expected: Shows `--graph`, `--endpoint` options

- [ ] **Step 4: Commit**

```bash
git add etl/pg-collect/src/main.rs
git commit -m "feat(pg-collect): add load and drop subcommands for SPARQL Update"
```

---

### Task 3: Fix RPM Multi-Repo in pg-collect

**Files:**
- Modify: `etl/pg-collect/src/main.rs`

- [ ] **Step 1: Fix the Rpm command handler to iterate all --rpm-repo specs**

Replace the current `Commands::Rpm` match arm (which only processes `rpm_repos[0]`):

```rust
        Commands::Rpm {
            repo,
            rpm_repos,
            distro_name,
            release_name,
            output,
        } => {
            eprintln!("=== PackageGraph RPM Collector ===");

            if let Some(url) = repo {
                // Single --repo mode
                eprintln!("Repository: {}", url);
                eprintln!("Distribution: {}", distro_name);
                eprintln!("Release: {}", release_name);
                eprintln!("Output: {}", output);
                eprintln!();

                let collector = RpmCollector::new(url, distro_name, release_name);
                collector.collect(&output)
            } else if !rpm_repos.is_empty() {
                // Multi --rpm-repo mode
                let mut total_packages = 0;
                let mut total_triples = 0;

                for (idx, repo_spec) in rpm_repos.iter().enumerate() {
                    let parts: Vec<&str> = repo_spec.splitn(3, ':').collect();
                    if parts.len() < 3 {
                        eprintln!("Error: --rpm-repo format is name:release:url, got: {}", repo_spec);
                        std::process::exit(1);
                    }

                    let rpm_distro = parts[0];
                    let rpm_release = parts[1];
                    let rpm_url = parts[2];

                    eprintln!("\n--- [{}/{}] {}/{} ---", idx + 1, rpm_repos.len(), rpm_distro, rpm_release);
                    eprintln!("Repository: {}", rpm_url);

                    // Each repo appends to same output file (or separate files)
                    let repo_output = if rpm_repos.len() == 1 {
                        output.clone()
                    } else {
                        let base = output.trim_end_matches(".nt");
                        format!("{}-{}-{}.nt", base, rpm_distro, rpm_release)
                    };

                    let collector = RpmCollector::new(
                        rpm_url.to_string(),
                        rpm_distro.to_string(),
                        rpm_release.to_string(),
                    );

                    match collector.collect(&repo_output) {
                        Ok((pkgs, triples)) => {
                            total_packages += pkgs;
                            total_triples += triples;
                        }
                        Err(e) => {
                            eprintln!("Error collecting {}/{}: {}", rpm_distro, rpm_release, e);
                            // Continue with other repos
                        }
                    }
                }

                Ok((total_packages, total_triples))
            } else {
                eprintln!("Error: Either --repo or --rpm-repo must be specified");
                std::process::exit(1);
            }
        }
```

- [ ] **Step 2: Verify it compiles**

```bash
cd etl/pg-collect && cargo build 2>&1 | tail -3
```

Expected: `Finished`

- [ ] **Step 3: Commit**

```bash
git add etl/pg-collect/src/main.rs
git commit -m "fix(pg-collect): iterate all --rpm-repo specs instead of only the first"
```

---

### Task 4: Enable Fuseki SPARQL Update Endpoint

**Files:**
- Modify: `fuseki/config.ttl`

- [ ] **Step 1: Add SPARQL Update endpoint to config.ttl**

```turtle
@prefix fuseki:  <http://jena.apache.org/fuseki#> .
@prefix rdf:     <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix ja:      <http://jena.hpl.hp.com/2005/11/Assembler#> .
@prefix tdb2:    <http://jena.apache.org/2016/tdb#> .

<#service> rdf:type fuseki:Service ;
    fuseki:name "packagegraph" ;
    fuseki:endpoint [ fuseki:operation fuseki:query ; fuseki:name "sparql" ] ;
    fuseki:endpoint [ fuseki:operation fuseki:query ; fuseki:name "query" ] ;
    fuseki:endpoint [ fuseki:operation fuseki:update ; fuseki:name "update" ] ;
    fuseki:endpoint [ fuseki:operation fuseki:gsp-r ; fuseki:name "get" ] ;
    fuseki:dataset <#dataset> .

# unionDefaultGraph: queries against the default graph see all named graphs.
# Each data source writes to its own named graph via SPARQL Update.
<#dataset> rdf:type tdb2:DatasetTDB2 ;
    tdb2:location "/data/tdb2" ;
    tdb2:unionDefaultGraph true .
```

- [ ] **Step 2: Commit**

```bash
git add fuseki/config.ttl
git commit -m "feat(fuseki): enable SPARQL Update endpoint for named graph writes"
```

---

### Task 5: Add pkg:DataSnapshot to Ontology

**Files:**
- Modify: `/Users/bharrington/Projects/packagegraph/ontology/core.ttl`

- [ ] **Step 1: Add DataSnapshot class and properties to core.ttl**

Add before the closing triples of core.ttl (after the last class definition):

```turtle
###  https://packagegraph.github.io/ontology/core#DataSnapshot
:DataSnapshot a owl:Class ;
    rdfs:subClassOf <http://www.w3.org/ns/prov#Entity> ;
    rdfs:label "Data Snapshot" ;
    rdfs:comment "Metadata about a named graph — when it was collected, from what source, whether it is current." ;
    <http://purl.obolibrary.org/obo/IAO_0000115> "A record of a data collection or enrichment run, tracking the named graph URI, source identifier, timestamp, and currency status for graph lifecycle management." .

###  https://packagegraph.github.io/ontology/core#snapshotSource
:snapshotSource a owl:DatatypeProperty ;
    rdfs:domain :DataSnapshot ;
    rdfs:range xsd:string ;
    rdfs:label "snapshot source" ;
    rdfs:comment "Identifier of the data source that produced this snapshot (e.g., 'pg-collect debian', 'enricher-osv')." ;
    <http://purl.obolibrary.org/obo/IAO_0000115> "A string identifying the tool and data source that produced this snapshot." .

###  https://packagegraph.github.io/ontology/core#snapshotTimestamp
:snapshotTimestamp a owl:DatatypeProperty ;
    rdfs:domain :DataSnapshot ;
    rdfs:range xsd:dateTime ;
    rdfs:label "snapshot timestamp" ;
    rdfs:comment "When this data snapshot was created." ;
    <http://purl.obolibrary.org/obo/IAO_0000115> "The date and time when the data collection or enrichment run completed." .

###  https://packagegraph.github.io/ontology/core#snapshotGraph
:snapshotGraph a owl:DatatypeProperty ;
    rdfs:domain :DataSnapshot ;
    rdfs:range xsd:anyURI ;
    rdfs:label "snapshot graph" ;
    rdfs:comment "The named graph URI this snapshot was loaded into." ;
    <http://purl.obolibrary.org/obo/IAO_0000115> "The URI of the named graph in Fuseki that contains this snapshot's data." .

###  https://packagegraph.github.io/ontology/core#isCurrent
:isCurrent a owl:DatatypeProperty, owl:FunctionalProperty ;
    rdfs:domain :DataSnapshot ;
    rdfs:range xsd:boolean ;
    rdfs:label "is current" ;
    rdfs:comment "Whether this snapshot represents the current active data for its source." ;
    <http://purl.obolibrary.org/obo/IAO_0000115> "A boolean flag indicating whether this snapshot is the most recent for its data source. Used for tiered storage lifecycle management." .
```

- [ ] **Step 2: Validate the ontology parses**

```bash
cd /Users/bharrington/Projects/packagegraph/ontology && uv run python -c "import rdflib; g = rdflib.Graph(); g.parse('core.ttl', format='turtle'); print(f'OK: {len(g)} triples')"
```

Expected: `OK: <number> triples` (higher than before)

- [ ] **Step 3: Copy updated ontology to platform ETL build context**

```bash
cp /Users/bharrington/Projects/packagegraph/ontology/*.ttl /Users/bharrington/Projects/packagegraph/platform/etl/ontology/
```

- [ ] **Step 4: Commit in ontology repo**

```bash
cd /Users/bharrington/Projects/packagegraph/ontology
git add core.ttl
git commit -m "feat(core): add pkg:DataSnapshot class for graph provenance metadata"
```

---

### Task 6: Fuseki Query Catalog Page

**Files:**
- Create: `fuseki/catalog.html`
- Modify: `fuseki/Containerfile`

- [ ] **Step 1: Create catalog.html with YASGUI and pre-loaded queries**

Create `fuseki/catalog.html` — a self-contained HTML page with YASGUI embedded via CDN, pre-loaded with the validated queries organized by category. The page auto-detects the Fuseki endpoint from its own URL.

The file should contain:
- HTML page with clean styling
- YASGUI loaded from `https://unpkg.com/@triply/yasgui/build/yasgui.min.js` and CSS
- JavaScript object containing named queries with descriptions, organized by category
- A sidebar/dropdown for selecting queries by name
- Ontology namespace prefixes pre-configured (`pkg:`, `deb:`, `rpm:`, `sec:`, `vcs:`, `foaf:`, `prov:`, `data:`)
- Endpoint auto-configured to `../sparql` (relative to catalog page location)

Query categories and examples to include:
- **Overview:** Total triples, Class distribution, Predicate distribution, Named graph inventory (`SELECT ?g (COUNT(*) AS ?triples) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g`)
- **Packages:** Package detail by name, Architecture breakdown, Source-binary mapping (top 10)
- **Dependencies:** Most-depended-on packages, Reverse deps by name, Version constraints count
- **Maintainers:** Top 10 maintainers, Packages per maintainer
- **Cross-Distro:** Packages by distribution, Packages shared across distros (`SELECT ?name (COUNT(DISTINCT ?dist) AS ?distros) WHERE { ?p pkg:packageName ?name . ?p pkg:partOfDistribution ?dist } GROUP BY ?name HAVING(COUNT(DISTINCT ?dist) > 1) ORDER BY DESC(?distros) LIMIT 20`)
- **Security:** CVEs per package, Unpatched vulnerabilities (post-enrichment placeholder)
- **VCS:** Upstream repos by stars, Recent commits (post-enrichment placeholder)

- [ ] **Step 2: Add COPY to Fuseki Containerfile**

Add after the existing COPY lines in `fuseki/Containerfile`:

```dockerfile
# Query catalog page
COPY catalog.html /fuseki/webapp/catalog.html
```

- [ ] **Step 3: Commit**

```bash
cd /Users/bharrington/Projects/packagegraph/platform
git add fuseki/catalog.html fuseki/Containerfile
git commit -m "feat(fuseki): add SPARQL query catalog page with YASGUI"
```

---

### Task 7: Python SPARQL Client for Enrichers

**Files:**
- Create: `etl/packagegraph/sparql_client.py`
- Create: `etl/tests/test_sparql_client.py`

- [ ] **Step 1: Write failing test for SparqlQueryClient**

```python
# etl/tests/test_sparql_client.py
"""Tests for the Fuseki SPARQL query client."""
import json
from unittest.mock import patch, MagicMock
import pytest
from packagegraph.sparql_client import SparqlQueryClient


def _mock_response(json_data, status_code=200):
    mock = MagicMock()
    mock.status_code = status_code
    mock.json.return_value = json_data
    mock.raise_for_status.return_value = None
    return mock


@pytest.mark.unit
class TestSparqlQueryClient:
    def test_query_returns_bindings(self):
        client = SparqlQueryClient("http://fuseki:3030/packagegraph")
        response_data = {
            "results": {
                "bindings": [
                    {"name": {"type": "literal", "value": "bash"}},
                    {"name": {"type": "literal", "value": "wget"}},
                ]
            }
        }
        with patch("packagegraph.sparql_client.requests.post", return_value=_mock_response(response_data)):
            results = client.query("SELECT ?name WHERE { ?p pkg:packageName ?name }")
            assert len(results) == 2
            assert results[0]["name"]["value"] == "bash"

    def test_query_package_names_returns_list(self):
        client = SparqlQueryClient("http://fuseki:3030/packagegraph")
        response_data = {
            "results": {
                "bindings": [
                    {"name": {"type": "literal", "value": "bash"}, "version": {"type": "literal", "value": "5.2"}},
                    {"name": {"type": "literal", "value": "wget"}, "version": {"type": "literal", "value": "1.21"}},
                ]
            }
        }
        with patch("packagegraph.sparql_client.requests.post", return_value=_mock_response(response_data)):
            results = client.query_package_names_and_versions()
            assert len(results) == 2
            assert results[0] == ("bash", "5.2")

    def test_query_github_homepages_returns_tuples(self):
        client = SparqlQueryClient("http://fuseki:3030/packagegraph")
        response_data = {
            "results": {
                "bindings": [
                    {
                        "pkg": {"type": "uri", "value": "https://example.org/pkg1"},
                        "homepage": {"type": "literal", "value": "https://github.com/owner/repo"},
                    }
                ]
            }
        }
        with patch("packagegraph.sparql_client.requests.post", return_value=_mock_response(response_data)):
            results = client.query_github_homepages()
            assert len(results) == 1
            assert results[0] == ("https://example.org/pkg1", "https://github.com/owner/repo")
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_sparql_client.py -q
```

Expected: `ModuleNotFoundError: No module named 'packagegraph.sparql_client'`

- [ ] **Step 3: Implement SparqlQueryClient**

```python
# etl/packagegraph/sparql_client.py
"""Thin client for querying Fuseki SPARQL endpoint."""
import requests


class SparqlQueryClient:
    """Queries a Fuseki SPARQL endpoint and returns parsed results."""

    def __init__(self, endpoint: str):
        self.endpoint = endpoint.rstrip("/")
        self.sparql_url = f"{self.endpoint}/sparql"

    def query(self, sparql: str) -> list[dict]:
        """Execute a SPARQL query and return bindings."""
        response = requests.post(
            self.sparql_url,
            data={"query": sparql},
            headers={"Accept": "application/sparql-results+json"},
            timeout=120,
        )
        response.raise_for_status()
        return response.json()["results"]["bindings"]

    def query_package_names_and_versions(self) -> list[tuple[str, str]]:
        """Get unique (package_name, version_string) pairs from all collections."""
        sparql = """
        PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
        SELECT DISTINCT ?name ?version WHERE {
            ?p a pkg:BinaryPackage .
            ?p pkg:packageName ?name .
            ?p pkg:hasVersion ?v .
            ?v pkg:versionString ?version .
        }
        """
        bindings = self.query(sparql)
        return [(b["name"]["value"], b["version"]["value"]) for b in bindings]

    def query_github_homepages(self) -> list[tuple[str, str]]:
        """Get (package_uri, homepage_url) for packages with GitHub homepages."""
        sparql = """
        PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
        SELECT DISTINCT ?pkg ?homepage WHERE {
            ?pkg a pkg:BinaryPackage .
            ?pkg pkg:homepage ?homepage .
            FILTER(CONTAINS(STR(?homepage), "github.com"))
        }
        """
        bindings = self.query(sparql)
        return [(b["pkg"]["value"], b["homepage"]["value"]) for b in bindings]
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_sparql_client.py -q
```

Expected: `3 passed`

- [ ] **Step 5: Commit**

```bash
git add etl/packagegraph/sparql_client.py etl/tests/test_sparql_client.py
git commit -m "feat(etl): add SPARQL query client for enricher Fuseki access"
```

---

### Task 8: Refactor Security Enricher to Query Fuseki

**Files:**
- Create: `etl/packagegraph/enrichers/__init__.py`
- Create: `etl/packagegraph/enrichers/security.py`
- Create: `etl/tests/test_enrichers/__init__.py`
- Create: `etl/tests/test_enrichers/test_security.py`

- [ ] **Step 1: Write failing test for SecurityEnricher**

```python
# etl/tests/test_enrichers/__init__.py
# (empty)

# etl/tests/test_enrichers/test_security.py
"""Tests for the Fuseki-backed SecurityEnricher."""
import json
import tempfile
from pathlib import Path
from unittest.mock import patch, MagicMock
import pytest
from packagegraph.enrichers.security import SecurityEnricher


@pytest.mark.unit
class TestSecurityEnricher:
    def test_enrich_writes_ntriples_for_vulnerable_package(self, tmp_path):
        mock_client = MagicMock()
        mock_client.query_package_names_and_versions.return_value = [
            ("openssl", "3.0.2-1"),
        ]

        osv_response = MagicMock()
        osv_response.status_code = 200
        osv_response.json.return_value = {
            "vulns": [{
                "id": "CVE-2022-0778",
                "summary": "Infinite loop in BN_mod_sqrt()",
                "severity": [{"type": "CVSS_V3", "score": "7.5"}],
                "published": "2022-03-15T00:00:00Z",
                "modified": "2022-03-16T00:00:00Z",
                "affected": [{"package": {"name": "openssl", "ecosystem": "Debian"}}],
            }]
        }

        output_file = tmp_path / "security.nt"

        with patch("packagegraph.enrichers.security.requests.post", return_value=osv_response):
            enricher = SecurityEnricher(
                sparql_client=mock_client,
                output_path=str(output_file),
                cache_dir=str(tmp_path / "cache"),
            )
            enricher.enrich()

        content = output_file.read_text()
        assert "CVE-2022-0778" in content
        assert "Vulnerability" in content
        assert "affectsVersion" in content

    def test_enrich_skips_unrelated_cves(self, tmp_path):
        mock_client = MagicMock()
        mock_client.query_package_names_and_versions.return_value = [
            ("bash", "5.2"),
        ]

        osv_response = MagicMock()
        osv_response.status_code = 200
        osv_response.json.return_value = {
            "vulns": [{
                "id": "CVE-2099-9999",
                "affected": [{"package": {"name": "other-package", "ecosystem": "Debian"}}],
            }]
        }

        output_file = tmp_path / "security.nt"

        with patch("packagegraph.enrichers.security.requests.post", return_value=osv_response):
            enricher = SecurityEnricher(
                sparql_client=mock_client,
                output_path=str(output_file),
                cache_dir=str(tmp_path / "cache"),
            )
            enricher.enrich()

        content = output_file.read_text()
        assert "affectsVersion" not in content
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_enrichers/test_security.py -q
```

Expected: `ModuleNotFoundError: No module named 'packagegraph.enrichers'`

- [ ] **Step 3: Implement SecurityEnricher**

```python
# etl/packagegraph/enrichers/__init__.py
# (empty)

# etl/packagegraph/enrichers/security.py
"""Security vulnerability enricher — queries Fuseki, calls OSV.dev, writes N-Triples."""
import json
import time
from pathlib import Path
from datetime import datetime, timedelta

import requests

from ..sparql_client import SparqlQueryClient
from ..ntriples_writer import NTriplesWriter
from ..namespaces import SEC, PKG, cve_uri, version_uri


class SecurityEnricher:
    """Enriches the package graph with CVE data from OSV.dev.

    Reads package names/versions from Fuseki via SPARQL.
    Writes vulnerability triples to an N-Triples file for loading via pg-collect load.
    """

    def __init__(
        self,
        sparql_client: SparqlQueryClient,
        output_path: str,
        cache_dir: str | None = None,
        cache_ttl_hours: int = 24,
    ):
        self.client = sparql_client
        self.output_path = output_path
        self.cache_dir = Path(cache_dir) if cache_dir else None
        self.cache_ttl = timedelta(hours=cache_ttl_hours)
        self.osv_api = "https://api.osv.dev/v1"

        if self.cache_dir:
            self.cache_dir.mkdir(parents=True, exist_ok=True)

    def enrich(self):
        """Query packages from Fuseki, check OSV, write vulnerability triples."""
        print("Querying Fuseki for package names and versions...")
        packages = self.client.query_package_names_and_versions()
        print(f"Found {len(packages)} packages to check.")

        # Deduplicate by name
        seen = set()
        unique_packages = []
        for name, version in packages:
            if name not in seen:
                seen.add(name)
                unique_packages.append((name, version))

        with open(self.output_path, "w") as f:
            for idx, (pkg_name, version_str) in enumerate(unique_packages, 1):
                if idx % 100 == 0:
                    print(f"  [{idx}/{len(unique_packages)}] Checking {pkg_name}...")

                vulns = self._query_osv(pkg_name)
                if vulns:
                    self._write_vuln_triples(f, pkg_name, version_str, vulns)

                time.sleep(0.5)  # Rate limit

        print(f"Security enrichment complete. Output: {self.output_path}")

    def _query_osv(self, package_name: str) -> list[dict] | None:
        """Query OSV API for vulnerabilities."""
        if self.cache_dir:
            cache_file = self.cache_dir / f"{package_name}.json"
            if cache_file.exists():
                age = datetime.now() - datetime.fromtimestamp(cache_file.stat().st_mtime)
                if age < self.cache_ttl:
                    with open(cache_file) as f:
                        return json.load(f).get("vulns", [])

        try:
            response = requests.post(
                f"{self.osv_api}/query",
                json={"package": {"name": package_name, "ecosystem": "Debian"}},
                timeout=30,
            )
            response.raise_for_status()
            data = response.json()

            if self.cache_dir:
                cache_file = self.cache_dir / f"{package_name}.json"
                with open(cache_file, "w") as f:
                    json.dump(data, f)

            return data.get("vulns", [])
        except Exception as e:
            print(f"  OSV error for {package_name}: {e}")
            return None

    def _write_vuln_triples(self, f, pkg_name: str, version_str: str, vulns: list[dict]):
        """Write vulnerability triples in N-Triples format."""
        for vuln in vulns:
            vuln_id = vuln.get("id", "")
            if not vuln_id:
                continue

            vuln_uri = str(cve_uri(vuln_id))

            # Type
            f.write(f"<{vuln_uri}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{SEC}Vulnerability> .\n")
            # CVE ID
            _write_literal(f, vuln_uri, f"{SEC}cveId", vuln_id)
            # Description
            if vuln.get("summary"):
                _write_literal(f, vuln_uri, f"{SEC}vulnerabilityDescription", vuln["summary"][:1000])
            # Severity
            for sev in vuln.get("severity", []):
                if sev.get("type") == "CVSS_V3":
                    _write_literal(f, vuln_uri, f"{SEC}severity", sev["score"])
            # Published
            if vuln.get("published"):
                _write_literal(f, vuln_uri, f"{SEC}publishedDate", vuln["published"])

            # Link to affected versions
            for affected in vuln.get("affected", []):
                affected_pkg = affected.get("package", {})
                if affected_pkg.get("name", "").lower() == pkg_name.lower():
                    # Link to our version — approximate match
                    ver_uri = str(version_uri("debian", "trixie", pkg_name, version_str))
                    f.write(f"<{vuln_uri}> <{SEC}affectsVersion> <{ver_uri}> .\n")
                    break


def _escape_nt(s: str) -> str:
    """Escape a string for N-Triples literal."""
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t")


def _write_literal(f, subject_uri: str, predicate_uri: str, value: str):
    """Write a literal triple in N-Triples format."""
    f.write(f'<{subject_uri}> <{predicate_uri}> "{_escape_nt(value)}" .\n')
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_enrichers/test_security.py -q
```

Expected: `2 passed`

- [ ] **Step 5: Commit**

```bash
git add etl/packagegraph/enrichers/ etl/tests/test_enrichers/
git commit -m "feat(etl): add Fuseki-backed SecurityEnricher with N-Triples output"
```

---

### Task 9: Refactor GitHub Enricher to Query Fuseki

**Files:**
- Create: `etl/packagegraph/enrichers/github.py`
- Create: `etl/tests/test_enrichers/test_github.py`

- [ ] **Step 1: Write failing test for GitHubEnricher**

```python
# etl/tests/test_enrichers/test_github.py
"""Tests for the Fuseki-backed GitHubEnricher."""
from unittest.mock import patch, MagicMock
import pytest
from packagegraph.enrichers.github import GitHubEnricher


def _mock_github_response(json_data, status_code=200, headers=None):
    mock = MagicMock()
    mock.status_code = status_code
    mock.json.return_value = json_data
    mock.headers = headers or {"X-RateLimit-Remaining": "4999"}
    mock.raise_for_status.return_value = None
    return mock


@pytest.mark.unit
class TestGitHubEnricher:
    def test_enrich_writes_repo_metadata(self, tmp_path):
        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ("https://packagegraph.github.io/data/package/debian/trixie/amd64/curl/7.88", "https://github.com/curl/curl"),
        ]

        repo_data = {
            "html_url": "https://github.com/curl/curl",
            "default_branch": "master",
            "description": "A command line tool for transferring data",
            "stargazers_count": 35000,
            "forks_count": 6000,
        }
        commits_data = [{
            "sha": "abc123def456",
            "commit": {
                "author": {"name": "Daniel Stenberg", "email": "daniel@haxx.se", "date": "2026-04-01T12:00:00Z"},
                "message": "Fix buffer overflow",
            }
        }]

        output_file = tmp_path / "github.nt"

        with patch("packagegraph.enrichers.github.requests.get") as mock_get:
            mock_get.side_effect = [
                _mock_github_response(repo_data),
                _mock_github_response(commits_data),
            ]
            enricher = GitHubEnricher(
                sparql_client=mock_client,
                output_path=str(output_file),
                github_token="fake-token",
                cache_dir=str(tmp_path / "cache"),
            )
            enricher.enrich()

        content = output_file.read_text()
        assert "Repository" in content
        assert "curl" in content
        assert "35000" in content
        assert "abc123def456" in content
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_enrichers/test_github.py -q
```

Expected: `ModuleNotFoundError`

- [ ] **Step 3: Implement GitHubEnricher**

```python
# etl/packagegraph/enrichers/github.py
"""GitHub VCS enricher — queries Fuseki for homepages, calls GitHub API, writes N-Triples."""
import re
import json
import time
from pathlib import Path
from datetime import datetime, timedelta

import requests

from ..sparql_client import SparqlQueryClient
from ..namespaces import VCS, PKG, FOAF, DATA, repo_uri, maintainer_uri


class GitHubEnricher:
    """Enriches the package graph with GitHub repository metadata.

    Reads package homepages from Fuseki via SPARQL.
    Writes VCS triples to an N-Triples file for loading via pg-collect load.
    """

    def __init__(
        self,
        sparql_client: SparqlQueryClient,
        output_path: str,
        github_token: str | None = None,
        cache_dir: str | None = None,
        cache_ttl_hours: int = 24,
    ):
        self.client = sparql_client
        self.output_path = output_path
        self.token = github_token
        self.cache_dir = Path(cache_dir) if cache_dir else None
        self.cache_ttl = timedelta(hours=cache_ttl_hours)
        self.api_base = "https://api.github.com"

        if self.cache_dir:
            self.cache_dir.mkdir(parents=True, exist_ok=True)

    def enrich(self):
        """Query GitHub homepages from Fuseki, fetch repo data, write triples."""
        print("Querying Fuseki for packages with GitHub homepages...")
        homepage_pairs = self.client.query_github_homepages()
        print(f"Found {len(homepage_pairs)} packages with GitHub URLs.")

        github_re = re.compile(r"https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?/?$")
        processed = set()

        with open(self.output_path, "w") as f:
            for pkg_uri_str, homepage in homepage_pairs:
                match = github_re.match(homepage)
                if not match:
                    continue

                owner, repo_name = match.group(1), match.group(2)
                repo_key = f"{owner}/{repo_name}"
                if repo_key in processed:
                    continue
                processed.add(repo_key)

                print(f"  Fetching {repo_key}...")
                self._process_repo(f, pkg_uri_str, owner, repo_name)

        print(f"GitHub enrichment complete. Output: {self.output_path}")

    def _api_get(self, endpoint: str) -> dict | list | None:
        """Make authenticated GitHub API request with caching."""
        if self.cache_dir:
            cache_key = endpoint.replace("/", "_")
            cache_file = self.cache_dir / f"{cache_key}.json"
            if cache_file.exists():
                age = datetime.now() - datetime.fromtimestamp(cache_file.stat().st_mtime)
                if age < self.cache_ttl:
                    with open(cache_file) as cf:
                        return json.load(cf)

        headers = {"Accept": "application/vnd.github.v3+json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"

        try:
            response = requests.get(f"{self.api_base}{endpoint}", headers=headers, timeout=30)
            response.raise_for_status()

            remaining = int(response.headers.get("X-RateLimit-Remaining", 100))
            if remaining < 100:
                time.sleep(2.0)

            data = response.json()

            if self.cache_dir:
                cache_file = self.cache_dir / f"{cache_key}.json"
                with open(cache_file, "w") as cf:
                    json.dump(data, cf)

            return data
        except Exception as e:
            print(f"    GitHub API error: {e}")
            return None

    def _process_repo(self, f, pkg_uri_str: str, owner: str, repo_name: str):
        """Fetch repo metadata and commits, write N-Triples."""
        repo_data = self._api_get(f"/repos/{owner}/{repo_name}")
        if not repo_data:
            return

        repo_url = repo_data.get("html_url", f"https://github.com/{owner}/{repo_name}")
        r_uri = str(repo_uri(repo_url))

        # Repository type
        f.write(f"<{r_uri}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{VCS}Repository> .\n")
        f.write(f"<{r_uri}> <{VCS}repositoryURL> <{repo_url}> .\n")

        if repo_data.get("default_branch"):
            _write_literal(f, r_uri, f"{VCS}defaultBranch", repo_data["default_branch"])
        if repo_data.get("description"):
            _write_literal(f, r_uri, f"{VCS}repositoryDescription", repo_data["description"])
        if repo_data.get("stargazers_count") is not None:
            _write_integer(f, r_uri, f"{VCS}starCount", repo_data["stargazers_count"])
        if repo_data.get("forks_count") is not None:
            _write_integer(f, r_uri, f"{VCS}forkCount", repo_data["forks_count"])

        # Link package to upstream
        f.write(f"<{pkg_uri_str}> <{PKG}homepage> <{repo_url}> .\n")

        # Fetch recent commits
        commits_data = self._api_get(f"/repos/{owner}/{repo_name}/commits?per_page=50")
        if commits_data:
            for entry in commits_data[:50]:
                sha = entry.get("sha", "")
                if not sha:
                    continue
                commit_uri = f"{DATA}commit/{sha[:12]}"
                commit_info = entry.get("commit", {})
                author_info = commit_info.get("author", {})

                f.write(f"<{commit_uri}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{VCS}Commit> .\n")
                _write_literal(f, commit_uri, f"{VCS}commitHash", sha)
                f.write(f"<{r_uri}> <{VCS}hasCommit> <{commit_uri}> .\n")

                if author_info.get("date"):
                    _write_literal(f, commit_uri, f"{VCS}commitDate", author_info["date"])
                if commit_info.get("message"):
                    _write_literal(f, commit_uri, f"{VCS}commitMessage", commit_info["message"][:500])

                if author_info.get("name") and author_info.get("email"):
                    m_uri = str(maintainer_uri(author_info["email"]))
                    f.write(f"<{commit_uri}> <{VCS}authoredBy> <{m_uri}> .\n")
                    f.write(f"<{m_uri}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{PKG}Maintainer> .\n")
                    _write_literal(f, m_uri, f"{FOAF}name", author_info["name"])


def _escape_nt(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t")


def _write_literal(f, subject_uri: str, predicate_uri: str, value: str):
    f.write(f'<{subject_uri}> <{predicate_uri}> "{_escape_nt(value)}" .\n')


def _write_integer(f, subject_uri: str, predicate_uri: str, value: int):
    f.write(f'<{subject_uri}> <{predicate_uri}> "{value}"^^<http://www.w3.org/2001/XMLSchema#integer> .\n')
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_enrichers/test_github.py -q
```

Expected: `1 passed`

- [ ] **Step 5: Commit**

```bash
git add etl/packagegraph/enrichers/github.py etl/tests/test_enrichers/test_github.py
git commit -m "feat(etl): add Fuseki-backed GitHubEnricher with N-Triples output"
```

---

### Task 10: Collection Entrypoint and CronJob Manifests

**Files:**
- Modify: `etl/entrypoint.sh`
- Create: `deploy/base/etl/cronjob-template.yaml`
- Create: `deploy/overlays/dev/jobs/collect-debian-trixie.yaml`
- Create: `deploy/overlays/dev/jobs/collect-fedora-43.yaml`
- Create: `deploy/overlays/dev/jobs/collect-centos-stream-9.yaml`
- Create: `deploy/overlays/dev/jobs/collect-centos-stream-10.yaml`
- Create: `deploy/overlays/dev/jobs/collect-opensuse-tw.yaml`
- Create: `deploy/overlays/dev/jobs/collect-fedora-rawhide.yaml`
- Create: `deploy/overlays/dev/jobs/enrich-security.yaml`
- Create: `deploy/overlays/dev/jobs/enrich-github.yaml`
- Modify: `deploy/overlays/dev/kustomization.yaml`
- Create: `deploy/base/secrets/github-token.yaml`

- [ ] **Step 1: Update entrypoint.sh for the SPARQL load pipeline**

Replace the current entrypoint with a new version that supports both the old TDB2 pipeline and the new SPARQL load pipeline. When `FUSEKI_ENDPOINT` is set, use pg-collect load/drop. Otherwise fall back to tdb2.tdbloader.

The entrypoint should:
1. Collect via pg-collect (existing)
2. Archive .nt to Minio (existing)
3. If `FUSEKI_ENDPOINT` is set: `pg-collect drop` then `pg-collect load` into the named graph
4. Otherwise: build TDB2 and upload snapshot (legacy path)

- [ ] **Step 2: Create CronJob template**

Create `deploy/base/etl/cronjob-template.yaml` with the common structure (imagePullSecrets, securityContext, resources, envFrom for minio-credentials). Per-distro jobs customize via patches.

- [ ] **Step 3: Create per-distro job patches**

Each file in `deploy/overlays/dev/jobs/` sets the specific env vars:
- `GRAPH_URI` — the named graph URI
- `REPO_URL` / `RPM_REPOS` — the repository URL(s)
- `REPO_TYPE` — debian or rpm
- `DISTRO_NAME` / `RELEASE_NAME` — for RPM collections
- `FUSEKI_ENDPOINT` — `http://fuseki.packagegraph.svc:3030/packagegraph`

- [ ] **Step 4: Create enrichment job manifests**

`enrich-security.yaml` and `enrich-github.yaml` run the Python enrichers with `FUSEKI_ENDPOINT` and write output .nt files, then use `pg-collect load` to load results.

- [ ] **Step 5: Create github-token secret template**

```yaml
# deploy/base/secrets/github-token.yaml
apiVersion: v1
kind: Secret
metadata:
  name: github-token
  namespace: packagegraph
type: Opaque
stringData:
  GITHUB_TOKEN: "replace-with-actual-token"
```

- [ ] **Step 6: Update kustomization.yaml to include new resources**

Add the CronJob template and github-token secret to `deploy/base/kustomization.yaml`. Add the per-distro patches to `deploy/overlays/dev/kustomization.yaml`.

- [ ] **Step 7: Commit**

```bash
git add deploy/ etl/entrypoint.sh
git commit -m "feat(deploy): add per-distro collection CronJobs and enrichment jobs

Six collection jobs (Debian trixie, Fedora 43, Fedora Rawhide,
CentOS Stream 9/10, openSUSE Tumbleweed) plus security and GitHub
enrichment jobs. All use SPARQL Update via pg-collect load/drop."
```

---

### Task 11: GitHub Actions CI Pipeline

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `.github/workflows/release.yml`

- [ ] **Step 1: Create ci.yml**

```yaml
# .github/workflows/ci.yml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  lint-ontology:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: astral-sh/setup-uv@v4
      - run: uv pip install rdflib --system
      - name: Validate ontology TTL files
        run: |
          for f in etl/ontology/*.ttl; do
            echo "Checking $f..."
            python -c "import rdflib; g = rdflib.Graph(); g.parse('$f', format='turtle'); print(f'  OK: {len(g)} triples')"
          done

  test-etl:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: etl
    steps:
      - uses: actions/checkout@v4
      - uses: astral-sh/setup-uv@v4
      - run: uv sync --frozen
      - run: uv run pytest -q --tb=short

  build-etl:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-buildx-action@v3
      - name: Build ETL image
        uses: docker/build-push-action@v6
        with:
          context: etl
          file: etl/Containerfile
          push: false
          tags: ghcr.io/packagegraph/etl:ci

  build-fuseki:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-buildx-action@v3
      - name: Build Fuseki image
        uses: docker/build-push-action@v6
        with:
          context: fuseki
          file: fuseki/Containerfile
          push: false
          tags: ghcr.io/packagegraph/fuseki:ci
```

- [ ] **Step 2: Create release.yml**

```yaml
# .github/workflows/release.yml
name: Release

on:
  push:
    tags: ['v*']

permissions:
  contents: write
  packages: write

jobs:
  ci:
    uses: ./.github/workflows/ci.yml

  release:
    needs: ci
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - uses: docker/setup-buildx-action@v3

      - name: Extract version
        id: version
        run: echo "tag=${GITHUB_REF#refs/tags/}" >> $GITHUB_OUTPUT

      - name: Build and push ETL image
        uses: docker/build-push-action@v6
        with:
          context: etl
          file: etl/Containerfile
          push: true
          tags: |
            ghcr.io/packagegraph/etl:${{ steps.version.outputs.tag }}
            ghcr.io/packagegraph/etl:latest

      - name: Build and push Fuseki image
        uses: docker/build-push-action@v6
        with:
          context: fuseki
          file: fuseki/Containerfile
          push: true
          tags: |
            ghcr.io/packagegraph/fuseki:${{ steps.version.outputs.tag }}
            ghcr.io/packagegraph/fuseki:latest

      - name: Create GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          generate_release_notes: true
```

- [ ] **Step 3: Commit**

```bash
git add .github/
git commit -m "feat: add GitHub Actions CI and release workflows

CI: lint ontology, run ETL tests, build both container images.
Release: build+push tagged images to GHCR, create GitHub Release."
```

---

## Progress Tracking

- [ ] Task 1: pg-collect SPARQL Client Module (Rust)
- [ ] Task 2: pg-collect `load` and `drop` Subcommands
- [ ] Task 3: Fix RPM Multi-Repo in pg-collect
- [ ] Task 4: Enable Fuseki SPARQL Update Endpoint
- [ ] Task 5: Add pkg:DataSnapshot to Ontology
- [ ] Task 6: Fuseki Query Catalog Page
- [ ] Task 7: Python SPARQL Client for Enrichers
- [ ] Task 8: Refactor Security Enricher to Query Fuseki
- [ ] Task 9: Refactor GitHub Enricher to Query Fuseki
- [ ] Task 10: Collection Entrypoint and CronJob Manifests
- [ ] Task 11: GitHub Actions CI Pipeline

**Total Tasks:** 11 | **Completed:** 0 | **Remaining:** 11
