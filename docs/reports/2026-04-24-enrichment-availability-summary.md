# Enrichment Data Availability Summary

**Date:** 2026-04-25
**Cluster:** west-3 (MicroShift)
**Goal:** Establish data availability for GitHub, npm-provenance, and Koji enrichment graphs

---

## Executive Summary

**Result: BLOCKED — No enrichment graphs populated**
**Direct CQ Impact: 0 CQ flips**

All three enrichment jobs failed to produce persistent graphs. The root cause is an **image version mismatch**: the deployed `pg-collect` binary on west-3 does not support incremental loading (`--max-repos`/`--load-graph`), forcing jobs into file-only mode where the entire corpus must be processed before any data reaches Fuseki. Both GitHub and Koji hit their job deadlines before completing processing.

**Unblocking action:** Build and deploy the current `pg-collect` source (which supports incremental mode), then re-run enrichment with `--max-repos` and `--load-graph`.

---

## Job Execution Summary

### GitHub Enrichment (`enrich-github`)

- **Job Name:** `enrich-github-1777104593`
- **Target Graph:** `https://packagegraph.github.io/graph/enrichment/github`
- **Packages Found:** 214,474 packages with GitHub homepages
- **Unique Repos Processed:** ~10,200 (before timeout)
- **Cache:** 2,922 pre-existing cache entries from Minio
- **Status:** ❌ Timed out at 121 minutes (2-hour deadline)
- **Rate:** ~93 repos/minute (GitHub API rate-limited at 5000 req/hr)
- **Outcome:** No graph created — file-only mode requires full completion before loading
- **Fix:** With `--max-repos 5000 --load-graph`, the enricher loads progressively within deadline

### npm-provenance Enrichment (`enrich-npm-provenance`)

- **Job Name:** `enrich-npm-prov-1777104427`
- **Target Graph:** `https://packagegraph.github.io/graph/enrichment/npm-provenance`
- **Packages Queried:** 2 npm packages
- **Status:** ✅ Completed (6 seconds)
- **Outcome:** 0 triples — no npm packages on west-3 have SLSA attestations
- **Graph created:** No (0 triples = empty graph not persisted by GSP loader)
- **Note:** This is a data gap, not a bug. West-3 has only 51 npm triples (2 packages).

### Koji Build Metadata Enrichment (`enrich-koji`)

- **Job Name:** `enrich-koji-1777104427`
- **Target Graph:** `https://packagegraph.github.io/graph/enrichment/koji`
- **Packages Found:** 460,569 RPM packages
- **Status:** ❌ Timed out at 62 minutes (1-hour deadline)
- **Rate:** 500ms/package (Koji XML-RPC rate limit)
- **Theoretical Runtime:** 63.9 hours (230,284 seconds)
- **Outcome:** No graph created — timed out during processing phase
- **Additional Issues:** Multiple Koji hub connection errors during processing
- **Fix:** Koji requires batch architecture — daily incremental runs with `--max-repos` scoping

---

## Graph Availability Verification

### Query Endpoint
- **SPARQL Endpoint:** `http://192.168.137.230:30030/packagegraph/sparql`

### Results

| Graph | Exists | Triple Count | Classification |
|-------|--------|--------------|----------------|
| `graph/enrichment/github` | ❌ No | 0 | Timeout before load phase |
| `graph/enrichment/npm-provenance` | ❌ No | 0 | Empty — no attestation data in dataset |
| `graph/enrichment/koji` | ❌ No | 0 | Timeout before load phase |

### Pre-Existing Enrichment Graphs (for reference)

| Graph | Triples |
|-------|---------|
| `graph/enrichment/advisory-dsa` | 202,532 |
| `graph/enrichment/advisory-rhsa` | 61,780 |

---

## Emitted Schema Analysis

**Cannot be completed** — no graphs were populated. Schema analysis requires successful enrichment.

However, from the source code (`src/enrich_github.rs`), the GitHub enricher emits:

| Category | Predicates |
|----------|-----------|
| Repo metadata | `vcs:repository`, `vcs:stars`, `vcs:forks`, `vcs:description`, `vcs:defaultBranch`, `vcs:isFork`, `vcs:isArchived`, `vcs:lastPush` |
| Language metrics | `met:languageName`, `met:languageBytes`, `met:languagePercentage` |
| License | `pkg:declaredLicense`, `spdx:licenseId` |
| Activity | `vcs:openIssues`, `vcs:watchers` |
| Topics | `vcs:topic` |

---

## CQ Answerability Analysis

### Rewrite-First CQs (blocked on query shape, not missing data)

These CQs can be answered once the GitHub enricher data is available AND queries are rewritten:

