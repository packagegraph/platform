# RHEL Rebuild Comparison Deriver — Design

**Date:** 2026-09-01
**Branch:** `alma-rocky-rhel`
**Status:** Design (rev 2, incorporating design-review findings) — awaiting review before implementation plan
**Predecessor:** [Alma/Rocky/RHEL spike report](../../reports/2026-09-01-alma-rocky-rhel-comparison.md)

## 1. Overview

The spike answered "how faithfully do AlmaLinux and Rocky Linux track RHEL?"
with ad-hoc SPARQL. This deriver promotes that comparison to a **first-class,
stored, queryable property of the graph**: for each rebuild source-package
**name**, it classifies the rebuild's **current (newest) build** against RHEL —
recording what that build is a rebuild *of* and how faithfully it tracks RHEL,
including whether it is **behind** RHEL (a security-relevant lag signal).

Scope is deliberately a **latest-per-name snapshot** (§5.2), not per-historical-
build; see finding-driven decision in §5.2. It follows the existing deriver
pattern (`derive_releases.rs` → `DerivePackageHistory` →
`graph/derived/package-history`) and the "classify by comparing versions across
distribution graphs" pattern already described by `pkg:freshnessStatus`.

### Goals

- Materialize, for each rebuild source-package name, a rebuild-lineage link and
  a fidelity status for its current build, into a derived graph refreshed on a
  schedule.
- Reuse existing ontology vocabulary where it fits; add the minimum new terms.
- Distinguish *behind* (lagging RHEL) from *ahead* (timing skew) via real,
  epoch-aware RPM version comparison.

### Non-goals (v1)

- Per-historical-build classification (only the current/newest build per name).
- Binary/arch-level or file-level comparison (source package is the unit).
- CRB / extras / debuginfo repos, non-x86_64 arches.
- Content-level (source hash) equivalence — status is NVR-string based, with
  documented normalization.
- Asserting `rhel-exclusive` as triples — that set is trivially query-derivable
  (RHEL names minus rebuild names) and is left as a query, not stored state.

## 2. Background: what already exists

From exploration of `../ontology` (pinned v0.12.0) and the ETL:

- **`:derivedFromDistribution`** (`core.ttl`) — Distribution→Distribution,
  *"CentOS from RHEL … distribution lineage for package tracking."* Reused
  as-is for the distribution-level assertion.
- **`:freshnessStatus`** — ObjectProperty, domain `PackageIdentity`, range
  `skos:Concept`, *"computed post-collection by comparing versions across
  distribution graphs"*; values are `pkg:fresh-*` concepts in
  `FreshnessStatusScheme`. This is the exact template for the new status term.
- **`:crossDistributionAlternative`** — symmetric, non-transitive "similar
  function" correspondence. NOT reused: rebuild lineage is directional and
  stronger than "similar function."
- **`:DataSnapshot` / `:isCurrent` / `:snapshotGraph`** — graph-currency
  vocabulary that exists in the ontology but is **not currently emitted by any
  collector or loader** (only referenced in docs and the unimplemented v0.5.0
  buildout plan). Therefore v1 readiness (§6) must **not** depend on it.
- **Ontology gating pattern** — `enrich_revdeps` / `enrich_blast_radius` run a
  `SELECT ?p WHERE { <term> a ?p } LIMIT 1` and refuse to run if empty. The
  deriver adopts this so it can merge before the ontology terms are synced.
- **No RPM version comparison exists** in the codebase — `rpmvercmp` is new.
- **Source packages carry no epoch.** `emit_source_package_triples` emits only a
  `versionString` (version-release) parsed from the `.src.rpm` filename, which
  has no epoch. Binary packages *do* emit epoch (`rpm.rs:820`, `rpm:epoch` at
  `:869`) and link to their source via `builtFromSource`. This drives the epoch
  rule in §4.

## 3. Ontology changes (`packagegraph/ontology`, → v0.13.0)

All additions live in the **core** module, consistent with
`:derivedFromDistribution` and `:freshnessStatus`.

> Portability note: the *terms* are generic (any rebuild lineage), but the
> classifier/normalizer in this deriver is **RPM/RHEL-specific**. Applying the
> vocabulary to Debian/Ubuntu would require a different normalizer; that is out
> of scope here.

