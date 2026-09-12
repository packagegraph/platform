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
emitted it. Most of this design is therefore wiring, not invention.

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
ecosystem token `gomod` corresponds to the URI segment `go`.

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
is the declaration-based path (§5), which covers the large `rust2rpm` /
`go2rpm` population today; the Haskell subpackage path is §12.4. The example
is used here because it makes the shape of the missing relationship legible,
not because §5 resolves it.

### Goals

1. Emit a traversable edge from a distribution package identity to the
   upstream registry package identity it repackages, for every ecosystem
   where the identifiers resolve deterministically.
2. Emit bundled-component edges from evidence already present in data we
   already fetch — no new artifact downloads.
3. Complete the `distro → registry → forge` chain by wiring the nine registry
   collectors that currently discard upstream repository URLs into the
   existing `UpstreamProject` hub.
4. Correct the RDFS domain violations found in the current
   `upstreamPackageName` emission sites.
5. Add version-level links where, and only where, both endpoints exist.

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
- **CPAN.** See §3.3 — CPAN cannot be resolved by string transform and is
  explicitly excluded from v1 rather than emitted as dangling references.

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

**This design uses those terms as-is. No ontology change is required for the
identity join.**

### 2.2 New terms

Two additions to `core.ttl`, both following the existing `upstream*` naming
family.

```turtle
:bundles a owl:ObjectProperty ;
    rdfs:label "bundles"@en ;
    IAO:0000115 "The subject package contains, by value, code from the object
      package — vendored, statically linked, or embedded in the shipped
      artifact rather than resolved as an external dependency. Distinct from
      :directlyDependsOn, which describes a resolved external requirement."@en ;
    rdfs:domain :PackageIdentity ;
    rdfs:range :PackageIdentity ;
    rdfs:isDefinedBy : .

:upstreamPackageRelease a owl:ObjectProperty ;
    rdfs:label "upstream package release"@en ;
    IAO:0000115 "Links a specific distribution package build to the specific
      upstream registry release it repackages. Version-level counterpart to
      :upstreamPackageIdentity."@en ;
    rdfs:domain :Package ;
    rdfs:range :Package ;
    rdfs:isDefinedBy : .
```

`npm:bundledDependency` is retained and emitted alongside `pkg:bundles` for
npm, since it already exists and is ecosystem-idiomatic. `cpan:containsModule`
(cpan.ttl:23) is left alone — it is a distribution→module relation, not
bundling, and is out of scope per §3.3.

### 2.3 Evidence annotation

Bundling evidence quality varies by an order of magnitude between ecosystems.
A `bundles` edge derived from an RPM `Provides: bundled(...)` (packager-
declared, frequently stale) must not be indistinguishable from one derived
from `vendor/modules.txt` (exact, machine-generated) when that tier arrives.

Reuse the existing confidence ladder rather than inventing one.
`collect_spec.rs:453` already grades detection methods:

```rust
"source0-domain" | "buildrequires-macro" => "high",
// name-prefix => "medium"
```

Every `bundles` edge and every `upstreamPackageIdentity` edge records its
derivation method via the existing `emit_dq_issue` mechanism
(`forge.rs::emit_dq_issue`), with these method tokens:

| Method | Confidence | Source |
|---|---|---|
| `rpm-bundled-provides` | high | `Provides: bundled(X)` in primary.xml |
| `npm-bundled-deps` | high | `bundledDependencies` in registry JSON |
| `ecosystem-provides` | high | `ghc-pkg(X)`, `crate(X)`, `python3dist(X)` etc. |
| `source0-domain` | high | Source0 URL domain (existing) |
| `name-prefix` | medium | `ghc-`, `python3-`, `rust-` prefix (existing) |

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
in the corresponding collector. The distro and release segments are **not**
guessable from the ecosystem token — two of them differ.

