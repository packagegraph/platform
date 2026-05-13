# CQ Validation Report — Rerun After Query Fixes (2026-04-24)

**Harness:** `etl/scripts/cq-validate.py`
**Frozen CQ commit:** `7dbe46ee9e45f675d1b2c5bd221bde836d765b01` (updated from `e1ad2d5`)
**Endpoint:** west-3 Fuseki (192.168.137.230:30030)
**Date:** 2026-04-24 (rerun after TEMP-01/SEC-08 query fixes + aggregate reclassification)

## Executive Summary

| Result | Count | Previous | Delta |
|--------|------:|----------:|------:|
| **Strict PASS** | **11** | 9 | **+2** |
| PASS_AGGREGATE | 4 | 0 | **+4** |
| **Effective PASS** | **15** | 9 | **+6** |
| MARGINAL | 3 | 7 | -4 |
| EMPTY | 35 | 37 | -2 |
| TIMEOUT | 0 | 0 | -- |
| ERROR | 1 | 1 | -- |
| **Total** | **54** | **54** | |

**Effective pass rate: 27.8%** (15/54), up from 16.7% (9/54).
**Strict pass rate: 20.4%** (11/54), up from 16.7% (9/54).

## Changes Applied

### Query Fixes (Ontology Commit 7dbe46e)

| CQ | Previous | New | Change Applied |
|----|----------|-----|----------------|
| **CQ-TEMP-01** | EMPTY | **PASS** (50 rows) | Fixed query: Added GRAPH clauses for NVD + advisory data. Added `BIND(xsd:dateTime(?advisoryDate) AS ?aDate)` to cast string dates. Dropped ecosystem column (blocked on workstream 5). |
| **CQ-SEC-08** | EMPTY | **PASS** (10 rows) | Fixed query: Changed `sec:hasCWE/sec:cweId` → `sec:cweId` (direct property). Added GRAPH clauses for OSV + NVD. Changed ecosystem filter to URI match (`<.../d/ecosystem/pypi>`) instead of `rdfs:label "PyPI"`. |
| **CQ-XD-01** | ERROR (503) | ERROR (BLOCKED) | Diagnosed and documented: no cross-distro PackageIdentity entities exist. Current `pkg:isVersionOf` uses distro-specific URIs. Requires Repology enrichment or materialized equivalence view. Status updated in frozen CQ doc. |

### Classification Changes (Harness Update)

**4 aggregate queries reclassified from MARGINAL to PASS_AGGREGATE:**

| CQ | Type | Output | Status |
|----|------|--------|--------|
| CQ-PM-04 | Dependency Chain Depth (COUNT) | 15 | PASS_AGGREGATE |
| CQ-DEP-01 | Direct vs Transitive (COUNT) | 12, 12 | PASS_AGGREGATE |
| CQ-DEP-02 | Dependency Type (GROUP BY) | 653,388 | PASS_AGGREGATE |
| CQ-ECO-03 | npm Dependency Depth (AVG) | 1.0 | PASS_AGGREGATE |

**3 aggregate queries remain MARGINAL (empty aggregates):**

| CQ | Type | Output | Status |
|----|------|--------|--------|
| CQ-PM-06 | Architecture Support (COUNT) | 0 | MARGINAL |
| CQ-PM-09 | Package Size Distribution (AVG/MIN/MAX) | 0, 0, 0 | MARGINAL |
| CQ-TEMP-03 | Maintainer Tenure (AVG/MIN/MAX) | 0, 0, 0 | MARGINAL |

The 4 PASS_AGGREGATE CQs are semantically correct — they're aggregate queries over real data that intentionally return single summary rows. The 3 MARGINAL are also aggregate queries, but return zeros because the underlying data doesn't exist yet (e.g., PM-06 returns 0 because no Fedora 43 aarch64 packages are loaded, PM-09 returns 0 because no openSUSE Tumbleweed data exists).

## Domain Breakdown (Effective PASS)

