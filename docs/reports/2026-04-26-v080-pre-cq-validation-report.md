# v0.8.0-pre CQ Validation Report

**Date:** 2026-04-26
**Endpoint:** west-3 Fuseki (192.168.137.230:30030), 103.9M triples + 11.7K v0.8.0-pre test triples
**Frozen CQ commit:** `7dbe46ee9e45f675d1b2c5bd221bde836d765b01`
**Test data:** 50 Cargo crates + 109 Gentoo packages (v0.8.0-pre forge triples)

## Summary

| Result | Count |
|--------|------:|
| PASS | 9 |
| PASS_AGGREGATE | 4 |
| **Effective PASS** | **13** |
| MARGINAL | 4 |
| EMPTY | 36 |
| ERROR | 1 |
| **Total** | **54** |

## Key Findings for the Ontology Team

### 1. CQ Query–Ontology Property Path Mismatch (VCS-01, VCS-02)

**The data exists. The queries can't reach it.**

VCS-01 queries:
```sparql
?package pkg:hasUpstreamProject/pkg:sourceCodeRepository ?repo .
?repo met:primaryLanguage ?language .
```

No producer has ever emitted `pkg:hasUpstreamProject` or `pkg:sourceCodeRepository`. What producers emit is:

```sparql
?identity pkg:upstreamRepository ?repo .
?repo met:primaryLanguage ?language .
```

**Proof:** The frozen VCS-01 query returns 0 rows. An equivalent query using `pkg:upstreamRepository` returns **220 C repos, 124 C++ repos, 43 Python repos** — from existing data that's been in west-3 for weeks.

```
=== Frozen VCS-01 (hasUpstreamProject chain) → 0 rows ===
=== Alternative (upstreamRepository) → 450+ repos with language data ===
```

**Action needed:** Either (a) update the CQ queries to use `pkg:upstreamRepository`, or (b) define `hasUpstreamProject`/`sourceCodeRepository` in the ontology and have producers emit them. Option (a) is immediate; option (b) requires a new property chain that no producer implements.

VCS-02 has the same pattern — uses `pkg:derivedFromCommit` which no producer emits. The specfile collector (planned) could emit commit hashes, but the frozen query uses a property chain (`?commit vcs:commitHash`, `?commit vcs:onBranch/vcs:branchName`) that would need matching producer output.

### 2. Missing `rdfs:label` on Architecture Entities (6 CQ regressions)

Six CQs that previously passed now return EMPTY:

| CQ | Title | Root cause |
|----|-------|-----------|
| PM-01 | Distribution Package Listing | `?arch rdfs:label "x86_64"` — no label emitted |
| PM-02 | Source-to-Binary Mapping | Same arch label issue |
| PM-03 | Virtual Package Providers | Same |
| PM-05 | Packages by Maintainer | Same pattern |
| PM-07 | License Distribution | Same pattern |
| LIC-03 | Cross-Release License Changes | Same pattern |

Architecture entities exist as URIs (`d/arch/x86_64`) and are linked to packages via `pkg:targetArchitecture`, but no collector emits `rdfs:label` on them. This worked in a prior TDB2 build, suggesting labels were present in an older collector version or were manually loaded.

**Action needed:** Either (a) collectors emit `rdfs:label "x86_64"` on architecture entities, or (b) CQ queries match on URI pattern instead of label (e.g., `FILTER(STRENDS(STR(?arch), "/x86_64"))`). Option (a) is the correct fix.

### 3. v0.8.0 Forge Triples Validated

The new forge library produces correct triples. For 50 Cargo crates:

| Predicate | Count | Notes |
|-----------|------:|-------|
| `pkg:upstreamRepository` | 50 | 100% coverage from `repository` field |
| `vcs:Repository` (type) | 50 | One per repo |
| `vcs:repositoryURL` | 50 | Canonical normalized URL |
| `vcs:hostedOn` | 50 | Links repo to forge instance |
| `vcs:Forge` (type) | 1 | Deduplicated per host (github.com) |
| `vcs:forgeSoftware` | 1 | `vcs:GitHub` individual |
| `vcs:forgeUrl` | 1 | `https://github.com` |

