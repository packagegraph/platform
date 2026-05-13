# CQ Validation Report — 2026-04-24

**Harness:** `etl/scripts/cq-validate.py`
**Frozen CQ commit:** `e1ad2d514d053280987f4ad37a0e9268ee3edde9`
**Endpoint:** west-3 Fuseki (192.168.137.230:30030)
**Dataset:** TDB2 ~14 GiB, ~40 graphs, ETL image `498ade052e85`

## Executive Summary

| Result | Count | Previous (Apr 23) | Delta |
|--------|------:|-------------------:|------:|
| PASS (>=5 rows) | **9** | 7 | **+2** |
| MARGINAL (1-4 rows) | 7 | 8 | -1 |
| EMPTY | 37 | 38 | -1 |
| TIMEOUT | 0 | 0 | -- |
| ERROR | 1 | 1 | -- |
| **Total** | **54** | **54** | |

**Overall pass rate: 16.7%** (9/54). Up from 13.0% (7/54).

## What Changed Since Apr 23

### New PASSes (+2)

| CQ | Title | Result | Rows | Cause |
|----|-------|--------|-----:|-------|
| CQ-SEC-05 | CVSS Version Comparison | PASS | 50 | NVD enrichment now provides 48,729 CVEs with CVSS scores across multiple versions (v2.0, v3.1, v4.0) |
| CQ-TEMP-02 | Package Obsolescence Between Releases | PASS | 500 | Fedora 42/43/44 multi-release data now fully loaded; frozen query updated to match available releases |

### NVD Enrichment Impact

The NVD feed enrichment completed today (enricher redeployed with `format_cve_ntriples()` refactor and rate-limit fix):

- **50,750** advisory-linked CVEs enriched (out of 51,749 in advisory graphs)
- **647,009** triples loaded into `graph/cve/nvd`
- publishedDate: 50,750 (100% of matched CVEs)
- CVSS scores: 48,729 (96%)
- CWE mappings: 41,229 (81%)

This directly enabled CQ-SEC-05 (CVSS version comparison). CQ-SEC-04 (advisory coverage) was already PASS.

## Domain Breakdown

### Passing Domains

| Domain | Pass Rate | CQs | Notes |
|--------|-----------|------|-------|
| SCR (Supply Chain Risk) | 2/9 | SCR-01, SCR-02 | Bus factor + maintainer overload. Remaining 7 blocked on missing data sources. |
| SEC (Security) | 2/8 | SEC-04, SEC-05 | Advisory coverage + CVSS comparison. SEC-01/02/03 need affected-range data. |
| TEMP (Temporal) | 1/3 | TEMP-02 | Cross-release obsolescence. TEMP-01 blocked (see below). |
| LIC (License) | 1/3 | LIC-02 | Non-SPDX licenses detected. LIC-01 needs SPDX entity enrichment. |
| PM (Package Mgmt) | 1/11 | PM-10 | Update frequency via lastReleaseDate. Lowest pass rate domain. |
| DEP (Dependency) | 1/5 | DEP-05 | Consistency checks. DEP-01/02 marginal. |
| XD (Cross-Distro) | 1/5 | XD-03 | Distro-specific packages. XD-01 errors (503 timeout). |

### Zero-Pass Domains

| Domain | CQs | Blocker |
|--------|------|---------|
| PROV (Provenance) | 0/4 | No SLSA/build provenance data yet. Needs npm-provenance enricher production run. |
| VCS (Version Control) | 0/2 | No GitHub/VCS enrichment loaded. Enricher exists but not yet run at scale. |
| ECO (Ecosystem) | 0/3 | ECO-01/02 need distro-specific metadata not yet emitted. ECO-03 marginal (npm). |
| SET (Package Sets) | 0/1 | No package set/group metadata emitted by collectors. |

## Detailed Results

### PASS (9)

| CQ | Title | Rows | Domain |
|----|-------|-----:|--------|
| CQ-SCR-01 | Bus Factor — Single-Maintainer Packages | 5 | SCR |
| CQ-SCR-02 | Maintainer Overload — Most Packages Per Person | 50 | SCR |
| CQ-SEC-04 | Security Advisory Coverage | 20 | SEC |
| CQ-SEC-05 | CVSS Version Comparison | 50 | SEC |
| CQ-TEMP-02 | Package Obsolescence Between Releases | 500 | TEMP |
| CQ-PM-10 | Package Update Frequency | 20 | PM |
| CQ-LIC-02 | Packages with Non-SPDX Licenses | 27 | LIC |
| CQ-DEP-05 | Dependency Consistency Check | 50 | DEP |
| CQ-XD-03 | Distribution-Specific Packages | 500 | XD |

