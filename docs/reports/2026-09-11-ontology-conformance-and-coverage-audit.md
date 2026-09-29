# Ontology conformance and coverage audit

**Date:** 2026-09-11
**Endpoint:** `https://packagegraph.di.riseproject.dev` (QLever, 52 named graphs, ~200M triples)
**Ontology:** `packagegraph/ontology` @ `v0.13.0` — 37 deployed modules, 36 SHACL shape files, 115 `sh:targetClass`
**Method:** a subset of SHACL property constraints compiled to SPARQL and counted exactly, corpus-wide. No sampling, and no full-SHACL claim — see the scope note under Summary.
**Scripts:** `etl/scripts/ontology-shape-check.py`, `etl/scripts/ontology-coverage-check.py`
**Status:** several findings below carry corrections from an independent review of PR #25 (`docs/reviews/pr25-f172603/`). Each correction is marked inline.

## Summary

Two questions, answered separately.

**Conformance — is what we emit valid?** 300 constraints checked against classes
that have data. **43 violate, 257 are clean, 0 errored.**

> **This is not a conformance verdict, and an earlier draft of this report
> presented it as one.** The checker translates five SHACL components —
> `minCount`, `maxCount`, `class`, `datatype`, `in` — and reported
> `skipped: 0`, which read as full coverage. It was not skipping zero; it was
> not looking. Against the pinned shapes it leaves **17 constraints
> untranslated** (4 `sh:pattern`, 6 `sh:minInclusive`, 5 `sh:maxInclusive`,
> 1 `sh:nodeKind`, 1 `sh:minLength`) and cannot reach **11 NodeShapes without
> `sh:targetClass`, 7 `sh:targetSubjectsOf` targets, and 15 `sh:sparql`
> constraints** at all.
>
> The consequence is concrete: `pkg:PURLShape`'s `sh:pattern` is one of the
> four never evaluated, so every versionless Maven purl in the corpus violates
> a constraint this audit reported nothing about. The checker now enumerates
> every `sh:*` term on each property shape and emits one skip record per
> untranslated component, reports its shape-discovery gaps, and exits non-zero
> on an incomplete run. Read the numbers above as *"43 violations among the
> constraints this subset covers"*, not as a corpus verdict.

### Re-run with the corrected checker

Re-run 2026-09-11 against the same endpoint, after the corrections above.
The result is **complete**: `checked=300 violating=43 clean=257 errors=0`,
with `unexecuted=0` and `target counts failed=0`, exiting 0. The violating and
clean counts are unchanged from the first run even though the corpus grew by
~338,000 identities, which is a reasonable stability signal for the subset
being measured.

What is new is that the scope is now stated rather than implied:

| | |
|---|---|
| Components translated | `minCount`, `maxCount`, `class`, `datatype`, `in` |
| Constraints translated | 536 |
| Constraints **not** translated | **17** — `minInclusive` 6, `maxInclusive` 5, `pattern` 4, `nodeKind` 1, `minLength` 1 |
| Shape targets unreachable | **33** — 11 NodeShapes without `sh:targetClass`, 15 `sh:sparql`, 7 `sh:targetSubjectsOf` |

**43 violating constraints is 41 distinct defects.** Five NodeShapes target
`PackageIdentity`, so `purl`'s `maxCount` (12,746) and `datatype` (2,674) are
each counted twice — the double-count predicted in Finding 7, now visible in
the results rather than inferred.

Two constraints improved by an order of magnitude since the first run, both in
areas PR #32 touched (forge URL normalization and repository IRI minting).
The coincidence is strong but I have not verified the attribution:

| Constraint | First run | Re-run | Change |
|---|---|---|---|
| `Repository.repositoryURL` datatype | 432,667 | **24,531** | −94% |
| `Forge.forgeUrl` datatype | 1,191 | **17** | −99% |

Two constraints appear that earlier tables did not list, both at 100%:
`Commit.commitTimestamp` `minCount` (1,211) and
`ProvenanceAttestation.attestationDigest` `minCount` (142).

Everything else moved only with corpus growth. `PackageIdentity.identityName`
`minCount` stands at **4,591,302 / 4,593,976 (99.94%)**, which confirms the
endpoint is still serving pre-#37 data — the identity fix had not yet reached
the index at the time of this run, so these figures are the "before" baseline
against which that work should be measured.

Coverage was re-run too, and the script now reports packagegraph-owned figures
directly — **78/281 classes (27.8%)** and **212/1005 properties (21.1%)** —
rather than the report having to correct a raw 6.5% by hand. Its denominators
previously came from globbing the ontology checkout, which pulled in
schema.org, SHACL's own vocabulary and negative test fixtures; they now come
from the 37 deployed modules.

Raw output: `shape-check-2026-09-11b.json`, `coverage-2026-09-11c.json`.

**Coverage — do we emit anything at all?** Of packagegraph-owned terms:

| | Populated | Total | |
|---|---|---|---|
| Classes | 78 | 281 | 27.8% |
| Properties | 212 | 1005 | 21.1% |
| `sh:targetClass` shapes with instances | 62 | 115 | 53.9% |

