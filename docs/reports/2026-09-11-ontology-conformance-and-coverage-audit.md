# Ontology conformance and coverage audit

**Date:** 2026-09-11
**Endpoint:** `https://packagegraph.di.riseproject.dev` (QLever, 52 named graphs, ~200M triples)
**Ontology:** `packagegraph/ontology` @ `v0.13.0` — 75 modules, 36 SHACL shape files, 115 `sh:targetClass`
**Method:** every SHACL property constraint compiled to SPARQL and counted exactly, corpus-wide. No sampling.
**Scripts:** `etl/scripts/ontology-shape-check.py`, `etl/scripts/ontology-coverage-check.py`

## Summary

Two questions, answered separately.

**Conformance — is what we emit valid?** 300 constraints checked against classes
that have data. **43 violate, 257 are clean, 0 errored.**

**Coverage — do we emit anything at all?** Of packagegraph-owned terms:

| | Populated | Total | |
|---|---|---|---|
| Classes | 78 | 281 | 27.8% |
| Properties | 212 | 1005 | 21.1% |
| `sh:targetClass` shapes with instances | 62 | 115 | 53.9% |

And the inverse: **28 properties and 2 classes are emitted, at volume, that the
ontology never declares.** Including `rpm:rpmProvides` (6.56M uses),
`rpm:rpmRequires` (4.51M), `deb:debDepends` (1.46M), and `core:checksum`
(1.13M) — none of which appear anywhere in the ontology, not even in a shape.

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

## Finding 3 — datatype mismatches: all eight are the collector's side

Every one is a compatible-but-different literal type, so no value is wrong —
only its declared type. Actual types measured per constraint:

| Constraint | Shape expects | Collector emits | Triples |
|---|---|---|---|
| `Repository.repositoryURL` | `xsd:anyURI` | `xsd:string` | 432,667 |
| `CVSSScore.baseScore` | `xsd:decimal` | `xsd:double` | 92,014 |
| `EPSSAssessment.epssScore` | `xsd:decimal` | `xsd:double` | 9,026 |
| `EPSSAssessment.epssPercentile` | `xsd:decimal` | `xsd:double` | 9,026 |
| `Forge.forgeUrl` | `xsd:anyURI` | `xsd:string` | 1,191 |
| `PackageIdentity.purl` | `xsd:anyURI` | `xsd:string` | 2,674 |
| `TransparencyLogEntry.logIndex` | `xsd:long` | `xsd:int` | 142 |
| `Builder.builderId` | `xsd:anyURI` | `xsd:string` | 1 |

Three reasons to fix these on the collector side rather than relax the shapes:

1. **`purl` proves the codebase already agrees with the shape.** 662,690 purl
   literals are correctly `xsd:anyURI`; only 2,674 are `xsd:string`. This is an
   inconsistency among emitters, not a shape that nobody can satisfy.
2. **`logIndex` as `xsd:int` is a latent overflow bug.** `xsd:int` is 32-bit
   and Rekor transparency-log indices are already in the hundreds of millions.
   The shape's `xsd:long` is correct and the collector will break on its own.
3. **`decimal` is the right type for CVSS and EPSS scores.** They are exact
   decimal quantities; `xsd:double` is binary floating point, so values round-
   trip inexactly and equality comparisons in SPARQL become unreliable.

## Finding 4 — real data-quality violations

These are neither naming nor datatype artifacts. They are the findings worth
filing as collector bugs.

| Constraint | Violations | Rate | Diagnosed cause |
|---|---|---|---|
| `PackageIdentity.purl` `minCount` | 3,826,660 | 89.9% | RPM-family + openSUSE collectors never emit purl |
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
emit it. purl is the cross-ecosystem join key, so fixing `rpm.rs` alone
recovers the bulk of 3.8M.

**Caveat on `maxCount` counts.** The figures above are same-graph counts, which
is the honest scope: `shape-check.json` counts values across the union of all
named graphs, so a subject appearing in several graphs with a different value
in each registers as a violation that does not exist within any single graph.
The two forms also count different units (distinct subjects vs. subject/graph
pairs), so they are not directly comparable. Only
`DistributionRelease.repoType` was purely a union-graph artifact (1 → 0); the
rest are real within individual graphs.

## Finding 5 — undeclared vocabulary

