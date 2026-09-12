# Cross-Ecosystem Package Identity and Bundled-Component Provenance — Design

Date: 2026-09-11
Status: Draft, pending review
Sub-project: 1 of 6 in the "nested package format" series (see §12)

## Revision note

This design was prompted by a question about a Fedora log line:

```
pg-collect@fedora-43-full[…]:   ghc-hoauth2 → 31 triples
```

None of those 31 triples connect `ghc-hoauth2` to the Haskell package it
repackages, even though `pg-collect-hackage` runs in production and collects
`hoauth2` on a schedule. The two datasets sit in the same graph, unjoined.

Investigation found that the ontology already declares the predicate for this
join, with a docstring describing this exact case, and that nothing has ever
emitted it.

**Revision 4**, after three review rounds. The one thing here that is genuine
invention rather than wiring is the per-capability `:UpstreamAssertion` (§2.2).

Revisions 1 and 2 both tried to carry the association on the flat `upstream*`
properties, and both were wrong for the same reason — those properties are
independently multi-valued on a single subject, so they express two unordered
sets rather than a set of pairs. Every join over them returns a cross-product.
That defect cannot be fixed by changing which triples are emitted; it requires
a node binding target, name, ecosystem, version, method, and confidence
together. §4.4, §5.1, and §7 are the three places the old model silently
produced wrong answers.

Revision 3 introduced that node but left it unable to bootstrap — its seed
query read the upstream name off the target identity, which does not exist in
the graph at the moment seeding runs — and under-constrained, admitting either
a build or an identity as owner and requiring neither method nor confidence.
Revision 4 adds `:assertionUpstreamName`, fixes the owner to exactly one
`PackageIdentity`, and specifies the full SHACL shape plus the one integrity
rule shapes cannot express (§11.15).

## 1. Overview

Package formats wrap other package formats. A Fedora RPM named `ghc-hoauth2`
is a repackaging of the Hackage package `hoauth2`; `python3-requests` is a
repackaging of PyPI `requests`; `rust-serde-devel` is a repackaging of the
crates.io crate `serde`. The same wrapping happens in Debian, Alpine, Nix,
Homebrew, and conda, and it happens *between* registries too (conda wraps
PyPI, many npm packages wrap native builds of C libraries).

Today the graph records this relationship as two dead-end literals:

```turtle
<d/pkg/fedora/43/x86_64/ghc-hoauth2>  pkg:upstreamEcosystem   <d/ecosystem/hackage> .
<d/pkg/fedora/43/x86_64/ghc-hoauth2>  pkg:upstreamPackageName "hoauth2" .   # literal
```

while the Hackage collector independently mints:

```turtle
<d/pkg/hackage/hackage/any/hoauth2>          a pkg:PackageIdentity ;
                                             pkg:packageName "hoauth2" .
<d/pkg/hackage/hackage/any/hoauth2/2.14.3>   a hkg:HackagePackage ;
                                             pkg:isVersionOf <…/any/hoauth2> .
```

Nothing joins them. A consumer must know to string-match a literal against a
`packageName`, guess the URI shape of the registry side, and know that the
name `g:a` the distro side records is written `g/a` in the registry
collector's URI — then percent-encoded.

Separately, a package can contain upstream code it does not declare as a
dependency. The `ghc-hoauth2` spec file vendors a second Hackage package:

```spec
%global binaryinstances binary-instances-1.0.6
Source1:  https://hackage.haskell.org/package/%{binaryinstances}/%{binaryinstances}.tar.gz
%ghc_lib_subpackage -l BSD-3-Clause %{binaryinstances}
```

The shipped artifact contains BSD-3-Clause code while its `License:` field
says `MIT`, and a CVE in `binary-instances` is invisible from `ghc-hoauth2`.
RPM has a standard declaration for this class of fact —
`Provides: bundled(...)` — and the codebase has **zero** references to it.

Note that resolving *this particular* `Source1` requires recursive `%global`
macro expansion, which is sub-project 3. What this design delivers on bundling
is the declaration-based path (§5), which covers the `rust2rpm` population
today; the Haskell subpackage path is sub-project 4 and the `go2rpm` population
needs §3.4's Go exclusion lifted first. The example is used here because it
makes the shape of the missing relationship legible, not because §5 resolves
it.

### Goals

1. Model each distro→upstream association as a qualified per-capability
   `:UpstreamAssertion` (§2.2) that binds target, ecosystem, optional version,
   method, and confidence together — and materialize
   `pkg:upstreamPackageIdentity` from it as the one-hop shortcut, for every
   ecosystem where the identifiers resolve deterministically.
2. Emit bundled-component assertions from evidence already present in data we
   already fetch — no new artifact downloads.
3. Complete the `distro → registry → forge` chain by wiring the nine registry
   collectors that currently discard upstream repository URLs into the
   existing `UpstreamProject` hub.
4. Correct three defects in the pinned ontology (§2.4): an undeclared
   `upstreamPackageVersion`, and wrong `rdfs:domain` on `upstreamEcosystem`
   and `upstreamPackageName` that makes four current emitters illegal.
5. Add version-level links where, and only where, both endpoints exist.

The landing order in §12 is load-bearing: two ontology PRs must be released
before any collector change, and `seed.rs` must be rewritten before the §7
deriver runs.

### Non-goals (this design)

- **Artifact inspection.** Go `vendor/modules.txt`, Maven shaded-JAR
  `META-INF/maven/*/pom.properties`, Python wheel `.libs/` directories, and
  `go version -m` on compiled binaries are the highest-fidelity bundling
  evidence that exists, and all of them require downloading and unpacking
  artifacts. That is a new collector class with real storage and time cost.
  Deferred to its own sub-project; the vocabulary here is designed to receive
  it without change.
- **RPM spec macro expansion.** Resolving `Source1:` to
  `binary-instances-1.0.6` requires recursive `%global` expansion that
  `collect_spec.rs::expand_macros` (collect_spec.rs:615-632) cannot do — it
  knows exactly four macros and never reads `%global` definitions. Sub-project
  3. This design therefore takes RPM bundling evidence only from
  `Provides: bundled(...)` in `primary.xml`, which needs no expansion.
- **Go `replace` directives.** These express identity *substitution* (a module
  replaced by a fork), not containment. Different relationship, different
  predicate, later.
- **Materialized transitive closure.** Direct edges are stored; multi-hop
  chains (`conda → pypi → forge`) are left to SPARQL property paths, matching
  the precedent set in commit `45a0aaf`. Revisit only if blast-radius queries
  demonstrate a need.
- **Version-aware seeding.** Making registry collectors fetch the specific
  versions distros pin would change the seed format, touch 11 collectors, and
  multiply collection volume. See §7 for why this design does not need it.
- **CPAN, Go, and conda.** All three name things in a namespace the registry
  collector does not key by, so none can be resolved by string transform.
  Excluded from v1 (§3.3, §3.4, §3.5) rather than emitted as dangling
  references. **Nine** ecosystems remain supported: hackage, pypi, npm, cargo,
  rubygems, maven, cran, hex, nuget.

## 2. Vocabulary

### 2.1 Already exists, never emitted

Verified in the sibling `packagegraph/ontology` repository. All three have
**zero** emit sites across `etl/pg-collect/src/`.

| Term | Location | Domain → Range |
|---|---|---|
| `pkg:upstreamPackageIdentity` | core.ttl:930-936 | `PackageIdentity` → `PackageIdentity` |
| `pkg:upstreamEquivalent` | core.ttl:921-928 | `PackageIdentity` ↔ `PackageIdentity` (symmetric) |
| `pkg:ecosystemPackage` | core.ttl:360-366 | `UpstreamProject` → `PackageIdentity` |
| `npm:bundledDependency` | npm.ttl:9-14 | `npm:NpmPackage` → `npm:NpmPackage` |

`pkg:upstreamPackageIdentity`'s definition (core.ttl:932) reads:

> "Links a distribution package identity to the upstream package identity in
> the native ecosystem (e.g., Debian python3-requests → PyPI requests).
> Replaces dead-end upstreamPackageName literal with a traversable object
> property — critical for supply chain analysis."

and `pkg:upstreamPackageName` (core.ttl:940) already carries its own
deprecation note: *"Retained for backward compatibility — prefer
upstreamPackageIdentity for new data."*

**This design uses those terms as-is.** The identity join itself needs no new
vocabulary; §2.4 covers the separate corrections required to existing terms.

### 2.2 New terms: the per-capability assertion

**Why a reified assertion is unavoidable.** The flat `upstream*` properties are
each independently multi-valued on a single subject. An RPM providing both
`python3dist(foo)` and `crate(bar)` yields:

```turtle
<pkg> pkg:upstreamEcosystem   <d/ecosystem/pypi>, <d/ecosystem/cargo> ;
      pkg:upstreamPackageName "foo", "bar" .
```

There is no pairing here — only two unordered sets. Any query joining the two
properties returns the full cross-product, so `cargo`/`foo` and `pypi`/`bar`
are as well-supported as the true pairs. This is why §4.4's seeding argument
and §7's deriver were both wrong in the previous revision: no amount of fixing
*which* triples are emitted can recover an association the model cannot
express.

