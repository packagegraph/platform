# PackageGraph v0.5.0 Feature Buildout — Design Spec

Created: 2026-04-13
Author: sovereign@local

## Goal

Complete the v0.5.0 release with five features that transform PackageGraph from a single-distro SPARQL endpoint into a multi-distribution package knowledge graph with security, VCS, and build metadata — architected for decade-scale operation.

## Architecture

### Named Graphs via SPARQL Update

Every data source writes to its own named graph in Fuseki via SPARQL UPDATE. Fuseki's `tdb2:unionDefaultGraph true` makes all graphs transparent to queries without explicit `GRAPH` clauses.

```
Named Graphs:
  graph:debian/trixie              — Debian stable binary/source packages
  graph:fedora/43                  — Fedora 43 packages
  graph:fedora/rawhide             — Fedora Rawhide (rolling)
  graph:centos-stream/9            — CentOS Stream 9
  graph:centos-stream/10           — CentOS Stream 10
  graph:opensuse/tumbleweed        — openSUSE Tumbleweed (rolling)
  graph:ontology                   — TBox (class/property definitions)
  graph:security/osv               — CVE/vulnerability data from OSV.dev
  graph:vcs/github                 — GitHub repo metadata and commits
  graph:meta/snapshots             — DataSnapshot provenance metadata
```

### Job Model

Each source is an independent Kubernetes CronJob (or manual Job). Jobs:

1. **Collect** data via pg-collect (Rust) → .nt file
2. **Archive** .nt to Minio (immutable, content-addressed)
3. **Drop** their named graph via `pg-collect drop`
4. **Load** triples into their named graph via `pg-collect load`

Enrichment jobs (security, GitHub) read from the live graph via SPARQL queries and write to their own named graphs. They track processing state in `graph:meta/snapshots` for future incremental operation.

Fuseki stays online during all writes. No TDB2 rebuild. No downtime.

### Rust/Python Split

**Rust (pg-collect) — all performance-critical data paths:**
- `pg-collect debian` — Debian repository collection (existing)
- `pg-collect rpm` — RPM repository collection (existing)
- `pg-collect load` — NEW: parse .nt, batch SPARQL INSERT DATA into named graph
- `pg-collect drop` — NEW: DROP GRAPH + write DataSnapshot metadata

**Python — API-bound enrichment orchestration only:**
- `SecurityEnricher` — queries OSV.dev API (rate-limited to ~2 req/sec)
- `GitHubEnricher` — queries GitHub API (rate-limited to 5000 req/hr)
- Both use `pg-collect load` for the SPARQL write step

Rationale: pg-collect is ~20x faster than Python for data processing. Network-bound enrichers don't benefit from Rust since they spend 99% of time waiting on API responses.

### Long-Term Data Lifecycle

The architecture is designed for decade-scale operation across many distributions.

**Scale projection:** At 10 years with all targets, the system will accumulate 500M-1B+ triples. Rolling distros (Tumbleweed, Rawhide) produce daily churn.

**Immutable snapshots:** Each collection run's .nt file is archived to Minio with a content-addressed hash. Old snapshots are never modified. This creates the warm/cold storage tier for historical queries.

**Tiered storage (future):**

| Tier | Data | Storage | Query |
|------|------|---------|-------|
| Hot | Current release per distro + security + VCS | Fuseki TDB2 (memory-mapped) | Full SPARQL |
| Warm | Last 2 years of snapshots | Compressed .nt.zst on Minio | Load-on-demand |
| Cold | Historical archive (3+ years) | .nt.zst in archive bucket | Batch processing only |