### MARGINAL (7)

| CQ | Title | Rows | Issue |
|----|-------|-----:|-------|
| CQ-PM-04 | Dependency Chain Depth | 1 | Property path query returns single aggregate row |
| CQ-PM-06 | Architecture Support Coverage | 1 | Returns 1 architecture summary row |
| CQ-PM-09 | Package Size Distribution | 1 | Single aggregate row |
| CQ-DEP-01 | Direct vs Transitive Dependencies | 1 | Single comparison row |
| CQ-DEP-02 | Dependency Type Distribution | 1 | Single type count |
| CQ-ECO-03 | npm Dependency Depth | 1 | Single aggregate |
| CQ-TEMP-03 | Maintainer Tenure Analysis | 1 | Single aggregate |

Most MARGINAL results return exactly 1 row — these are aggregate queries that produce correct results but hit the `<5 rows` threshold. They are semantically correct.

### ERROR (1)

| CQ | Title | Error | Root Cause |
|----|-------|-------|------------|
| CQ-XD-01 | Equivalent Packages Across Distributions | HTTP 503 | Query joins across PackageIdentity entities spanning all distribution graphs. Exceeds Fuseki 5-minute query timeout. Needs GRAPH-scoped rewrite or materialized equivalence view. |

### Key Blocked CQs

| CQ | Title | Blocker | Unblocking Action |
|----|-------|---------|-------------------|
| CQ-TEMP-01 | Vulnerability Window Analysis | Frozen query uses no GRAPH clauses; data lives in named graphs | Rewrite with explicit `GRAPH <graph/cve/nvd>` and `GRAPH ?g` clauses (confirmed working manually in prior session) |
| CQ-SCR-03 | Orphan Risk — Stale Maintainership | No `maintainerSince` data | No authoritative source identified |
| CQ-SCR-06/07/09 | Patch Lag / Unpatched / MTTR | `advisoryForPackage` doesn't resolve to concrete versioned packages | Workstream 5: advisory-to-package resolution |
| CQ-SEC-01/02/03 | Affected Packages / Unpatched / Version Range | No distro-capable affected-range data | Task 3 from vulnerability backbone plan (distro ranges) |
| CQ-SEC-08 | CWE Classification | Frozen query may not match current CWE triple shape | Verify query against `sec:hasCWE` / `sec:cweId` pattern from NVD enricher |
| CQ-PROV-01-04 | Build provenance chain | No provenance data loaded | Run npm-provenance enricher, then Koji enricher |
| CQ-VCS-01/02 | Repository language / commit tracing | No VCS enrichment data | Run GitHub enricher at scale |
| CQ-XD-01 | Cross-distro equivalence | Query timeout (503) | GRAPH-scope the query or pre-materialize equivalence |

## Near-Term Opportunities

These CQs could flip to PASS with targeted work:

1. **CQ-TEMP-01** (Vulnerability Window) — Data exists (51K CVEs with publishedDate + advisories with advisoryDate). Frozen query needs GRAPH clause rewrite. Estimated effort: query-only change in ontology repo.

2. **CQ-SEC-08** (CWE Classification) — NVD enricher now emits 41,229 CWE mappings. If the frozen query matches the `sec:hasCWE`/`sec:cweId` pattern, this should work. Needs query verification.

3. **CQ-XD-05** (Repology Equivalence) — Repology enricher exists and was running. If data is loaded, needs query verification against actual triple shape.

4. **CQ-VCS-01/02** (Repository data) — GitHub enricher exists. Running it would populate VCS graph and potentially flip both.

## Trend

```
Date       PASS  MARG  EMPTY  ERR  TIMEOUT  Note
2026-04-23    7     8     38    1       0    Post-deriver rebuild, lastReleaseDate
2026-04-24    9     7     37    1       0    NVD refresh (50K CVEs), rate-limit fix
```

## Methodology Notes

- **PASS**: Query returns >=5 rows of data
- **MARGINAL**: Query returns 1-4 rows (may be aggregate queries with correct single-row results)
- **EMPTY**: Query returns 0 rows (data or schema gap)
- **ERROR**: Query failed (HTTP error, parse error)
- **TIMEOUT**: Query exceeded 310s client timeout
- Queries are frozen to ontology commit `e1ad2d5` — working-tree changes are not tested
- LIMIT 5 auto-appended to queries without explicit LIMIT
