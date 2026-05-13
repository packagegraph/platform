# Querying PackageGraph

Two interfaces for exploring PackageGraph's RDF knowledge graph: a browser-based
SPARQL editor and a Jupyter notebook. No Java required.

**Current dataset:** ~37.5M triples across 10 named graphs (Fedora 43/Rawhide,
Debian trixie, openSUSE Tumbleweed, RHEL 9/10, CentOS Stream 9/10, Homebrew,
Gentoo, plus security and enrichment graphs).

---

## Table of Contents

- [Connecting to Fuseki](#connecting-to-fuseki)
- [1. YASGUI Web Interface](#1-yasgui-web-interface)
- [2. Jupyter Notebook](#2-jupyter-notebook)
- [3. curl / SPARQL](#3-curl--sparql)
- [SPARQL Reference](#sparql-reference)
- [Administration](#administration)

---

## Connecting to Fuseki

All query interfaces need access to the Fuseki SPARQL endpoint. There are two
ways to connect depending on your environment.

### Option A: Port-Forward (development / local use)

Requires `oc` CLI logged into the cluster:

```bash
oc port-forward svc/fuseki 3030:3030 -n packagegraph
```

The endpoint becomes: `http://localhost:3030/packagegraph/sparql`

This stays open as long as the terminal is running. Open a second terminal for
queries.

### Option B: External Route (if exposed)

If the cluster has an OpenShift Route or Ingress configured for Fuseki, use the
external URL directly:

```
https://fuseki-packagegraph.<cluster-domain>/packagegraph/sparql
```

The current dev cluster exposes Fuseki at:
`https://fuseki-packagegraph.apps.kafka.tel/packagegraph/sparql`

### Verifying Connectivity

```bash
# Health check
curl -s http://localhost:3030/$/ping
# → should return: "OK"

# Quick triple count
curl -s -X POST http://localhost:3030/packagegraph/sparql \
  -H "Accept: application/sparql-results+json" \
  -d "query=SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }" \
  | python3 -m json.tool
```

---

## 1. YASGUI Web Interface

**File:** `query/yasgui.html`

A browser-based SPARQL query editor with syntax highlighting, autocomplete,
and tabular results. No installation required — it's a single HTML file that
loads YASGUI from CDN.

### Setup

1. Start a port-forward (see [Connecting to Fuseki](#connecting-to-fuseki))
2. Open `query/yasgui.html` in your browser

That's it. The page defaults to `http://localhost:3030/packagegraph/sparql`.
If your endpoint is different, update it in the endpoint bar at the top.

### Usage

- **Write SPARQL** in the editor — it has syntax highlighting and prefix
  autocomplete for all PackageGraph namespaces
- **Click example buttons** to load pre-built queries:
  - Top packages by version count
  - Distribution statistics
  - Dependencies of a package
  - Vulnerable packages
  - Cross-distribution equivalences
  - Most active maintainers
  - Packages with GitHub repos
  - Source → binary relationships
- **Run** with the play button or Ctrl+Enter
- **Results** render as a sortable, filterable table
- **Share** queries via the URL — YASGUI encodes the query in the URL hash

### Deploying to the Cluster

To serve YASGUI alongside Fuseki in the cluster:

```bash
# Create ConfigMap from the HTML file
oc create configmap yasgui-page \
  --from-file=index.html=query/yasgui.html \
  -n packagegraph

# Mount as a volume in the Fuseki deployment (or deploy a separate nginx)
# The simplest approach: nginx sidecar or static file server
```

For most use cases, opening the local HTML file while port-forwarding is
sufficient.

---

## 2. Jupyter Notebook

**File:** `query/explore-packagegraph.ipynb`

An interactive notebook with 15 pre-built SPARQL queries that return results
as pandas DataFrames. Best for data exploration, analysis, and visualization.

### Setup

```bash
# Install dependencies
pip install sparqlwrapper pandas

# Or with uv
uv pip install sparqlwrapper pandas

# Start Jupyter
jupyter notebook query/explore-packagegraph.ipynb
```

Make sure Fuseki is accessible (port-forward or external route). The notebook
defaults to `http://localhost:3030/packagegraph/sparql` — change the `ENDPOINT`
variable in the first cell if needed.

### Included Queries

| # | Query | What It Shows |
|---|-------|---------------|
| 1 | Dataset overview | Resource types and counts |
| 2 | Distribution statistics | Packages and versions per distro |
| 3 | Package search | Find packages by name substring |
| 4 | Dependency graph | What a package depends on |
| 5 | Reverse dependencies | Who depends on a package |
| 6 | Vulnerable packages | CVEs sorted by recency |
| 7 | CVE details | All properties of a specific CVE |
| 8 | Maintainer analysis | Maintainers by package count |
| 9 | Source package mapping | Source packages with most binaries |
| 10 | Cross-distro comparison | Same package across distributions |
| 11 | VCS repositories | Packages linked to GitHub repos |
| 12 | SLSA provenance | Build attestations from Koji |
| 13 | Data freshness | When each source was last collected |
| 14 | Named graphs | Graph inventory with triple counts |
| 15 | Custom query | Empty cell for your own SPARQL |

### The `query()` Helper

The notebook provides a `query(sparql_str)` function that:
- Prepends all PackageGraph prefixes automatically
- Executes against the configured endpoint
- Returns a pandas DataFrame

```python
# Example: find all packages with "ssl" in the name
query("""
SELECT ?name ?version ?distro WHERE {
  ?p a pkg:BinaryPackage ;
     pkg:packageName ?name ;
     pkg:hasVersion ?v ;
     pkg:partOfDistribution ?d .
  ?v pkg:versionString ?version .
  ?d rdfs:label ?distro .
  FILTER(CONTAINS(LCASE(?name), "ssl"))
}
LIMIT 20
""")
```

All `pkg:`, `sec:`, `vcs:`, `met:`, `slsa:`, `deb:`, `rpm:` prefixes are
available without declaration.

---

## 3. curl / SPARQL

Query Fuseki directly with `curl`. Pipe through `jq` for JSON, or use
tab-separated output for shell scripting.

### Distribution Statistics

```bash
curl -s -H 'Accept: text/tab-separated-values' \
  'http://localhost:3030/packagegraph/sparql' \
  --data-urlencode 'query=
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
    SELECT ?distro (COUNT(DISTINCT ?p) AS ?packages) WHERE {
      ?p a pkg:Package ; pkg:partOfRelease ?r .
      ?r ^pkg:hasRelease/rdfs:label ?distro .
    } GROUP BY ?distro ORDER BY DESC(?packages)' | column -t
```

### Search Packages

```bash
curl -s -H 'Accept: application/sparql-results+json' \
  'http://localhost:3030/packagegraph/sparql' \
  --data-urlencode 'query=
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    SELECT ?name ?version WHERE {
      ?p pkg:packageName ?name ; pkg:hasVersion ?v .
      ?v pkg:versionString ?version .
      FILTER(CONTAINS(LCASE(?name), "openssl"))
    } LIMIT 20' | jq '.results.bindings[] | {name: .name.value, version: .version.value}'
```

### Named Graph Inventory

```bash
curl -s -H 'Accept: text/tab-separated-values' \
  'http://localhost:3030/packagegraph/sparql' \
  --data-urlencode 'query=
    SELECT ?graph (COUNT(*) AS ?triples) WHERE {
      GRAPH ?graph { ?s ?p ?o }
    } GROUP BY ?graph ORDER BY DESC(?triples)' | column -t
```

### Composing with Shell Tools

```bash
# Export vulnerable packages as CSV
curl -s -H 'Accept: application/sparql-results+json' \
  'http://localhost:3030/packagegraph/sparql' \
  --data-urlencode 'query=
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    PREFIX sec: <https://purl.org/packagegraph/ontology/security#>
    SELECT ?pkg ?cve WHERE {
      ?v sec:cveId ?cve ; sec:affectsPackage ?p .
      ?p pkg:packageName ?pkg .
    } LIMIT 1000' \
  | jq -r '["package","cve"], (.results.bindings[] | [.pkg.value, .cve.value]) | @csv' > vulns.csv
```

---

## SPARQL Reference

### Namespace Prefixes

Use these prefixes in YASGUI and raw SPARQL queries. The Jupyter notebook and
CLI canned queries add them automatically.

```sparql
PREFIX pkg:  <https://purl.org/packagegraph/ontology/core#>
PREFIX sec:  <https://purl.org/packagegraph/ontology/security#>
PREFIX vcs:  <https://purl.org/packagegraph/ontology/vcs#>
PREFIX slsa: <https://purl.org/packagegraph/ontology/slsa#>
PREFIX met:  <https://purl.org/packagegraph/ontology/metrics#>
PREFIX deb:  <https://purl.org/packagegraph/ontology/deb#>
PREFIX rpm:  <https://purl.org/packagegraph/ontology/rpm#>
PREFIX prov: <http://www.w3.org/ns/prov#>
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
PREFIX rdf:  <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX xsd:  <http://www.w3.org/2001/XMLSchema#>
```

### Key Classes

| Prefix | Class | Description |
|--------|-------|-------------|
| `pkg:` | `BinaryPackage` | An installable package (e.g., `curl_7.88.1-10_amd64.deb`) |
| `pkg:` | `SourcePackage` | Source package that produces binaries |
| `pkg:` | `Version` | A specific version of a package |
| `pkg:` | `Dependency` | A dependency relationship between packages |
| `pkg:` | `Maintainer` | A package maintainer |
| `pkg:` | `Distribution` | A Linux distribution (Debian, Fedora, etc.) |
| `pkg:` | `License` | A software license (SPDX) |
| `pkg:` | `DataSnapshot` | Metadata about a collection/enrichment run |
| `sec:` | `Vulnerability` | A CVE or security advisory |
| `vcs:` | `Repository` | A source code repository (GitHub) |
| `vcs:` | `Commit` | A git commit |
| `vcs:` | `Release` | A tagged release |
| `met:` | `ProgrammingLanguage` | A programming language |
| `met:` | `CodeMetrics` | Language composition metrics |
| `slsa:` | `ProvenanceAttestation` | SLSA build provenance |

### Common Properties

| Property | Domain → Range | Description |
|----------|---------------|-------------|
| `pkg:packageName` | Package → string | Package name |
| `pkg:hasVersion` | Package → Version | Links to version |
| `pkg:versionString` | Version → string | Version string |
| `pkg:description` | Package → string | Package description |
| `pkg:homepage` | Package → string | Upstream homepage URL |
| `pkg:maintainedBy` | Package → Maintainer | Package maintainer |
| `pkg:builtFromSource` | BinaryPackage → SourcePackage | Source origin |
| `pkg:hasDependency` | Package → Dependency | Dependency link |
| `pkg:dependencyTarget` | Dependency → Package | What is depended on |
| `pkg:dependencyType` | Dependency → string | depends, recommends, etc. |
| `pkg:partOfDistribution` | Package → Distribution | Distribution membership |
| `pkg:equivalentInDistribution` | Package → Package | Cross-distro equivalence |
| `pkg:hasLicense` | Package → License | License claim |
| `sec:cveId` | Vulnerability → string | CVE identifier |
| `sec:severity` | Vulnerability → string | CVSS score or vector |
| `sec:affectsVersion` | Vulnerability → Version | Affected version |
| `vcs:stargazerCount` | Repository → integer | GitHub stars |
| `foaf:name` | Maintainer → string | Person name |

### Query Patterns

**Find a specific package across all distributions:**
```sparql
SELECT ?distro ?version WHERE {
  ?p pkg:packageName "nginx" ;
     pkg:hasVersion ?v ;
     pkg:partOfDistribution ?d .
  ?v pkg:versionString ?version .
  ?d rdfs:label ?distro .
}
```

**Transitive dependencies (2 levels deep):**
```sparql
SELECT ?direct ?indirect WHERE {
  ?p pkg:packageName "bash" ;
     pkg:hasDependency ?d1 .
  ?d1 pkg:dependencyTarget ?t1 .
  ?t1 pkg:packageName ?direct .
  OPTIONAL {
    ?t1 pkg:hasDependency ?d2 .
    ?d2 pkg:dependencyTarget ?t2 .
    ?t2 pkg:packageName ?indirect .
  }
}
LIMIT 100
```

**Packages with CVEs and their fix status:**
```sparql
SELECT ?pkg ?cve ?fixed_ver WHERE {
  ?v_entity sec:cveId ?cve ;
            sec:affectsVersion ?ver .
  ?pkg pkg:hasVersion ?ver ; pkg:packageName ?pkg_name .
  OPTIONAL { ?v_entity sec:fixedInVersion ?fv . ?fv pkg:versionString ?fixed_ver }
}
ORDER BY ?pkg
```

**Named graph contents (data is partitioned by collection source):**
```sparql
SELECT ?graph (COUNT(*) AS ?triples) WHERE {
  GRAPH ?graph { ?s ?p ?o }
}
GROUP BY ?graph
ORDER BY DESC(?triples)
```

---

## Administration

### Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│  OpenShift Cluster (namespace: packagegraph)                         │
│                                                                      │
│  ┌──────────────┐    ┌───────────────────┐                           │
│  │  ETL CronJob │───>│ fuseki (writer)    │  ← replicas=1, has       │
│  │  (collect +  │    │ Port 3030          │    SPARQL UPDATE         │
│  │   enrich)    │    │ PVC: fuseki-tdb2   │                           │
│  └──────────────┘    └───────────────────┘                           │
│                                                                      │
│                      ┌───────────────────┐    ┌──────────────────┐   │
│                      │ fuseki-reader ×N   │<───│ YASGUI / Notebook│   │
│                      │ Port 3030          │    │ / CLI queries    │   │
│                      │ emptyDir (loaded   │    └──────────────────┘   │
│                      │  from Minio)       │                           │
│                      └───────────────────┘                           │
│                              │                                       │
│  ┌────────────────┐          │                                       │
│  │  Minio (S3)    │──────────┘  TDB2 snapshots loaded on startup    │
│  │  (persistent)  │            .nt archives + enricher cache         │
│  └────────────────┘                                                  │
└──────────────────────────────────────────────────────────────────────┘
```

**Writer** (`fuseki`): Single instance with persistent TDB2 on a PVC. Receives
writes from ETL CronJobs via SPARQL UPDATE. Always replicas=1.

**Readers** (`fuseki-reader`): Stateless read-only replicas. Each loads TDB2
from the latest Minio snapshot on startup (init container). Scale to N replicas
for query throughput. No SPARQL UPDATE or Graph Store write endpoints exposed.

### Fuseki Endpoints

Fuseki exposes multiple endpoints per dataset, configured in
`fuseki/config.ttl`:

| Path | Operation | Purpose |
|------|-----------|---------|
| `/packagegraph/sparql` | SPARQL Query | Read-only queries (primary) |
| `/packagegraph/query` | SPARQL Query | Alias for `/sparql` |
| `/packagegraph/update` | SPARQL Update | Write operations (INSERT/DELETE) |
| `/packagegraph/get` | Graph Store (read) | Download named graphs |
| `/packagegraph/data` | Graph Store (r/w) | Upload/download graphs |

**Query interfaces should use `/sparql` or `/query`.** The update and data
endpoints are for the ETL pipeline only.

### Fuseki Configuration

**Config file:** `fuseki/config.ttl`

Key settings:
- `tdb2:location "/data/tdb2"` — TDB2 storage directory (PVC-backed for writer, emptyDir for readers)
- `tdb2:unionDefaultGraph true` — queries against the default graph see all
  named graphs transparently. This means `SELECT ... WHERE { ?s ?p ?o }` returns
  results from all collection graphs without requiring `GRAPH ?g { ... }`.
- `arq:queryTimeout "30000,120000"` — 30-second soft timeout, 120-second hard
  kill. Protects against runaway queries from YASGUI or `query-raw`.

### JVM Memory Tuning

TDB2 relies on memory-mapped files cached by the OS. **Setting the JVM heap
too high steals memory from the OS file cache and tanks performance.** See
[apache/jena#2099](https://github.com/apache/jena/discussions/2099).

Rule of thumb: **set JVM heap to ~1/3 of container memory, leave the rest for
OS file cache.** The `JAVA_OPTIONS` env var controls this:

| Environment | Container Limit | JVM Heap (`-Xmx`) | OS File Cache |
|------------|----------------|-------------------|---------------|
| Base | 4 GiB | 1.5 GiB | ~2.5 GiB |
| Dev | 6 GiB | 2 GiB | ~4 GiB |
| Prod | 4 GiB | 1.5 GiB | ~2.5 GiB |
| Readers | 4 GiB | 1.5 GiB | ~2.5 GiB |

Never set `-Xmx` higher than half the container memory limit. If you increase
the container memory limit, increase the OS cache share, not the JVM heap.

### Horizontal Scaling (Read Replicas)

The read-replica pattern scales query throughput without increasing writer
complexity. Each reader is a stateless Fuseki instance that loads TDB2 from
the latest Minio snapshot on startup.

**When to scale:**
- Single instance handles ~100 concurrent simple queries well
- If response times degrade under load, add readers
- Readers are cheap — each uses ~4 GiB memory and loads in 2-5 minutes

**Scale up:**

```bash
# Scale to 3 read replicas
make scale-readers N=3

# Or directly:
oc scale deployment/fuseki-reader --replicas=3 -n packagegraph
```

**Refresh after ETL run:**

After a collection/enrichment job completes and archives a new TDB2 snapshot
to Minio, trigger a rolling restart so readers pick up the new data:

```bash
make refresh-readers

# Or directly:
oc rollout restart deployment/fuseki-reader -n packagegraph
```

Each reader restarts one at a time (rolling update), so query availability is
maintained during refresh.

**Scale down:**

```bash
make scale-readers N=0   # Stop all readers (writer still available)
```

**Routing queries to readers:**

Point query interfaces at the reader service instead of the writer:

| Service | Internal URL | Use |
|---------|-------------|-----|
| `fuseki` (writer) | `http://fuseki.packagegraph.svc:3030/packagegraph/sparql` | ETL writes + admin queries |
| `fuseki-reader` | `http://fuseki-reader.packagegraph.svc:3030/packagegraph/sparql` | User queries via YASGUI/CLI/notebook |

For external access, create a separate Route pointing to `fuseki-reader`
instead of `fuseki` to direct user traffic to the read pool.

**Data consistency:**

Readers load TDB2 snapshots from Minio. There is a lag between when the writer
loads new data and when readers see it:

1. ETL job writes to writer's Fuseki
2. ETL job archives TDB2 snapshot to Minio
3. `make refresh-readers` triggers rolling restart
4. Each reader loads the new snapshot from Minio (~2-5 min per replica)

During step 4, some readers serve old data while others serve new data. This
is acceptable for the read workload (package metadata doesn't change between
snapshots). If exact consistency is required, drain all readers first:
`make scale-readers N=0`, wait, then `make scale-readers N=3`.

### Named Graphs

Data is partitioned into named graphs by collection source:

| Graph URI | Source |
|-----------|--------|
| `graph/fedora/43` | Fedora 43 (77K packages) |
| `graph/fedora/rawhide` | Fedora Rawhide (76K packages) |
| `graph/debian/trixie` | Debian trixie (69K packages) |
| `graph/opensuse/tumbleweed` | openSUSE Tumbleweed (56K packages) |
| `graph/rhel/9` | RHEL 9 (31K packages) |
| `graph/rhel/10` | RHEL 10 (9K packages) |
| `graph/centos-stream/9` | CentOS Stream 9 |
| `graph/centos-stream/10` | CentOS Stream 10 |
| `graph/homebrew` | Homebrew |
| `graph/gentoo` | Gentoo (dependency-only) |
| `graph/security/osv` | OSV vulnerability data |
| `ontology` | OWL ontology (TBox) |

Graph URIs are prefixed with `https://packagegraph.github.io/`.

### Monitoring

```bash
# Health check
curl -s http://localhost:3030/$/ping

# Dataset statistics (triple count, named graphs)
curl -s http://localhost:3030/$/stats/packagegraph | python3 -m json.tool

# Server metrics
curl -s http://localhost:3030/$/metrics

# Kubernetes pod status
oc get pods -n packagegraph -l app=fuseki

# Fuseki logs
oc logs -n packagegraph deployment/fuseki --tail=50
```

### Data Lifecycle

**Collection** runs as K8s CronJobs (weekly). Each job:
1. Collects package metadata via `pg-collect` (Rust binary)
2. Writes N-Triples to a `.nt` file
3. Archives the `.nt` file to Minio (content-addressed)
4. Drops the old named graph in Fuseki (`pg-collect drop`)
5. Loads the new data into Fuseki (`pg-collect load`)

**Enrichment** runs as separate CronJobs (weekly, after collection):
1. Queries Fuseki for packages to enrich
2. Calls external APIs (GitHub, OSV) with caching
3. Writes enrichment triples to `.nt` file with provenance
4. Loads into Fuseki under `graph/enrichment/*` named graphs

**Manual data operations:**

```bash
# Reload a specific graph from an .nt file
pg-collect drop --graph "https://packagegraph.github.io/graph/debian/trixie" \
  --endpoint http://localhost:3030/packagegraph
pg-collect load /path/to/packages.nt \
  --graph "https://packagegraph.github.io/graph/debian/trixie" \
  --endpoint http://localhost:3030/packagegraph

# Trigger a collection job manually
oc create job --from=cronjob/collect-debian-trixie manual-debian -n packagegraph

# Trigger an enrichment job manually
oc create job --from=cronjob/enrich-license manual-license -n packagegraph
```

### Backup and Recovery

TDB2 data is stored on a PVC (`fuseki-tdb2`). Collection artifacts are
archived to Minio with content-addressed paths.

```bash
# List archived snapshots in Minio
mc ls pgraph/packagegraph/tdb2/

# Download latest snapshot
mc cp pgraph/packagegraph/tdb2/latest /tmp/latest-hash
HASH=$(cat /tmp/latest-hash)
mc cp pgraph/packagegraph/tdb2/$HASH/tdb2.tar.gz /tmp/

# Restore from snapshot (stop Fuseki first)
oc scale deployment/fuseki --replicas=0 -n packagegraph
# ... restore TDB2 from tar ...
oc scale deployment/fuseki --replicas=1 -n packagegraph
```

### Performance Considerations

- **Query timeout:** Configured at 30s soft / 120s hard (`arq:queryTimeout`
  in `config.ttl`). Queries exceeding the soft timeout can be interrupted;
  those exceeding the hard timeout are killed. Users see a timeout error.
  Add `LIMIT` to exploratory queries to stay well under the timeout.
- **JVM heap vs OS cache:** TDB2 performance depends on the OS file cache,
  not the JVM heap. See [JVM Memory Tuning](#jvm-memory-tuning) above.
  **Never increase `-Xmx` without increasing the container memory limit too.**
- **unionDefaultGraph:** Enabled, so all queries see all named graphs. This
  simplifies querying but means cross-graph queries can be slower than
  targeting a specific graph with `GRAPH <uri> { ... }`.
- **Concurrent queries:** Fuseki handles concurrent read queries well. Write
  operations (SPARQL UPDATE, graph loading) acquire exclusive locks. For
  high concurrent query load, use [read replicas](#horizontal-scaling-read-replicas).
- **Scaling:** Single instance handles ~100 concurrent simple queries. Beyond
  that, scale read replicas: `make scale-readers N=3`. Each replica adds
  ~4 GiB memory and independent query capacity.

### Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| Connection refused on port 3030 | Port-forward not running or Fuseki pod not ready | `oc port-forward svc/fuseki 3030:3030 -n packagegraph` |
| Empty results for all queries | No data loaded or wrong endpoint | Check `query-graphs` output; verify endpoint URL |
| "Service Unavailable" from route | Fuseki pod crashing (OOM) | Check `oc logs`; increase memory limit in deployment |
| Slow queries (>10s) | Missing LIMIT or scanning all triples | Add `LIMIT`; use specific graph with `GRAPH <uri>` |
| CORS errors in YASGUI | Fuseki not configured for cross-origin | Port-forward bypasses CORS; for external access, configure Fuseki CORS filter |
| "No such dataset" error | Wrong dataset name in endpoint URL | Endpoint must include `/packagegraph/` (the dataset name) |
| "QueryExecException: timeout" | Query exceeded 30s soft limit | Add `LIMIT`, use `GRAPH <uri>` to target specific graph, simplify query |
| Fuseki OOM killed on startup | JVM heap + OS cache > container limit | Check `JAVA_OPTIONS` — heap should be ~1/3 of container limit |
| Read replicas serve stale data | Readers haven't reloaded after ETL run | `make refresh-readers` — triggers rolling restart to reload from Minio |
