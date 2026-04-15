# Deploy Fixes and Enrichment Implementation Plan

Created: 2026-04-14
Author: sovereign@local
Status: VERIFIED
Approved: Yes
Iterations: 0
Worktree: No
Type: Feature

## Summary

**Goal:** Fix 7 broken collector manifests, add enricher deadlines, deploy Fuseki text index for substring search, recollect Fedora 43 + Debian Trixie with Provides/maintainer data, run security enricher, and enrich RHEL/CentOS upstream repos.

**Architecture:** Manifest fixes + Fuseki config update + sequential cluster job execution.

**Tech Stack:** Kubernetes manifests (YAML), Fuseki text index (Jena text), `oc` CLI for job execution.

## Scope

### In Scope

- Fix 7 broken manifest shell scripts (double `fi` from COLLECTOR_FULL_RELOAD insertion)
- Add `activeDeadlineSeconds` to all enricher jobs
- Deploy Fuseki text index config for `packageName`, `description`, `upstreamPackageName`
- Recollect Fedora 43 and Debian Trixie with new image
- Run security enricher (`enrich-security-system`)
- Run GitHub VCS enricher for RHEL/CentOS upstream repo coverage

### Out of Scope

- Fedora Rawhide recollection (same pattern as 43, can follow)
- Language ecosystem collectors (cargo/pypi/gomod with spidering — separate work)
- Flatpak/Snap seed command implementation

## Autonomous Decisions

