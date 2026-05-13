# ADR-0001: CQ Data Contract

**Status:** Proposed — awaiting ontology team signoff per Task 1 DoD
**Date:** 2026-04-22
**Context:** Platform CQ Data Remediation Plan, Phase 1

## Decision

The platform ETL pipeline commits to the following data emission contract for all collectors and enrichers. This contract defines the required graph shape for the competency question (CQ) suite to be answerable on production data.

## SD-1: Release Traversal

**Canonical traversal:**

```
Package --partOfRelease--> DistributionRelease --partOfDistribution--> Distribution
```

**Required triples per collector run:**

| Subject | Predicate | Object | Required |
|---------|-----------|--------|----------|
| Package | `pkg:partOfRelease` | DistributionRelease | YES |
| DistributionRelease | `pkg:partOfDistribution` | Distribution | YES |
| Distribution | `pkg:hasRelease` | DistributionRelease | YES (inverse convenience) |
| DistributionRelease | `rdf:type` | `pkg:DistributionRelease` | YES |
| Distribution | `rdf:type` | `pkg:Distribution` | YES |
| Distribution | `rdfs:label` | string | YES |

**Rationale:** CQs use the inverse path `^pkg:hasRelease/rdfs:label` to filter by distribution name. Without `hasRelease` and `rdfs:label` on Distribution, these queries return zero rows.

## SD-2: Release Identifier Fields

| Distro Type | Property | Value | Example |
|-------------|----------|-------|---------|
| Numbered release | `pkg:releaseVersion` | version string | Fedora: `"43"` |
| Named release | `pkg:releaseCodename` | codename string | Debian: `"trixie"` |
| Rolling release | `pkg:releaseCodename` | identifier | Arch: `"arch"`, openSUSE: `"tumbleweed"` |
| Both available | both properties | respective values | Debian 13: `releaseCodename "trixie"` + `releaseVersion "13"` |

**Rationale:** CQs use `releaseVersion "43"` for Fedora. Current production emits `releaseCodename "43"` which is semantically incorrect (43 is a version number, not a codename).

## SD-3: Maintainer Identity Contract

**Required pattern:**

```
Package --maintainedBy--> Person|SoftwareAgent
```

**Required triples:**

| Subject | Predicate | Object | Required |
|---------|-----------|--------|----------|
| Package | `pkg:maintainedBy` | Person or SoftwareAgent URI | YES |
| Person | `rdf:type` | `pkg:Person` | YES |
| Person | `foaf:name` | string | YES |
| Person | `rdfs:label` | string | YES |

**Optional detailed pattern:**

```
Package --hasMaintenanceRole--> Maintainer --heldBy--> Person
```

**Breaking change:** Current production types maintainer targets as `pkg:Maintainer`. This changes to `pkg:Person`. The Maintainer class becomes a role node, not a direct target of `maintainedBy`.

## SD-4: Tenure Semantics

`pkg:maintainerSince` — **Phase 2, not emitted in Phase 1.**

Allowed derivation sources (precedence order):
1. Explicit source-of-truth maintainer assignment date
2. Packaging VCS history (first maintainer assignment for current relationship)
3. Historical PackageGraph snapshot history

If none available: do not emit. Mark ecosystem as unsupported for SCR-03.

## SD-5: Package Recency Semantics

`pkg:lastReleaseDate` — **Phase 2, not emitted in Phase 1.**

Allowed derivation sources (precedence order):
1. Authoritative package publication timestamp from repository
2. Historical PackageGraph observation of version publication
3. Upstream release metadata after identity resolution

If none available: do not emit.

## SD-6: Security Data Contract

**Required vulnerability triples (from OSV/NVD):**

| Subject | Predicate | Object | Required |
|---------|-----------|--------|----------|
| Vulnerability | `sec:cveId` | string | YES (when CVE exists) |
| Vulnerability | `sec:publishedDate` | xsd:dateTime | YES |
| Vulnerability | `sec:hasCVSSScore` | CVSSScore URI | YES (when score exists) |
| CVSSScore | `sec:baseScore` | xsd:decimal | YES |
| CVSSScore | `sec:cvssVersion` | string | YES |
| Vulnerability | `sec:hasAffectedRange` | AffectedRange bnode | YES |
| AffectedRange | `sec:affectsPackageName` | string | YES |
| AffectedRange | `sec:affectsEcosystem` | Ecosystem URI | YES |
| AffectedRange | `sec:rangeType` | SKOS concept URI | YES |