**Why `:PackageRelationship` cannot be reused.** The existing reification
(core.ttl:1610-1647, emitted today by `enrich_repology.rs:231-253`) hangs off
an identity: `:hasPackageRelationship` has `rdfs:domain :PackageIdentity`
(core.ttl:1621) and `uris.rs:501` keys the node on `(identity_a, identity_b)`.
A `PackageIdentity` has many builds, so every build of `ghc-hoauth2` would
share one relationship node and the version cross-product would return in a
different shape. The declared upstream version belongs to a *build*, not to an
identity.

**The dedicated assertion.** One node per capability, binding all five facets
together:

```turtle
:UpstreamAssertion a owl:Class ;
    rdfs:label "Upstream Assertion"@en ;
    IAO:0000115 "A qualified, single-capability assertion that a distribution
      package derives from one specific upstream ecosystem package. Binds
      exactly one owning source identity, exactly one target upstream
      identity, the canonical upstream name, the ecosystem, the relation kind,
      and the method and confidence by which the association was established;
      optionally the specific source build and the upstream version that build
      declared. Exists because the flat upstream* properties are independently
      multi-valued on a single subject and therefore cannot express which name
      belongs to which ecosystem or version."@en ;
    rdfs:isDefinedBy : ;
    rdfs:subClassOf owl:Thing .

:hasUpstreamAssertion a owl:ObjectProperty ;
    IAO:0000115 "Links the owning package identity to one of its upstream
      assertions. The owner is always an identity, never a build: a build
      carries an assertion only indirectly, via :assertionSourceBuild."@en ;
    rdfs:domain :PackageIdentity ;          # NOT :PackageEntity — see below
    rdfs:range :UpstreamAssertion ;
    rdfs:isDefinedBy : .

:assertionTarget a owl:ObjectProperty ;     # required, exactly one
    rdfs:domain :UpstreamAssertion ;
    rdfs:range :PackageIdentity ;
    rdfs:isDefinedBy : .

:assertionUpstreamName a owl:DatatypeProperty ;  # required, exactly one
    IAO:0000115 "The canonical upstream package name in its native registry,
      after all ecosystem-specific normalization has been applied — the exact
      string the registry collector will use as its package name. Carried on
      the assertion itself so that consumers, including seed generation, can
      read it without dereferencing the target, which may not yet exist in the
      graph."@en ;
    rdfs:domain :UpstreamAssertion ;
    rdfs:range xsd:string ;
    rdfs:isDefinedBy : .

:assertionSourceBuild a owl:ObjectProperty ;    # optional, at most one
    IAO:0000115 "The specific versioned build this assertion was derived
      from, when the evidence is build-specific (e.g. a versioned RPM
      Provides). Absent when the evidence is version-independent. Where
      present it must be a build of the asserting identity — i.e.
      :assertionSourceBuild/:isVersionOf equals the identity that holds this
      assertion via :hasUpstreamAssertion."@en ;
    rdfs:domain :UpstreamAssertion ;
    rdfs:range :Package ;
    rdfs:isDefinedBy : .

:assertionUpstreamVersion a owl:DatatypeProperty ;  # optional, at most one
    IAO:0000115 "The upstream version as declared by this capability. Bound
      to this assertion's target, never to the subject as a whole."@en ;
    rdfs:domain :UpstreamAssertion ;
    rdfs:range xsd:string ;
    rdfs:isDefinedBy : .

:assertionEcosystem a owl:ObjectProperty ;  # required
    rdfs:domain :UpstreamAssertion ;
    rdfs:range :Ecosystem ;
    rdfs:isDefinedBy : .

:assertionRelation a owl:ObjectProperty ;   # required
    IAO:0000115 "Whether the source repackages the target (it is the target,
      restated for a distribution) or bundles it (it contains the target by
      value alongside its own code)."@en ;
    rdfs:domain :UpstreamAssertion ;
    rdfs:range skos:Concept ;               # UpstreamRelationScheme, below
    rdfs:isDefinedBy : .
```

A new SKOS scheme distinguishes the two relations, following the pattern of
`MatchMethodScheme` (skos-schemes.ttl:369-407):

```turtle
pkg:upstream-repackages  a skos:Concept .   # the source IS the target, redistributed
pkg:upstream-bundles     a skos:Concept .   # the source CONTAINS the target by value
```

#### Why the owner is exactly one `PackageIdentity`

An earlier draft gave `:hasUpstreamAssertion` the domain `:PackageEntity`, so
either a build or an identity could own an assertion. That contradicted the
class's own description, which says every assertion binds a *source identity*,
and it reintroduced ambiguity: a consumer would have to check which kind of
subject it found before knowing whether the version facet was meaningful.

The model is now unambiguous. The owner is always the identity. Build
specificity is expressed by the optional `:assertionSourceBuild`, which must
be a build *of that same identity* — a constraint SHACL enforces (§11.15) and
which prevents an assertion from claiming evidence from an unrelated package.

#### Why the canonical upstream name is required

`:assertionUpstreamName` is not redundant with `:assertionTarget`, and this is
the property that makes seed generation possible at all.

`seed.rs` runs *before* the registry collector, to tell it what to fetch. At
that moment the target identity IRI is a well-formed reference to a node that
**does not yet exist** — it has no `pkg:packageName`, no type, nothing. A seed
query that reads the name off the target (as a previous revision's did) can
only ever return names for packages already collected, which is precisely the
set that does not need seeding. It would bootstrap nothing.

Carrying the post-transform canonical name as a literal on the assertion makes
it self-contained: the pairing, the ecosystem, and the fetchable name are all
readable from one node with no dereference. It also means the name is recorded
*after* the §3.2 transforms — PEP 503 normalization, Cargo feature stripping,
Maven `g:a → g/a` — so what seeds the collector is exactly what the collector
will key by.

Reverse-parsing the target IRI to recover the name is not an acceptable
substitute: it would require every consumer to know and re-implement the
per-ecosystem URI shape and its percent-encoding, which is the coupling §3.1
exists to eliminate.

#### Method and confidence: reuse by domain *replacement*

`:matchMethod` (core.ttl:1633) and `:matchConfidence` (core.ttl:1641) already
model exactly this and are already emitted by `enrich_repology.rs`. Both
currently declare `rdfs:domain :PackageRelationship`.

That declaration must be **replaced**, not supplemented. Multiple `rdfs:domain`
statements on one property are conjunctive — their *intersection* applies — so
adding `rdfs:domain :UpstreamAssertion` alongside the existing one would entail
that every subject of `:matchMethod` is both a `PackageRelationship` **and** an
`UpstreamAssertion`, wrongly typing every assertion as a relationship and every
existing Repology relationship as an assertion. This is the same gotcha called
out in ontology issue #7, and it is a silent one: nothing fails, the data just
becomes wrong under reasoning.

The ontology PR must therefore delete the existing line and write:

```turtle
# REPLACES  :matchMethod rdfs:domain :PackageRelationship .
:matchMethod     rdfs:domain [ a owl:Class ;
                               owl:unionOf ( :PackageRelationship :UpstreamAssertion ) ] .
# REPLACES  :matchConfidence rdfs:domain :PackageRelationship .
:matchConfidence rdfs:domain [ a owl:Class ;
                               owl:unionOf ( :PackageRelationship :UpstreamAssertion ) ] .
```

This is the same monotonic widening argument as §2.4: it removes entailments,
adds none. The alternative — minting `:assertionMethod` / `:assertionConfidence`
— would leave two vocabularies for one concept and force every confidence
consumer to query both.

`pkg:match-repackage-detected` (skos-schemes.ttl:389) already exists and reads
*"repackaging of another package under a different name"*, which is the
correct `matchMethod` for the common case.

**Shortcuts derived from assertions.** Two convenience properties remain, both
materialized *from* assertions and never emitted independently:

```turtle
:bundles a owl:ObjectProperty ;
    IAO:0000115 "The subject package contains, by value, code from the object
      package. Shortcut for an :UpstreamAssertion whose :assertionRelation is
      upstream-bundles; carries no method, confidence, or version."@en ;
    rdfs:domain :PackageIdentity ;
    rdfs:range :PackageIdentity ;
    rdfs:isDefinedBy : .

:upstreamPackageRelease a owl:ObjectProperty ;
    IAO:0000115 "Links a specific distribution build to the specific upstream
      registry release it repackages. Derived only from an :UpstreamAssertion
      that binds both, never by joining independently multi-valued
      properties."@en ;
    rdfs:domain :Package ;
    rdfs:range :Package ;
    rdfs:isDefinedBy : .
```

`pkg:upstreamPackageIdentity` (§2.1) is likewise retained as the materialized
identity-only shortcut, so the §8 one-hop query keeps working. The invariant
is: **every shortcut edge must have a backing assertion; no shortcut is ever
emitted without one.**

**URI keying.** The assertion node must be keyed on everything that
distinguishes it, or distinct capabilities collide:

```rust
// uris.rs — new
pub fn upstream_assertion_uri(
    source: &str,        // identity or build URI
    ecosystem: &str,
    target_name: &str,
    version: Option<&str>,
) -> String
```

Keying on `(source, target)` alone — as `package_relationship_uri` does — is
what reintroduces the cross-product, and is the specific mistake this section
exists to avoid.

`npm:bundledDependency` (npm.ttl:9-14) is **not** emitted in v1. Its range is
`npm:NpmPackage`, the versioned class, but `bundledDependencies` in a package
manifest is a bare array of names with no versions, so no legal object can be
constructed. Emitting an identity URI there would violate the range. npm
bundling is expressed through `pkg:bundles` at identity level only; revisit
`npm:bundledDependency` if and when the artifact-inspection tier (sub-project 2, §12)
supplies resolved versions.

`cpan:containsModule` (cpan.ttl:23) is left alone — it is a
distribution→module relation, not bundling, and is out of scope per §3.3.

### 2.3 Evidence annotation

Bundling evidence quality varies by an order of magnitude between ecosystems.
A `bundles` edge derived from an RPM `Provides: bundled(...)` (packager-
declared, frequently stale) must not be indistinguishable from one derived
from `vendor/modules.txt` (exact, machine-generated) when that tier arrives.

**`emit_dq_issue` cannot carry this, and the previous revision was wrong to
say it could.** Its signature (forge.rs:784-791) is:

```rust
pub fn emit_dq_issue(writer, detector, field, raw_value, issue_type, severity)
```

It takes **no subject**, so the `dq:DataQualityIssue` node it mints
(forge.rs:800-806) is unlinked from the package it concerns, and it has no
confidence parameter at all. It is a logging facility for bad input, not an
evidence model.

Evidence rides on the assertion node from §2.2 instead, via the existing
`:matchMethod` and `:matchConfidence`:

| Evidence | `:matchMethod` | `:matchConfidence` | Source |
|---|---|---|---|
| versioned ecosystem `Provides` | `match-capability-declared` | `1.0` | `crate(X) = 1.2.3` — name *and* version emitted by the build system |
| unversioned ecosystem `Provides` | `match-capability-declared` | `0.95` | `ghc-pkg(X)`, `python3dist(X)` |
| `Provides: bundled(X)` | `match-capability-declared` | `0.9` | packager-declared per Fedora policy |
| npm `bundledDependencies` | `match-capability-declared` | `0.9` | manifest-declared |
| Source0 registry domain | `match-repackage-detected` | `0.8` | `collect_spec.rs:687-694` |
| name prefix only | `match-name-heuristic` | `0.5` | `ghc-`, `python3-`, `rust-` (collect_spec.rs:757-764) |

**`match-upstream-verified` must not be used here**, although an earlier
revision proposed it for the versioned-`Provides` row. Its definition
(skos-schemes.ttl:372) is:

> "Identity confirmed via upstream project metadata (e.g., PyPI project URL
> matches RPM Source URL)."