| Domain | Effective PASS | Previous | Delta | Notes |
|--------|---------------:|----------:|------:|-------|
| PM (Package Mgmt) | 2/11 | 1/11 | +1 | PM-04 reclassified |
| SCR (Supply Chain) | 2/9 | 2/9 | -- | No change |
| SEC (Security) | 3/8 | 2/8 | +1 | SEC-08 fixed |
| TEMP (Temporal) | 2/3 | 1/3 | +1 | TEMP-01 fixed |
| LIC (License) | 1/3 | 1/3 | -- | No change |
| DEP (Dependency) | 3/5 | 1/5 | +2 | DEP-01, DEP-02 reclassified |
| XD (Cross-Distro) | 1/5 | 1/5 | -- | XD-01 diagnosed but still blocked |
| ECO (Ecosystem) | 1/3 | 0/3 | +1 | ECO-03 reclassified |
| PROV (Provenance) | 0/4 | 0/4 | -- | No data |
| VCS (Version Control) | 0/2 | 0/2 | -- | No data |
| SET (Package Sets) | 0/1 | 0/1 | -- | No data |

## New PASSes

| CQ | Title | Rows | Root Cause Fixed |
|----|-------|-----:|------------------|
| **CQ-TEMP-01** | Vulnerability Window Analysis | 50 | Missing GRAPH clauses + date type mismatch (string vs dateTime) |
| **CQ-SEC-08** | CWE Classification | 10 | Wrong property path (`hasCWE/cweId` → `cweId`), missing GRAPH clauses, wrong ecosystem filter |

## Reclassified (MARGINAL → PASS_AGGREGATE)

4 aggregate queries with real data now correctly classified as PASS_AGGREGATE. These were never broken — they produce correct single-row summaries but failed the arbitrary `>=5 rows` threshold.

3 aggregate queries over empty data sets remain MARGINAL (PM-06, PM-09, TEMP-03) — this is correct behavior, as they return zeros due to missing underlying data, not because they're aggregate queries.

## Still Blocked

| CQ | Status | Blocker | Next Action |
|----|--------|---------|-------------|
| CQ-XD-01 | ERROR (BLOCKED) | No cross-distro PackageIdentity data | Repology enrichment or materialized equivalence view |
| CQ-SCR-03/04/05 | EMPTY | Missing `maintainerSince` | No authoritative source identified |
| CQ-SCR-06/07/09 | EMPTY | `advisoryForPackage` doesn't resolve to versioned packages | Workstream 5 (advisory resolution) |
| CQ-SEC-01/02/03 | EMPTY | No distro affected-range data | Vulnerability backbone Task 3 |
| CQ-PROV-01-04 | EMPTY | No provenance data | Run npm-provenance + Koji enrichers |
| CQ-VCS-01/02 | EMPTY | No VCS data | Run GitHub enricher at scale |

## Baseline Regression Check

**All 9 baseline PASS CQs from 2026-04-24 remain PASS:**

- ✅ CQ-SCR-01 (Bus Factor)
- ✅ CQ-SCR-02 (Maintainer Overload)
- ✅ CQ-SEC-04 (Advisory Coverage)
- ✅ CQ-SEC-05 (CVSS Version Comparison)
- ✅ CQ-TEMP-02 (Package Obsolescence)
- ✅ CQ-PM-10 (Update Frequency)
- ✅ CQ-LIC-02 (Non-SPDX Licenses)
- ✅ CQ-DEP-05 (Dependency Consistency)
- ✅ CQ-XD-03 (Distro-Specific Packages)

**No regressions.**

## Methodology

- **PASS**: Query returns >=5 rows of data
- **PASS_AGGREGATE**: Single-row aggregate query (COUNT/AVG/MIN/MAX) with semantically correct result
- **Effective PASS**: PASS + PASS_AGGREGATE combined
- **MARGINAL**: Non-aggregate query returning 1-4 rows (may indicate missing data)
- **EMPTY**: Query returns 0 rows
- **ERROR**: Query failed (HTTP error, parse error, BLOCKED status)
- **TIMEOUT**: Query exceeded 310s client timeout

## Impact

This fast-pass improved the CQ pass count by **+6** (from 9 to 15 effective) without any new enrichment jobs:
- **+2 strict PASS** from query fixes (TEMP-01, SEC-08)
- **+4 PASS_AGGREGATE** from correct classification of aggregate queries with real data (PM-04, DEP-01, DEP-02, ECO-03)
- **3 aggregates remain MARGINAL** because they aggregate over empty data sets (PM-06, PM-09, TEMP-03)

Next improvements require enrichment: Repology (XD-01/05), GitHub (VCS-01/02), provenance (PROV-01-04), affected-range (SEC-01/02/03), advisory resolution (SCR-06/07/09), multi-arch data (PM-06: Fedora 43 aarch64, PM-09: openSUSE Tumbleweed).
