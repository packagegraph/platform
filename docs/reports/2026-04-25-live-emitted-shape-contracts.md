# Live Emitted Shape Contracts

**Date:** 2026-04-25
**Purpose:** Operational source of truth for CQ rewrite work. For each enrichment graph, record the emitted entity types, predicates, join paths that exist, and join paths explicitly absent.
**Scope:** Current `pg-collect` emitter behavior plus live west-3 validation evidence where available.

---

## 1. GitHub Enrichment Graph

**Graph URI:** `https://packagegraph.github.io/graph/enrichment/github`

**Evidence**
- Code: `etl/pg-collect/src/enrich_github.rs`
- Live validation: `docs/reports/2026-04-25-github-enrichment-strategy-validation.md`
- Deployment note: current committed CronJob is still old-mode; live validation used a one-off job

### Entity Types Emitted

- `vcs:Repository`
- `vcs:Release`
- `pkg:License`
- `foaf:Person`
- `pkg:ContributorAccount`
- `pkg:Contribution` (provisional)
- `pkg:EmailObservation` (provisional)

### Predicates Emitted

**Repository metadata**
- `vcs:repositoryURL`
- `vcs:repositoryDescription`
- `vcs:defaultBranch`
- `vcs:stargazerCount`
- `vcs:forkCount`
- `vcs:openIssuesCount`
- `vcs:subscriberCount`
- `vcs:isArchived`
- `vcs:isFork`
- `vcs:topic`
- `pkg:lastCommitDate`

**Language / release / head metadata**
- `met:languageName`
- `met:languageBytes`
- `met:totalBytes`
- `met:primaryLanguage` (provisional)
- `vcs:hasRelease`
- `vcs:tagName`
- `vcs:releaseDate`
- `vcs:isPreRelease`
- `vcs:headCommitHash` (provisional)
- `vcs:repositoryCreatedAt` (provisional)
- `vcs:lastActivityDate` (provisional)
- `vcs:projectHomepage` (provisional)
- `vcs:diskUsageKB` (provisional)
- `vcs:openPullRequestCount` (provisional)

**License**
- `pkg:licenseName`
- `pkg:hasLicense`
- `pkg:spdxId`

**Contributor / identity**
- `pkg:hasAccount`
- `pkg:accountPlatform`
- `pkg:accountUsername`
- `pkg:accountUrl`
- `pkg:contributesTo`
- `pkg:hasContributor`
- `pkg:contributor` (on `pkg:Contribution`, provisional)
- `pkg:repository` (on `pkg:Contribution`, provisional)
- `vcs:commitCount` (on `pkg:Contribution`)

**Email observation**
- `pkg:hasEmailObservation` (provisional)
- `pkg:observedEmail` (provisional)
- `pkg:observedAt` (provisional)

### Join Paths That Exist

- Repository to release:
  - `?repo vcs:hasRelease ?release`
- Repository to license:
  - `?repo pkg:hasLicense ?license`
- Repository to contributor agent:
  - `?repo pkg:hasContributor ?person`
- Person to contributor account:
  - `?person pkg:hasAccount ?account`
- Person to contribution edge:
  - `?contrib pkg:contributor ?person ; pkg:repository ?repo`
- Person to observed email history:
  - `?person pkg:hasEmailObservation ?obs`
- Repository to dominant language:
  - `?repo met:primaryLanguage ?lang`

### Join Paths Explicitly Absent

- No package-to-commit lineage:
  - absent `pkg:derivedFromCommit`
- No full commit graph:
  - absent commit entities, `vcs:parentCommit`, branch topology
- No package-to-repository join introduced by this enricher:
  - no guarantee of `pkg:hasUpstreamProject/pkg:sourceCodeRepository` from this graph alone
- No provenance build chain:
  - absent `prov:wasGeneratedBy`, `prov:wasAssociatedWith`, `pkg:wasBuiltBy`
- No commit signature graph:
  - absent `att:DigitalSignature`, `att:hasSignature`

### CQ-Relevant Notes

- `VCS-01`: data-side enablement exists via `met:primaryLanguage`, but frozen query still needs join-path alignment.
- `PROV-04`: partial contributor shape exists, but it is GitHub contributor data only, not full provenance semantics.
- `VCS-02`: only HEAD hash is present; full commit-to-release tracing is absent.

### Minimal Probes

```sparql
ASK {
  GRAPH <https://packagegraph.github.io/graph/enrichment/github> {
    ?repo a vcs:Repository ;
          met:primaryLanguage ?lang ;
          vcs:hasRelease ?release ;
          pkg:hasContributor ?person .
  }
}
```

```sparql
SELECT ?p (COUNT(*) AS ?count) WHERE {
  GRAPH <https://packagegraph.github.io/graph/enrichment/github> {
    ?s ?p ?o .
  }
}
GROUP BY ?p
ORDER BY DESC(?count)
LIMIT 25
```

---