| Ecosystem token | Distro seg | Release seg | Name transform | Verified at |
|---|---|---|---|---|
| `hackage` | `hackage` | `hackage` | none | hackage.rs:278 |
| `pypi` | `pypi` | `index` | **PEP 503 normalize** | pypi.rs:509 |
| `npm` | `npm` | `registry` | none | npm.rs:194 |
| `cargo` | `cargo` | `crates.io` | none | cargo_collect.rs:281 |
| `rubygems` | `rubygems` | `org` | none | rubygems.rs:201 |
| `gomod` | **`go`** | `modules` | none | gomod.rs:309 |
| `maven` | `maven` | `central` | **`g:a` → `g/a`** | maven.rs:907 |
| `cran` | `cran` | `cran` | none | cran.rs:209 |
| `hex` | `hex` | `pm` | none | hex_collect.rs:266 |
| `nuget` | `nuget` | `gallery` | none | nuget.rs:234 |
| `cpan` | — | — | **excluded, see §3.3** | cpan.rs:195 |
| `conda` | — | — | **excluded, see §3.4** | conda.rs:198 |

All identities are `package_identity_uri(distro_seg, release_seg, "any", transformed_name)`.

Three rows carry traps that a naive `token == segment` implementation would
get wrong, producing well-formed URIs that match nothing:

- **`gomod` → `go`.** The distro side writes `pkg:upstreamEcosystem
  <d/ecosystem/gomod>` (rpm.rs maps `golang(X)` to the token `gomod`), but
  `gomod.rs:309` mints identities under the segment `go`.
- **`maven` separator.** `rpm.rs` derives `"g:a"` from `mvn(group:artifact)`,
  while `maven.rs:907` mints the path segment `g/a`.
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

### 3.4 conda is excluded

`conda.rs:198` takes its distro, release, and subdir from CLI arguments
(`main.rs:352-365`), defaulting to `conda`/`conda-forge`/`linux-64`. The
subdir is an architecture, not `"any"`. There is no static row that is correct
for all invocations.

conda is also the one registry collector that emits `pkg:upstreamEcosystem`
*outward* (conda.rs:354-370, pointing at pypi/cran/cargo), which makes it a
natural candidate for the *subject* side of this edge in a later pass rather
than the object side.

## 4. Emission sites and subject altitude

### 4.1 The existing domain violation

`pkg:upstreamPackageName` has `rdfs:domain :Package` (core.ttl:942).
`PackageIdentity` is `rdfs:subClassOf :PackageEntity` (core.ttl:1466) — it is
**not** a subclass of `:Package`.

Four collectors emit `upstreamPackageName` on a `PackageIdentity` subject:

| Site | Subject | Correct? |
|---|---|---|
| rpm.rs:1115-1131 | versioned `pkg_uri` | yes |
| collect_spec.rs:441-445 | `PackageIdentity` | **domain violation** |
| debian.rs:612-623 | `PackageIdentity` | **domain violation** |
| collect_salsa.rs:340-352 | `PackageIdentity` | **domain violation** |
| collect_sources.rs:241-262 | `PackageIdentity` | **domain violation** |

Under RDFS entailment these wrongly infer those identities into `:Package`.
This is the same bug class that commit `45a0aaf` fixed for
`pkg:hasUpstreamProject`, rediscovered in a neighbouring predicate.

### 4.2 The correction, and the inverted altitude of the new edge

The new `pkg:upstreamPackageIdentity` has domain `PackageIdentity` — the
**opposite** altitude from `upstreamPackageName`. Both must be emitted, at
different subjects, from the same call site:

```turtle
# identity-level — the new traversable edge
<d/pkg/fedora/43/x86_64/ghc-hoauth2>
    pkg:upstreamPackageIdentity <d/pkg/hackage/hackage/any/hoauth2> .

# package-level — the retained literal, on the versioned subject
<d/pkg/fedora/43/x86_64/ghc-hoauth2/2.14.3-2.fc44>
    pkg:upstreamPackageName "hoauth2" ;
    pkg:upstreamPackageVersion "2.14.3" .
```

Emitting the new edge on the versioned package, or the literal on the
identity, reintroduces `45a0aaf`. Tests in §11 assert both altitudes
explicitly.