### 3.1 `:rebuildOf` — package-level rebuild lineage (asserted only on match)

```turtle
:rebuildOf a owl:ObjectProperty ;
    rdfs:label "rebuild of"@en ;
    IAO:0000115 "Links a source package in a downstream rebuild distribution to the specific upstream source package it reproduces (e.g. an AlmaLinux SRPM to the RHEL SRPM it was rebuilt from). Directional build-lineage; asserted only when the downstream NVR matches an upstream build exactly or after documented normalization."@en ;
    rdfs:comment "This source package is a rebuild of a specific upstream source package"@en ;
    rdfs:domain :SourcePackage ;
    rdfs:range :SourcePackage ;
    rdfs:subPropertyOf prov:wasDerivedFrom ;
    rdfs:isDefinedBy : .
```

Because `:rebuildOf` is a `prov:wasDerivedFrom` subproperty (a genuine lineage
claim), it is emitted **only for `exact` / `vendor-patched` / `modular-equivalent`
matches** — cases where the downstream demonstrably rebuilt that upstream SRPM.

### 3.2 `:comparedAgainst` — non-lineage baseline (for ahead/behind)

```turtle
:comparedAgainst a owl:ObjectProperty ;
    rdfs:label "compared against"@en ;
    IAO:0000115 "Records the upstream source package used as the comparison baseline when classifying a rebuild build whose NVR does NOT match any upstream build under normalization (ahead / behind / equivalent-version). This is a comparison reference, NOT a provenance/lineage claim — e.g. an 'ahead' rebuild may derive from a newer or not-yet-visible upstream build, and an 'equivalent-version' match is only rpmvercmp-equal, not a proven rebuild of that SRPM."@en ;
    rdfs:comment "Upstream baseline used for an ahead/behind classification; not a lineage claim"@en ;
    rdfs:domain :SourcePackage ;
    rdfs:range :SourcePackage ;
    rdfs:isDefinedBy : .
```

Deliberately **not** a `prov:wasDerivedFrom` subproperty (addresses the false-
provenance risk for ahead/behind).

### 3.3 `:rebuildTrackingStatus` — fidelity status (mirrors `:freshnessStatus`)

```turtle
:rebuildTrackingStatus a owl:ObjectProperty ;
    rdfs:label "rebuild tracking status"@en ;
    IAO:0000115 "Classifies how faithfully a rebuild source package's current build tracks its upstream (RHEL). Values are SKOS concepts from the RebuildTrackingScheme. Computed post-collection by comparing source NVRs across distribution graphs with epoch-aware RPM version comparison and documented normalization."@en ;
    rdfs:comment "How faithfully this rebuild source package's current build tracks upstream"@en ;
    rdfs:domain :SourcePackage ;
    rdfs:range skos:Concept ;
    rdfs:isDefinedBy : .
```

### 3.4 `pkg:RebuildTrackingScheme` (`skos-schemes.ttl`)

Seven concepts, mirroring `FreshnessStatusScheme`:

| Concept | Meaning | Emits `:rebuildOf`? |
|---|---|---|
| `pkg:track-exact` | Current build's NVR is byte-identical to an upstream build. | yes |
| `pkg:track-vendor-patched` | `strip_vendor()` of the current build's NVR equals an unmodified upstream build's NVR — i.e. the downstream NVR is provably the upstream NVR plus an anchored vendor tag. | yes |
| `pkg:track-modular-equivalent` | Modular package; base NVR (pre-`.module…`) matches upstream, module build-context differs. | yes |
| `pkg:track-ahead` | No NVR match; current build EVR > upstream newest EVR (rebuild leads the snapshot). | no (`:comparedAgainst`) |
| `pkg:track-behind` | No NVR match; current build EVR < upstream newest EVR (**rebuild lags upstream — investigate**). | no (`:comparedAgainst`) |
| `pkg:track-equivalent-version` | No NVR match under any normalization, but `rpmvercmp` reports the current build's EVR **equal** to an upstream build's EVR (e.g. from ignored separators or leading-zero differences). Recorded as a comparison, **not** a lineage claim. | no (`:comparedAgainst`) |
| `pkg:track-exclusive` | Source name absent from upstream (branding / distro extras). | no |