That describes a *corroboration* between two independent sources. An RPM
`Provides: crate(serde) = 1.0.200` is a single-source declaration generated by
`rust2rpm` from the crate's own manifest — strong evidence, but nothing has
been cross-checked against upstream project metadata. Labelling it
`upstream-verified` would overstate it and would make genuinely corroborated
matches indistinguishable from declared ones.

Nor is `match-repackage-detected` (skos-schemes.ttl:389, *"Package appears to
be a repackaging…"*) right for it — "appears to be" understates a declaration
the build system emitted mechanically.

One new SKOS concept is therefore added to `MatchMethodScheme`, and it is the
correct home for four of the six rows:

```turtle
pkg:match-capability-declared a skos:Concept ;
    skos:inScheme pkg:MatchMethodScheme ;
    skos:prefLabel "capability declared"@en ;
    skos:definition "Identity declared by the packaging system itself, via a
      generated capability or manifest field that names the upstream package
      directly — for example an RPM Provides: crate(name) emitted by rust2rpm,
      or a bundledDependencies entry in an npm manifest. Stronger than
      heuristic name matching because the packaging tooling asserted it, but
      weaker than upstream-verified because nothing has been corroborated
      against independent upstream project metadata."@en .
```

The remaining two rows use existing concepts. The numeric confidences replace
the three-token ladder at `collect_spec.rs:453` for assertions; that ladder
stays where it is for DQ records, which are a different thing.

`emit_dq_issue` retains its proper role in this design: recording *failures*
(§10) — an unresolvable ecosystem token, a CPAN module that cannot be mapped
to a distribution. It is never used to record a successful association.

### 2.4 Corrections to existing ontology terms

Three defects in the pinned ontology block this design. All are in the sibling
`packagegraph/ontology` repository and must land and be released before the
collector changes.

> Filed as **packagegraph/ontology#9**. The two *new* terms in §2.2
> (`:bundles`, `:upstreamPackageRelease`) are deliberately not in that issue —
> they are contingent on this design being accepted, whereas the three defects
> below are real regardless.

**(a) `pkg:upstreamPackageVersion` is undeclared.** `rpm.rs:1130` has been
emitting it, and the §7 deriver joins on it, but grep over `core.ttl` finds no
declaration. Declare it. This property *is* genuinely version-specific, so it
is the one member of the family that belongs on `:Package`:

```turtle
:upstreamPackageVersion a owl:DatatypeProperty ;
    rdfs:label "upstream package version"@en ;
    IAO:0000115 "The version of the upstream software in its native package
      ecosystem, as declared by a versioned ecosystem capability such as
      crate(foo) = 1.2.3. Version-specific, and therefore asserted on the
      versioned Package rather than on its identity."@en ;
    rdfs:domain :Package ;
    rdfs:range xsd:string ;
    rdfs:isDefinedBy : .
```

**(b) and (c) `upstreamEcosystem` and `upstreamPackageName` have the wrong
domain.** Both are declared `rdfs:domain :Package` (core.ttl:917 and
core.ttl:942), but four of their five emitters write them on a
`PackageIdentity` subject — `collect_spec.rs:433-445`, `debian.rs:613-623`,
`collect_salsa.rs:340-352`, `collect_sources.rs:241-262` — and
`PackageIdentity` is `rdfs:subClassOf :PackageEntity` (core.ttl:1466), not of
`:Package`. Those four are live RDFS domain violations today.

The emitters are right and the ontology is wrong. Both properties are
semantically version-independent: which ecosystem a package comes from, and
what it is called upstream, do not change between builds of the same package.
`pkg:upstreamPackageIdentity` — the new edge this design centres on — already
has `rdfs:domain :PackageIdentity` for exactly that reason.

Widen both to the existing common superclass:

```turtle
:upstreamEcosystem   rdfs:domain :PackageEntity .   # was :Package
:upstreamPackageName rdfs:domain :PackageEntity .   # was :Package
```

`:PackageEntity` (core.ttl:1649-1654) exists for precisely this case; its own
definition says it was *"Introduced to allow dependency properties to accept
both version-specific packages and version-independent identities without
collapsing their types under RDFS/OWL reasoning."*

This was considered and rejected in favour of moving the emitters to the
`:Package` altitude. That alternative is not implementable: `collect_spec.rs`,
`collect_salsa.rs`, and `collect_sources.rs` have no versioned package URI and
cannot construct one, because the release component (`-2.fc44`) comes from RPM
repodata while the spec's own `Release: 2%{?dist}` is an unexpanded macro
requiring sub-project 3. Widening the domain resolves the violation without
blocking on out-of-scope work.

Widening a domain is monotonically safe for existing data: it removes
entailments (fewer resources inferred into `:Package`) rather than adding
them, and every triple legal under the old domain remains legal.

**Sequencing.** The ontology is consumed as a pinned, released artifact
(loaded into QLever by `upload-ontology.sh`), so these changes must land in a
strict order:

1. Ontology PR: declare `upstreamPackageVersion`; widen the two domains; add
   `:bundles` and `:upstreamPackageRelease` from §2.2. Bump the ontology
   version and update `CHANGELOG.md`.
2. Ontology release + `upload-ontology.sh` run, so the graph carries the new
   declarations.
3. Only then, the collector changes in this design.

Emitting `pkg:bundles` or `pkg:upstreamPackageRelease` against an ontology
that does not declare them produces triples no consumer can reason over, and
SHACL validation in `core.shacl.ttl` would flag them.

## 3. The registry identity resolver

### 3.1 The function

One new function in `uris.rs`. It is the single point where an ecosystem token
becomes a registry URI, so that the mapping exists in exactly one place.

```rust
/// Resolve an ecosystem token plus an upstream package name to the
/// PackageIdentity URI that the corresponding registry collector mints.
///
/// Returns None for ecosystems whose identifiers cannot be resolved by
/// string transform (see cpan) or whose URI segments are runtime-configured
/// (see conda). Callers MUST treat None as "emit no edge, record a DQ
/// issue" — never as "fall back to a guess".
pub fn registry_identity_uri(ecosystem: &str, name: &str) -> Option<String>
```

Returning `Option` rather than `String` is the mechanism that prevents
silently minting dangling references for ecosystem tokens we do not handle.

### 3.2 The mapping table

Every row below was verified against the live `package_identity_uri(...)` call
in the corresponding collector. The release segment is **not** guessable from
the ecosystem token — `pypi`/`index`, `npm`/`registry`, `cargo`/`crates.io`,
`rubygems`/`org`, `maven`/`central`, `hex`/`pm`, `nuget`/`gallery` all differ,
and three ecosystems need a name transform on top. Nine of the twelve rows are
supported; the three exclusions are each a namespace mismatch no string
transform can bridge.

| Ecosystem token | Distro seg | Release seg | Name transform | Verified at |
|---|---|---|---|---|
| `hackage` | `hackage` | `hackage` | none | hackage.rs:278 |
| `pypi` | `pypi` | `index` | **PEP 503 normalize** | pypi.rs:509 |
| `npm` | `npm` | `registry` | none | npm.rs:194 |
| `cargo` | `cargo` | `crates.io` | **strip `/feature` suffix** | cargo_collect.rs:281 |
| `rubygems` | `rubygems` | `org` | none | rubygems.rs:201 |
| `gomod` | — | — | **excluded, see §3.4** | gomod.rs:309 |
| `maven` | `maven` | `central` | **`g:a` → `g/a`** | maven.rs:907 |
| `cran` | `cran` | `cran` | none | cran.rs:209 |
| `hex` | `hex` | `pm` | none | hex_collect.rs:266 |
| `nuget` | `nuget` | `gallery` | none | nuget.rs:234 |
| `cpan` | — | — | **excluded, see §3.3** | cpan.rs:195 |
| `conda` | — | — | **excluded, see §3.5** | conda.rs:198 |

All identities are `package_identity_uri(distro_seg, release_seg, "any", transformed_name)`.

Three rows carry traps that a naive `token == segment` implementation would
get wrong, producing well-formed URIs that match nothing:

- **`gomod` → `go` was a trap, and is now moot.** The distro side writes
  `pkg:upstreamEcosystem <d/ecosystem/gomod>` while `gomod.rs:309` mints under
  the segment `go`. That rename is real, but it is not the reason Go is
  excluded — §3.4 is. Recorded here because a future implementer lifting the
  exclusion must handle both the rename *and* module-root resolution.
- **`maven` separator.** `rpm.rs` derives `"g:a"` from `mvn(group:artifact)`,
  while `maven.rs:907` mints the path segment `g/a`.
- **`cargo` feature suffixes.** Fedora's `rust2rpm` emits one `Provides` per
  Cargo feature — `crate(serde)`, `crate(serde/derive)`, `crate(serde/std)` —
  and `rpm.rs:1056` strips only the `crate(` … `)` wrapper, yielding
  `"serde/derive"`. Minting from that gives
  `…/cargo/crates.io/any/serde%2Fderive`, while `cargo_collect.rs:281` mints
  from the bare crate name `serde`. Everything after the first `/` must be
  dropped, and the several feature capabilities on one package then collapse
  to one assertion per crate rather than one per feature — which is correct,
  since features are build variants of the same crate, not distinct upstreams.
  Note `normalize_librust_crate_name` (collect_spec.rs:1016-1023) does **not**
  do this: it handles Debian's `librust-foo+feature-dev` shape, splitting on
  `+` and stripping `-dev`. Fedora's separator is `/`. Both are needed.
- **`pypi` normalization.** The distro side strips a prefix without case
  folding — `strip_ecosystem_prefix` (collect_spec.rs:1000-1007) is a plain
  prefix strip — so `python3-Foo` yields `"Foo"`, while PyPI normalizes names
  to lowercase with runs of `-_.` collapsed to `-`. PEP 503 normalization must
  be applied to the name before minting. Hackage, by contrast, is
  case-sensitive and Fedora's Haskell guidelines preserve upstream case
  (`ghc-HUnit` → `HUnit`), so it must **not** be normalized.

### 3.3 CPAN is excluded, deliberately

`cpan.rs` is internally inconsistent in a way that this design must not
propagate. It keys identities by **distribution**:

```rust
// cpan.rs:195
let identity_uri = package_identity_uri("cpan", "cpan", "any", &release.distribution);
```

but points dependency edges at **modules**:

```rust
// cpan.rs:272
let target_uri = package_identity_uri("cpan", "cpan", "any", &dep.module);
```

Distributions and modules are different namespaces — the distribution
`libwww-perl` ships the module `LWP::UserAgent`. CPAN dependency edges
therefore already dangle today, independent of this work.

The distro side gives us a module name (`perl(Module::Name)`, parsed at
rpm.rs) and Perl RPM names are distribution-shaped but lossy. Resolving one to
the other requires a MetaCPAN module→distribution lookup, i.e. a network
call and a cache — a different kind of change from a string transform.

`registry_identity_uri("cpan", _)` returns `None` in v1. A DQ issue with
detector `cpan-module-distribution-unresolved` records each skipped case, so
the cost of the gap is measurable and the follow-up is scoped by real data.
Fixing `cpan.rs`'s own inconsistency is filed as follow-on work (§12).

### 3.4 Go is excluded: import paths are not module paths

The `gomod` row looks like the simplest case — the module path *is* the
repository — and the previous revision treated it as a plain string mapping
with only the `gomod`→`go` segment rename to watch for. That is wrong.

RPM's `golang(X)` capabilities are **import paths**: `go-rpm-macros` emits one
per package directory, so a single module yields many, e.g.
`golang(github.com/foo/bar)`, `golang(github.com/foo/bar/pkg/baz)`,
`golang(github.com/foo/bar/internal/qux)`.

`gomod.rs` keys identities by **module root**, and does so deliberately —
`gomod.rs:162` routes every path through `resolve_module_root` before minting:

```rust
// gomod.rs:51-69 (abridged)
fn resolve_module_root(&self, import_path: &str) -> Option<String> {
    // known-module prefix cache, then negative cache, then:
    // "Try progressively shorter prefixes" against the Go proxy
}
```

So `github.com/foo/bar/pkg/baz` collapses to the identity
`…/go/modules/any/github.com%2Ffoo%2Fbar`. A resolver that mints from the raw
import path produces `…/github.com%2Ffoo%2Fbar%2Fpkg%2Fbaz`, which matches
nothing — and it does so for the *majority* of `golang()` capabilities, since
only the module-root package shares a name with its module.

This cannot be fixed by a string transform. `resolve_module_root` is
network-dependent (Go proxy lookups over shortening prefixes) and stateful (two
caches). Reusing it would change `registry_identity_uri` from a pure function
into one performing I/O, which is a different contract and a different
testing story.

`registry_identity_uri("gomod", _)` therefore returns `None` in v1, with a DQ
issue under detector `go-import-path-unresolved`. A follow-up can lift
`resolve_module_root` into a shared, cache-backed resolver used by both
`gomod.rs` and this path; the volume of skipped capabilities recorded by the DQ
issues is what should justify that work.

Note this makes **three** exclusions — CPAN, Go, and conda — sharing one shape:
the distro side names a thing in a namespace the registry collector does not key
by. Only a lookup reconciles them. The nine remaining ecosystems in §3.2 are
genuinely pure string transforms, and that distinction is the real content of
the table.

### 3.5 conda is excluded

`conda.rs:198` takes its distro, release, and subdir from CLI arguments
(`main.rs:352-365`), defaulting to `conda`/`conda-forge`/`linux-64`. The
subdir is an architecture, not `"any"`. There is no static row that is correct
for all invocations.

conda is also the one registry collector that emits `pkg:upstreamEcosystem`
*outward* (conda.rs:354-370, pointing at pypi/cran/cargo), which makes it a
natural candidate for the *subject* side of this edge in a later pass rather
than the object side.

## 4. Emission sites and subject altitude

### 4.1 Altitudes after the §2.4 corrections

With `upstreamEcosystem` and `upstreamPackageName` widened to
`:PackageEntity`, every existing emitter becomes legal at the subject it
already uses, and the new edge slots in at the identity:

| Property | Domain (post-§2.4) | Subject used |
|---|---|---|
| `pkg:upstreamPackageIdentity` | `:PackageIdentity` | identity |
| `pkg:bundles` | `:PackageIdentity` | identity |
| `pkg:upstreamEcosystem` | `:PackageEntity` | either (unchanged per collector) |
| `pkg:upstreamPackageName` | `:PackageEntity` | either (unchanged per collector) |
| `pkg:upstreamPackageVersion` | `:Package` | versioned package |
| `pkg:upstreamPackageRelease` | `:Package` | versioned package |

For a Fedora RPM providing `ghc-pkg(hoauth2) = 2.14.3`, the complete output —
assertion first, shortcuts materialized from it, flat literals retained
unchanged for backward compatibility:

```turtle
# 1. the assertion — the authoritative, self-contained binding
<…/assertion/fedora-43/ghc-hoauth2/hackage/hoauth2/2.14.3>
    a pkg:UpstreamAssertion ;
    pkg:assertionTarget        <d/pkg/hackage/hackage/any/hoauth2> ;
    pkg:assertionUpstreamName  "hoauth2" ;              # seeds the collector
    pkg:assertionEcosystem     <d/ecosystem/hackage> ;
    pkg:assertionRelation      pkg:upstream-repackages ;
    pkg:assertionSourceBuild   <d/pkg/fedora/43/x86_64/ghc-hoauth2/2.14.3-2.fc44> ;
    pkg:assertionUpstreamVersion "2.14.3" ;
    pkg:matchMethod            pkg:match-capability-declared ;
    pkg:matchConfidence        "1.0"^^xsd:decimal .

# 2. identity-level — owner link plus the materialized one-hop shortcut
<d/pkg/fedora/43/x86_64/ghc-hoauth2>
    pkg:hasUpstreamAssertion    <…/assertion/fedora-43/ghc-hoauth2/hackage/hoauth2/2.14.3> ;
    pkg:upstreamPackageIdentity <d/pkg/hackage/hackage/any/hoauth2> .

# 3. package-level — the pre-existing flat literals, subjects unchanged
<d/pkg/fedora/43/x86_64/ghc-hoauth2/2.14.3-2.fc44>
    pkg:upstreamPackageName    "hoauth2" ;
    pkg:upstreamPackageVersion "2.14.3" .
```

Block 3 is retained verbatim from today's behaviour and is **advisory only**
(§7.2) — it is the ambiguous form. Block 1 is what the deriver and `seed.rs`
read. Block 2's shortcut exists solely so the §8 query stays one hop.

`rpm.rs` keeps `upstreamPackageName` on the versioned package (where it
already is) and `collect_spec.rs` / `debian.rs` / `collect_salsa.rs` /
`collect_sources.rs` keep it on the identity (where they already have it).
**No emitter moves subjects.** The only change to those call sites is the
addition of the new `upstreamPackageIdentity` edge, which always goes on an
identity.

This matters for implementability: three of those four collectors have no
versioned package URI in scope and cannot construct one (§2.4). Had the
correction gone the other way, they would have blocked on sub-project 3.

### 4.2 Threading the identity URI into `rpm.rs`

`rpm.rs::emit_ecosystem_triples` (rpm.rs:1041-1046) currently takes only the
versioned URI:

```rust
fn emit_ecosystem_triples(&self, writer, pkg_uri: &str, deps: &[RpmDep]) -> Result<usize>
```

It needs the identity to emit the new edge. `identity_uri` is already live at
the call site (bound at rpm.rs:835, called from rpm.rs:1015), so this is a
single added parameter — no new derivation, no new state.

The other four sites already receive identity URIs
(`collect_spec.rs:415-420` takes `identity_uris: &[String]`) and need no
plumbing change at all.

### 4.3 Where the resolver is called

`registry_identity_uri` is called at each site that currently computes an
upstream ecosystem and name:

- `rpm.rs::emit_ecosystem_triples` (rpm.rs:1041-1136) — from `Provides:`
  capability strings, mapping table at rpm.rs:1054-1110
- `collect_spec.rs::emit_ecosystem_triples` (collect_spec.rs:415-467) — from
  `detect_ecosystem` (collect_spec.rs:635-782)
- `debian.rs:612-623`, `collect_salsa.rs:340-352`, `collect_sources.rs:241-262`

`collect_spec.rs:416` currently builds the ecosystem IRI inline with
`format!("{DATA}ecosystem/{}", …)` instead of calling `uris::ecosystem_uri`
(uris.rs:405-407). The output is byte-identical; consolidate onto the helper
while touching the function.

### 4.4 Seeding: why the flat properties cannot make this join reliable

`seed.rs::discover_by_ecosystem` (seed.rs:16-47) queries Fuseki for
`upstreamPackageName` literals filtered by ecosystem and writes them to a seed
file; `hackage.rs:62-65` and its ten siblings feed that file straight into
`collect()`. The registry-side node is therefore created **from the exact
string the distro run recorded** — byte-identical by construction, for the 11
collectors wired this way (`cran.rs` and `conda.rs` are not).

**That argument is false against the current data model, and a previous
revision of this spec proposed a fix that does not work.** The defect is not
which triples are emitted — it is that the flat properties cannot express
pairing at all (§2.2).

`rpm.rs::emit_ecosystem_triples` latches a single `emitted_ecosystem` boolean
on the *first* matching capability (rpm.rs:1113-1119) while writing
`upstreamPackageName` for *every* matching capability in the same loop
(rpm.rs:1120-1126). A package providing both `python3dist(foo)` and
`crate(bar)` emits one `upstreamEcosystem → pypi` alongside two names.

Replacing that boolean with a `HashSet` — as the previous revision proposed —
emits the *missing* `upstreamEcosystem → cargo` triple, and changes nothing
about attribution. The subject then carries two ecosystems and two names as
two unordered sets, and `seed.rs`'s join returns all four combinations.
`cargo`/`foo` is exactly as well-supported as `cargo`/`bar`. **A set-valued
fix cannot recover a pairing the model never stored.**

The real fix is the per-capability assertion (§2.2), and it changes what
`seed.rs` must read.

#### `seed.rs` rewrite

`discover_by_ecosystem` (seed.rs:16-47) currently joins the two independent
properties:

```sparql
?pkg pkg:upstreamPackageName ?name .
?pkg pkg:upstreamEcosystem ?eco .
FILTER(STR(?eco) = "{ecosystem}" || CONTAINS(STR(?eco), "ecosystem/{ecosystem}"))
```

It must instead read the pairing off a single assertion node, where ecosystem
and name are bound together by construction:

```sparql
SELECT DISTINCT ?name WHERE {
  GRAPH ?g {
    ?assertion a pkg:UpstreamAssertion ;
               pkg:assertionEcosystem   <d/ecosystem/{ecosystem}> ;
               pkg:assertionUpstreamName ?name .
  }
} ORDER BY ?name
```

**The name must come from `:assertionUpstreamName`, not from the target.** A
draft of this section joined `?assertion pkg:assertionTarget ?target . ?target
pkg:packageName ?name`, which cannot bootstrap anything: `seed.rs` runs *to
decide what the registry collector should fetch*, so at that moment the target
identity is an IRI with no triples describing it. `?target pkg:packageName
?name` would bind only for packages the registry collector had already
collected — exactly the set that needs no seeding. The query would return a
shrinking subset of what it returned before the rewrite, and new upstream
packages would never be discovered.

Reading the literal off the assertion also means the seed carries the
*post-transform* name (§3.2) — PEP 503-normalized, feature-stripped,
`g:a`-rewritten — which is what the registry collector will key by. The two
sides therefore agree by construction rather than by coincidence.

This also removes the current string-matching `FILTER` over the ecosystem IRI,
since the assertion points at the `Ecosystem` resource directly.

Only once both the assertion emission and this query land is the seeded
registry node guaranteed to correspond to the ecosystem it was filed under.
Until then, seeding remains best-effort and §3's join may reference registry
nodes that were never collected — which is tolerable (an unresolvable IRI is
inert) but is not the "lossless" property this section originally claimed.

The size of the existing pollution is measurable before fixing:

```sparql
SELECT ?pkg (COUNT(DISTINCT ?name) AS ?names) (COUNT(DISTINCT ?eco) AS ?ecos)
WHERE { ?pkg pkg:upstreamPackageName ?name ; pkg:upstreamEcosystem ?eco . }
GROUP BY ?pkg
HAVING (COUNT(DISTINCT ?name) > 1 || COUNT(DISTINCT ?eco) > 1)
```

Every row is a package whose upstream association is currently ambiguous, and
whose names may have been seeded into the wrong registry collector.

## 5. Bundled components

### 5.1 RPM: `Provides: bundled(X)`

Fedora packaging policy requires bundled code to be declared as
`Provides: bundled(<capability>)`. The inner capability is the *same* string
format that `rpm.rs::emit_ecosystem_triples` (rpm.rs:1054-1110) already parses:

```
bundled(crate(nom))           → cargo / nom          → assertion emitted
bundled(npm(lodash))          → npm   / lodash       → assertion emitted
bundled(python3dist(six))     → pypi  / six          → assertion emitted
bundled(golang(github.com/…)) → gomod / …            → None (§3.4), DQ issue only
```

The extractor is therefore small: strip the `bundled(` … `)` wrapper, pass the
remainder to the existing capability mapper, pass the result through
`registry_identity_uri`, emit `pkg:bundles`. The capability parser is reused
unchanged rather than duplicated.

**This does not cover the motivating `ghc-hoauth2` case, and should not be
claimed to.** Fedora's Haskell packaging uses `%ghc_lib_subpackage` (which
emits a real subpackage providing `ghc-pkg(binary-instances)`) rather than a
`bundled()` declaration, so the vendored `Source1` component in that spec is
discoverable only through subpackage structure and spec macro expansion —
sub-projects 3 and 4.

