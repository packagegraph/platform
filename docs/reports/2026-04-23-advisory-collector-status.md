# Advisory Collector Updates — CQ Validation Impact Report

**Date:** 2026-04-23
**Author:** Platform team
**Audience:** Ontology team, management
**Cluster:** west-3 (production)

---

## Summary

Three advisory collector features shipped today. The most impactful — RPM updateinfo integration — resolves the core problem that blocked advisory CQ validation: linking security advisories to concrete versioned packages. **1,822 advisory→package links** are now live in the Fedora 43 graph, with zero SPARQL resolution overhead. The advisory-side join for SCR-06, SCR-07, and SCR-09 is now populated for RPM-family ecosystems.

---

## Security Data Inventory (west-3, live)

### Advisory Sources

| Source | Graph | Advisories | Package Links | CVE Refs | Status |
|--------|-------|-----------|---------------|----------|--------|
| **RPM updateinfo** (F43) | `graph/fedora/43` | 280 | **1,822** | 661 | **NEW — functional** |
| **Bodhi RSS** (F43) | `graph/advisory/fedora/43` | 652 | 297 | 0 | NEW — limited resolution |
| **GLSA** (Gentoo) | `graph/advisory/gentoo` | 50 | 0 | 279 | NEW — no package graph |
| RHSA (existing) | `graph/enrichment/advisory-rhsa` | 10,568 | 0 | — | Advisory entities only |
| DSA (existing) | `graph/enrichment/advisory-dsa` | 50,633 | 0 | — | Advisory entities only |

### Vulnerability Sources

| Source | Graph | Vulnerabilities | Affected Ranges | Ecosystems |
|--------|-------|----------------|-----------------|------------|
| **OSV bulk** | `graph/security/osv` | **238,751** | 231,727 | 11 (npm, PyPI, Go, Cargo, Maven, RubyGems, NuGet, Packagist, Swift, Hex, Pub) |
| Alpine in-graph | `graph/alpine/*` | ~7,000 | — | Alpine |

**OSV is loaded and operational on west-3.**

---

## What Changed Today

### 1. RPM Updateinfo Integration (Primary Deliverable)

**Problem:** Advisory collectors (Bodhi RSS, RHSA, DSA) emitted advisory entities and CVE cross-references but lacked `sec:advisoryForPackage` links to concrete packages — the join required by SCR-06/07/09 and TEMP-01.

**Root cause:** Prior approaches required post-hoc SPARQL resolution against the package graph. This failed because advisory NVRs referenced updated package versions not yet in the graph snapshot.

**Solution:** Parse `updateinfo.xml` directly during RPM `collect()`. Advisory packages are matched against a NEVRA lookup set built during package emission in the same collection pass. Resolution is deterministic — every `advisoryForPackage` link is guaranteed valid because the package was emitted seconds earlier from the same repository snapshot.

**Result:**
- 280 security advisories → 1,822 concrete package links
- 100% of links join to packages with `pkg:partOfRelease`
- No SPARQL, no stale-graph problem
- Fedora 43 and 44 CronJobs updated to collect from both releases and updates repos

### 2. Bodhi RSS Collector (Fedora)

Parses Fedora's Bodhi RSS feed for advisory metadata. Provides broader coverage (652 advisories including historical) but relies on SPARQL-based NVR→binary resolution. Resolved 297 of 912 NVRs — limited by graph freshness. Useful as a supplementary source; updateinfo is authoritative for repos that include it.

### 3. GLSA XML Collector (Gentoo)

Parses Gentoo Linux Security Advisories in XML format. 50 advisories with structured affected-range data (version constraints). Package resolution requires a Gentoo package graph, which does not currently exist on west-3. Advisory entities and CVE cross-references (279 links) are loaded.

---

## CQ Validation Impact

### Before Today

| CQ | Status | Blocker |
|----|--------|---------|
| SCR-06 (advisory→package resolution) | **BLOCKED** | No `sec:advisoryForPackage` links for any RPM distro |
| SCR-07 (compound vulnerability) | **BLOCKED** | Requires advisory→package→dependency chain |
| SCR-09 (advisory coverage per package) | **BLOCKED** | Cannot count advisories per package |
| TEMP-01 (vulnerability window) | **EMPTY** | No advisory dates for temporal join |
| SEC-01 (vulnerabilities by package) | **PARTIAL** | OSV affected ranges present for language ecosystems only |

### After Today