And the inverse: **28 properties and 2 classes are emitted into the production
graph in a role the ontology does not declare.** Including `rpm:rpmProvides`
(6.56M uses), `rpm:rpmRequires` (4.51M), `deb:debDepends` (1.46M), and
`core:checksum` (1.13M) — none of which appear anywhere in the ontology, not
even in a shape — plus `rpm:RPMGroup` (523,728), which is declared as a class
and written as a predicate.

Scanning the collector source instead of the graph finds **61**: 60 terms with
no declaration at all, and that one role violation. The two figures count
different things and neither supersedes the other — the graph shows what
actually shipped, the source shows what the code can emit including paths that
have not run. Both are stated wherever they are used; earlier drafts quoted
"28+2" and "29+1" interchangeably, which was simply unreconciled arithmetic.

The headline: **the ontology is a sound specification and the collectors have
drifted from it.** Not one of the 43 violations is best fixed by weakening a
shape. An earlier draft of this report proposed four shape relaxations that
would have erased ~9.4M violations at a stroke; every one of them turned out to
be the ontology being right — see "The ontology is not the thing to change"
below. `make validate` on the ontology repo passes unmodified, which is the
short version of the same point.

## Methodology, and why not pyshacl

The ontology repo already contains `scripts/production_shacl_validate.py`
(written 2026-04-20, never executed — its report is still `TBD` placeholders).
It cannot produce a valid result as written:

1. It runs a CONSTRUCT with SPARQLWrapper's JSON return format and parses the
   result as JSON-LD. CONSTRUCT returns RDF, not SPARQL-JSON bindings, so the
   parse fails — and a bare `except` turns that into an empty graph, which the
   caller records as "No data found" and scores as neither pass nor fail. The
   script then **exits 0**. A silent pass.
2. Its `LIMIT n` bounds *triples*, not instances, with no `ORDER BY`. That cuts
   an arbitrary subject in half, so every truncated subject manufactures
   "missing required property" violations that do not exist in the data.
3. It loads 2 of the 36 shape files.
4. It targets `graph/security`, which does not exist. The real graph is
   `graph/security/osv`.

Beyond the bugs, the *approach* does not work here. pyshacl needs the data in
memory, so it needs a sample, and sampling a graph database produces artifacts
that swamp the signal:

- Subject-complete sampling leaves every `sh:class` constraint failing, because
  the linked node's `rdf:type` is outside the sample.
- Pulling those types in makes the linked nodes themselves validation targets
  carrying only a type triple, so they fail their own shapes' `minCount`s.

Measured on a 25-subject sample: **6 pass / 7 fail** without object types,
**0 pass / 13 fail** with them. Neither number means anything. Worked example:
`BinaryPackage` → `sh:class pkg:SourcePackage` sampled as 25/25 FAIL; the exact
corpus-wide count is **0**.

Compiling each constraint to a `COUNT` query instead is exact over all ~200M
triples, has no sampling error, and is cheap on QLever.

One correctness detail that bit us: SHACL's `sh:class` is satisfied by
`rdf:type/rdfs:subClassOf*`, not a direct `rdf:type`. Testing direct type only
reported `Dependency.dependencyTarget` at **7,917,911/7,917,911 (100%)**,
because `pkg:PackageEntity` is an abstract superclass nothing is typed as
directly. The correct subClassOf-aware form gives **293,331 (3.7%)**.

## Finding 1 — `rdfs:label` is required by 7 shapes and essentially never emitted

One root cause, seven constraints, 5.3M affected subjects.

| Class | Missing `rdfs:label` | Rate |
|---|---|---|
| `PackageIdentity` | 4,256,379 | 100% |
| `Capability` | 908,627 | 100% |
| `DataQualityIssue` | 63,252 | 100% |
| `License` | 11,891 | 100% |
| `DistributionRelease` | 31 | 100% |
| `Vulnerability` | 55,517 | 16.9% |
| `Distribution` | 11 | 45.8% |

All seven are collector-side. `rdfs:label` in this ontology is a
human-readable description, not a restatement of the name — the examples give
`pkg:Capability` both `pkg:capabilityName "libssl.so.3"` and
`rdfs:label "OpenSSL shared library libssl.so.3"`. Nothing in the corpus
supplies that second string.

| Class | What it actually carries | What is missing |
|---|---|---|
| `PackageIdentity` | `packageName`, purl, repo links | a description |
| `Capability` | `capabilityName` only | a description |
| `DataQualityIssue` | `issueType`, `severity`, `detectedAt` | a description |
| `License` | **`rdf:type` only** — 19,858 triples, nothing else | everything |
| `Distribution`, `DistributionRelease` | partial; 11–31 singleton nodes | name/label metadata |
| `Vulnerability` | `rdfs:label` *is* the canonical CVE/OSV id | the identifier |

`License` is the worst of them: every `pkg:License` node in the corpus is a
bare typed node with no identifier, name, or SPDX id at all.

`Vulnerability` splits by producer: `graph/security/osv` has **0** violations,
while `graph/cve/nvd` has **56,172** and the Alpine graphs another **~22,700**
(`alpine/edge/riscv64` 8,753, `alpine/v3.20` 6,987, `alpine/v3.20/aarch64`
6,987). Two collectors omit what a third emits correctly — which is the
clearest evidence that the requirement is satisfiable and the shape is right.