What §5.1 covers in v1 is the `rust2rpm` population, where
`bundled(crate(...))` is the standard and widely-populated declaration. The
`go2rpm` population is **not** covered despite `bundled(golang(...))` being
equally standard, because Go import paths cannot be resolved to module
identities without the lookup described in §3.4 — those capabilities are
detected and recorded as DQ issues rather than dropped silently, so the volume
is measurable and can justify lifting the exclusion.

`bundled()` capabilities frequently carry no version, and where they do, the
version belongs to that specific component. Under the §2.2 assertion model
this is expressible from day one: each bundled component gets its own
`:UpstreamAssertion` with `:assertionRelation pkg:upstream-bundles` and, when
declared, its own `:assertionUpstreamVersion` bound to that component's
target. A package bundling three components produces three assertions, and no
version is ever attributable to the wrong one.

This is the case that previously forced a deferral. The earlier revision kept
bundled versions out of v1 because a flat `pkg:upstreamPackageVersion` literal
on the bundling package cannot say *which* component it describes. That
limitation belonged to the flat model, not to the data — the assertion removes
it.

What remains deferred is the §7 `upstreamPackageRelease` edge for bundles: it
is scoped to `upstream-repackages` only, because a release-level identity edge
would misrepresent containment as identity. The bundled version is recorded on
the assertion and is queryable there.