- **Text index fields:** `packageName`, `description`, `upstreamPackageName` — these are the most useful for user queries. `homepage` excluded (URLs aren't good substring targets).
- **Enricher deadlines:** 3600s (1 hour) for enrichers that process 80K+ items. Security enricher gets 7200s (2 hours) since it hits external APIs.

## Context for Implementer

- **Broken manifests:** The `COLLECTOR_FULL_RELOAD` conditional was inserted by a Python script that doubled the `if` and `fi`. The fix is to replace the mangled line with a clean conditional.
- **Fuseki text index:** Requires adding `text:TextDataset` wrapping the TDB2 dataset in config.ttl, plus `text:EntityMap` for the indexed properties. The text index is built on first query.
- **Collector jobs:** Run sequentially with `oc create job --from=cronjob/X` and `oc wait --for=condition=complete`.
- **Key files:**
  - `deploy/overlays/dev/jobs/collect-*.yaml` — collector manifests
  - `deploy/overlays/dev/jobs/enrich-*.yaml` — enricher manifests
  - `fuseki/config.ttl` — Fuseki server config
  - `deploy/base/fuseki/configmap.yaml` — Fuseki config deployed to cluster

## Assumptions

- Fuseki is healthy and responsive — supported by ping returning timestamp — all tasks depend on this
- The new ETL image (with Provides parsing, maintainer labels) is already pushed to ghcr.io — Tasks 4-6 depend on this
- RHEL 9/10 data is already loaded in Fuseki — Task 6 depends on this

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Text index config change requires Fuseki restart | High | Low | Fuseki has Recreate strategy — brief downtime acceptable |
| Fedora 43 GSP upload timeout at 1800s | Low | Medium | Incremental append (no DROP) + retry logic already in place |
| Security enricher hits API rate limits | Medium | Low | Enricher has built-in backoff; 7200s deadline gives plenty of time |

## Goal Verification

### Truths

1. All 7 broken manifests pass `oc apply` without errors
2. Fuseki text index allows `text:query` SPARQL queries for package name search
3. Fedora 43 graph has `pkg:upstreamEcosystem` and `pkg:upstreamPackageName` triples
4. Debian Trixie graph has updated maintainer labels
5. Security enrichment graph has vulnerability data
6. RHEL/CentOS packages have upstream repo coverage via enrichment

### Artifacts

- 7 fixed collector manifests
- Updated `fuseki/config.ttl` and `configmap.yaml` with text index
- Enricher manifests with `activeDeadlineSeconds`

## Progress Tracking

- [x] Task 1: Fix broken collector manifests
- [x] Task 2: Add enricher deadlines
- [x] Task 3: Deploy Fuseki text index
- [x] Task 4: Recollect Fedora 43 and Debian Trixie
- [x] Task 5: Run security enricher
- [x] Task 6: RHEL/CentOS upstream enrichment

**Total Tasks:** 6 | **Completed:** 6 | **Remaining:** 0

## Implementation Tasks

### Task 1: Fix Broken Collector Manifests

**Objective:** Fix the 7 manifests with malformed `COLLECTOR_FULL_RELOAD` shell syntax.
**Dependencies:** None

**Files:**

- Modify: `deploy/overlays/dev/jobs/collect-conda.yaml`
- Modify: `deploy/overlays/dev/jobs/collect-void.yaml`
- Modify: `deploy/overlays/dev/jobs/collect-flatpak.yaml`
- Modify: `deploy/overlays/dev/jobs/collect-snap.yaml`
- Modify: `deploy/overlays/dev/jobs/collect-npm.yaml`
- Modify: `deploy/overlays/dev/jobs/collect-pypi.yaml`
- Modify: `deploy/overlays/dev/jobs/collect-gomod.yaml`

**Key Decisions / Notes:**

- Replace `if [ ... ]; then if [ ... ]; then pg-collect drop ... ; fi ; fi && \n                  fi` with clean:
  ```
  if [ "${COLLECTOR_FULL_RELOAD:-0}" = "1" ]; then
    pg-collect drop --graph "$GRAPH_URI" --endpoint "$FUSEKI_ENDPOINT"
  fi
  pg-collect load ...
  ```

**Definition of Done:**

- [ ] All 7 manifests have valid shell syntax
- [ ] `oc apply -k deploy/overlays/dev` succeeds
- [ ] No `fi` syntax errors in any manifest

**Verify:**

- `oc kustomize deploy/overlays/dev | grep -c 'kind:' && echo "resources valid"`

---

### Task 2: Add Enricher Deadlines

**Objective:** Add `activeDeadlineSeconds` to all enricher CronJobs to prevent zombie jobs.
**Dependencies:** None

**Files:**

- Modify: `deploy/overlays/dev/jobs/enrich-license.yaml`
- Modify: `deploy/overlays/dev/jobs/enrich-metrics.yaml`
- Modify: `deploy/overlays/dev/jobs/enrich-vcs-activity.yaml`
- Modify: `deploy/overlays/dev/jobs/enrich-github-vcs.yaml`
- Modify: `deploy/overlays/dev/jobs/enrich-security-system.yaml`
- Modify: `deploy/overlays/dev/jobs/enrich-security-lang.yaml`
- Modify: `deploy/overlays/dev/jobs/enrich-advisory.yaml`
- Modify: `deploy/overlays/dev/jobs/enrich-koji.yaml`
- Modify: `deploy/overlays/dev/jobs/enrich-npm-provenance.yaml`

**Key Decisions / Notes:**

- GitHub VCS enricher: 7200s (processes 22K+ repos with API calls)
- License/Metrics/VCS-Activity: 3600s (process 80K items from cache)
- Security enrichers: 7200s (external API calls with rate limiting)
- Advisory/Koji/NPM-provenance: 3600s

**Definition of Done:**

- [ ] All enricher manifests have `activeDeadlineSeconds`
- [ ] `oc apply` succeeds

**Verify:**

- `grep activeDeadlineSeconds deploy/overlays/dev/jobs/enrich-*.yaml | wc -l` should be 9

---

### Task 3: Deploy Fuseki Text Index

**Objective:** Add text index configuration to Fuseki for substring package name search via SPARQL `text:query`.
**Dependencies:** None

**Files:**

- Modify: `fuseki/config.ttl`
- Modify: `deploy/base/fuseki/configmap.yaml`

**Key Decisions / Notes:**

- Add `jena-text` dependency — Fuseki 5.3.0 includes it in the full JAR
- Wrap TDB2 dataset in `text:TextDataset` with Lucene backend
- Index: `pkg:packageName`, `pkg:description`, `pkg:upstreamPackageName`
- Text index stored at `/data/text-index` (inside TDB2 PVC)
- Example SPARQL: `?s text:query (pkg:packageName "openssl") .`
- Requires Fuseki restart after config change

**Definition of Done:**

- [ ] config.ttl has text index configuration
- [ ] configmap.yaml matches config.ttl
- [ ] After restart, `text:query` SPARQL returns results

**Verify:**

- SPARQL query with `text:query` returns package results

---

### Task 4: Recollect Fedora 43 and Debian Trixie

**Objective:** Re-run Fedora 43 and Debian Trixie collectors with the latest image to populate upstream ecosystem Provides and maintainer labels.
**Dependencies:** Tasks 1, 3 (manifests fixed, Fuseki restarted with text index)

**Files:** None (cluster operations)

**Key Decisions / Notes:**

- Run sequentially to avoid Fuseki contention
- Use incremental append (no DROP) — new data adds to existing graph
- Fedora 43 produces ~8M triples, needs 1800s deadline
- Debian Trixie produces ~5.8M triples

**Definition of Done:**

- [ ] Fedora 43 graph has `pkg:upstreamEcosystem` triples
- [ ] Debian Trixie graph has updated data
- [ ] Both jobs complete without error

**Verify:**

- SPARQL query for `upstreamEcosystem` in Fedora 43 graph returns results

---

### Task 5: Run Security Enricher

**Objective:** Execute `enrich-security-system` to populate vulnerability data.
**Dependencies:** Task 4 (Fedora data loaded)

**Files:** None (cluster operations)

**Key Decisions / Notes:**

- Run as background job — may take 1-2 hours
- The security enricher queries OSV.dev API for vulnerability data
- Results go to `graph/enrichment/security-system`

**Definition of Done:**

- [ ] Security enricher job completes (or is running)
- [ ] Security graph has vulnerability triples

**Verify:**

- `oc get job enrich-security-system-* -n packagegraph` shows Complete or Running

---

### Task 6: RHEL/CentOS Upstream Enrichment

**Objective:** Run GitHub VCS enricher scoped to find upstream repos for RHEL/CentOS packages.
**Dependencies:** Task 4 (Fedora provides data helps seed)

**Files:** None (cluster operations)

**Key Decisions / Notes:**

- The GitHub VCS enricher already completed one full run with 21K repos
- RHEL packages may have homepage URLs pointing to GitHub repos not yet in the enrichment graph
- Use `enrichedAt` freshness to only process new repos
- Can run incrementally — appends to existing enrichment graph

**Definition of Done:**

- [ ] Enricher job launched successfully
- [ ] RHEL packages with GitHub homepages have upstream repo data

**Verify:**

- SPARQL query for RHEL packages with VCS Repository triples returns results

## Deferred Ideas

- Fedora Rawhide recollection (same pattern, do after 43 stabilizes)
- Language ecosystem collectors with spidering (cargo/pypi/gomod — pending Provides data)
- Flatpak/Snap seed command implementation
