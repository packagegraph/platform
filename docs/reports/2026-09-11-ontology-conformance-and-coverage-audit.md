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

The headline is not "the data is broken." Most violations trace to a handful of
places where the ontology and the collectors disagree about a name or a
datatype. Fix the disagreements and the violation count collapses.

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

This is a **model gap, not a collector bug**, everywhere the class already
carries an equivalent name property. `pkg:Capability` instances carry exactly
two predicates — `rdf:type` and `pkg:capabilityName` (1,595,057 each) — and
nothing else. The shape demands both `rdfs:label` and `pkg:capabilityName`,
which is redundant.

`Vulnerability` is the exception and **is** a collector bug: the shape calls
`rdfs:label` the canonical OSV/CVE identifier. `graph/security/osv` has **0**
violations; `graph/cve/nvd` has **46,010 of 52,268 (88%)**. The NVD collector
omits what the OSV collector emits.

**Recommendation:** drop the redundant `rdfs:label` `minCount` from the six
shapes whose class has a name property; fix the NVD collector to emit
`rdfs:label`.

## Finding 2 — the ontology contradicts itself on `PackageIdentity`'s name

`PackageIdentityShape` requires `pkg:identityName`. Reality:

| Predicate on `PackageIdentity` | Uses |
|---|---|
| `pkg:packageName` | 4,963,521 |
| `pkg:identityName` | 2,674 |

Both are declared `owl:DatatypeProperty` in `core.ttl` (lines 1112 and 583).
`core.shacl.ttl` uses `packageName` in four shapes and `identityName` in exactly
one — `PackageIdentityShape`, line 278. So the reported **99.94% violation rate
is an ontology bug**, not a data problem. The collectors are consistent; the
shape is the outlier.

**Recommendation:** change `PackageIdentityShape` to `pkg:packageName` and
either deprecate `identityName` or document what distinguishes it. This single
edit removes 4.25M violations.

## Finding 3 — datatype mismatches, six of them at 100%

A 100% datatype violation means the collector emits one literal type and the
shape expects another. These are unambiguous and cheap to fix.

| Constraint | Violations | Rate |
|---|---|---|
| `EPSSAssessment.epssScore` | 9,026 | 100% |
| `EPSSAssessment.epssPercentile` | 9,026 | 100% |
| `TransparencyLogEntry.logIndex` | 142 | 100% |
| `Forge.forgeUrl` | 17 | 100% |
| `Builder.builderId` | 1 | 100% |
| `CVSSScore.baseScore` | 64,568 | 67.0% |
| `Repository.repositoryURL` | 24,531 | 17.0% |
| `PackageIdentity.purl` | 2,674 | 0.06% |

`CVSSScore.baseScore` at 67% (not 100%) means *some* producers get it right —
worth finding which, since that settles whether the shape or the collector is
wrong.

## Finding 4 — real data-quality violations

These are neither naming nor datatype artifacts. They are the findings worth
filing as collector bugs.

| Constraint | Violations | Rate | Reading |
|---|---|---|---|
| `PackageIdentity.purl` `minCount` | 3,826,660 | 89.9% | purl emitted by only a few collectors |
| `Dependency.hasVersionConstraint` `sh:class` | 1,385,911 | 17.5% | constraint node not typed `VersionConstraint` |
| `CVSSScore.baseScore` `minCount` | 31,863 | 33.0% | a third of CVSS scores have no base score |
| `Dependency.dependencyTarget` `sh:class` | 293,331 | 3.7% | target not under `PackageEntity` |
| `RangeEvent.eventVersion` `maxCount` | 22,781 | 7.1% | multiple versions on one event |
| `CVSSScore.vectorString` `maxCount` | 7,513 | 7.8% | duplicate vector strings |
| `PackageIdentity.purl` `maxCount` | 9,422 | 0.22% | more than one purl per identity |
| `VersionConstraint.versionConstraintValue` `maxCount` | 14,014 | 0.60% | |
| `NixPackage.attrPath` `maxCount` | 1,689 | 1.5% | |
| `Person.name` `maxCount` | 218 | 0.84% | |

`PackageIdentity.purl` at 89.9% missing is the one to weigh first: purl is the
cross-ecosystem join key, and nine in ten identities lack it.

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

## Recommended sequence

Do **not** turn on a CI gate first. 43 constraints violate today; a gate would
go permanently red and tell you nothing about whether the shapes or the
collectors are wrong.

1. **Ontology fixes** — `PackageIdentityShape` → `packageName` (−4.25M
   violations); drop redundant `rdfs:label` `minCount` from the six shapes whose
   class has a name property (−5.24M); declare the 28 undeclared properties and
   2 classes, or remove them from the collectors.
2. **Collector fixes** — NVD `rdfs:label` (46,010); the six 100% datatype
   mismatches; `CVSSScore.baseScore` missing on 33%.
3. **Re-run both scripts.** Everything above is mechanical; the residue is the
   real backlog.
4. **Then gate.** Coverage regression is the cheap, CI-able one: baseline the
   numbers and fail when they drop. Conformance is a nightly job — the full
   sweep is 300 queries and the `PackageIdentity` drill-down alone took 42.9s.
5. **Fix the upstream script or delete it.** `production_shacl_validate.py`
   exits 0 on total failure; leaving it in place is worse than having nothing,
   because it looks like coverage.

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