`primary.xml` is already parsed and already carries the full `Provides:` list
(`rpm.rs::parse_primary_metadata`, rpm.rs:575), so this adds **no fetching**.

### 5.2 npm: `bundledDependencies`

`npm.rs` does not currently deserialize `bundledDependencies` /
`bundleDependencies` at all. Add the field to the version struct, and for each
entry emit an assertion **first**, then materialize the shortcut from it —
per the §2.2 invariant that no shortcut is ever emitted without a backing
assertion. A draft of this section said "emit exactly one triple", which
contradicted that invariant:

```turtle
<d/pkg/npm/registry/any/express>
    pkg:hasUpstreamAssertion <…/assertion/npm/express/lodash> ;
    pkg:bundles              <d/pkg/npm/registry/any/lodash> .   # shortcut

<…/assertion/npm/express/lodash>
    a pkg:UpstreamAssertion ;
    pkg:assertionTarget       <d/pkg/npm/registry/any/lodash> ;
    pkg:assertionUpstreamName "lodash" ;
    pkg:assertionEcosystem    <d/ecosystem/npm> ;
    pkg:assertionRelation     pkg:upstream-bundles ;
    pkg:assertionSourceBuild  <d/pkg/npm/registry/any/express/4.19.2> ;
    pkg:matchMethod           pkg:match-capability-declared ;
    pkg:matchConfidence       "0.9"^^xsd:decimal .
```

