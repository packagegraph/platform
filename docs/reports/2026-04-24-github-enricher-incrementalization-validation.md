# GitHub Enricher Incrementalization Validation Report

**Date:** 2026-04-24
**Plan:** `docs/plans/2026-04-24-github-enricher-incrementalization.md`

## Implementation Status: COMPLETE (Deployment Blocked)

All code changes implemented and tested. Deployment to west-3 blocked by CRI-O image store isolation — functional validation deferred to next deployment cycle or CI/CD pipeline.

## Completed Work

### Task 1: Ranked Candidate Selection Query

**File:** `etl/pg-collect/src/sparql.rs`

**Added:** `query_github_candidates(graph_uri, limit) -> Vec<(owner, repo, package_count)>`

**Query logic:**
- Joins packages with GitHub homepages
- Normalizes to owner/repo via SPARQL string operations
- Counts packages per repo (`COUNT(DISTINCT ?pkg)`)
- LEFT JOIN against enrichment graph to detect already-enriched repos
- Orders by: `alreadyEnriched ASC, packageCount DESC, owner ASC, repo ASC`
- Returns bounded result set via LIMIT

**Tests:** 2 unit tests pass
- `test_query_github_candidates_ranking` — verifies unenriched repos rank before enriched
- `test_query_github_candidates_deterministic_ordering` — verifies lexicographic tiebreaker

### Task 2: Bounded Batch Mode with Internal GSP Loading

**Files Modified:**
- `etl/pg-collect/src/main.rs` — Added `--max-repos` and `--load-graph` CLI flags to `EnrichGithub`
- `etl/pg-collect/src/enrich_github.rs` — Added `enrich_incremental()` method
- `etl/pg-collect/src/sparql.rs` — Added `gsp_post_file()` public method (wraps existing `gsp_upload()`)

**CLI behavior:**
- When `--load-graph` present but `--max-repos` absent → exits with error message
- When both present → calls `enrich_incremental()`
- When `--load-graph` absent → calls original `enrich()` (backward compatible, file-only mode)

**Incremental flow:**
1. Query `query_github_candidates(graph_uri, max_repos)` for ranked batch
2. Process each repo via existing `process_repo()` logic
3. Flush NTriplesWriter to file
4. Call `gsp_post_file()` to load batch via GSP POST (append mode)
5. Log completion: repos processed, triples emitted

**Error handling:**
- GSP POST failure → returns `Err()`, stops enricher, leaves batch file on disk for debugging
- Retry logic: GSP POST retries 3 times on server errors (inherited from `gsp_upload()`)

**Tests:** 2 new unit tests pass
- `test_enrich_incremental_respects_max_repos` — verifies bounded processing stops at candidate query limit
- `test_gsp_post_file_propagates_http_errors` — verifies HTTP errors propagate correctly (expects 4 requests due to retry logic)

**Contributor link deferred:** The incremental path does NOT emit `contributesTo`/`hasContributor` links. The ranked query returns only `(owner, repo, package_count)` — not per-package/maintainer bindings. Repo metadata, language composition, and license are the priority for CQ recovery.

### Task 3: CronJob Update

**File:** `deploy/overlays/dev/jobs/enrich-github.yaml`

**Changes:**
- Command: `pg-collect enrich-github --max-repos 5000 --load-graph https://packagegraph.github.io/graph/enrichment/github`
- Removed shell-level `pg-collect drop` and `pg-collect load` commands
- Kept Minio cache warm/sync before/after enrichment
- Kept `unset MINIO_*` pattern for local-only cache during enrichment
- Updated `imagePullPolicy: Never` (west-3 has no ghcr.io access, images manually deployed)
- Still writes to `/tmp/enrichment/github.nt` as diagnostic artifact

**Validation:** `oc apply --dry-run=client` succeeded — manifest is syntactically valid

## Deployment Blocker

**Issue:** CRI-O (OpenShift container runtime) and podman maintain separate image stores. The new image (b5e0beec08a9e) was built on berstuk and loaded into host podman, but CRI-O cannot access podman's image store.

**Attempted:**
- `podman save | ssh k8s1 podman load` — loaded into host podman, not CRI-O
- `sudo podman load` — same issue (podman != CRI-O)
- `skopeo copy containers-storage:ghcr.io/packagegraph/etl:latest containers-storage:ghcr.io/packagegraph/etl:v-incr-test` — succeeded, image visible in `crictl images`, but pods still get `ErrImageNeverPull`
- Image digest pinning → same result

**Root cause:** CRI-O's image store location or configuration may differ from what skopeo/podman target. Or cached image manifests are interfering.

**Workarounds for production deployment:**
1. Use a local image registry (e.g., deploy `registry:2` to the cluster, push there, pull from localhost:5000)
2. Deploy via CI/CD pipeline that has working image distribution
3. Manually debug CRI-O image store configuration on west-3 node

## Test Suite Results

```
cargo test --lib

test result: ok. 250 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 39.79s
```

All tests pass, including the 4 new tests for incremental functionality.

## Code Verification

**Build status:** Clean (no errors, 34 warnings for unused variables in unrelated modules)

**Affected files:**
- `etl/pg-collect/src/sparql.rs` — +85 lines (1 new method, 2 tests)
- `etl/pg-collect/src/enrich_github.rs` — +47 lines (1 new method, 1 test)
- `etl/pg-collect/src/main.rs` — +20 lines (2 CLI args, validation logic, mode dispatch)
- `deploy/overlays/dev/jobs/enrich-github.yaml` — removed drop+load, added --max-repos/--load-graph

## Next Steps for Functional Validation

When image deployment is resolved:

1. Launch test job: `oc create job --from=cronjob/enrich-github enrich-github-incr-test-1 -n packagegraph`
2. Verify graph existence: `ASK { GRAPH <https://packagegraph.github.io/graph/enrichment/github> { ?s ?p ?o } }` → should return `true`
3. Check triple count: `SELECT (COUNT(*) AS ?triples) WHERE { GRAPH <...> { ?s ?p ?o } }` → should be > 0
4. Predicate inventory: `SELECT ?p (COUNT(*) AS ?count) WHERE { GRAPH <...> { ?s ?p ?o } } GROUP BY ?p` → verify `vcs:repositoryURL`, `vcs:starCount`, `met:languageName`, `pkg:licenseName` appear
5. Launch second job, verify it enriches different repos (not the same initial 5000)

**Expected first-run outcome:**
- 5000 repos processed (~33 minutes at 200ms rate limit)
- ~50K-100K triples emitted (10-20 triples per repo average)
- Graph loaded via GSP POST
- Cache synced to Minio

**Expected second-run outcome:**
- Different 5000 repos selected (ranked query deprioritizes already-enriched)
- Additional triples appended to graph
- No DROP occurs — prior data preserved