SHACL (`core.shacl.ttl`) and examples (`core.examples.ttl`) updated per the
`FreshnessStatusScheme` precedent.

### 3.5 Version bump & allowlist

- `CHANGELOG.md`; version → v0.13.0. No new module files (terms land in
  `core.ttl` / `skos-schemes.ttl`), so `sync-ontology.sh` keeps 37 modules; bump
  the pinned version reference.

## 4. Version comparison & normalization (new Rust, pure & unit-tested)

### 4.1 `rpmvercmp` + epoch-aware EVR

New module implementing the standard RPM version-compare algorithm (alternating
digit/alpha runs; digits numeric, alpha lexical; `~` before all; `^` after),
validated against RPM's canonical `rpmvercmp` test vectors. EVR comparison:
epoch (numeric) then version then release via `rpmvercmp`.

**Source epoch rule (finding 3):** source nodes have no epoch. The deriver
obtains a source package's epoch via `source ←builtFromSource← binary
→hasVersion→ epoch`, taking the **max epoch across binaries built from that
source** (RPM epoch is spec-global; max is a safe tie-break if subpackages ever
differ). If no binary/epoch is found, epoch defaults to `0`. This rule is
applied to **both** sides so comparison is symmetric. Documented as a limitation
where a source has no surviving binary in the collected repos.

### 4.2 Normalization (finding 6 — tightened)

- `module_base(ver)` — truncate at the first `.module` marker so
  `1.9.0-4.module+el9.8.0+24092+hash` and `1.9.0-4.module_el9.6.0+148+hash` both
  reduce to `1.9.0-4`.
- `strip_vendor(rel)` — remove only **anchored, distro-specific** suffixes via
  explicit regex grammars (e.g. `\.rocky(\.\d+)*$`, `\.alma(\.\d+)*$`, and the
  observed `\.el9(_\d+)?\.rocky\.\d+\.\d+$` / `…\.0\.\d+$` re-tag forms). The
  bare "strip any trailing `.0.N`" rule is **removed** — it could erase a
  legitimate upstream release component.

Normalization for `vendor-patched` is applied **only to the downstream
candidate**, then matched against **unmodified** upstream builds (finding 6).
`modular_equivalent` compares `module_base` of both sides (the module context is
inherently vendor-specific, so symmetric truncation is correct there).

## 5. Deriver design (`etl/pg-collect/src/derive_comparison.rs`)

Mirrors `ReleaseDeriver`: constructed with a Fuseki endpoint; `derive()`
queries, classifies in Rust, writes N-Triples.

### 5.1 Inputs — pairing & distribution IRIs

Rebuild→upstream pairs are passed as repeatable `--pair
<rebuild_graph_iri>=<rhel_graph_iri>`, **default: four pairs over six graphs**:

```
almalinux/9  = rhel/9      rocky/9  = rhel/9
almalinux/10 = rhel/10     rocky/10 = rhel/10
```

- Pair IRIs are **parsed and validated as absolute IRIs** before any SPARQL
  interpolation (finding 9 — injection safety).
- The **distribution IRI is not derived from the graph name** (finding 5). For
  each graph, the deriver queries `SELECT DISTINCT ?d { GRAPH <g> { ?p pkg:partOfDistribution ?d } }`
  and requires **exactly one** value; zero or multiple → fail closed for that
  pair with a clear error. `<rebuild_dist> :derivedFromDistribution <rhel_dist>`
  is emitted from those resolved IRIs.

### 5.2 Algorithm (latest-per-name snapshot)

**Attachment decision (finding 2):** the feature is explicitly a *latest-per-name
snapshot*. For each rebuild source name, the status and lineage attach to the
**newest source build node** (`Vr`, newest by epoch-aware EVR), which uniquely
represents the current build. Older historical source nodes are intentionally
not annotated. (A dedicated name-level source-identity node would be cleaner and
is noted as a future refinement.)

Per rebuild graph:

1. Load, per graph: `{name → [(sourceNodeURI, versionString, epoch)]}` for
   `?s a pkg:SourcePackage`, with epoch via the §4.1 binary join.
2. For each rebuild name `N`, pick `Vr` = newest by EVR (deterministic: max EVR;
   ties broken by lexicographically smallest source node URI).
   - `N ∉ upstream` → `track-exclusive`. No link.
   - Else classify `Vr` against upstream builds of `N`, first rule that holds:
     1. exact string equals some upstream build → `track-exact`
     2. `strip_vendor(Vr)` equals some **unmodified** upstream build's NVR → `track-vendor-patched`
     3. `module_base(Vr)` equals `module_base` of some upstream build → `track-modular-equivalent`
     4. else EVR-compare `Vr` vs upstream newest `Vh`: `>`→`track-ahead`, `<`→`track-behind`, `==`→`track-equivalent-version`

   Rules 1–3 are string/normalization matches that establish the downstream
   reproduced that specific upstream SRPM, so they assert `:rebuildOf`. Rule 4's
   `==` is EVR equality **without** a provable NVR correspondence (it can arise
   from `rpmvercmp` ignoring separators or leading zeros), so it asserts only
   `:comparedAgainst` — never lineage (finding 2).
   - **Deterministic target selection (finding 7):** when a matched rule admits
     multiple upstream builds, choose the **newest matching upstream build by
     EVR**, ties broken by smallest URI; if >1 remain indistinguishable, emit a
     `dq` data-quality note and still pick deterministically.
   - Emit `:rebuildTrackingStatus`. Emit `:rebuildOf` → the chosen upstream
     source node for exact/vendor-patched/modular-equivalent; emit
     `:comparedAgainst` → upstream newest for ahead/behind/equivalent-version;
     no link for exclusive.
3. Emit the distribution-level `:derivedFromDistribution` once per pair (§5.1).

### 5.3 Output & transactional replacement (finding 4)

Derived graph `https://packagegraph.github.io/graph/derived/rhel-rebuilds`.
Replacement is **staged and atomic**, not drop-then-additive-load:

1. Load the run's N-Triples into a run-specific staging graph
   `…/derived/rhel-rebuilds-staging-<runtoken>` (runtoken passed in via CLI/env
   since scripts have no clock/RNG).
2. Validate **structural invariants** (finding 3), not a fixed status set (a
   valid snapshot need not exhibit all statuses):
   - non-zero triple count;
   - **exactly one** `:rebuildTrackingStatus` per selected rebuild source node;
   - every status object is a concept in `pkg:RebuildTrackingScheme`;
   - **link type matches status**: exactly one `:rebuildOf` for exact/
     vendor-patched/modular-equivalent; exactly one `:comparedAgainst` for
     ahead/behind/equivalent-version; neither for exclusive;
   - **pair coverage**: every configured release-level pair produced ≥1 status;
     and exactly one `:derivedFromDistribution` per **distinct resolved
     distribution pair** (note: release-level pairs collapse at the distribution
     level — `almalinux/9` and `almalinux/10` both resolve to `distro/almalinux`,
     so the four default pairs yield two distribution triples: almalinux→rhel and
     rocky→rhel);
   - **no unexpected subjects**: every status/link subject is a source-package
     node from a configured rebuild graph.
3. Single Fuseki update to replace production atomically
   (`DROP SILENT GRAPH <prod>; ADD <staging> TO <prod>; DROP <staging>` in one
   request, or `COPY <staging> TO <prod>` which is defined as atomic replace).
4. On any failure before step 3, production is untouched; staging is dropped.

`DeriveReport` counts per status. (This intentionally improves on
`DerivePackageHistory`'s non-atomic drop+load; that command is out of scope to
change here but the pattern is noted for later.)

### 5.4 Gating

`check_ontology_terms()` verifies `:rebuildOf`, `:comparedAgainst`,
`:rebuildTrackingStatus`, and `pkg:RebuildTrackingScheme` are declared; errors
with an actionable message if not (pattern from `enrich_revdeps`).

## 6. CLI & deploy

- Subcommand `pg-collect derive-rebuild-comparison` with `--endpoint`,
  `--output`, repeatable `--pair`, `--run-token`, and `--load` (mirrors
  `DerivePackageHistory` at `main.rs:1120`/`2369`).