`:assertionSourceBuild` **is** available here and should be set: unlike the
RPM `bundled()` path, npm reads `bundledDependencies` from a specific version
object in the registry document (`npm.rs:243-244` already resolves
`versions.get(version)`), so the build the declaration came from is known
exactly. No `:assertionUpstreamVersion` is set, because the field carries
names only.

`npm:bundledDependency` is **not** emitted, for the reason given in §2.2: its
range is `npm:NpmPackage` (the versioned class), while `bundledDependencies`
is a bare name array carrying no versions. There is no legal object to
construct, and supplying an identity URI would violate the range. Both
subject and object here are identities, matching `pkg:bundles`'s declared
domain and range.

Note that the field is a JSON array of names, but npm also permits the
legacy boolean `bundledDependencies: true` (meaning "bundle everything in
`dependencies`"). Deserialization must tolerate both shapes and emit nothing
for the boolean form, which carries no per-package information.

The registry document is already fetched; this reads a field already in the
response body.

### 5.3 Not in this design

No other ecosystem has a bundling declaration available in metadata we already
fetch. Rust `-sys` crates, Ruby vendored C extensions, and CRAN `src/`
vendoring have no manifest at all; Go, Maven, and Python wheels have excellent
manifests that live inside artifacts we do not download. See §12.

## 6. Completing the chain: registry → forge

Only two of twelve registry collectors currently reach the `UpstreamProject`
hub:

| Collector | Repo URL handling today |
|---|---|
| `cargo_collect.rs:327-330` | `forge::extract_forge_url` → `forge::emit_upstream_repo` ✓ |
| `maven.rs:961-970` | `normalize_forge_url_canonical` → `emit_upstream_project` ✓ (hand-rolled) |
| `cpan.rs:260-263` | emits `pkg:hasRepository` on the versioned node — non-hub predicate, wrong subject |
| `hackage.rs:326` | `source-repository` stanza **not parsed**; only `pkg:homepage` literal |
| `pypi.rs:543-544` | `project_urls` not deserialized; only `home_page` → literal |
| `npm.rs:228-229` | `repository` field **not deserialized**; only `homepage` literal |
| `rubygems.rs:239-241` | `source_code_uri` emitted as a **literal** |
| `hex_collect.rs:313-314` | `links["GitHub"]` — a GitHub URL — written as a **literal** |
| `gomod.rs` | module path **is** the repo URL; no forge call anywhere |
| `cran.rs:264-265` | `URL` → literal; `BugReports` not parsed |
| `nuget.rs:271` | `projectUrl` → literal; nuspec `repository` not read |
| `conda.rs` | no `dev_url`/`source` handling |

Without this, the `distro → registry → forge` chain terminates at the registry
for ten of twelve ecosystems, which defeats the purpose of §3.

Each of the nine fixable collectors routes its repository URL through the
existing shared helper:

```rust
if let Some(extraction) = crate::forge::extract_forge_url(&repo_url) {
    crate::forge::emit_upstream_repo(writer, &identity_uri, &extraction, None)?;
}
```

`emit_upstream_repo` (forge.rs:960-993) already mints the hub node internally
via `emit_upstream_project` (forge.rs:990-1004) and writes
`pkg:upstreamRepository` on the identity — the correct subject per
core.ttl:950. No new forge logic is required.

Per-collector notes:

- **`gomod.rs`** — the module path is the repository. `github.com/go-chi/chi/v5`
  needs the major-version suffix (`/v2`, `/v5`, …) stripped before forge
  matching, and non-forge module paths (`golang.org/x/…`, vanity domains) will
  correctly fail to match and emit nothing.
- **`hackage.rs`** — requires parsing the cabal `source-repository head` /
  `source-repository this` stanza, which the collector currently skips. The
  `bug-reports` field is already parsed (hackage.rs:250) but never emitted.
- **`cpan.rs`** — replace the `pkg:hasRepository`-on-versioned-node write
  (cpan.rs:260-263) with the standard helper on the identity. This is a
  behaviour change to an existing triple and is called out in §10.
- **`pypi.rs` / `nuget.rs`** — require deserializing a field not currently in
  the response struct (`project_urls`, nuspec `repository`).
- **`hex_collect.rs` / `rubygems.rs` / `cran.rs` / `npm.rs`** — the URL is
  already in hand and is being stringified; route it through the helper and
  keep the literal.

Additionally, emit `pkg:ecosystemPackage` (core.ttl:360-366) from the hub back
to the registry identity, which is what makes "every ecosystem package derived
from this project" a one-hop query.

## 7. Version-level links: a deriver, not a collector

### 7.1 Why not in the collectors

Registry collectors store exactly **one** version per package — whichever was
current at collection time:

- `hackage.rs:126-163` fetches the `preferred` version
- `npm.rs:189` uses `dist-tags.latest`
- `cargo_collect.rs:348` resolves one specific version per run

`seed.rs:23-31` selects `?name` only; no version flows from the distro side to
the registry side.

So if Fedora 43 ships `hoauth2 2.14.3` while Hackage's preferred version is
`2.15.0`, the graph contains `…/hoauth2/2.15.0` and not `…/hoauth2/2.14.3`.
A collector emitting a version-level edge would be minting a reference to a
node that does not exist — precisely the failure mode §3.1 returns `Option` to
prevent.

### 7.2 The deriver

#### The cross-product defect, and why the deriver reads assertions only

A previous revision proposed this query:

```sparql
# WRONG — do not implement
?distroPkg pkg:upstreamPackageVersion  ?v ; pkg:isVersionOf ?distroId .
?distroId  pkg:upstreamPackageIdentity ?upId .
?upRelease pkg:isVersionOf ?upId ; pkg:versionString ?v .
```

`?v` and `?upId` come from two independently multi-valued properties. A
package with two upstream identities and two declared versions produces up to
four candidate pairs, and a false pair binds whenever the wrong registry
package happens to publish the same version string — which for common versions
like `1.0.0` or `2.0.0` is not rare. It would emit confidently wrong
`upstreamPackageRelease` edges, which is worse than emitting none.

The deriver therefore reads **only** from `:UpstreamAssertion`, where target
and version are bound to each other by construction:

```sparql
CONSTRUCT {
  ?sourceBuild pkg:upstreamPackageRelease ?upRelease .
}
WHERE {
  ?assertion a pkg:UpstreamAssertion ;
             pkg:assertionRelation        pkg:upstream-repackages ;
             pkg:assertionSourceBuild     ?sourceBuild ;
             pkg:assertionTarget          ?upId ;
             pkg:assertionUpstreamVersion ?v .
  ?upRelease pkg:isVersionOf   ?upId ;
             pkg:versionString ?v .
}
```

Every variable that must correspond is drawn from one assertion node, so no
cross-product is possible. Assertions lacking `:assertionSourceBuild` or
`:assertionUpstreamVersion` simply do not bind and emit nothing — the absence
of a version is expressed by the pattern failing, not by a guess.

`:assertionRelation` is pinned to `upstream-repackages`. Bundled components
are excluded here: a bundled version, where declared, is bound on its own
assertion and is version-modelled from day one (§5.1), but a
`upstreamPackageRelease` edge would misrepresent containment as identity.

Properties of this approach:

- Dangling references are structurally impossible — `?upRelease` must already
  exist to bind.
- False pairings are structurally impossible — every correspondence comes from
  a single node.
- Zero additional fetching.
- Coverage improves automatically as registry version coverage improves,
  with no code change.

The flat `pkg:upstreamPackageVersion` literal (rpm.rs:1130) is **not** read by
the deriver. It remains emitted and, per ontology issue #9, finally declared,
for backward compatibility with existing consumers — but it is not an
authoritative binding and this design treats it as advisory only.

Version-string equality is exact-match on `pkg:versionString`. RPM version
normalization (epochs, tilde/caret ordering) is deliberately *not* applied: a
mismatch should surface as a missing edge and a DQ record, not as a
speculative match. `rpmver.rs` exists if a later pass wants fuzzy matching, but
it is out of scope here.

### 7.3 Scheduling

The deriver joins across distro graphs and registry graphs, so it must run
after both. It slots alongside the existing enrichment stages
(`enrich-revdeps`, `enrich-blast-radius`) rather than inside any collector's
timer.

## 8. Query patterns

The motivating question — "is the Fedora package `ghc-hoauth2` really the
Hackage package `hoauth2`?" — becomes a single-hop lookup with no property
paths and no traversal:

```sparql
SELECT ?upstream WHERE {
  ?id pkg:packageName "ghc-hoauth2" ;
      pkg:upstreamPackageIdentity ?upstream .
}
# → <d/pkg/hackage/hackage/any/hoauth2>
```

Cross-distro convergence — every distribution package repackaging PyPI
`requests`:

```sparql
SELECT ?distroPkg WHERE {
  ?distroPkg pkg:upstreamPackageIdentity <d/pkg/pypi/index/any/requests> .
}
```

The full chain to the forge hub, enabled by §6:

```sparql
SELECT ?repo WHERE {
  ?id pkg:packageName "ghc-hoauth2" ;
      pkg:upstreamPackageIdentity ?up .
  ?up pkg:upstreamRepository ?repo .
}
```

Hidden bundled components — the supply-chain question that motivated §5:

```sparql
SELECT ?pkg ?bundled WHERE {
  ?pkg pkg:bundles ?bundled .
  ?bundled pkg:upstreamRepository ?repo .
}
```

Multi-hop repackaging (`conda → pypi → forge`) via property path, since the
closure is not materialized:

```sparql
SELECT ?origin WHERE {
  ?id pkg:upstreamPackageIdentity+ ?origin .
  FILTER NOT EXISTS { ?origin pkg:upstreamPackageIdentity ?_ }
}
```

Evidence-filtered querying — only associations the build system declared,
excluding name-prefix guesses:

```sparql
SELECT ?source ?target ?conf WHERE {
  ?a a pkg:UpstreamAssertion ;
     pkg:assertionTarget ?target ;
     pkg:matchConfidence ?conf .
  ?source pkg:hasUpstreamAssertion ?a .
  FILTER(?conf >= 0.9)
}
```

This is the query the shortcut properties cannot answer, and the reason the
assertion is worth its triple cost.

## 9. Data flow

Every association is emitted as an assertion first; the shortcut edges are
materialized from it in the same pass, never independently.

```
primary.xml  ─┬─► rpm.rs::emit_ecosystem_triples
              │     ├─ Provides: crate(X) = 1.2.3  ─┐
              │     ├─ Provides: ghc-pkg(X)        ─┼─► one UpstreamAssertion per capability
              │     └─ Provides: bundled(...)      ─┘   (target + ecosystem + version?
              │                                          + matchMethod + matchConfidence)
              │                                              │
dist-git spec ─► collect_spec.rs::emit_ecosystem_triples ────┤
              │     └─ detect_ecosystem                      │
              │                                              ▼
              │                                    materialized shortcuts:
              │                                    upstreamPackageIdentity, bundles
              │
registry APIs ─► hackage/pypi/npm/… collectors
                    ├─ package identity + version nodes  (existing)
                    ├─ repo URL → forge::emit_upstream_repo → UpstreamProject hub   (§6, new)
                    └─ npm bundledDependencies → UpstreamAssertion + bundles        (§5.2, new)

                         ▼ (after all collection)
                    seed.rs reads assertions, not flat properties        (§4.4, changed)
                    derive_upstream_release reads assertions only        (§7, new)
```

## 10. Error handling and data quality

- `registry_identity_uri` returning `None` emits **no edge** and records a DQ
  issue with detector `ecosystem-unresolved`, field = the ecosystem token.
  This is the only sanctioned response; no fallback guessing.
- CPAN skips emit a DQ issue with detector
  `cpan-module-distribution-unresolved`, so the size of the §3.3 gap is
  measurable from the graph.
- The existing `no-forge-match` DQ issue (collect_spec.rs:232-241) continues
  to fire for registry-hosted Source0 URLs such as Hackage tarballs. This is
  correct — a registry URL genuinely is not a forge URL — but it is currently
  logged at `info` severity and will now be accompanied by a successful
  ecosystem resolution. Consider demoting it where an ecosystem *was*
  resolved; tracked as a follow-up, not changed here.
- **Behaviour change:** `cpan.rs:260-263` currently writes
  `pkg:hasRepository` on the versioned node. §6 replaces this with
  `pkg:upstreamRepository` on the identity. Any consumer of the old triple
  breaks. Grep found no consumers in `etl/` or `query/`; confirm before
  merging.
- `pkg:upstreamPackageName` literals are **retained at their current values
  and their current subjects** — no emitter changes altitude (§4.1).
  `enrich_taxonomy.rs:88-92` and `seed.rs:16-47` read them and keep working
  unchanged; the seed mechanism in particular is what makes §4.4 hold, so it
  cannot be broken by this change.
- The §2.4 domain widening is monotonic: it removes entailments rather than
  adding them, so no currently-valid triple becomes invalid. Consumers that
  relied on `upstreamPackageName` entailing `rdf:type :Package` lose that
  inference — but that inference was the bug, and grep finds no consumer
  depending on it.
- **Ordering hazard.** The ontology release (§2.4 and §2.2) must precede the
  collector changes. Emitting `:UpstreamAssertion`, `pkg:bundles`, or
  `pkg:upstreamPackageRelease` against the currently-pinned ontology yields
  triples with undeclared predicates that no consumer can reason over. Note
  that current `core.shacl.ttl` would **not** flag this — it has no closed
  shape and no constraint on these terms (§11.15-16); the ordering discipline
  is the only protection until those shapes exist.
- **Seeding is best-effort until §4.4 lands in full.** The assertion emission
  and the `seed.rs` query rewrite must both ship, and a full collection cycle
  must complete, before seeds correspond reliably to their ecosystem. Landing
  the emission alone corrupts nothing — an unresolvable IRI is inert — but
  identity edges for mixed-capability packages may reference registry nodes
  that were never collected.
- **The flat `upstream*` properties remain ambiguous by construction** and no
  amount of collector change fixes that (§2.2). They are retained for
  backward compatibility only. Any consumer needing a trustworthy
  name↔ecosystem or target↔version pairing must read `:UpstreamAssertion`;
  consumers joining the flat properties are reading a cross-product whether
  or not they realize it. This is worth stating in the ontology docstrings
  when #9 lands.
- Graph-size impact: these are additive triples on existing subjects. No new
  named graphs, no graph URI changes — the collision risk that has bitten
  before does not apply here.

## 11. Testing

Unit tests, per the existing `#[cfg(test)]` convention in each collector:

1. `registry_identity_uri` returns the exact URI minted by each collector, for
   all nine supported ecosystems (twelve table rows minus the three
   exclusions). Each assertion hard-codes the expected string
   and is paired with a comment citing the collector line it mirrors. These
   are the regression tests for the §3.2 traps.
2. `registry_identity_uri("gomod", "github.com/foo/bar/pkg/baz")` returns
   `None` (§3.4) — it must **not** mint `…/go/modules/any/…pkg%2Fbaz`, which
   matches no node `gomod.rs` produces.
3. `registry_identity_uri("maven", "g:a")` produces
   `…/maven/central/any/g%2Fa` — **percent-encoded, not a path separator**.
   `encode()` (uris.rs:82-85) matches Python's `quote(component, safe="")`, so
   the `/` produced by the `g:a → g/a` name transform is escaped by
   `package_identity_uri`. `maven.rs:905` builds the same `g/a` name and gets
   the same encoding, which is what makes the two sides match. An expectation
   ending in a literal `g/a` would pass against a hand-built string and fail
   against the collector.
4. `registry_identity_uri("pypi", "Foo.Bar")` produces `…/any/foo-bar`
   (PEP 503), while `registry_identity_uri("hackage", "HUnit")` preserves
   case.
5. `registry_identity_uri` returns `None` for all three excluded ecosystems —
   `cpan`, `gomod`, `conda` — and a DQ issue is recorded for each.
5b. `registry_identity_uri("cargo", "serde/derive")` produces
   `…/cargo/crates.io/any/serde`, identical to `("cargo", "serde")`. A fixture
   RPM with `crate(serde)`, `crate(serde/derive)`, and `crate(serde/std)`
   yields exactly **one** assertion, not three.
6. **Altitude tests** (the `45a0aaf` regression guard): assert
   `upstreamPackageIdentity` appears on the identity subject and *never* on
   the versioned subject, and that `pkg:bundles` does likewise. One per
   affected collector. Assert also that no emitter's `upstreamPackageName`
   subject *changed* — these are characterization tests locking in current
   behaviour, since §4.1 deliberately moves nothing.
7. `bundled(crate(nom))` parses to `("cargo", "nom")` and yields a `pkg:bundles`
   edge; `bundled(` with unbalanced parens yields no edge and no panic.
8. npm `bundledDependencies` as a name array, absent, empty array, and the
   legacy boolean `true` form — the boolean emits nothing and does not panic.
   Assert `npm:bundledDependency` is never emitted (§2.2).
9. Each of the nine §6 collectors emits `pkg:upstreamRepository` on the
   identity for a forge URL, and nothing for a non-forge URL.
10. `gomod.rs`'s **forge wiring** (§6, unaffected by the §3.4 exclusion):
    major-version suffix stripping `github.com/go-chi/chi/v5` →
    `github.com/go-chi/chi`, and no emission for `golang.org/x/…` or vanity
    domains that match no forge.

Integration:

11. A fixture pair — a Fedora `ghc-hoauth2` primary.xml excerpt and a Hackage
    `hoauth2` cabal response — produces a graph in which the §8 query returns
    exactly one binding. This is the end-to-end assertion that the design's
    motivating question is answered.
12. The §7 deriver emits an edge when versions match and **no** edge when the
    registry holds a different version, asserted against a fixture with a
    deliberate version mismatch.
13. **The cross-product regression test.** A fixture RPM providing
    `python3dist(foo) = 1.0.0` and `crate(bar) = 1.0.0` — deliberately the
    same version string, which is what makes a false pair bind — must yield
    exactly two `UpstreamAssertion` nodes, exactly two
    `upstreamPackageIdentity` shortcuts, and from §7 exactly **two**
    `upstreamPackageRelease` edges (`foo→pypi/foo@1.0.0`,
    `bar→cargo/bar@1.0.0`) and never four. Assert explicitly that
    `pypi/foo@1.0.0` is *not* linked from the `bar` assertion. This is the
    single most important test in the design; both previous revisions would
    have failed it.
14. `seed.rs`'s rewritten query (§4.4) against the same fixture returns `foo`
    for `pypi` and `bar` for `cargo`, and neither name for the other
    ecosystem.

**SHACL does not currently cover any of this, and a previous revision claimed
otherwise.** `grep` over `core/core.shacl.ttl` for `sh:closed`,
`upstreamEcosystem`, `upstreamPackageName`, `upstreamPackageVersion`, and
`upstreamPackageIdentity` returns **nothing** — there is no closed shape and
no constraint on the family. Existing validation would not have caught the
undeclared `upstreamPackageVersion` and will not catch a recurrence. Two
additions are needed, filed against the ontology repo:

15. An explicit `sh:NodeShape` for `:UpstreamAssertion`. The `sh:maxCount 1`
    constraints are what mechanically enforce the pairing this whole revision
    is about — an assertion that admitted two targets or two versions would
    reintroduce the cross-product inside the node meant to prevent it. A
    previous draft of this item listed only three of the seven constraints and
    omitted method, confidence, and the owner link entirely:

    ```turtle
    pkg:UpstreamAssertionShape a sh:NodeShape ;
        sh:targetClass pkg:UpstreamAssertion ;

        # exactly one owner, and it must be an identity
        sh:property [ sh:path [ sh:inversePath pkg:hasUpstreamAssertion ] ;
                      sh:class pkg:PackageIdentity ;
                      sh:minCount 1 ; sh:maxCount 1 ;
                      sh:message "An upstream assertion must be owned by exactly one package identity."@en ] ;

        sh:property [ sh:path pkg:assertionTarget ;
                      sh:class pkg:PackageIdentity ;
                      sh:minCount 1 ; sh:maxCount 1 ] ;
        sh:property [ sh:path pkg:assertionUpstreamName ;
                      sh:datatype xsd:string ;
                      sh:minCount 1 ; sh:maxCount 1 ] ;
        sh:property [ sh:path pkg:assertionEcosystem ;
                      sh:class pkg:Ecosystem ;
                      sh:minCount 1 ; sh:maxCount 1 ] ;
        sh:property [ sh:path pkg:assertionRelation ;
                      sh:minCount 1 ; sh:maxCount 1 ;
                      sh:in ( pkg:upstream-repackages pkg:upstream-bundles ) ] ;
        sh:property [ sh:path pkg:matchMethod ;
                      sh:minCount 1 ; sh:maxCount 1 ] ;
        sh:property [ sh:path pkg:matchConfidence ;
                      sh:datatype xsd:decimal ;
                      sh:minValue 0.0 ; sh:maxValue 1.0 ;
                      sh:minCount 1 ; sh:maxCount 1 ] ;

        # optional, but at most one each
        sh:property [ sh:path pkg:assertionUpstreamVersion ;
                      sh:datatype xsd:string ; sh:maxCount 1 ] ;
        sh:property [ sh:path pkg:assertionSourceBuild ;
                      sh:class pkg:Package ; sh:maxCount 1 ] .
    ```

15b. A **SPARQL-based constraint** for the one rule shape arithmetic cannot
    express: where `:assertionSourceBuild` is present, it must be a build of
    the owning identity. Without this an assertion could cite evidence from an
    unrelated package and still validate.

    ```sparql
    # violation if this returns any rows
    SELECT ?assertion WHERE {
      ?owner     pkg:hasUpstreamAssertion ?assertion .
      ?assertion pkg:assertionSourceBuild ?build .
      FILTER NOT EXISTS { ?build pkg:isVersionOf ?owner }
    }
    ```
16. A **term-existence test**: every predicate IRI emitted by any collector
    must have a declaration in the ontology. This is the general guard — it
    would have caught `upstreamPackageVersion` when it was first emitted,
    rather than at design-review time years later. Implementable as a test
    that extracts `{PKG}`-prefixed format strings from
    `etl/pg-collect/src/**` and checks each against the released `core.ttl`.

## 12. Follow-on sub-projects

This is sub-project 1 of a series identified while scoping. The remainder, in
dependency order:

2. **Artifact-inspection bundling tier** — Go `vendor/modules.txt` and
   `go version -m`, Maven shaded-JAR `META-INF/maven/*/pom.properties`, Python
   wheel `.libs/`. Highest-fidelity evidence; needs a download-and-unpack
   collector class. Emits into `pkg:bundles` unchanged.
3. **A real RPM spec model** — recursive `%global`/`%define` expansion,
   `%package` subpackage tree, `%if`/`%bcond` conditional context, generator
   detection (`cabal-rpm`, `rust2rpm`, `go2rpm`, `pyproject-rpm-macros`).
   Prerequisite for anything spec-derived. Includes the open question of
   whether to write a resolver or shell out to `rpmspec --parse`.
4. **Spec-derived bundling and subpackages** — `Source1..N`,
   `%ghc_lib_subpackage -l BSD-3-Clause` (per-component licenses that differ
   from the header `License:` field), `%package devel/doc/prof` roles,
   `Provides: %{name}-static` static-linkage signals. Depends on 3.
5. **Ecosystem-native dependency graphs** — `BuildRequires: ghc-aeson-devel`
   → `hackage:aeson`, using the §3 resolver in the reverse direction.
6. **Generalize beyond RPM** — Debian `dh-cargo`/`pybuild`/`dh_haskell`,
   `debian/copyright` `Files-Excluded`, Gentoo, Nix.

### Landing order

```
1. ontology PR A — defect repairs                      (§2.4, filed as ontology#9)
2. ontology PR B — UpstreamAssertion + relation scheme (§2.2, to be filed)
     + matchMethod/matchConfidence union domains
     + UpstreamAssertion SHACL shape                   (§11.15)
3. ontology release + upload-ontology.sh
4. collector changes: assertions + shortcuts           (§3, §4, §5, §6)
5. seed.rs query rewrite                               (§4.4)
6. full collection cycle
7. derive_upstream_release                             (§7)
```

PRs A and B are independent and can be authored in parallel, but both must be
in the same release before step 4. Steps 5 and 6 gate step 7.

The `rpm.rs` `HashSet` fix proposed in the previous revision is **dropped**.
It would have emitted the missing `upstreamEcosystem` triples without making
any name attributable (§4.4), producing a more complete cross-product rather
than a correct pairing. The per-capability assertion supersedes it entirely.
Fixing the latched boolean is still worthwhile for the flat properties' own
completeness, but it is cosmetic once assertions exist and is not a
prerequisite for anything.

### Smaller items surfaced and deliberately not fixed here

- `cpan.rs`'s distribution-vs-module inconsistency (§3.3), which makes its
  existing dependency edges dangle today.
- No registry collector emits `pkg:partOfEcosystem`, so "all Hackage packages"
  is not traversable from the `Ecosystem` hub — only the distro side points at
  it.
- Go `replace` directives as an identity-substitution relation.
- `npm:bundledDependency` remains declared but unemitted until the
  artifact-inspection tier can supply resolved versions (§2.2).
- The `no-forge-match` DQ issue (collect_spec.rs:232-241) fires at `info` for
  every registry-hosted `Source0`; consider demoting it where an ecosystem was
  successfully resolved.