**Delta ingestion (future):** For rolling distros, diff previous .nt against current .nt (pg-collect's deterministic blank node IDs make this possible), emit only INSERT/DELETE deltas. This replaces full graph reload when scale demands it.

**Compaction (future):** Merge daily deltas into weekly summaries after 30 days, monthly after 1 year. Drop version-specific triples for packages no longer in any current release.

**v0.5.0 uses full graph replace (A1)** — acceptable at current scale (~5M triples per source, ~20 min SPARQL load). The named-graph structure and immutable .nt artifacts position for delta ingestion when scale demands it.

## Ontology Alignment

### What aligns well

- **Multi-distro is first-class.** `pkg:Distribution`, `pkg:DistributionRelease`, `pkg:partOfDistribution`/`pkg:partOfRelease` support multiple distros in the same endpoint.
- **Dual typing enables polymorphic queries.** `deb:BinaryPackage rdfs:subClassOf pkg:BinaryPackage` and `rpm:BinaryRPM rdfs:subClassOf pkg:BinaryPackage`. Query `?p a pkg:BinaryPackage` returns all distros; query `?p a rpm:BinaryRPM` returns only RPM packages.
- **Cross-distro equivalence exists.** `pkg:equivalentInDistribution` (Package → Package) links identical packages across distros. Works across named graphs via union default graph.
- **Security links to Version, not Package.** `sec:affectsVersion` range is `pkg:Version`. A CVE affecting openssl 3.0.2 links to version URIs across multiple distro graphs.
- **VCS links through SourcePackage → UpstreamProject → Repository.** Three-hop chain works across named graphs via URI references.

### Ontology additions for v0.5.0

Add `pkg:DataSnapshot` class to `core.ttl` for graph provenance:

```turtle
pkg:DataSnapshot a owl:Class ;
  rdfs:subClassOf prov:Entity ;
  rdfs:label "Data Snapshot" ;
  rdfs:comment "Metadata about a named graph — when it was collected, from what source, whether it is current." .

pkg:snapshotSource a owl:DatatypeProperty ;
  rdfs:domain pkg:DataSnapshot ;
  rdfs:range xsd:string .

pkg:snapshotTimestamp a owl:DatatypeProperty ;
  rdfs:domain pkg:DataSnapshot ;
  rdfs:range xsd:dateTime .

pkg:isCurrent a owl:DatatypeProperty ;
  rdfs:domain pkg:DataSnapshot ;
  rdfs:range xsd:boolean .
```

Each collection/enrichment job writes a `pkg:DataSnapshot` instance into `graph:meta/snapshots`. This provides the provenance layer needed for tiered storage and graph lifecycle management.

### Known gaps (acceptable for v0.5.0)

- **No openSUSE-specific ontology extension.** openSUSE uses RPM format, so `rpm:BinaryRPM` subclassing works. OBS-specific concepts deferred.
- **No unified temporal pattern** for when a triple was asserted (vs when the package was published). `pkg:DataSnapshot` covers graph-level timestamps; triple-level provenance deferred.
- **`pkg:partOfDistribution` has no domain constraint.** Schema hygiene issue — any resource can claim this property. Controlled by job discipline, not ontology enforcement.

## Components

### 1. pg-collect Extensions (Rust)

**`pg-collect load`** — New subcommand:
- Reads N-Triples file line by line
- Batches into SPARQL `INSERT DATA { GRAPH <uri> { ... } }` requests (configurable batch size, default 10K triples)
- HTTP POST to Fuseki's SPARQL Update endpoint
- Reports progress (triples loaded, time, throughput)
- Writes `pkg:DataSnapshot` metadata to `graph:meta/snapshots`

**`pg-collect drop`** — New subcommand:
- Sends `DROP GRAPH <uri>` via SPARQL Update
- Updates `pkg:DataSnapshot` isCurrent=false for the old snapshot

**RPM multi-repo fix** — The current `main.rs` only processes the first `--rpm-repo` spec. Fix to iterate over all specs, invoking `RpmCollector` for each and appending to the same output file.

### 2. Fuseki Configuration

- Enable SPARQL Update endpoint in `config.ttl` (add `fuseki:endpoint [ fuseki:operation fuseki:update ; fuseki:name "update" ]`)
- Update endpoint restricted to cluster-internal access (ClusterIP service, not exposed via Route)
- `tdb2:unionDefaultGraph true` already configured

### 3. Collection Jobs (Kubernetes)

Six collection CronJobs, one per distro source:

| Job | Distro | RPM Repo URL | Schedule |
|-----|--------|-------------|----------|
| `collect-debian-trixie` | debian | http://deb.debian.org/debian | Weekly |
| `collect-fedora-43` | fedora/43 | https://dl.fedoraproject.org/pub/fedora/linux/releases/43/Everything/x86_64/os/ | Weekly |
| `collect-fedora-rawhide` | fedora/rawhide | https://dl.fedoraproject.org/pub/fedora/linux/development/rawhide/Everything/x86_64/os/ | Weekly |
| `collect-centos-stream-9` | centos-stream/9 | https://mirror.stream.centos.org/9-stream/BaseOS/x86_64/os/ | Weekly |
| `collect-centos-stream-10` | centos-stream/10 | https://mirror.stream.centos.org/10-stream/BaseOS/x86_64/os/ | Weekly |
| `collect-opensuse-tw` | opensuse/tumbleweed | https://download.opensuse.org/tumbleweed/repo/oss/ | Weekly |

Each job runs: `pg-collect {debian|rpm} → pg-collect drop → pg-collect load`.

Kustomize structure: base Job template + per-distro patches in overlays.

### 4. Enrichment Jobs

**Security (OSV):**
- Refactor `SecurityEnricher` to query Fuseki's SPARQL endpoint for package names/versions (instead of loading a TTL file into rdflib)
- Call OSV.dev POST `/v1/query` API per package name with ecosystem filter
- Write vulnerability triples to a temp .nt file
- Load into `graph:security/osv` via `pg-collect load`
- Track last-checked timestamp per package in DataSnapshot metadata
- Schedule: Daily

**GitHub VCS:**
- Refactor `GitHubEnricher` to query Fuseki's SPARQL endpoint for packages with GitHub homepage URLs (instead of loading a TTL file into rdflib)
- Call GitHub API for repo metadata (stars, forks, description, default branch) and recent commits
- Write VCS triples to a temp .nt file
- Load into `graph:vcs/github` via `pg-collect load`
- Requires `GITHUB_TOKEN` from dedicated `github-token` Kubernetes Secret
- Schedule: Weekly

### 5. Query Catalog

Static HTML page at `/catalog` in the Fuseki container:
- YASGUI JavaScript library embedded
- Pre-configured endpoint: `/packagegraph/sparql`
- All ontology namespace prefixes pre-loaded
- 20+ named queries organized by category:
  - **Overview:** triple count, class distribution, predicate distribution, graph inventory
  - **Packages:** package detail, architecture breakdown, source-binary mapping
  - **Dependencies:** most-depended-on, reverse deps, version constraints
  - **Maintainers:** top maintainers, packages per maintainer
  - **Cross-Distro:** packages shared across distros, equivalent packages
  - **Security:** CVEs per package, unpatched vulnerabilities, severity breakdown
  - **VCS:** upstream repos by stars, contributor overlap, recent commits

File: `fuseki/catalog.html` copied into `/fuseki/webapp/` during container build.

### 6. CI/CD Pipeline

GitHub Actions for the `platform` repo:

**On push/PR to main (`ci.yml`):**
1. `lint-ontology` — validate TTL files via `uv run python -c "import rdflib; ..."`
2. `test-etl` — `uv run pytest -q`
3. `build-etl` — `podman build` ETL image (verifies Rust compilation)
4. `build-fuseki` — `podman build` Fuseki image

**On version tag `v*` (`release.yml`):**
1. All CI checks above
2. Push images to `ghcr.io/packagegraph/etl:<tag>` and `ghcr.io/packagegraph/fuseki:<tag>`
3. Create GitHub Release

No auto-deploy. Deployment remains manual via `oc apply -k deploy/overlays/dev`.

## Scope

### In Scope
- pg-collect `load` and `drop` subcommands (Rust)
- RPM multi-repo fix in pg-collect
- Fuseki SPARQL Update endpoint enablement
- 6 collection CronJob manifests
- Security enrichment job (OSV.dev)
- GitHub VCS enrichment job
- `pkg:DataSnapshot` ontology addition
- Query catalog HTML page
- GitHub Actions CI pipeline
- `github-token` Kubernetes Secret

### Out of Scope
- Delta ingestion (future — when scale demands)
- Tiered storage implementation (future — warm/cold in Minio)
- Compaction strategy (future)
- Koji build enrichment (deferred — lower priority)
- Repology cross-distro mapping (deferred — needs all distros loaded first)
- openSUSE-specific ontology extension
- Auto-deployment to cluster
- SPARQL Update authentication (cluster-internal restriction sufficient for v0.5.0)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| RPM mirror URLs change or are unavailable | Medium | Job fails for one distro | Use metalink URLs where available; retry logic in pg-collect; jobs are independent so one failure doesn't block others |
| GitHub API rate limit exhaustion during enrichment | High | Partial VCS data | Cache responses with TTL; track last-enriched per repo; use conditional requests (If-Modified-Since) |
| SPARQL UPDATE slower than expected at scale | Low | Longer load times | Batch size tunable; future path to delta ingestion; bulk tdb2.tdbloader available as fallback |
| Fuseki memory pressure with multiple named graphs | Medium | OOM/slow queries | Monitor via /$/stats endpoint; increase memory limit if needed; tiered storage moves old data to cold |
| OSV API returns inconsistent data across ecosystems | Medium | Incorrect vulnerability links | Version matching is approximate in v0.5.0; log mismatches for manual review |