## 2. Koji Enrichment Graph

**Graph URI:** `https://packagegraph.github.io/graph/enrichment/koji`

**Evidence**
- Code: `etl/pg-collect/src/enrich_koji.rs`
- Availability report: `docs/reports/2026-04-24-enrichment-availability-summary.md`

### Entity Types Emitted

- `pkg:BuildActivity`

### Predicates Emitted

- `pkg:packageName`
- `slsa:buildSystem`
- `slsa:buildOwner`
- `slsa:buildStartTime`
- `slsa:buildEndTime`

### Join Paths That Exist

- None beyond attributes on the build node itself.

### Join Paths Explicitly Absent

- No build-to-package object link:
  - absent `pkg:wasBuiltBy`
- No build-to-source or build-to-binary linkage:
  - absent `prov:used`, `prov:wasGeneratedBy`
- No owner-as-agent entity:
  - `slsa:buildOwner` is a string literal, not a person/account URI
- No attestation or signature graph

### CQ-Relevant Notes

- This is useful as build metadata, but not enough by itself to satisfy `PROV-01` or `PROV-03` without query narrowing or producer expansion.

### Minimal Probes

```sparql
ASK {
  GRAPH <https://packagegraph.github.io/graph/enrichment/koji> {
    ?b a pkg:BuildActivity ;
       slsa:buildSystem "koji" .
  }
}
```

```sparql
SELECT ?p (COUNT(*) AS ?count) WHERE {
  GRAPH <https://packagegraph.github.io/graph/enrichment/koji> {
    ?s ?p ?o .
  }
}
GROUP BY ?p
ORDER BY DESC(?count)
```

---

## 3. npm Provenance Enrichment Graph

**Graph URI:** `https://packagegraph.github.io/graph/enrichment/npm-provenance`

**Evidence**
- Code: `etl/pg-collect/src/enrich_npm_provenance.rs`
- Availability report: `docs/reports/2026-04-24-enrichment-availability-summary.md`

### Entity Types Emitted

- `slsa:Attestation`

### Predicates Emitted

- `slsa:predicateType`
- `slsa:hasAttestation`
- `slsa:attestationCount`

### Join Paths That Exist

- Package to attestation:
  - `?pkg slsa:hasAttestation ?att`

### Join Paths Explicitly Absent

- No SLSA level:
  - absent `slsa:slsaLevel`
- No provenance bundle decomposition:
  - no builder, subject, materials, invocation, signatures
- No attestation verification graph:
  - absent `att:*`
- No package-to-build or package-to-commit linkage

### CQ-Relevant Notes

- This is enough to prove “attestation exists” but not enough for richer provenance CQs without query narrowing or producer expansion.
- Availability report previously confirmed west-3 had zero triples because no current npm packages had attestations.

### Minimal Probes

```sparql
ASK {
  GRAPH <https://packagegraph.github.io/graph/enrichment/npm-provenance> {
    ?pkg slsa:hasAttestation ?att .
    ?att a slsa:Attestation .
  }
}
```

```sparql
SELECT ?p (COUNT(*) AS ?count) WHERE {
  GRAPH <https://packagegraph.github.io/graph/enrichment/npm-provenance> {
    ?s ?p ?o .
  }
}
GROUP BY ?p
ORDER BY DESC(?count)
```

---

## 4. Repology Enrichment Graph

**Graph URI:** `https://packagegraph.github.io/graph/enrichment/repology`

**Evidence**
- Code: `etl/pg-collect/src/enrich_repology.rs`
- Validation plan: `docs/plans/2026-04-24-repology-xd05-validation.md`

### Entity Types Emitted

- No new entity types emitted.

### Predicates Emitted

- `pkg:crossDistributionAlternative`

### Join Paths That Exist

- Package identity to package identity:
  - `?pkg1 pkg:crossDistributionAlternative ?pkg2`

### Join Paths Explicitly Absent

- No explicit equivalence node/entity
- No provenance or evidence node for why two package identities are linked
- No version/status predicates from Repology are emitted
- No direct package-instance join if a CQ expects concrete package resources instead of `pkg:PackageIdentity`

### CQ-Relevant Notes

- `XD-05` is the main consumer candidate.
- This graph is best treated as a cross-distribution identity link layer, not a rich metadata layer.

### Minimal Probes

```sparql
ASK {
  GRAPH <https://packagegraph.github.io/graph/enrichment/repology> {
    ?a pkg:crossDistributionAlternative ?b .
  }
}
```

```sparql
SELECT ?a ?b WHERE {
  GRAPH <https://packagegraph.github.io/graph/enrichment/repology> {
    ?a pkg:crossDistributionAlternative ?b .
  }
}
LIMIT 20
```

---

## Recommended Use

Use these contracts as the default reference for:
- CQ rewrite planning
- west-3 validation reports
- ontology requests tied to real producer shapes

When an enricher changes shape, update this file in the same change set or attach a replacement contract report.