| CQ ID | Current Query Expects | Enricher Emits | Required Rewrite |
|-------|----------------------|----------------|-----------------|
| VCS-01 | `met:primaryLanguage` | `met:languageName` + `met:languageBytes` | Change predicate names, add percentage calculation |
| PROV-01 | `pkg:wasBuiltBy` / `prov:used` linkage | `pkg:BuildActivity` type (Koji) | Align property paths to Koji output |
| PROV-02 | `slsa:hasProvenance` / `slsa:slsaLevel` | `slsa:hasAttestation` / `slsa:attestationCount` | Align predicates (but npm data is empty) |
| PROV-03 | `prov:wasAssociatedWith` agent entity | `slsa:buildOwner` (string literal) | Change to string match instead of entity join |

### Missing-Emitter CQs (blocked on missing enricher functionality)

| CQ ID | Missing Data | Why Blocked |
|-------|--------------|-------------|
| VCS-02 | `pkg:derivedFromCommit`, `vcs:commitHash`, `vcs:onBranch` | No enricher produces commit-level data — GitHub enricher is repo-level only |
| PROV-04 | `pkg:Contributor`, `pkg:contributesTo` | No enricher produces contributor role triples |

---

## Root Cause: Image Version Mismatch

### What happened

The `pg-collect` binary deployed on west-3 (`ghcr.io/packagegraph/etl:latest`, image ID `b5e0beec08a9e`) is an **older version** that predates the GitHub enricher incrementalization work. It supports only:

```
pg-collect enrich-github --endpoint <ENDPOINT> --output <OUTPUT> --cache-dir <CACHE_DIR>
```

The current source code (in `platform/etl/pg-collect/`) adds incremental mode:

```
pg-collect enrich-github --endpoint <ENDPOINT> --output <OUTPUT> --cache-dir <CACHE_DIR> \
  --max-repos 5000 --load-graph <GRAPH_URI>
```

In file-only mode, the enricher must:
1. Process ALL repos → write to file
2. Drop existing graph
3. Load file to Fuseki via GSP

Step 1 takes longer than the job deadline for both GitHub (2h) and Koji (1h), so steps 2-3 never execute.

In incremental mode, the enricher:
1. Processes repos up to `--max-repos` limit
2. Loads progressively to Fuseki during processing
3. Syncs cache to Minio
4. Next run picks up where it left off (cache-aware deduplication)

### Fix Required

1. **Build** current `pg-collect` from source: `cd platform/etl/pg-collect && cargo build --release`
2. **Deploy** to west-3 via `podman save/load` workflow (ghcr.io unreachable from cluster)
3. **Re-apply** CronJob manifests with `--max-repos` and `--load-graph` flags
4. **Re-run** enrichment jobs

---

## Additional Operational Findings

### CronJob Configuration Drift

| Issue | Source Manifest | Deployed CronJob |
|-------|----------------|-----------------|
| Image tag | `:latest` | `:v-incr-test` |
| Pull policy | `IfNotPresent` | `Never` |
| GitHub args | `--max-repos 5000 --load-graph` | (not supported by deployed binary) |

Source manifests were applied during this execution to fix tag/policy, but the underlying binary version remains old.

### Minio Cache Performance

- **GitHub:** 252 MiB cache (2,922 entries) synced in 2 seconds (87-100 MiB/s)
- **Koji:** 0 cache entries (first run for this enricher)
- **Cache sync:** Bidirectional — downloads before enrichment, uploads after

### Koji Rate Limiting

Koji XML-RPC endpoint (`https://koji.fedoraproject.org/kojihub`) rate-limits at ~2 req/sec. With 460K packages:
- Full enumeration: 63.9 hours
- Per 1-hour job run: ~7,200 packages
- Multiple connection errors observed during processing

---

## Next Steps (Priority Order)

1. **Deploy updated pg-collect binary** — Build from current source and deploy via podman save/load
2. **Re-run GitHub enrichment** with `--max-repos 5000 --load-graph` — should complete within 2h deadline
3. **Re-run Koji enrichment** with `--max-repos 5000 --load-graph` — requires multiple daily runs
4. **Verify graphs** — Repeat SPARQL verification queries
5. **Query alignment plan** — Once graphs exist, create plan to rewrite frozen CQs
6. **VCS-02 / PROV-04** — Evaluate whether to build commit-level and contributor enrichers

---

## Appendix: Verification Queries

```sparql
-- Graph existence
ASK { GRAPH <https://packagegraph.github.io/graph/enrichment/github> { ?s ?p ?o } }

-- Triple count
SELECT (COUNT(*) AS ?triples)
WHERE { GRAPH <https://packagegraph.github.io/graph/enrichment/github> { ?s ?p ?o } }

-- Predicate inventory
SELECT ?p (COUNT(*) AS ?count)
WHERE { GRAPH <https://packagegraph.github.io/graph/enrichment/github> { ?s ?p ?o } }
GROUP BY ?p ORDER BY DESC(?count)

-- Sample triples
SELECT ?s ?p ?o
WHERE { GRAPH <https://packagegraph.github.io/graph/enrichment/github> { ?s ?p ?o } }
LIMIT 5
```