`pkg:upstreamEcosystem` is left on its current subjects for now — changing it
would break `enrich_taxonomy.rs:88-92` and `seed.rs:16-47`, which read it off
the identity. Noted as follow-on.

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

### 4.4 Why this join is lossless

`seed.rs::discover_by_ecosystem` (seed.rs:16-47) queries Fuseki for
`upstreamPackageName` literals filtered by ecosystem and writes them to a seed
file; `hackage.rs:62-65` and its ten siblings feed that file straight into
`collect()`. The registry-side node is therefore created **from the exact
string the distro run recorded** — byte-identical by construction, for the 11
collectors wired this way (`cran.rs` and `conda.rs` are not).

This makes the identity join deterministic rather than heuristic for the
seeded path, and is the reason §3.2's transforms are the only normalization
needed.

## 5. Bundled components

### 5.1 RPM: `Provides: bundled(X)`

Fedora packaging policy requires bundled code to be declared as
`Provides: bundled(<capability>)`. The inner capability is the *same* string
format that `rpm.rs::emit_ecosystem_triples` (rpm.rs:1054-1110) already parses:

```
bundled(crate(nom))           → cargo / nom
bundled(golang(github.com/…)) → gomod / github.com/…
bundled(npm(lodash))          → npm / lodash
bundled(python3dist(six))     → pypi / six
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
sub-projects 3 and 4. What §5.1 does cover in v1 is the large population of
`rust2rpm`- and `go2rpm`-generated packages, where `bundled(crate(...))` and
`bundled(golang(...))` are the standard and widely-populated declaration.

`bundled()` capabilities frequently carry no version. That is expected:
`pkg:bundles` is identity-level, so no version is required. Versions on
bundled components are **not** modelled in v1 — a package bundling several
components cannot attribute a flat `pkg:upstreamPackageVersion` literal to any
one of them, so version-bearing bundles need a qualified (reified) structure.
The §7 deriver therefore covers repackaging only, not bundling. Deferred with
the artifact-inspection tier (§12.2), where exact per-component versions
arrive anyway.

`primary.xml` is already parsed and already carries the full `Provides:` list
(`rpm.rs::parse_primary_metadata`, rpm.rs:575), so this adds **no fetching**.

### 5.2 npm: `bundledDependencies`

`npm.rs` does not currently deserialize `bundledDependencies` /
`bundleDependencies` at all. Add the field to the version struct, and for each
entry emit both:

```turtle
<npm-identity> pkg:bundles           <d/pkg/npm/registry/any/lodash> .
<npm-identity> npm:bundledDependency <d/pkg/npm/registry/any/lodash> .
```

`npm:bundledDependency` (npm.ttl:9-14) has domain and range `npm:NpmPackage`,
which is the versioned class — so that triple goes on the versioned subject
while `pkg:bundles` goes on the identity. Same split as §4.2.

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

A new `derive_upstream_release` stage, following the existing pattern of
`derive_comparison.rs` / `derive_releases.rs`, runs after collection and emits
an edge **only where both endpoints are present**:

```sparql
CONSTRUCT {
  ?distroPkg pkg:upstreamPackageRelease ?upRelease .
}
WHERE {
  ?distroPkg  pkg:upstreamPackageVersion  ?v ;
              pkg:isVersionOf             ?distroId .
  ?distroId   pkg:upstreamPackageIdentity ?upId .
  ?upRelease  pkg:isVersionOf             ?upId ;
              pkg:versionString           ?v .
}
```

Properties of this approach:

- Dangling references are structurally impossible — `?upRelease` must already
  exist to bind.
- Zero additional fetching.
- No collector changes; it consumes `pkg:upstreamPackageVersion`, which
  `rpm.rs:1128-1131` already emits from versioned ecosystem `Provides`.
- Coverage improves automatically as registry version coverage improves,
  with no code change.

Version-string equality is exact-match on `pkg:versionString`. RPM
version normalization (epochs, tilde/caret ordering) is deliberately *not*
applied: a mismatch should surface as a missing edge and a DQ record, not as a
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

## 9. Data flow

```
primary.xml  ─┬─► rpm.rs::emit_ecosystem_triples
              │     ├─ Provides: ghc-pkg(X)     → registry_identity_uri → upstreamPackageIdentity
              │     └─ Provides: bundled(...)   → registry_identity_uri → bundles
              │