## Finding 2 — the collectors violate `packageName`'s declared domain

> **Status.** The migration described here covered 38 call sites but **missed
> the RPM `Provides:` path**, which reaches identities through the `_once`
> writers that the migration's pattern did not match. That path is
> `rpmProvides` — 6,558,436 triples, the highest-volume route in the corpus —
> so the "8,510,084 violations resolved" figure quoted when the migration
> landed was overstated. Fixed at `rpm.rs:1385` via
> `write_package_identity_once`, which preserves the per-file dedup that made
> that path use `_once` to begin with. Found by independent review of PR #25.

`PackageIdentityShape` requires `pkg:identityName`. Reality:

| Predicate on `PackageIdentity` | Uses |
|---|---|
| `pkg:packageName` | 4,963,521 |
| `pkg:identityName` | 2,674 |

This looks at first like the shape being the outlier — 99.94% of instances
disagree with it. It is not. The two properties are deliberately different:

- `pkg:identityName` — `rdfs:domain pkg:PackageIdentity`, defined as *"the
  package name as used for the version-agnostic identity... distinct from the
  versioned packageName on Package instances"* (`core.ttl:583`).
- `pkg:packageName` — `rdfs:domain pkg:Package` (`core.ttl:1112`).

The four other shapes that use `packageName` target `Package`, `MetaPackage`,
`PhantomPackage` and `SourcePackage` — all Package-family, where it is correct.
None of them targets `PackageIdentity`.

So emitting `packageName` on a `PackageIdentity` violates that property's
declared domain. Under RDFS entailment it infers every `PackageIdentity` to
also be a `Package`, collapsing precisely the version-agnostic/versioned
distinction the class exists to draw — and `PackageIdentity`'s own definition
says dependencies point at it *instead of* versioned instances.

**Recommendation:** fix the collectors to emit `pkg:identityName` on
`PackageIdentity` nodes. 4,253,705 violations, and a real semantic defect
rather than a naming quibble.

## Finding 3 — datatype mismatches: four of eight are the collector's side

Every one is a compatible-but-different literal type, so no value is wrong —
only its declared type. Actual types measured per constraint:

| Constraint | Shape expects | Observed in store | Triples | Whose side |
|---|---|---|---|---|
| `Repository.repositoryURL` | `xsd:anyURI` | `xsd:string` | 432,667 | collector |
| `CVSSScore.baseScore` | `xsd:decimal` | `xsd:double` | 92,014 | **store** |
| `EPSSAssessment.epssScore` | `xsd:decimal` | `xsd:double` | 9,026 | **store** |
| `EPSSAssessment.epssPercentile` | `xsd:decimal` | `xsd:double` | 9,026 | **store** |
| `Forge.forgeUrl` | `xsd:anyURI` | `xsd:string` | 1,191 | collector |
| `PackageIdentity.purl` | `xsd:anyURI` | `xsd:string` | 2,674 | collector — **fixed** |
| `TransparencyLogEntry.logIndex` | `xsd:long` | `xsd:int` | 142 | **store** |
| `Builder.builderId` | `xsd:anyURI` | `xsd:string` | 1 | collector |

### Correction: three of these are not the collector's side

This finding was headed "all eight are the collector's side". Four are not,
and the recommendation that followed from it would have changed emitters that
are already correct.

`enrich_nvd.rs:606` writes `"..."^^<xsd:decimal>` and `enrich_epss.rs` does the
same at four sites. `ntriples.rs`'s `write_integer` serialises `xsd:integer`,
not `xsd:int`. What the audit measured is what QLever **stores**: it
normalises `decimal` to `double` and the integer family to `int`. The emitted
RDF is right; the query result is a different representation of it.

Two consequences. There is nothing to fix for `baseScore`, `epssScore`,
`epssPercentile` or `logIndex` — 110,208 of the 546,741 triples in this table.
And the "latent 32-bit overflow" argument in the previous draft was
unsupported: `xsd:integer` is unbounded, so Rekor indices in the hundreds of
millions were never at risk from the collector. An `xsd:integer`-versus-`long`
mismatch against `attestation.shacl.ttl:180` does remain, but it is a shape
question, not an overflow.

The general lesson is that this checker cannot distinguish emitted RDF from
stored representation, because it only ever sees the latter. Datatype findings
need verifying against raw collector output before they are attributed.

The four genuine collector rows stand, and `purl` is the argument for fixing
them rather than relaxing the shapes: 662,690 purl literals are correctly
`xsd:anyURI` and only 2,674 were `xsd:string`, so it was an inconsistency among
emitters rather than a shape nobody can satisfy. Those 2,674 are fixed.

Credit: independent review of PR #25 (`docs/reviews/pr25-f172603/`).

## Finding 4 — real data-quality violations

These are neither naming nor datatype artifacts. They are the findings worth
filing as collector bugs.