28 properties and 2 classes are emitted that the ontology never declares.
Highest-volume:

| Term | Uses |
|---|---|
| `rpm:rpmProvides` | 6,558,436 |
| `rpm:rpmRequires` | 4,507,102 |
| `deb:debDepends` | 1,456,519 |
| `core:checksum` | 1,129,117 |
| `deb:debProvides` | 363,480 |
| `core:upstreamPackageVersion` | 186,722 |
| `nix:attrPath` | 116,156 |
| `rpm:RPMGroup` | 523,728 |

`rpmProvides`, `rpmRequires` and `debDepends` appear **nowhere** in the
ontology — not in a module, not in a shape. `core:checksum` does not exist
either; only `choco:checksum` does. `nix:attrPath` and `nix:NixPackage` appear
*only* in `nix.shacl.ttl`, so a shape constrains a class and property its own
module never defines — which means the ontology's `make validate` does not
check that shapes reference declared terms.

Anyone writing queries from the published ontology cannot discover 12.7M
triples' worth of predicates.

## Finding 6 — 53 of 115 shapes have no data, and 203 classes are unpopulated

Shapes exist for classes that were never populated: `CVE`, `Checksum`,
`DataSnapshot`, `DebPackage`, `GoModule`, `HexPackage`, `Image`, `InstalledFile`,
`Contributor`, `CodeMetrics`, `BitBakeRecipe`, `BuildrootPackage`, `Ebuild`,
`Branch`, `Diff`, `Feed`, and 37 more.

This is the check that speaks to the **40 of 65 competency questions returning
zero rows** found while benchmarking. Empty CQ results are a coverage failure,
not a conformance failure — the data is not wrong, it is absent. Note
`graph/nuget` and `graph/hex` hold **6 triples each**, and
`graph/enrichment/forge-version` holds 14.

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
3. **`maven#MavenEcosystem` is an ontology class URI used as an instance** of
   `pkg:Ecosystem`. Almost certainly meant to be `d/ecosystem/maven`, which
   exists.
4. **`pkg:partOfEcosystem` has 0 uses.** The property connecting
   `Distribution` to `Ecosystem` is declared and shaped but never emitted, so
   the 18 `Ecosystem` nodes float unattached to the distribution graph.

### Step 2 — collectors, by expected impact

| Fix | Where | Removes |
|---|---|---|
| Emit `pkg:identityName` on `PackageIdentity` instead of `pkg:packageName` (domain violation) | identity emitter, all collectors | 4,253,705 |
| Emit `rdfs:label` on `PackageIdentity` | identity emitter | 4,256,379 |
| Emit `purl` for RPM-family + openSUSE | `rpm.rs` | ~3.8M |
| Emit `rdf:type pkg:VersionConstraint` on constraint nodes | version-constraint emitter | 1,405,318 |
| Emit `rdfs:label` on `Capability` (a description, not the name) | capability emitter | 908,627 |
| `Repository.repositoryURL`: `xsd:string` → `xsd:anyURI` | vcs emitter | 432,667 |
| `CVSSScore.baseScore`: `xsd:double` → `xsd:decimal` | `enrich_nvd.rs` / OSV | 92,014 |
| Emit `rdfs:label` on `Vulnerability` (canonical CVE/OSV id) | NVD + Alpine secdb | 78,899 |
| Give `pkg:License` nodes any content at all | license emitter | 19,858 |
| Emit `CVSSScore.baseScore` where missing | OSV collector | 30,016 |
| Emit `rdf:type` on dependency-target nodes | dependency emitter | 29,555 |
| `EPSS` score/percentile: `xsd:double` → `xsd:decimal` | `enrich_epss.rs` | 18,052 |
| Dedupe `RangeEvent.eventVersion` | OSV range emitter | 22,781 |
| Dedupe `VersionConstraint.versionConstraintValue` | version-constraint emitter | 14,031 |
| Dedupe `CVSSScore.vectorString` | CVSS emitter | 7,519 |
| Resolve conflicting `purl` per identity | purl emitter | 7,215 |
| `Forge.forgeUrl`, `Builder.builderId`, `purl`: → `xsd:anyURI` | respective emitters | 3,866 |
| `TransparencyLogEntry.logIndex`: `xsd:int` → `xsd:long` | attestation emitter | 142 |
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