| CQ | Status | Evidence |
|----|--------|----------|
| SCR-06 | **PARTIALLY UNBLOCKED** | 1,822 advisory→package joins in Fedora 43. Full coverage requires multi-arch collection + all Fedora releases. |
| SCR-07 | **PARTIALLY UNBLOCKED** | Advisory→package→dependency chain traversable for covered packages. Compound vuln join also requires `sec:hasCVSSScore` from OSV. |
| SCR-09 | **PARTIALLY UNBLOCKED** | Advisory counts per package queryable for F43 updates repo packages. |
| TEMP-01 | **STILL BLOCKED for RPM** | Advisory dates loaded (280) and package links exist (1,822), but TEMP-01 also requires `sec:publishedDate` on the vulnerability entity. OSV bulk covers language ecosystems (npm, PyPI, Go, etc.) but the OSV collector does not collect distro ecosystems — no Fedora-side vulnerability data exists to complete the temporal join. |
| SEC-01 | **NO CHANGE** | SEC-01 uses `sec:hasAffectedRange`, `sec:affectsEcosystem`, and `sec:affectsPackageName` on vulnerability entities — not advisory links. The updateinfo CVE cross-references (`sec:addressesVulnerability`) do not participate in this query. SEC-01 status depends on OSV affected-range coverage, which is present for language ecosystems but absent for distro ecosystems. |

**"Partially unblocked" means:** The advisory-side data contract is now satisfied — `sec:advisoryForPackage` links exist pointing to concrete `pkg:BinaryPackage` entities with `pkg:partOfRelease` context. End-to-end CQ queries that also require vulnerability-side data (`sec:publishedDate`, `sec:hasCVSSScore`, `sec:hasAffectedRange`) depend on OSV enrichment completeness.

**Critical gap for RPM CQs:** The OSV bulk collector covers language ecosystems (npm, PyPI, Go, Cargo, etc.) but explicitly skips distro ecosystems (`osv.rs:42` returns `None` for distro mappings). There is currently no Fedora/RHEL/Debian/Alpine vulnerability data in `graph/security/osv`. This means TEMP-01 and SEC-01 remain non-functional for RPM-family ecosystems even though the advisory side is now populated. Closing this gap requires either extending the OSV collector to handle distro ecosystem mappings or sourcing distro vulnerability data through another path.

---

## Ontology Contract Compliance

All emitted triples conform to `extensions/security/security.ttl`:

| Property | Contract | Compliance |
|----------|----------|------------|
| `sec:advisoryForPackage` | Domain: SecurityAdvisory, Range: Package with partOfRelease | ✅ 100% of 1,822 links verified |
| `sec:advisoryDate` | `xsd:dateTime` | ✅ |
| `sec:advisorySeverity` | SKOS concept from SeverityScheme | ✅ |
| `sec:advisoryType` | SKOS concept from AdvisoryCategoryScheme | ✅ |
| `sec:addressesVulnerability` | SecurityAdvisory → Vulnerability | ✅ |
| `sec:hasAffectedRange` | Domain: Vulnerability (not Advisory) | ✅ Fixed during review — GLSA initially attached to advisory |

**Ontology issue found and corrected:** GLSA collector initially emitted `sec:hasAffectedRange` from `sec:SecurityAdvisory` nodes, violating the ontology's domain constraint on `sec:Vulnerability` (security.ttl:206). Corrected to attach affected ranges to vulnerability entities via `sec:addressesVulnerability`.

---

## Architecture Decision: Updateinfo vs Bodhi

| Criterion | RPM Updateinfo | Bodhi RSS |
|-----------|---------------|-----------|
| Package resolution | Deterministic (same repo snapshot) | SPARQL-dependent (stale graph risk) |
| Resolution rate | 16% (arch-filtered) to ~100% (full) | 33% (297/912, version freshness gap) |
| Coverage | Packages in current repo snapshot | All historical advisories |
| External dependency | None (repodata is already fetched) | Bodhi RSS endpoint + Fuseki SPARQL |
| Severity data | ✅ From updateinfo XML | ❌ RSS lacks severity |

**Recommendation:** Updateinfo is the authoritative advisory source for RPM repos that include it. Bodhi RSS provides historical breadth but lacks the package-resolution guarantee required by CQ contracts. The Bodhi advisory graph (`graph/advisory/fedora/43`) can be deprecated once updateinfo coverage is validated across all target releases.

---

## Remaining Gaps

### Data Gaps (require collection, not code)

| Gap | Impact | Remediation |
|-----|--------|-------------|
| Gentoo package graph missing on west-3 | GLSA collector cannot resolve atoms to packages | Collect Gentoo packages |
| Multi-arch updateinfo | F43 aarch64/riscv64 CronJobs lack updates repo | Add updates repo URLs |
| RHSA `advisoryForPackage` | 10,568 advisories without package links | Requires RHEL-specific resolution strategy |
| DSA `advisoryForPackage` | 50,633 advisories without package links | Requires Debian-specific resolution strategy |

### Upstream Dependencies (outside advisory workstream)