| Constraint | Violations | Rate | Diagnosed cause |
|---|---|---|---|
| `PackageIdentity.purl` `minCount` | 3,826,660 | 89.9% | dependency-target identities, which have no version to put in a purl — see below |
| `Dependency.hasVersionConstraint` `sh:class` | 1,385,911 | 17.5% | 1,405,318 constraint nodes are **untyped** |
| `CVSSScore.baseScore` `minCount` | 31,863 | 33.0% | all 30,016 in `graph/security/osv` |
| `Dependency.dependencyTarget` `sh:class` | 293,331 | 3.7% | 29,555 target nodes are **untyped** |
| `RangeEvent.eventVersion` `maxCount` | 22,781 | 7.1% | multiple versions on one event |
| `VersionConstraint.versionConstraintValue` `maxCount` | 14,031 | 0.60% | |
| `CVSSScore.vectorString` `maxCount` | 7,519 | 7.8% | duplicate vector strings |
| `PackageIdentity.purl` `maxCount` | 7,215 | 0.17% | conflicting purls on one identity |
| `NixPackage.attrPath` `maxCount` | 1,689 | 1.5% | |
| `Person.name` `maxCount` | 498 | 0.84% | |

Two diagnoses change the fix:

**The `sh:class` failures are missing type triples, not wrong targets.** Both
offending object sets are entirely `<UNTYPED>` — 1,405,318 version-constraint
nodes and 29,555 dependency-target nodes carry no `rdf:type` at all. The
corpus has 2,340,586 correctly typed `VersionConstraint` instances, so roughly
37% of constraint nodes are emitted without their type triple. One emitter
path, not a modelling disagreement.

**`purl` is missing by ecosystem, not at random.** The top graphs are
`opensuse/tumbleweed` (546,218), `fedora/rawhide` (511,603), `fedora/43`
(441,246), `fedora/42` (440,264) — RPM-family only. Maven, PyPI, npm and Cargo
emit it.

### Correction: it is not that `rpm.rs` never emits purl

An earlier draft concluded "RPM-family + openSUSE collectors never emit purl"
and put the fix at "emit purl in `rpm.rs`". That diagnosis was wrong, and the
fix it implied is not implementable.

`rpm.rs:849` *does* emit purl, correctly typed `xsd:anyURI`, for every binary
package it collects. What it does not emit purl for are the identity nodes it
creates for **dependency targets** — every `requires`, `provides`, `conflicts`
and `obsoletes` name becomes a `pkg:PackageIdentity` via
`package_identity_uri(distro, release, arch, dep_name)`. The code says so at
`rpm.rs:1267`: "Dependency targets point to canonical identity URI (no
version)". With dozens of dependencies per package across four RPM-family
distros, those nodes are the 3.8M.

They have no version by construction — an RPM `Requires: libssl.so.3` names a
capability, not a release — so no versioned purl can be synthesized for them.
This is why the volume is RPM-family-heavy: RPM records far more fine-grained
soname and file dependencies than Debian or the language ecosystems.

### The constraint set is internally inconsistent here

Three statements in the ontology cannot all hold:

1. `pkg:directlyDependsOn` is `rdfs:range pkg:PackageEntity`, and its own
   definition says "The target may be a concrete Package or a
   **version-independent PackageIdentity**." So a version-independent identity
   as a dependency target is explicitly sanctioned.
2. `pkg:PackageIdentityShape` requires `sh:minCount 1` on `pkg:purl`.
3. `pkg:PURLShape` constrains `pkg:purl` with `sh:pattern "^pkg:[a-z]+/.+@.+"`,
   which requires an `@version` component.

A version-independent identity cannot satisfy 2 and 3 together. Note also that
in the purl specification the version component is **optional** —
`pkg:rpm/fedora/bash` is a well-formed purl — so the pattern is stricter than
the standard it implements.