For Gentoo (10 packages, 109 versions):

| Predicate | Count | Notes |
|-----------|------:|-------|
| `pkg:upstreamRepository` | 8 | 1 unique repo (libxml2 on GitLab) — low coverage because Gentoo HOMEPAGE is usually a project site, not a forge URL |

**Observation for ontology team:** The `vcs:forgeSoftware` individuals (`vcs:GitHub`, `vcs:GitLab`, `vcs:Forgejo`, `vcs:SourceHut`, `vcs:Bitbucket`, `vcs:Savannah`, `vcs:cgit`) need to be defined in `vcs.ttl` if they aren't already. The forge library emits references to them.

### 4. CQs That PASS Correctly

| Domain | Pass/Total | What works |
|--------|-----------|-----------|
| DEP | 3/5 | Dependency chains, type distribution, consistency checks |
| LIC | 2/3 | SPDX distribution, non-SPDX detection |
| PM | 2/11 | Dependency depth, update frequency |
| SCR | 2/9 | Bus factor, maintainer overload (from GitHub enricher) |
| SEC | 1/8 | Advisory coverage |
| TEMP | 1/3 | Package obsolescence between releases |
| XD | 1/5 | Distribution-specific packages |
| ECO | 1/3 | npm dependency depth |

### 5. CQ Domains That Are Fully Blocked

| Domain | Block reason |
|--------|-------------|
| **VCS (0/2)** | Query uses `hasUpstreamProject/sourceCodeRepository` — never emitted. Data exists via `upstreamRepository`. |
| **PROV (0/4)** | No provenance data loaded. Koji enricher exists but times out at scale. NPM attestation enricher has signing triples but no production run. |
| **SET (0/1)** | No package-set data loaded. |
| **SEC (1/8)** | 7 blocked: SEC-01/02/03 need distro-specific affected-range data. SEC-06/07/08 need CWE/patch chain. SEC-05 regressed to MARGINAL. |

### 6. Specific Issues for Ontology Review

| Issue | Ontology concern | Severity |
|-------|-----------------|----------|
| `hasUpstreamProject` / `sourceCodeRepository` in CQ but not in producer output | Are these deprecated in favor of `upstreamRepository`? If so, update CQ definitions. | High — blocks VCS-01/02 |
| `rdfs:label` on entity nodes (Architecture, Distribution) | Should entities always have labels? Or should CQs use URI matching? | Medium — blocks 6 CQs |
| `vcs:forgeSoftware` individuals | Are `vcs:GitHub`, `vcs:GitLab`, etc. defined in vcs.ttl v0.8.0? | Low — forge library emits them |
| Gentoo compound license expressions (`BSD curl ISC test? ( BSD-4 )`) | SPDX license URI builder produces invalid URIs. Need license expression parser or DQ observation. | Low — DQ handles it |
| `pkg:derivedFromCommit` / `vcs:commitHash` / `vcs:onBranch` | What producer is expected to emit these? VCS-02 depends on them. | Medium — specfile collector could provide |

## Recommendations

1. **Immediate (ontology team):** Update VCS-01 and VCS-02 CQ queries to use `pkg:upstreamRepository` instead of `hasUpstreamProject/sourceCodeRepository`. This unblocks VCS-01 immediately with existing west-3 data.

2. **Immediate (platform team):** Emit `rdfs:label` on Architecture entities in all distro collectors. Recovers 6 CQ passes.

3. **Short-term:** Confirm `vcs:forgeSoftware` individuals are defined in vcs.ttl v0.8.0. If not, add them.

4. **Medium-term:** Define the expected producer output shape for VCS-02 (`derivedFromCommit` chain) so the specfile collector can target it.

5. **Deferred:** PROV domain CQs require either a working Koji enricher (currently times out) or the new specfile collector with commit provenance.