- **Readiness / snapshot consistency (findings 8 & 1).** The ETL does not emit
  `:DataSnapshot`/`:isCurrent` today, so v1 readiness is built **only on
  metadata that actually exists**. Before deriving, and failing closed on
  violation, the deriver checks for **each** input graph:
  - the graph exists and its source-package count is > 0 (fail closed if empty
    or absent — guards against a mid-replacement or failed collection);
  - `pkg:partOfDistribution` resolves to **exactly one** distribution IRI (§5.1);
  - a plausibility floor: source-package count ≥ a configurable minimum
    (e.g. the spike saw ~1,900–2,235), rejecting a truncated collection.
  Same-collection-cycle watermarking and timestamp-skew warnings require
  snapshot/collection-run metadata that the ETL does not yet produce; they are
  **explicitly deferred** and listed as a prerequisite-if-wanted in §8, not a v1
  gate. `behind`/`ahead` remain valid signals under point-in-time snapshots;
  they are labeled "investigate," never defect assertions.
- Deploy job `deploy/overlays/dev/jobs/derive-rhel-rebuilds.yaml`, scheduled
  after the RHEL/Alma/Rocky collection jobs, running the readiness check +
  derive + staged load.

## 7. Testing

- **Unit (pure, TDD):** `rpmvercmp` vs canonical RPM vectors (incl. `~`/`^` and
  epoch); `strip_vendor` (anchored grammars, incl. cases that must NOT strip);
  `module_base`; EVR epoch rule; the classifier decision table (one test per
  status using real spike examples — `openssl-3.5.5-6.el9_8` exact,
  `cloud-init …el9.rocky.0.1` vendor-patched, `apache-commons-cli …module_el9…`
  modular-equivalent, synthesized ahead/behind incl. an epoch-bearing case,
  an EVR-equal-but-NVR-differs case → `equivalent-version` (asserts
  `comparedAgainst`, **not** `rebuildOf`), `rocky-logos` exclusive);
  deterministic target selection under multiple candidates.
- **Deriver logic:** small two-graph N-triples fixtures → assert emitted
  status / `rebuildOf` vs `comparedAgainst` triples, and that distribution IRI
  resolution fails closed on zero/multiple `partOfDistribution`.
- **Gating & readiness:** actionable errors when terms are absent, or an input
  graph is empty/absent, below the plausibility floor, or resolves to zero/
  multiple distribution IRIs.
- **Integration (manual, documented):** re-run against the six spike graphs in
  local Fuseki; sanity-check counts against the spike report.

## 8. Sequencing / rollout

1. Ontology PR (v0.13.0) — `:rebuildOf`, `:comparedAgainst`,
   `:rebuildTrackingStatus`, `RebuildTrackingScheme` + SHACL + examples +
   changelog.
2. Platform: `rpmvercmp` + normalization + epoch rule (TDD) →
   `derive_comparison.rs` (gated, staged replace) → CLI → tests. Merges
   independently; deriver stays gated until (3).
3. Sync ontology into platform (`sync-ontology.sh`, bump pin), load TBox, enable
   the deploy job with readiness checks.
4. Update the spike report / `docs/QUERYING.md` with example rebuild-status
   queries; refresh the stale triple-count figures in `docs/rhel-collection.md`.
5. *(Optional, deferred — not a v1 gate.)* If collection-cycle currency checks
   are wanted, first implement snapshot publication (the v0.5.0 `DataSnapshot` /
   `isCurrent` / `snapshotGraph` work), then extend §6 readiness with
   currency/skew gating. v1 ships without it, using the existing-metadata
   readiness in §6.

## 9. Risks / open points

- **Epoch derivation** relies on the source↔binary `builtFromSource` join; a
  source with no surviving binary in the collected repos falls back to epoch 0
  (documented limitation, could misorder a since-removed epoch-bearing package).
- **rpmvercmp correctness** underpins newest-build selection and ahead/behind —
  mitigated by the canonical test vectors.
- **Point-in-time skew** still produces some ahead/behind; `behind` is a *signal
  to investigate*, never a defect assertion. v1 has no skew bound (currency/skew
  checks are deferred with snapshot publication, §8 step 5); consumers should
  read ahead/behind as snapshot-relative.
- **Modular normalization** truncates at `.module`; a legitimately differing
  pre-module release falls through to ahead/behind (safe).
- **CRB scope:** a few `rhel-exclusive` names may live in a rebuild's CRB (not
  collected). Noted in output docs; adding CRB is a follow-up.
