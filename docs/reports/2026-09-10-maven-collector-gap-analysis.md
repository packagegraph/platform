# Maven Collector — Functional Gap Analysis

**Date:** 2026-09-10
**Branch:** `feat/sparql-perf-harness` (report only; unrelated to that branch's code)
**Question:** The `maven` graph holds only 1,926 distinct artifacts and 2 of 5
Maven competency questions return nothing. Is `etl/pg-collect/src/maven.rs`
functionally complete, and if not, what is missing?
**Method:** Read `maven.rs` (5,615 lines) and `maven_version.rs` (173 lines)
against live SPARQL measurements of the promoted index
(`https://packagegraph.di.riseproject.dev`, 140,977,324 triples, 51 named
graphs, sampled 2026-09-10).

## TL;DR

- **The collector is not functionally complete.** Its core emission is
  competent, but five structural gaps limit what the Maven graph can answer,
  independent of corpus size.
- **No parent POM resolution is the root cause of most of it.** `parent` is
  parsed but used only for string interpolation; the parent POM is never
  fetched. Consequence: **43% of dependency edges (4,881 of 11,373) carry no
  version**, and BOM/aggregator structure is absent from the graph entirely.
- **Maven Central's search API is 95% discarded** — `SearchDoc` deserializes
  exactly one field. No release timestamp is captured, which makes every
  temporal question structurally unanswerable for Maven at any corpus size.
- **No `<developers>` parsing**, so Maven artifacts are invisible to the entire
  maintainer-oriented CQ family. **No checksums**, so `CQ-PROV` cannot answer.
- **28% of referenced artifacts are dangling** — 748 of 2,674
  `PackageIdentity` nodes have no `Package`, and nothing measures or reports it.
- Two of the empty Maven CQs are **not** collector faults: `CQ-MVN-04` asks for
  an artifact absent from the corpus, and `CQ-MVN-05` needs diff-enricher
  output no collector emits.
- One **cross-collector inconsistency**: Maven emits version-free purls,
  debian emits version-qualified ones. purl-keyed joins between them cannot
  match.

## Corpus as measured

| Metric | Value |
|---|---|
| `MavenArtifact` / `Package` / `Version` nodes | 2,604 each |
| Distinct `groupId:artifactId` | **1,926** |
| `PackageIdentity` nodes | 2,674 |
| `Dependency` nodes | 11,373 |
| `VersionConstraint` nodes | 6,492 |
| `DependencyExclusion` | 442 |
| `Repository` / `Tag` / `License` | 372 / 387 / 166 |
| Total triples in graph | 160,510 |

`packageName` holds the full coordinate (`antlr:antlr`, `ai.h2o:h2o-tree-api`),
so 1,926 is a true distinct-artifact count. The 678-node gap to 2,604 is
additional versions of already-counted artifacts.

Context on recency: this graph held **5 triples** in two separate samples on
2026-09-09 and earlier on 2026-09-10, then jumped to 160,510 in the following
rebuild. Maven collection is newly functional, so these gaps are first-look
findings, not regressions.

## What the collector does emit

`emit_package_metadata` (`maven.rs:736-829`) writes, per artifact:

- `pkg:Package` + `maven:MavenArtifact` rdf:type
- `pkg:PackageIdentity` with `pkg:identityName`, `pkg:packageName`, and
  `pkg:purl`; `pkg:isVersionOf` linking version to identity
- `maven:groupId`, `maven:artifactId`, `pkg:packageName`
- `pkg:Version` with `pkg:versionString`, linked via `pkg:hasVersion`
- `pkg:partOfDistribution`
- `pkg:description`, `pkg:homepage` (conditional)
- `pkg:licenseName` + `pkg:hasLicense` → SPDX URI + `pkg:License` type
- `pkg:upstreamRepository` → `vcs:Repository` + `vcs:cloneUrl` (conditional on
  `<scm>`)
- `vcs:packagedFromTag` → `vcs:Tag` + `vcs:tagName` (skipped for `HEAD`)

`emit_resolved_dep` (`maven.rs:856+`) writes dependency edges with scope, type,
classifier, optional flag, exclusions, and version constraints.

This is a reasonable core. The gaps below are about what never reaches it.

---

## Gap A — No parent POM resolution

**Severity: high. Root cause of Gaps F and part of the low licence/SCM coverage.**

`PomMetadata.parent` is populated by the POM parser (`maven.rs:1508-1519`), but
its only consumer is `${parent.version}` property interpolation. Two
consequences follow from code, not inference:

1. **The parent POM is never fetched.** `collect_recursive` issues one
   `fetch_pom` per artifact (`maven.rs:1061`) for the artifact's own POM. There
   is no second fetch for `pom.parent`.
2. **`dependencyManagement` is only consulted locally.**
   `lookup_in_dependency_management` (`maven.rs:1719-1730`) iterates
   `pom.dependency_management` — the current POM's own block. An inherited
   `<dependencyManagement>` entry is unreachable.

Maven's dependency model is inheritance-first: a child POM routinely declares
`<dependency>` with no `<version>` because a BOM or `spring-boot-starter-parent`
supplies it. `resolve_dependency` (`maven.rs:1263-1315`) accepts that outcome —
`is_emittable` at line 1300 requires only `group_id` and `artifact_id` to be
free of unresolved `${...}`:

```rust
let is_emittable = !contains_unresolved(&group_id) && !contains_unresolved(&artifact_id);
```

so the edge is emitted with `version: None`.

**Measured on the live index:**

| | |
|---|---|
| `pkg:Dependency` nodes | 11,373 |
| with a `pkg:hasVersionConstraint` | 6,492 |
| **unversioned** | **4,881 (43%)** |

Separately, the parent-child artifact relationship is **never emitted as a
triple**. There is no `maven:parentArtifact` or equivalent, so BOM structure,
starter-parent hierarchies, and multi-module aggregation are invisible to any
query.

**Fix:** fetch `pom.parent` recursively (bounded, with cycle detection), merge
inherited `<dependencyManagement>`, `<licenses>`, `<scm>`, and `<properties>`
per Maven's resolution rules, and emit the parent relationship. This is the
largest change proposed here and the highest value.

## Gap B — Maven Central search API almost entirely discarded

**Severity: high. Cheapest fix in this report.**

```rust
#[derive(Debug, Deserialize)]
struct SearchDoc {
    #[serde(rename = "latestVersion")]
    latest_version: String,
}
```

That is the complete struct (`maven.rs:57-60`). The same Solr response carries
at minimum `timestamp` (release time, epoch milliseconds), `p` (packaging), `ec`
(extension/classifier list), `versionCount`, and the `g`/`a`/`v` coordinates.

**Consequences:**

- **No release date exists for any Maven artifact.** Every temporal question is
  therefore structurally unanswerable for Maven regardless of how many artifacts
  are collected — `CQ-TEMP-01/02/03`, `CQ-PM-10` (update frequency),
  `CQ-SCR-06` (patch lag by distribution), `CQ-SCR-09` (MTTR).
- **No packaging type**, so `jar`, `pom`, `war`, and `bundle` artifacts are
  indistinguishable. A `pom`-packaged BOM looks like a shipping library.

Confirmed absent: `grep -icE "timestamp|packaging|versionCount"` over
`maven.rs` returns **0**.

**Fix:** add two `#[serde]` fields and emit `pkg:releaseDate` (or the
ontology's existing temporal predicate) plus a packaging literal. Two fields
unlock a whole CQ family.

## Gap C — No developer or maintainer extraction

**Severity: high.**

`grep -icE "developers|contributor|organization"` over all 5,615 lines returns
**0**. POM `<developers>`/`<contributors>` is Maven's canonical maintainer
source, and `<organization>` its canonical vendor field.

**Consequence:** Maven artifacts are absent from every maintainer-oriented
question — `CQ-PM-05` (packages by maintainer), and the social-risk family
`CQ-SCR-01` (bus factor), `-02` (maintainer overload), `-03` (orphan risk),
`-04` (turnover), `-05` (cross-distro maintainer overlap). `CQ-SCR-01` returns
68,588 rows on the live index; none of them are Maven.

**Fix:** parse `<developers>` into the same maintainer/agent shape the RPM and
Debian collectors already emit, so Maven joins the existing queries rather than
needing new ones.

## Gap D — No checksums or digests

**Severity: medium.**

`grep -icE "sha1|checksum|digest"` returns **0**. Maven Central publishes
`.sha1` and `.md5` beside every artifact, and `.asc` signatures for most.

**Consequences:** `CQ-PROV-01` through `-04` are 4/4 empty. Maven cannot
participate in the attestation modelling `enrich_npm_provenance.rs` already
performs for npm, and there is no integrity anchor to support rebuild
comparison of Java artifacts.

**Fix:** one additional HTTP GET per artifact for `<artifact>.jar.sha1`, emitted
as the ontology's existing digest predicate. Note the cost: it doubles request
volume, so it should respect the existing `HttpCache`.

## Gap E — 28% dangling dependency targets, unmeasured

**Severity: medium (transparency, not correctness).**

Live measurement:

| | |
|---|---|
| `pkg:PackageIdentity` nodes | 2,674 |
| identities reachable via `pkg:isVersionOf` from a `Package` | 1,926 |
| **dangling** | **748 (28%)** |

`emit_resolved_dep` mints an identity URI for every dependency target, while the
bounded BFS (`depth` / `fan_out`, see `test-corpus.toml`) stops before
collecting many of them. The result is 748 identity nodes with a name and purl
but no artifact, version, licence, or SCM.

This is a defensible consequence of bounded traversal. The problem is that
**nothing reports it**, so a graph consumer cannot distinguish "this artifact
has no licence" from "this artifact was never fetched."

**Fix:** emit a marker on identities that were referenced but not collected
(or a per-run counter in the collector's output), so completeness is queryable.

## Gap F — Low field completeness where the feature exists

**Severity: medium. Largely downstream of Gap A.**

Across the 2,604 `Package` nodes:

| Field | Present | Coverage |
|---|---|---|
| `pkg:description` | 2,008 | 77.1% |
| `pkg:homepage` | 1,323 | 50.8% |
| `pkg:licenseName` | 988 | **37.9%** |
| `pkg:upstreamRepository` | 801 | **30.8%** |
| `vcs:packagedFromTag` | 391 | **15.0%** |

The emission code for all five is present and correct. In Maven, `<licenses>`
and `<scm>` are very commonly declared once in a parent POM and inherited by
every module — so these low numbers are predicted by Gap A rather than by a
defect in the emitters. Fixing parent resolution should lift licence and SCM
coverage substantially with no change to `emit_package_metadata`.

---

## Not collector faults

Three empty queries have causes outside `maven.rs`:

- **`CQ-MVN-04` (Source Location for an Artifact Version)** — the query asks
  for `org.springframework:spring-core` 6.2.0. The corpus contains **zero**
  `org.springframework` artifacts. The model supports the query; the corpus does
  not contain its subject. Pure coverage.
- **`CQ-MVN-05` (Source Diff to Previous Version)** — requires
  `vcs:correspondingPackageVersion`, `vcs:hasDiff`, `vcs:previousRelease`,
  `vcs:diffUrl`, `vcs:linesAdded`/`linesDeleted`/`filesChanged`. No collector
  emits these; they are diff-enricher output (cf. `enrich_diff.rs`).
- **`CQ-PID-01` (High-Confidence Cross-Ecosystem Matches)** — requires
  `pkg:hasPackageRelationship`, `pkg:relationshipTarget`, `pkg:matchMethod`,
  `pkg:matchConfidence`. Cross-ecosystem identity enricher output, absent from
  the promoted index.

For the record, three Maven CQs **do** now answer: `CQ-MVN-01` (4 rows, 99ms),
`CQ-MVN-02` (4 rows, 226ms), `CQ-MVN-03` (2 rows, 54ms).

## Cross-collector inconsistency: purl versioning

Both collectors place `pkg:purl` on `PackageIdentity`, which is consistent. The
values are not:

| Graph | Example purl | Identities | Packages |
|---|---|---|---|
| `maven` | `pkg:maven/org.bouncycastle/bcprov-jdk15on` | 2,674 | 2,604 |
| `debian/trixie` | `pkg:deb/debian/node-js-beautify@1.15.4%2Bdfsg%2B~cs1` | 103,210 | 68,755 |
| `debian/trixie/arm64` | `pkg:deb/debian/efp@1.6-3?arch=arm64` | 68,126 | — |

Maven emits **version-free** purls; debian emits **version-qualified** ones,
which is why debian has more identity nodes than packages. Any purl-keyed join
across the two ecosystems will fail to match.

Version-free is the more defensible choice for a node whose purpose is
version-independent identity — but the decision should be made once and applied
uniformly, since `CQ-PID-01` and the cross-ecosystem equivalence queries
(`CQ-XD-01`, `CQ-XD-05`) depend on it.

## Recommended order

1. **Parent POM fetch + inherited `dependencyManagement`/`licenses`/`scm`**
   (Gap A). Fixes 43% unversioned edges; should lift Gap F's licence and SCM
   coverage as a side effect. Largest effort, largest payoff.
2. **Two serde fields for `timestamp` and `p`** (Gap B). Smallest change in this
   report; unlocks every temporal Maven query.
3. **Parse `<developers>`** (Gap C). Unlocks the `CQ-SCR` family for Maven by
   reusing the maintainer shape RPM/Debian already emit.
4. **Fetch `.sha1`** (Gap D). Unlocks `CQ-PROV`; respect `HttpCache` given the
   extra request per artifact.
5. **Report dangling-target count** (Gap E). Makes bounded-traversal
   completeness queryable instead of invisible.
6. **Decide purl versioning** once, across collectors.

Items 2, 3, and 5 are small and independent of item 1.

## Method notes and limitations

- All measurements are against a single promoted index sampled on 2026-09-10.
  That index changed three times in roughly 24 hours (107.4M → 114.2M → 141.0M
  triples), so the percentages above describe one snapshot, not a stable state.
- Code findings are from reading `maven.rs` at commit `ff42b0c`; the negative
  findings (Gaps B, C, D) rest on case-insensitive greps returning zero and
  should be cheap to re-verify.
- Gap F's attribution to Gap A is reasoning about Maven's inheritance
  conventions, not a measurement. Confirming it means resolving parents for a
  sample of the 1,616 artifacts lacking a licence and checking how many inherit
  one.
- No claim is made about collector *performance*; this analysis is about
  functional completeness only.