**Required advisory triples:**

| Subject | Predicate | Object | Required |
|---------|-----------|--------|----------|
| Advisory | `sec:advisoryId` | string | YES |
| Advisory | `sec:advisoryDate` | string | YES |
| Advisory | `sec:addressesVulnerability` | Vulnerability URI | YES |
| Advisory | `sec:advisoryForPackage` | Package URI (see SD-7) | YES (when resolvable to concrete Package) |
| Advisory | `sec:advisoryType` | SKOS concept URI | YES |
| Advisory | `sec:advisorySeverity` | SKOS concept URI | YES (when available) |

## SD-7: advisoryForPackage Target Semantics

`sec:advisoryForPackage` must target a concrete, versioned `pkg:Package` instance with release context.

**Allowed targets:** `pkg:BinaryPackage` or `pkg:SourcePackage` that participate in `pkg:partOfRelease`.

**Disallowed targets:** `pkg:PackageIdentity`, package-name string literals, synthetic placeholder nodes.

If an enricher can only resolve a package name or abstract identity, it must NOT emit `advisoryForPackage`. The ontology range remains `pkg:Package`; no range relaxation is part of this contract.

## Ecosystem Capability Matrix

| Field | RPM (Fedora/RHEL) | Debian | Alpine | Homebrew | npm/PyPI/Cargo/Go |
|-------|:-:|:-:|:-:|:-:|:-:|
| `partOfRelease` | native | native | native | native | native |
| `partOfDistribution` | native | native | native | native | native |
| `hasRelease` (inverse) | **emit** | **emit** | **emit** | **emit** | **emit** |
| `releaseVersion` | **emit** | native | native | n/a | n/a |
| `releaseCodename` | n/a | native | native | native | native |
| `rdfs:label` on Distribution | **emit** | **emit** | **emit** | **emit** | **emit** |
| `maintainedBy` → Person | **fix** | **fix** | **fix** | **fix** | n/a |
| `foaf:name` on Person | **emit** | **emit** | **emit** | **emit** | n/a |
| `maintainerSince` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `lastReleaseDate` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `publishedDate` (vuln) | enriched (OSV) | enriched (OSV) | enriched (OSV) | n/a | enriched (OSV) |
| `hasCVSSScore` | enriched (OSV) | enriched (OSV) | enriched (OSV) | n/a | enriched (OSV) |
| `hasAffectedRange` | enriched (OSV) | enriched (OSV) | enriched (OSV) | n/a | enriched (OSV) |
| `advisoryForPackage` | unsupported (needs NVRA resolution) | unsupported (needs version-aware resolution) | unsupported | n/a | n/a |

Legend: **native** = source provides directly, **enriched** = derived from external API, **emit** = must add to collector, **fix** = must change emission pattern, **unsupported** = no authoritative source

## CQ Support Level After Phase 1

| CQ | Status | Blocking Issues |
|----|--------|----------------|
| SCR-01 | **Supported** | Release traversal + maintainer identity fixes |
| SCR-02 | **Supported** | Maintainer identity + foaf:name |
| SCR-03 | **Unsupported** | Requires maintainerSince + lastReleaseDate (Phase 2) |
| SCR-04 | **Supported** | Release traversal + maintainer identity |
| SCR-05 | **Partial** | Requires upstreamRepository enrichment |
| SCR-06 | **Supported** | Requires security data loading (Phase 3) |
| SCR-07 | **Supported** | Requires security data loading (Phase 3) |
| SCR-08 | **Supported** | Requires security data loading (Phase 3) |
| SCR-09 | **Supported** | Requires security data loading (Phase 3) |

## Consequences

- All collectors must emit `hasRelease` inverse edges and `rdfs:label` on Distribution
- RPM collector must emit `releaseVersion` for numbered releases
- All collectors must change maintainer typing from `Maintainer` to `Person`
- SCR-03 is explicitly deferred to Phase 2
- Security CQs depend on Phase 3 data loading