There is a second symptom of the same tension. `pkg:PackageIdentity` is
version-agnostic by definition (`identityName` is documented as "distinct from
the versioned `packageName`"), yet `rpm.rs` and `debian.rs` build the identity's
purl *with* the version in it. Where a release carries two versions of one
package, both write a different purl to the same identity URI — which is
exactly the `maxCount` violation at 7,215. A version-less purl on the identity
would make that class of violation impossible rather than merely rarer.

This one needs an ontology-owner decision, not a collector patch; it is filed
in the open questions below.

**Caveat on `maxCount` counts.** The figures above are same-graph counts, which
is the honest scope: `shape-check.json` counts values across the union of all
named graphs, so a subject appearing in several graphs with a different value
in each registers as a violation that does not exist within any single graph.
The two forms also count different units (distinct subjects vs. subject/graph
pairs), so they are not directly comparable. Only
`DistributionRelease.repoType` was purely a union-graph artifact (1 → 0); the
rest are real within individual graphs.

## Finding 5 — undeclared vocabulary

28 properties and 2 classes are emitted **into the production graph** in a role
the ontology does not declare. Highest-volume:

| Term | Uses | Problem |
|---|---|---|
| `rpm:rpmProvides` | 6,558,436 | undeclared |
| `rpm:rpmRequires` | 4,507,102 | undeclared |
| `deb:debDepends` | 1,456,519 | undeclared |
| `core:checksum` | 1,129,117 | undeclared |
| `rpm:RPMGroup` | 523,728 | declared `owl:Class`, written as a predicate |
| `deb:debProvides` | 363,480 | undeclared |
| `core:upstreamPackageVersion` | 186,722 | undeclared |
| `nix:attrPath` | 116,156 | constrained by a shape, declared nowhere |

These counts come from a historical `coverage-check.json`, not from a fresh
sweep, and are quoted as that run's results rather than as current production
state.

`rpmProvides`, `rpmRequires` and `debDepends` appear **nowhere** in the
ontology — not in a module, not in a shape. `core:checksum` does not exist
either; only `choco:checksum` does. `nix:attrPath` and `nix:NixPackage` appear
*only* in `nix.shacl.ttl`, so a shape constrains a class and property its own
module never defines — which means the ontology's `make validate` does not
check that shapes reference declared terms.

Anyone writing queries from the published ontology cannot discover 12.7M
triples' worth of predicates.

### The graph understates this by half

Counting from the graph only finds terms whose code path ran during a
collection. Scanning the collector source instead — which
`pg_collect::vocab` now does on every `cargo test` — finds **61**, spread
across 13 modules:

| Module | Findings | Module | Findings |
|---|---|---|---|
| `core` | 17 | `bsdpkg` | 4 |
| `vcs` | 16 | `metrics` | 3 |
| `deb` | 6 | `nix` | 3 |
| `rpm` | 5 | `chocolatey` | 2 |
| others (`bitbake`, `buildroot`, `flatpak`, `maven`, `xbps`) | 1 each | | |

The 31 the graph misses are not lower-risk — they are the same defect sitting
behind a branch that has not run yet. Sixteen are the GitHub repository
metadata in `enrich_github.rs` (`vcs:isArchived`, `vcs:isFork`,
`vcs:openIssuesCount`, `vcs:topic`, …), which will land at scale the first
time that enrichment runs broadly.

**What this scan does and does not cover.** 53 findings are predicate-position
uses and 4 are `rdf:type` objects, both read directly off a writer call site,
so the required role is known. The remaining 4 — the `deb:debDepends` family —
are assembled in the dependency-kind table at `debian.rs:965` and reach the
writer through a variable, so no call site states their role; they are checked
for declaration only and carry an explicit `unknown-role` marker. The gate
does not claim role coverage it does not have. It also skips `#[cfg(test)]`
modules: a term referenced only from a fixture is not emitted into any graph,
and counting it overstates what the collectors produce.

### Six are worse than undeclared: right name, wrong module

The gate also surfaced a class the graph cannot show at all. For six terms
the local name *is* declared — in another module, scoped by `rdfs:domain` to
a different class. The name resolves, so nothing looks wrong, but the triple
asserts something false:

| Emitted | Declared elsewhere as | Why the prefix swap is not the fix |
|---|---|---|
| `vcs:hasRelease` | `core:hasRelease` | domain `:Distribution`, range `:DistributionRelease` — a distro release like Debian 12, not a forge release |
| `vcs:repositoryStatus` | `core:repositoryStatus` | domain `:Repository`, the *package* repository, not the VCS one |
| `core:observedAt` | `vcs:observedAt` | domain `vcs:ForgeVersionObservation`; this subject is a `pkg:EmailObservation` |
| `core:versionConstraint` | `deb:` / `rpm:versionConstraint` | domain `deb:Dependency`; emitted generically by `emit/rdf.rs` for every ecosystem |
| `deb:installedSize` | `flatpak:` / `opkg:installedSize` | domain `flatpak:FlatpakApp` |
| `chocolatey:isPrerelease` | `nuget:isPrerelease` | domain `nuget:NuGetPackage`; `choco:ChocolateyPackage` is not a subclass of it |

Each needs a declaration in its own module, not a repointed prefix. Under
`rdfs:domain` entailment, repointing would infer that a VCS repository is a
`:Distribution` and a Chocolatey package is a `nuget:NuGetPackage`.

### `rpm:RPMGroup` is a role violation, not a missing declaration

`rpm:RPMGroup` is emitted 523,728 times, and both of the previous accounts of
it were wrong.

The first draft listed it among terms "the ontology never declares." It does
declare it: `rpm.ttl:550`, `rpm:RPMGroup a owl:Class`, with four individuals
typed by it.

The second draft therefore struck it as a false finding. That was the worse
error. `rpm.rs:948` and `emit/rpm_ext.rs:41` both do:

```rust
writer.write_literal(&pkg_uri, &format!("{RPM}RPMGroup"), group)?;
```

That uses a declared **class** URI as a **predicate**, with a string literal
object. The URI resolves, so a declared-or-not test says it is fine; what is
wrong is the role. Half a million triples assert a class as a property.

The remedy is neither a declaration nor a deletion: a real property —
`rpm:inGroup` — pointing at one of the four existing `rpm:RPMGroup`
individuals, so the group becomes a resource instead of a string.

This is also why the vocabulary gate now records and checks **roles** rather
than URIs. A membership test could not see this defect, and in its first form
it accepted it.

Credit: found by independent review of PR #25
(`docs/reviews/pr25-f172603/`).

## Finding 6 — 53 of 115 shapes have no data, and 203 classes are unpopulated

Shapes exist for classes that were never populated: `CVE`, `Checksum`,
`DataSnapshot`, `DebPackage`, `GoModule`, `HexPackage`, `Image`, `InstalledFile`,
`Contributor`, `CodeMetrics`, `BitBakeRecipe`, `BuildrootPackage`, `Ebuild`,
`Branch`, `Diff`, `Feed`, and 37 more.

This is the check that speaks to the **40 of 65 competency questions returning
zero rows** found while benchmarking. Note `graph/nuget` and `graph/hex` hold
**6 triples each**, and `graph/enrichment/forge-version` holds 14.

> **Correction: "empty CQ means absent data" does not hold.** An earlier draft
> said empty CQ results are "a coverage failure, not a conformance failure —
> the data is not wrong, it is absent." At least one is neither: ontology issue
> #7 shows the "unpatched vulnerabilities in web frameworks" CQ joins on an
> identity-side `pkg:hasUpstreamProject`, which no collector emits *by design*,
> because that property is `rdfs:domain :SourcePackage` and writing it on an
> identity would infer every such identity to be a SourcePackage. `forge.rs`
> carries an explicit comment saying so and four files have regression tests
> asserting the edge is absent. That CQ returns zero rows against correct data
> and always will.
>
> A zero-row CQ can mean absent data, a query defect, a graph or inference
> scope mismatch, or a filter that excludes everything. A term histogram cannot
> distinguish them, so "40 of 65" is a count of questions to investigate, not a
> measure of missing data. Each needs attributing individually before any of it
> is called a coverage failure.

Caveat on the headline percentage: a naive count says 6.5% of declared classes
are populated, but 872 of the 1,134 unpopulated classes are `schema.org` and
140 more are SHACL/dash vocabulary that the ontology files import. Restricted
to packagegraph-owned terms the real figures are 27.8% of classes and 21.1% of
properties.

## Finding 7 — overlapping shapes

Five NodeShapes target `PackageIdentity` (`PackageIdentityShape`, `PURLShape`,
`CPEShape`, `FreshnessStatusShape`, `PackageIdentitySourceShape`) and two target
`BinaryPackage`. Two constraint triples are therefore defined twice, and
`PackageIdentity.purl`'s `maxCount` and `datatype` violations are each reported
twice. Harmless to correctness, but it double-counts in any report and makes
"which shape failed" ambiguous.

## Resolution plan

Every violating constraint has been triaged to a side. Do **not** turn on a CI
gate first: 43 constraints violate today, so a gate would go permanently red
without telling anyone which side is wrong.

### The ontology is not the thing to change

An earlier draft of this report proposed four shape edits that would have
removed ~9.4M violations at a stroke: repoint `PackageIdentityShape` from
`pkg:identityName` to `pkg:packageName`, and drop the `rdfs:label` `minCount`
from `PackageIdentityShape`, `CapabilityShape` and `DataQualityIssueShape` on
the grounds that each class already had a name property.

That was wrong, and the ontology repo's own `make validate` caught it: `main`
passes unmodified, and repointing the shape broke 36 usages across 24 example
files. Checking the definitions explains why.

- `pkg:identityName` is `rdfs:domain pkg:PackageIdentity` and its definition
  reads *"distinct from the versioned packageName on Package instances"*.
  `pkg:packageName` is `rdfs:domain pkg:Package`. The distinction is
  deliberate. The four other shapes using `packageName` target `Package`,
  `MetaPackage`, `PhantomPackage` and `SourcePackage` — all Package-family,
  where it is correct. None targeted `PackageIdentity`.
- The `rdfs:label` requirements are not redundant either. The examples give
  `pkg:Capability` both `pkg:capabilityName "libssl.so.3"` and
  `rdfs:label "OpenSSL shared library libssl.so.3"` — a human-readable
  description, not a restatement of the name.

So the ontology and its examples are internally consistent, and every
violation is the collectors having drifted from a deliberate model. Weakening
the shapes to match the drift would have destroyed the only specification of
what the data is supposed to look like, and it would have done so under cover
of a 9.4M-violation improvement.

`PackageIdentity.packageName` is a concrete example of why this matters:
emitting it violates `packageName`'s declared domain, so under RDFS entailment
every `PackageIdentity` would be inferred to also be a `Package` — collapsing
exactly the version-agnostic/versioned distinction the class exists to draw.

Three things in the ontology repo are still worth doing, none of them a shape
relaxation:

- **Declare the 28 undeclared properties and 2 classes**, or delete them from
  the collectors. `rpm:rpmProvides` alone is 6.56M triples that no consumer can
  discover from the published ontology.
- **Add a shape-hygiene check to `make validate`** asserting that every
  `sh:path` and `sh:targetClass` resolves to a declared term. That check would
  have caught `nix:attrPath` and `nix:NixPackage`, which exist only in
  `nix.shacl.ttl` and are declared nowhere.
- **Collapse the five NodeShapes targeting `PackageIdentity`** (finding 7), or
  accept that violations against it are double-reported.

### Open questions for the ontology owners

These are modelling calls, not defects, and they should be decided by whoever
owns the model rather than inferred from what the collectors happen to emit:

1. **Should language registries be `pkg:Distribution`?** They are today, and
   the links are load-bearing — `d/distro/pypi` alone is the object of 4,172
   `pkg:partOfDistribution` triples. But the class is defined as *"A complete
   operating system distribution"*, and `pkg:Ecosystem` means something
   different here (a family of distributions, *"the Debian ecosystem including
   Ubuntu derivatives"*), already carrying `pkg:upstreamEcosystem` (137,205)
   and `sec:affectsEcosystem` (15,473) via `d/ecosystem/pypi`. Either widen
   the definition, or stop typing registries as distributions — but not both.
2. **Should `repoType` and `releaseCodename` be required?** `repoType`'s own
   definition is Koji/RPM vocabulary (*"Build repos are Koji build targets"*)
   with no analogue in a registry, and rolling releases have no codename by
   definition. If `DistributionRelease` is meant only for OS distro releases,
   the requirements are right and item 1 is the real issue.
3. ~~**`maven#MavenEcosystem` is an ontology class URI used as an instance**~~
   — **withdrawn.** `maven.ttl:161` declares it
   `maven:MavenEcosystem a owl:NamedIndividual, pkg:Ecosystem`. It is a
   correctly declared individual, not a class used as one, and the proposed
   migration to `d/ecosystem/maven` would have renamed a validly published
   term. The audit's declared-term set omitted named individuals, which is the
   same generator gap that made `slsa:L2` and `att:GPG` look undeclared.
4. **`pkg:partOfEcosystem` has 0 uses.** The property connecting
   `Distribution` to `Ecosystem` is declared and shaped but never emitted, so
   the 18 `Ecosystem` nodes float unattached to the distribution graph.
5. **Should a `PackageIdentity`'s purl carry a version?** This is the single
   highest-volume question in the audit: 3,826,660 `minCount` violations plus
   7,215 `maxCount` violations turn on it, and it cannot be resolved
   collector-side. The class is version-agnostic and `directlyDependsOn`
   explicitly sanctions version-independent identities as dependency targets,
   but `PURLShape`'s `sh:pattern` requires `@version` while
   `PackageIdentityShape` requires the property outright. The purl
   specification makes the version component optional. Three ways out:
   - **Version-less identity purl** (`pkg:rpm/fedora/bash`): relax the pattern
     to `^pkg:[a-z]+/[^@]+(@.+)?$`, and stop putting the EVR in the identity's
     purl in `rpm.rs`/`debian.rs`. Resolves both violation classes.
     Requires an ontology change and a re-collection. Note that relocating the
     versioned purl to `Package` is **not** available as a sub-option:
     `pkg:purl` is `rdfs:domain pkg:PackageIdentity`, and a second
     `rdfs:domain` intersects rather than widens, so it would infer every such
     Package to be a PackageIdentity. Keeping a versioned identifier on the
     versioned node needs its own property, or an export-layer field.
   - **Don't type unresolved dependency targets as `PackageIdentity`.** More
     faithful — an RPM `Requires: libssl.so.3` names a capability, and
     `pkg:Capability` already exists and is already used this way by
     `debian.rs` — but it changes the shape of the graph that consumers query
     for dependency traversal, and would need `dependencyTarget`'s `sh:class`
     revisited.
   - **Resolve dependencies to concrete providers at collection time**, so
     targets are real packages with real purls. Semantically best, most
     expensive, and impossible for dependencies satisfied outside the
     collected repo set.

### Step 2 — collectors, by expected impact

| Fix | Where | Removes |
|---|---|---|
| Emit `pkg:identityName` on `PackageIdentity` instead of `pkg:packageName` (domain violation) | identity emitter, all collectors | 4,253,705 |
| Emit `rdfs:label` on `PackageIdentity` | identity emitter | 4,256,379 |
| Add `rpm:inGroup` → an `rpm:RPMGroup` individual, replacing the class-URI-as-predicate | `rpm.rs`, `emit/rpm_ext.rs` | 523,728 |
| ~~Emit `purl` for RPM-family + openSUSE~~ — **blocked**, and misdiagnosed: `rpm.rs` already emits purl for real packages. The 3.8M are dependency-target identities with no version to put in a purl. Needs the purl-versioning decision in open question 5. | `rpm.rs` | ~3.8M |
| Emit `rdf:type pkg:VersionConstraint` on constraint nodes | version-constraint emitter | 1,405,318 |
| Emit `rdfs:label` on `Capability` (a description, not the name) | capability emitter | 908,627 |
| `Repository.repositoryURL`: `xsd:string` → `xsd:anyURI` | vcs emitter | 432,667 |
| ~~`CVSSScore.baseScore`: `xsd:double` → `xsd:decimal`~~ — **not a defect**: `enrich_nvd.rs:606` already emits `xsd:decimal`; the `double` is QLever store normalization | — | 92,014 |
| Emit `rdfs:label` on `Vulnerability` (canonical CVE/OSV id) | NVD + Alpine secdb | 78,899 |
| Give `pkg:License` nodes any content at all | license emitter | 19,858 |
| Emit `CVSSScore.baseScore` where missing | OSV collector | 30,016 |
| Emit `rdf:type` on dependency-target nodes | dependency emitter | 29,555 |
| ~~`EPSS` score/percentile: `xsd:double` → `xsd:decimal`~~ — **not a defect**: `enrich_epss.rs` already emits `xsd:decimal` | — | 18,052 |
| Dedupe `RangeEvent.eventVersion` | OSV range emitter | 22,781 |
| Dedupe `VersionConstraint.versionConstraintValue` | version-constraint emitter | 14,031 |
| Dedupe `CVSSScore.vectorString` | CVSS emitter | 7,519 |
| Resolve conflicting `purl` per identity — same root cause as the 3.8M; a version-less identity purl removes the conflict by construction | purl emitter | 7,215 |
| `Forge.forgeUrl`, `Builder.builderId`: → `xsd:anyURI` | respective emitters | 1,192 |
| ~~`purl`: → `xsd:anyURI`~~ — **done**, `maven.rs` was the only emitter using `xsd:string` | `maven.rs` | 2,674 |
| ~~`TransparencyLogEntry.logIndex`: `xsd:int` → `xsd:long`~~ — **not a defect**: `write_integer` emits unbounded `xsd:integer`; an `integer`-vs-`long` shape question remains | — | 142 |
| Emit `label` / `distributionName` / `repoType` on the 24–31 `Distribution` and `DistributionRelease` singletons | distro metadata emitter | ~100 |

The datatype rows are a single mechanical sweep — pick the right `xsd:` type at
the emit site. The dedupe rows need a look at *why* two values arrive, which
may be one upstream record parsed twice.

### Step 3 — one modelling question to settle, not fix

`Distribution.distributionName` and `DistributionRelease.repoType` are missing
on `gentoo`, `void`, `snap`, `rubygems`, `pypi`, `nuget` — the language
ecosystems and app stores, which are modelled as `Distribution` but have no
distribution name or repo type in any meaningful sense. Either they should not
be `Distribution`, or those properties should not be required. Volume is
trivial (one node per graph); the modelling answer is not.

### Step 4 — re-run, then gate

Re-run both checkers. Everything in steps 1–2 is mechanical, so what survives
is the real backlog. Then:

- **Undeclared and misused vocabulary — done, in `cargo test`.**
  `pg_collect::vocab` resolves every whole-string `format!("{PREFIX}Term")` in
  the crate against a checked-in manifest of declared terms *and their roles*,
  then checks each use against the role its argument position requires:
  predicate ⇒ property, `rdf:type` object ⇒ class, other object ⇒ class or
  individual. It runs offline in the normal test job and needs no endpoint.
  Verified against both bug classes: renaming `packageName` to `packageNaem`
  fails with "used as property, but not declared at all", and writing a
  declared class in predicate position fails with "used as property, but the
  ontology declares it only as class" — the `rpm:RPMGroup` defect.

  The manifest is generated from the 37-module `EXPECTED_FILES` allowlist in
  `sync-ontology.sh`, which is what actually ships, and records the ontology
  commit it was built from. An earlier version globbed `**/*.ttl` and swept 34
  non-deployed files — mostly negative SHACL fixtures — which inflated the
  declared set by 15 terms and would have let the gate accept vocabulary the
  published ontology does not have.

  The 61 pre-existing findings are baselined in a `KNOWN_BAD` list that is only
  allowed to shrink — a second test fails if an entry becomes declared in the
  role used, or stops being emitted that way, so the baseline cannot rot into
  noise.
- **Coverage regression in CI** — cheap (two queries), baseline the numbers,
  fail when they drop.
- **Conformance nightly** — 300 queries; the `PackageIdentity` drill-down alone
  took 42.9s, and some legitimate aggregates exceed the proxy's 60s
  `proxy_read_timeout` (a `License` histogram returned HTTP 504), so this
  belongs on the host against port 7001 rather than through the public proxy.

### Step 5 — delete or fix the upstream script

`production_shacl_validate.py` exits 0 on total failure. Leaving it in place is
worse than having nothing, because it looks like coverage.

## Also fixed during this audit

QLever's resource limits were binding, not headroom: systemd logged a "6G
memory peak" against `Memory=6g`, and the `pkg:Capability` predicate histogram
— an ordinary aggregate over one class — failed outright with "Tried to
allocate 250 kB, but only 114.5 kB were available" under `-m 4G`. Raised to
`-m 24G -c 4G -e 2G`, `Memory=32g`, and `-j 8`/`--cpus=8` (from 4 of 12 cores,
while the server idles ~0.5% under query load). The failing query now completes
in 42.9s. See `deploy/quadlet/qlever.container`.

## Reproducing

```bash
# Conformance: every property constraint -> one exact COUNT query
etl/scripts/ontology-shape-check.py \
  --shapes-dir ../ontology --out /tmp/shape-check.json

# Coverage: declared terms vs terms in use, two queries plus a local diff
etl/scripts/ontology-coverage-check.py \
  --ontology-dir ../ontology --out /tmp/coverage-check.json
```

Both are `uv` inline-dependency scripts — no venv to manage. Both default to
the public endpoint and are read-only.

The undeclared-vocabulary gate needs neither:

```bash
# Offline; reads etl/pg-collect/ontology-vocab.txt, no endpoint
cd etl/pg-collect && cargo test --lib vocab

# Regenerate the manifest after bumping etl/ONTOLOGY_VERSION
etl/scripts/gen-ontology-vocab.py --ontology-dir ../ontology
```