dist-git spec ─► collect_spec.rs::emit_ecosystem_triples
              │     └─ detect_ecosystem          → registry_identity_uri → upstreamPackageIdentity
              │
registry APIs ─► hackage/pypi/npm/… collectors
                    ├─ package identity + version nodes  (existing)
                    ├─ repo URL → forge::emit_upstream_repo → UpstreamProject hub   (§6, new)
                    └─ npm bundledDependencies → bundles                            (§5.2, new)

                         ▼ (after all collection)
                    derive_upstream_release  →  upstreamPackageRelease   (§7, new)
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
- `pkg:upstreamPackageName` literals are **retained** at their current values
  and subjects-as-corrected. `enrich_taxonomy.rs:88-92` and `seed.rs:16-47`
  read them and must keep working; the seed mechanism in particular is what
  makes §4.4 hold, so it cannot be broken by this change.
- Graph-size impact: these are additive triples on existing subjects. No new
  named graphs, no graph URI changes — the collision risk that has bitten
  before does not apply here.

## 11. Testing

Unit tests, per the existing `#[cfg(test)]` convention in each collector:

1. `registry_identity_uri` returns the exact URI minted by each collector, for
   all ten supported ecosystems. Each assertion hard-codes the expected string
   and is paired with a comment citing the collector line it mirrors. These
   are the regression tests for the §3.2 traps.
2. `registry_identity_uri("gomod", …)` produces a `/go/modules/` path, **not**
   `/gomod/`.
3. `registry_identity_uri("maven", "g:a")` produces `…/maven/central/any/g/a`.
4. `registry_identity_uri("pypi", "Foo.Bar")` produces `…/any/foo-bar`
   (PEP 503), while `registry_identity_uri("hackage", "HUnit")` preserves
   case.
5. `registry_identity_uri("cpan", …)` and `("conda", …)` return `None`.
6. **Altitude tests** (the `45a0aaf` regression guard): assert
   `upstreamPackageIdentity` appears on the identity subject and *never* on
   the versioned subject; assert `upstreamPackageName` appears on the
   versioned subject and *never* on the identity subject. One such test per
   affected collector.
7. `bundled(crate(nom))` parses to `("cargo", "nom")` and yields a `pkg:bundles`
   edge; `bundled(` with unbalanced parens yields no edge and no panic.
8. npm `bundledDependencies` present / absent / empty-array.
9. Each of the nine §6 collectors emits `pkg:upstreamRepository` on the
   identity for a forge URL, and nothing for a non-forge URL.
10. `gomod` major-version suffix stripping: `github.com/go-chi/chi/v5` →
    `github.com/go-chi/chi`.

Integration:

11. A fixture pair — a Fedora `ghc-hoauth2` primary.xml excerpt and a Hackage
    `hoauth2` cabal response — produces a graph in which the §8 query returns
    exactly one binding. This is the end-to-end assertion that the design's
    motivating question is answered.
12. The §7 deriver emits an edge when versions match and **no** edge when the
    registry holds a different version, asserted against a fixture with a
    deliberate version mismatch.

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

Smaller items surfaced by this design and deliberately not fixed here:

- `cpan.rs`'s distribution-vs-module inconsistency (§3.3), which makes its
  existing dependency edges dangle.
- `pkg:upstreamEcosystem` subject altitude differs between `rpm.rs` and the
  other four emitters (§4.2).
- No registry collector emits `pkg:partOfEcosystem`, so "all Hackage packages"
  is not traversable from the `Ecosystem` hub — only the distro side points at
  it.
- Go `replace` directives as an identity-substitution relation.