| Dependency | Required by | Available? | Gap |
|------------|-------------|------------|-----|
| `sec:publishedDate` on CVE entities | TEMP-01 | **No for distro ecosystems.** OSV bulk covers npm/PyPI/Go/etc. but not Fedora/RHEL/Debian. OSV collector skips distro ecosystems (`osv.rs:42`). | Need canonical CVE metadata source (NVD or similar) |
| `sec:hasCVSSScore` | SCR-07 | Language ecosystems only (via OSV) | Same gap — need CVE backbone |
| `sec:hasAffectedRange` for distro packages | SEC-01 | **No.** OSV affected ranges exist for language ecosystems. Distro-level affected-range data does not exist in the graph. | Need distro-capable affected-range ingestion |

**Key insight:** Advisory `sec:addressesVulnerability` cross-references are NOT equivalent to vulnerability affected-range coverage. The advisory side (what we built today) and the vulnerability side (what SEC-01/TEMP-01 require) are separate data concerns with separate sources.

### CQ Claim Separation

To avoid overclaiming, CQ readiness should be evaluated on three axes independently:

| Axis | What it means | CQs that depend on it |
|------|--------------|----------------------|
| **Advisory-side** | `sec:advisoryForPackage` links to concrete packages | SCR-06, SCR-07, SCR-09 |
| **Vulnerability-side** | `sec:publishedDate`, `sec:hasCVSSScore`, `sec:hasAffectedRange` on vulnerability entities | TEMP-01, SEC-01, SCR-07 |
| **End-to-end** | Both sides join through shared CVE entities | All above |

**Current state:** Advisory-side is satisfied for Fedora 43 (1,822 links). Vulnerability-side is satisfied for language ecosystems (238K OSV vulns). Neither side covers the other's ecosystems — there is no end-to-end join for RPM distros yet.

### Deferred Tier 2 Features

- RPM comps.xml (package groups/categories)
- RPM modules.yaml (modularity streams)
- Debian debtags (structured package tagging)
- Gentoo metadata.xml (upstream info, USE flags)

---

## Recommended Architecture

### Canonical CVE Metadata Collector (Recommended Next)

Build a CVE backbone collector that emits canonical vulnerability entities regardless of ecosystem:

**Source:** NVD (National Vulnerability Database) or equivalent CVE publication feed
**Emits per CVE:**
- `sec:cveId`
- `sec:publishedDate`
- `sec:hasCVSSScore` (v3.1/v4.0)
- `sec:hasCWE`

**Why this unblocks TEMP-01:** Fedora advisories already link to CVE entities via `sec:addressesVulnerability`. If those same CVE entities have `sec:publishedDate` from NVD, the TEMP-01 join (advisory date vs vulnerability publication date) becomes possible through the shared CVE URI — no distro-specific OSV data needed.

**Design principles:**
1. Advisory collectors (updateinfo, Bodhi, GLSA, RHSA, DSA) focus on advisory→package linkage
2. The CVE backbone collector provides canonical vulnerability metadata (dates, scores, CWEs)
3. Affected-range modeling remains a separate vulnerability-ingest concern (OSV for language ecosystems, distro-specific feeds for distro ecosystems)

### Distro Affected-Range Ingestion (For SEC-01)

SEC-01 requires `sec:hasAffectedRange` with `sec:affectsEcosystem` and `sec:affectsPackageName` on vulnerability entities. Options:

1. **Extend OSV collector for distro ecosystems** — if OSV publishes Fedora/Debian/Alpine data (to be investigated)
2. **Distro-native vulnerability feeds** modeled into AffectedRange nodes
3. **Resolver/deriver** from advisory+package data back to vulnerability/package-name ranges (weaker, but possible)

---

## Next Steps

### Immediate Priority: Canonical CVE Metadata (Task 2)

Source NVD, emit `sec:publishedDate` + `sec:hasCVSSScore` + `sec:hasCWE` per CVE entity. Uses the same `d/cve/CVE-YYYY-NNNNN` URI pattern already in use — join key is confirmed working.

**Unblocks:**
- **TEMP-01** — advisory dates + CVE publication dates = vulnerability window
- **SCR-06** — advisory→package join + CVE publication metadata
- **SCR-09** — advisory coverage per package + CVE metadata

**Does NOT unblock:**
- **SCR-07** — still needs distro-aware affected-range data (Task 3)
- **SEC-01** — entirely driven by affected ranges, not CVE metadata (Task 3)

### Separate Workstream: Distro Affected Ranges (Task 3)

Do not mix into Task 2. Requires either:
- Distro-capable vulnerability feeds emitting `sec:hasAffectedRange` / `sec:affectsEcosystem` / `sec:affectsPackageName`
- Or an explicit unsupported determination per ecosystem

### Other

- **Extend multi-arch CronJobs** — Add updates repo to F43 aarch64, F44, and rawhide jobs
- **Collect Gentoo packages** — Enables GLSA package resolution
- **RHSA/DSA package resolution** — Design approach for RHEL and Debian advisory→package linking
- **Bodhi graph deprecation decision** — After validation, decide whether to drop `graph/advisory/fedora/43` in favor of updateinfo-only
