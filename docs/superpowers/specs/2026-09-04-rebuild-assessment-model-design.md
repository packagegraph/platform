# Rebuild Assessment Model — Design

**Date:** 2026-09-04
**Branch:** `alma-rocky-rhel`
**Status:** Design (rev 2, incorporating audit findings against the ontology's current — uncommitted — state) — awaiting review before implementation plan
**Supersedes:** [RHEL rebuild comparison deriver design](2026-09-01-rhel-rebuild-comparison-deriver-design.md) (rev 4) and its implementation on this branch, commits `72d0f3a..cd2cc5c` — the flat `pkg:rebuildTrackingStatus`/`pkg:RebuildTrackingScheme` vocabulary those commits emit against **was never released** and has been replaced.
**Ontology dependency:** [`packagegraph/ontology#5`](https://github.com/packagegraph/ontology/pull/5) (open, unmerged) — `DD-RB: Reified Rebuild Assessment with Presence / Fidelity / Drift Axes`, `docs/design-decisions.md`.

## 1. What changed and why this is a new design, not a patch

The prior implementation emitted one flat triple per rebuild source package: `SourcePackage --rebuildTrackingStatus--> concept`, plus `rebuildOf`/`comparedAgainst` asserted directly on the source package whenever a version match was found. Ontology PR #5 replaces this with a **reified `pkg:RebuildAssessment`** node per package, carrying three independent axes (presence, fidelity, drift) with distinct baselines, required provenance (timestamp, versioned method, confidence, source snapshot), and an evidence-gated promotion path for `rebuildOf` that this data source cannot satisfy. Every layer of the prior implementation — the classifier's return type, triple emission, all staging-validation SPARQL, the ontology gate, the CLI comment — assumed the flat shape and needs rebuilding against the reified one.

**Reused unchanged:** `rpmver.rs` (`rpmvercmp`/`evr_cmp`) and `rebuild_norm.rs` (`strip_vendor`/`module_base`). DD-RB's `rebuild-norm/v1` algorithm matches these almost exactly (epoch-aware `rpmvercmp`, missing epoch treated as 0, anchored vendor-suffix stripping, module-marker truncation). `sparql.rs`'s `query_source_builds`/`resolve_distribution`/epoch-aggregation are also unchanged — the per-graph `{name → Vec<Build>}` shape they produce is exactly what the new classifier needs too.

**Rewritten:** `rebuild_classify.rs` (new return shape: three independent outcomes, not one flat status+link) and all of `derive_comparison.rs` (assessment construction, staging validation, ontology gate). Small updates: CLI comment, deploy job comment.

### Goals
- Emit ontology-conformant `RebuildAssessment` nodes for every rebuild source-package name, matching PR #5's SHACL shapes exactly.
- Preserve the hard-won safety properties from the prior implementation: fail-closed readiness checks, staged atomic graph replacement, exact-subject-set validation before swap.
- Surface real ontology friction discovered while building a conformant producer, before PR #5 merges (§9).

### Non-goals
- **`rebuildOf` / lineage promotion.** Per the ontology's own rule, committed lineage requires independent provenance evidence (SRPM digest match, vendor build provenance, operator confirmation) that RPM repository metadata cannot provide. This deriver **never emits `rebuildOf`**; every assessment sets `lineageConfirmed=false` and omits `lineageEvidence`. Lineage promotion is a distinct, future, evidence-driven process (e.g. an operator workflow or a build-provenance enricher) — out of scope here. This is stated as a hard invariant, not a placeholder: staging validation enforces zero `rebuildOf` triples in this deriver's output (§6.1).
- **`module:stream`-scoped modular matching.** No collector in this pipeline captures module-stream metadata, so the modular-equivalent tier can only do base-NVR matching. The ontology's own algorithm rule (DD-RB) requires a producer in this position to publish under a **distinct method id** with a correspondingly lower confidence for that tier — not to claim the full method while silently weakening it (§3, §4).
- Per-historical-build assessment (still a latest-per-name snapshot, as before).
- Binary/arch-level comparison, CRB/extras repos, non-x86_64.
- Running the ontology's actual SHACL shapes against staging (§6.1) — out of scope for this deriver; Rust-side checks are a targeted, enumerated set of fail-closed invariants, not a SHACL engine.

## 2. Ontology terms consumed (PR #5)

All in `core.ttl`/`skos-schemes.ttl`, namespace `https://purl.org/packagegraph/ontology/core#`.

| Term | Kind | Domain → Range | Cardinality |
|---|---|---|---|
| `RebuildAssessment` | Class | — | — |
| `assessmentOf` | ObjectProperty | `RebuildAssessment` → `SourcePackage` | exactly 1 |
| `assessedAt` | DatatypeProperty | `RebuildAssessment` → `xsd:dateTime` | exactly 1 |
| `assessmentMethod` | DatatypeProperty | `RebuildAssessment` → `xsd:string` | exactly 1 |
| `assessedAgainstSnapshot` | ObjectProperty | `RebuildAssessment` → `DataSnapshot` | exactly 1 |
| `hasUpstreamCounterpart` | DatatypeProperty | `RebuildAssessment` → `xsd:boolean` | exactly 1 |
| `rebuildFidelity` | ObjectProperty | `RebuildAssessment` → `skos:Concept` (`RebuildFidelityScheme`) | ≤1, required iff counterpart=true |
| `fidelityBaseline` | ObjectProperty | `RebuildAssessment` → `SourcePackage` | ≤1, required iff fidelity matched |
| `rebuildDrift` | ObjectProperty | `RebuildAssessment` → `skos:Concept` (`RebuildDriftScheme`) | ≤1, required iff counterpart=true |
| `comparedAgainst` | ObjectProperty | `RebuildAssessment` → `SourcePackage` | ≤1, required iff drift present |
| `ambiguousCandidate` | ObjectProperty | `RebuildAssessment` → `SourcePackage` | ≥2, only iff fidelity=unknown-by-ambiguity |
| `assessmentConfidence` | DatatypeProperty | `RebuildAssessment` → `xsd:decimal` | ≤1, in [0.0, 1.0] |
| `lineageConfirmed` | DatatypeProperty | `RebuildAssessment` → `xsd:boolean` | exactly 1 |
| `lineageEvidence` | DatatypeProperty | `RebuildAssessment` → `xsd:string` | present iff `lineageConfirmed=true` |
| `rebuildOf` (unchanged decl, new semantics) | ObjectProperty (Asymmetric, Irreflexive) | `SourcePackage` → `SourcePackage` | never emitted by this deriver |
| `DataSnapshot` | Class (pre-existing) | requires `rdfs:label` | minted by this deriver |
| `derivedFromDistribution` (pre-existing, untouched) | ObjectProperty | `Distribution` → `Distribution` | unchanged from prior design |

`RebuildFidelityScheme`: `fidelity-exact`, `fidelity-vendor-patched`, `fidelity-modular-equivalent`, `fidelity-unknown`.
`RebuildDriftScheme`: `drift-even`, `drift-ahead`, `drift-behind`, `drift-version-equivalent`.

Key SHACL constraints this design must satisfy (`RebuildAssessmentShape`, `RebuildAssessmentResultsShape`, `RebuildAssessmentAmbiguityShape`, `RebuildAssessmentSelfBaselineShape`): counterpart=false forbids fidelity/drift/baselines/ambiguous-candidates; counterpart=true requires exactly one fidelity and one drift; a matched fidelity tier requires `fidelityBaseline`; `fidelity-unknown` forbids `fidelityBaseline`; `rebuildDrift` present requires `comparedAgainst`; `ambiguousCandidate` requires `fidelity-unknown` and ≥2 entries; an assessed package must never be its own baseline.

## 3. Algorithm (`rebuild-norm/v1-nostream`, RPM-family instantiation)

**Method identity.** The current DD-RB text is explicit: a producer that cannot verify module-stream membership "must not silently weaken `rebuild-norm/v1` under its own name" and "must publish under a distinctly versioned method id (e.g. `rebuild-norm/v1-nostream`)." This deriver has no module-stream metadata available (§1 non-goals), so it emits `pkg:assessmentMethod "rebuild-norm/v1-nostream"` — **not** `rebuild-norm/v1` — everywhere. Every other rule below (candidate scope, version comparison, exact/vendor tiers, drift, ambiguity, lineage) is identical between the two method ids; only the modular-equivalent tier's implementation and confidence differ (§4).

Per rebuild source-package **name** (latest-per-name snapshot, as before), using upstream and rebuild `Build{node_uri, epoch, version, release}` lists already produced by `sparql.rs`:

1. **Presence.** Upstream candidates for this name non-empty? If empty → `hasUpstreamCounterpart=false`; emit no fidelity/drift/baselines; `lineageConfirmed=false`. Done.
2. **Fidelity ladder** (only when counterpart exists), evaluated in order, **first tier with ≥1 candidate decides the outcome — do not fall through past a tier that produced any candidates, ambiguous or not**. Each tier's equality test is **canonical EVR equality after epoch normalization** — epoch equal (missing epoch = 0) **and** `rpmvercmp` on the (possibly tier-normalized) version-release string returns `Equal` — **not raw string equality**. This is a correctness fix from rev 1: `rpmvercmp` tolerates RPM-ignored separator/leading-zero differences that raw string comparison would incorrectly treat as a non-match, and the ontology's exact-match rule is defined in exactly those terms (DD-RB: "canonical EVR equality — string equality of `E:V-R` **after** epoch normalization", read together with `rpmvercmp` already being the project's chosen ordered-comparison primitive). Concretely, replace every `nvr() == candidate` check below with `rb.epoch == candidate.epoch && rpmvercmp(&normalized_nvr, &candidate.nvr()) == Ordering::Equal`:
   - **Exact**: candidates = upstream builds canonically EVR-equal to `rb.nvr()` (no normalization applied).
   - **Vendor-patched** (only if `strip_vendor(rb.nvr()) != rb.nvr()`): candidates = upstream builds canonically EVR-equal to `strip_vendor(rb.nvr())` (upstream side unmodified — matches the prior design's normalize-downstream-only rule).
   - **Modular-equivalent** (only if `module_base(rb.nvr()) != rb.nvr()`): candidates = upstream builds canonically EVR-equal to `module_base(rb.nvr())` **when also reduced through `module_base`** (i.e. compare `module_base(rb.nvr())` against `module_base(candidate.nvr())`, not the candidate's raw NVR) — this is the base-NVR-only approximation; it does **not** verify same-module-stream, which is exactly why this deriver is `-nostream` (§1, §4).
   - At the deciding tier: **exactly 1 candidate** → that fidelity concept, `fidelityBaseline` = that build, confidence per §4. **≥2 candidates** → **ambiguous**: `fidelity-unknown`, `ambiguousCandidate` = all tied builds, no `fidelityBaseline`, confidence 0.5. This is a deliberate behavior change from the prior (unreleased) classifier, which silently tie-broke by URI — DD-RB requires ties to be surfaced, not resolved.
   - **All three tiers empty**: `fidelity-unknown`, no baseline, no ambiguous candidates, confidence per §4.
3. **Drift** (always computed alongside fidelity, whenever counterpart exists; independent baseline — upstream's newest build for the name, not the fidelity baseline; **unaffected by the exact-match fix above** — drift's even/version-equivalent split is deliberately based on raw-string vs. `rpmvercmp` comparison, which is a different, already-correct distinction): pick upstream newest by EVR (existing `pick_newest`: EVR-descending, URI tiebreak). Then:
   - `(epoch, version, release)` tuple identical to newest → `drift-even`.
   - Else `evr_cmp(...) == Equal` (rpmvercmp-equal, strings differ — e.g. leading-zero difference) → `drift-version-equivalent`. (Disjoint from `drift-even` by construction, per the SKOS definitions.)
   - Else `evr_cmp(...) == Greater` → `drift-ahead`.
   - Else → `drift-behind`.
   `comparedAgainst` = upstream newest, always set alongside drift.
4. **Lineage**: `lineageConfirmed=false` always; `lineageEvidence` never set; `rebuildOf` never emitted (§1 non-goals).

## 4. Confidence values

The ontology now **pins** these numerically for `rebuild-norm/v1` (`core.ttl`'s `assessmentConfidence` comment; DD-RB's confidence table) — this is no longer implementation-defined, and rev 1 of this design had it wrong (it used 1.0 for the unmatched case, not 0.7). `rebuild-norm/v1-nostream` reuses the `v1` table unchanged **except** `fidelity-modular-equivalent`, which DD-RB requires to be *lower* than `v1`'s 0.85 to reflect the dropped stream check; this design uses the value DD-RB's own example gives (0.6):

| Outcome | `v1` (for reference) | **`v1-nostream` (this deriver)** |
|---|---|---|
| `fidelity-exact` | 1.0 | **1.0** |
| `fidelity-vendor-patched` | 0.9 | **0.9** |
| `fidelity-modular-equivalent` | 0.85 | **0.6** ← lowered: stream membership unverified |
| `fidelity-unknown`, no candidates at any tier | 0.7 | **0.7** |
| `fidelity-unknown`, ambiguous tie | 0.5 | **0.5** |

A method that claims determinism (as `rebuild-norm/v1`/`v1-nostream` both do) must reproduce this table exactly for the same inputs — these are fixed constants in code, not tunable.

## 5. Construction

### 5.1 `RebuildAssessment` IRI

**Rev 1 mistake, corrected:** a stable per-package IRI (`{node_uri}/assessment`, reused across runs) makes the assessment's identity outlive the specific upstream state it observed — a later run overwrites the *meaning* of the same IRI, which the ontology's framing directly warns against for `assessedAgainstSnapshot` (§5.2) and, by the same reasoning, applies to `RebuildAssessment` itself: it is explicitly documented as a "reified, **timestamped** observation," not a mutable latest-state resource. Because the whole derived graph is atomically replaced every run (§7), making the IRI run-scoped costs nothing functionally — the previous run's differently-IRI'd assessment nodes are simply dropped by the swap, not accumulated.

**Fixed scheme:** `format!("{}/assessment/{}", rb.node_uri, encode(run_token))` (e.g. `.../src/almalinux/9/openssl/3.5.5-6.el9_8/assessment/2026-09-04t1200z-hostname`). Still a suffix on the already-known `node_uri` from the `Build` struct (not a fresh `source_uri()` call — the pure `assess()`/`build_report` layer never has decomposed distro/release/name/version). `run_token` is already threaded into `derive()`/`load_atomic` and already validated against injection (`valid_run_token`), so no new input surface. The intended query pattern is `SELECT ?a WHERE { ?a pkg:assessmentOf <source> }` for "the current assessment," not a bookmarked per-package URL — consistent with `assessmentOf` being the ontology's canonical, asserted query direction (§2).

Only the canonical `assessmentOf` direction is asserted; `hasRebuildAssessment` (the optional inverse) is **not emitted** — DD-RB is explicit that validation must not depend on inverse inference, and emitting an unused inverse is dead weight.

### 5.2 `DataSnapshot`

**Rev 1 mistake, corrected:** keying the snapshot IRI on `upstream_graph_iri` alone means every run against that graph — including a later run after upstream has genuinely changed (new z-stream builds, a package removed) — cites the *same* `DataSnapshot` IRI. That directly contradicts `assessedAgainstSnapshot`'s own definition, which exists precisely so "results (including closed-world absence) are reproducible" against a specific, identifiable state. A `run_token` appearing only in the *label text* doesn't fix this: the IRI, not the label, is what other triples reference and what reproducibility depends on.

**Fixed scheme:** one per **(upstream graph, run)** — `{DATA}snapshot/rebuild-norm/v1-nostream/{encode(upstream_graph_iri)}/{encode(run_token)}`, still reused across every rebuild pair *within the same run* that cites the same upstream graph (e.g. `almalinux/9` and `rocky/9` both citing `rhel/9` in one `derive()` invocation still share one snapshot node), but no longer reused *across* runs. `rdfs:label` (its only required field) can now safely include the graph and run token for human legibility: `"RHEL 9 snapshot for rebuild-norm/v1-nostream, run <run_token>"`.

Because the IRI is a deterministic function of `(upstream_graph_iri, run_token)`, every pair sharing both computes the identical string within one run — no Rust-level cache needed. The definition triples (`a pkg:DataSnapshot`, `rdfs:label`) are emitted via the existing `NTriplesWriter::write_triple_once`/`write_literal_once`, so repeated references within a run emit the definition exactly once regardless of call order.

### 5.3 Timestamp

`assessedAt` is captured **once per `derive()` invocation** (not per assessment, not inside the pure core) and threaded in as a parameter — consistent with `run_token` already being supplied by the caller rather than generated inside pure code. This keeps `build_report` a pure, deterministic function for testing (fixed injected timestamp in fixtures).

### 5.4 Emitted triples, per case

**Case A — no upstream counterpart:**
```turtle
<rebuild-source/assessment/run> a pkg:RebuildAssessment ;
    pkg:assessmentOf <rebuild-source> ;
    pkg:assessedAt "<ts>"^^xsd:dateTime ;
    pkg:assessmentMethod "rebuild-norm/v1-nostream" ;
    pkg:assessedAgainstSnapshot <snapshot/upstream-graph/run> ;
    pkg:hasUpstreamCounterpart false ;
    pkg:lineageConfirmed false .
```

**Case B — matched fidelity (exact/vendor-patched/modular-equivalent):**
```turtle
<rebuild-source/assessment/run> a pkg:RebuildAssessment ;
    pkg:assessmentOf <rebuild-source> ;
    pkg:assessedAt "<ts>"^^xsd:dateTime ;
    pkg:assessmentMethod "rebuild-norm/v1-nostream" ;
    pkg:assessedAgainstSnapshot <snapshot/upstream-graph/run> ;
    pkg:hasUpstreamCounterpart true ;
    pkg:rebuildFidelity pkg:fidelity-{exact|vendor-patched|modular-equivalent} ;
    pkg:fidelityBaseline <upstream-build> ;
    pkg:rebuildDrift pkg:drift-{even|ahead|behind|version-equivalent} ;
    pkg:comparedAgainst <upstream-newest> ;
    pkg:assessmentConfidence "1.0|0.9|0.6"^^xsd:decimal ;
    pkg:lineageConfirmed false .
```
(Confidence per §4: 1.0 exact, 0.9 vendor-patched, **0.6** modular-equivalent — not 0.85, per the `-nostream` table.)

**Case C — fidelity unknown, no ambiguity:** as Case B but `pkg:rebuildFidelity pkg:fidelity-unknown`, no `fidelityBaseline`, `pkg:assessmentConfidence "0.7"^^xsd:decimal`; drift/comparedAgainst still present.

**Case D — ambiguous:** as Case C plus `pkg:ambiguousCandidate <build-1>, <build-2>, ...` (≥2), `pkg:assessmentConfidence "0.5"^^xsd:decimal`.

`derivedFromDistribution` is emitted exactly as in the prior design — once per distinct resolved distribution pair, unaffected by this change.

## 6. Code structure

- **`rpmver.rs`, `rebuild_norm.rs`** — unchanged.
- **`rebuild_classify.rs`** — rewritten. Old `Classification`/`Link`/`classify()` deleted (no dead code left behind). New:
  ```rust
  pub struct FidelityOutcome { pub concept: &'static str, pub baseline: Option<String>, pub ambiguous_candidates: Vec<String>, pub confidence: f64 }
  pub struct DriftOutcome { pub concept: &'static str, pub compared_against: String }
  pub struct Assessment { pub has_upstream_counterpart: bool, pub fidelity: Option<FidelityOutcome>, pub drift: Option<DriftOutcome> }
  pub fn assess(rebuild_newest: &Build, upstream: &[Build]) -> Assessment
  ```
  (`Build` struct unchanged.)
- **`derive_comparison.rs`** — rewritten:
  - `RebuildReport` gains: `assessments: usize`, `presence_true`/`presence_false: usize`, `fidelity_counts: BTreeMap<String,usize>`, `drift_counts: BTreeMap<String,usize>`, `ambiguous_count: usize`, `assessment_subjects: BTreeSet<String>` (the `assessmentOf` targets — the expected-subject-set for staging validation), plus unchanged `pairs`, `distribution_count`, `triples`. **Old `statuses`/`status_subjects` fields removed.**

    Invariants a test can assert directly: `assessments == presence_true + presence_false == assessment_subjects.len()`; `fidelity_counts.values().sum() == presence_true` (fidelity is emitted iff counterpart is true); `drift_counts.values().sum() == presence_true` (drift always accompanies fidelity); `ambiguous_count <= fidelity_counts["fidelity-unknown"]` (ambiguity is one of two ways to reach `fidelity-unknown`, the other being zero candidates at every tier).
  - `build_report` (pure core) takes injected `assessed_at: &str` and `run_token: &str` (both threaded straight through from `derive()`'s caller-supplied inputs, matching how `run_token` already flows into `load_atomic`); `run_token` feeds **both** the per-upstream-graph `DataSnapshot` IRI (§5.2) and the per-source assessment IRI (§5.1). Assembles per-pair `PairData` as before (unchanged I/O boundary in `derive()`); for each pair, mints/reuses the upstream `DataSnapshot`, calls `assess()` per rebuild name, emits per §5.4, tallies the report.
  - `derive()` additionally rejects a pair whose `rebuild_graph == rhel_graph` (defensive: guarantees `assessmentOf`'s target and any baseline can never coincide, matching `RebuildAssessmentSelfBaselineShape`'s intent) alongside the existing duplicate-rebuild-graph check.
  - `check_ontology_terms` — new term list: `RebuildAssessment`, `assessmentOf`, `hasUpstreamCounterpart`, `rebuildFidelity`, `rebuildDrift`, `fidelityBaseline`, `comparedAgainst`, `assessedAgainstSnapshot`, `lineageConfirmed`, `RebuildFidelityScheme`, `RebuildDriftScheme`. **Old `REBUILD_TERMS`/`TRACK_CONCEPTS` deleted entirely** — gating on now-nonexistent terms would always fail closed, which is safe but gives a useless error message.
  - `validate_staging` — new checks (§6.1), old flat-shape checks deleted.
  - `load_atomic`, `atomic_swap_update`, `valid_run_token` — **unchanged** (graph-replacement mechanics are orthogonal to triple shape).
- **`sparql.rs`** — unchanged.
- **`main.rs`** — CLI flags unchanged (`--endpoint`, `--output`, `--pair`, `--min-sources`, `--run-token`, `--load`); update the doc comment describing what's derived; pass the new `report` fields into `load_atomic`'s expected-count arguments.
- **`deploy/overlays/dev/jobs/derive-rhel-rebuilds.yaml`** — stays `suspend: true`; update the prerequisite comment to reference ontology PR #5 / v0.13.0-reified specifically (the flat v0.13.0 draft it previously referenced is superseded).

### 6.1 Staging validation (replaces the prior 10 checks)

**Scope, stated honestly (correcting rev 1's overclaim):** these are a targeted, enumerated set of fail-closed Rust/SPARQL checks against the **staging** graph, run before the atomic swap — they are **not** a substitute for running PR #5's actual SHACL shapes, and this design does not claim full equivalence with them. Running the real SHACL validator against derived output remains a separate, out-of-scope concern (a manual or CI-side step, not part of this deriver). What follows is deliberately closer to that coverage than rev 1's list, closing the gaps an audit found: concept-scheme allowlists, datatype correctness (not just presence), full max-cardinality (not just the cases rev 1 happened to need), `DataSnapshot` shape, and unexpected subjects/objects beyond just the assessment-subject set. Any violation drops staging and errors (prod untouched).

1. **Assessment count**: `COUNT(DISTINCT ?a WHERE { ?a a pkg:RebuildAssessment })` == `report.assessments`.
2. **Assessment-subject set equality**: `DISTINCT ?s WHERE { ?a pkg:assessmentOf ?s }` == `report.assessment_subjects` (exact-set guard, same technique as the prior design, reused against the new anchor point).
3. **Required-field cardinality** (exactly 1 each): zero assessments missing, or carrying >1 of, `assessmentOf`/`assessedAt`/`assessmentMethod`/`assessedAgainstSnapshot`/`hasUpstreamCounterpart`/`lineageConfirmed`.
4. **Optional-field max-cardinality** (≤1 each): zero assessments with >1 distinct value for `rebuildFidelity`/`rebuildDrift`/`fidelityBaseline`/`comparedAgainst`/`assessmentConfidence`/`lineageEvidence`.
5. **Datatype correctness**, not just presence: zero `assessedAt` values that aren't `xsd:dateTime`-typed literals; zero `assessmentMethod`/`lineageEvidence` that aren't `xsd:string`; zero `hasUpstreamCounterpart`/`lineageConfirmed` that aren't `xsd:boolean`; zero `assessmentConfidence` that isn't `xsd:decimal`.
6. **Confidence range**: zero `assessmentConfidence` values outside `[0.0, 1.0]`.
7. **Concept-scheme allowlist**: zero `rebuildFidelity` objects outside the four `pkg:fidelity-*` concepts; zero `rebuildDrift` objects outside the four `pkg:drift-*` concepts (the SPARQL equivalent of SHACL `sh:in`).
8. `hasUpstreamCounterpart=false` ⇒ zero assessments also carrying `rebuildFidelity`/`rebuildDrift`/`fidelityBaseline`/`comparedAgainst`/`ambiguousCandidate`.
9. `hasUpstreamCounterpart=true` ⇒ zero assessments missing `rebuildFidelity` or missing `rebuildDrift`.
10. `rebuildDrift` present ⇒ zero missing `comparedAgainst`.
11. Matched fidelity (`exact`/`vendor-patched`/`modular-equivalent`) ⇒ zero missing `fidelityBaseline`.
12. `fidelity-unknown` ⇒ zero carrying `fidelityBaseline`.
13. `ambiguousCandidate` present ⇒ zero not-`fidelity-unknown`, and zero with fewer than 2 distinct candidates.
14. **Self-baseline**: zero assessments where `assessmentOf` target equals `fidelityBaseline` or `comparedAgainst`.
15. **Unexpected baseline/candidate subjects**: every `fidelityBaseline`/`comparedAgainst`/`ambiguousCandidate` object must be a known upstream `SourcePackage` URI actually present in one of the configured upstream graphs for this run — not an arbitrary or stale IRI. (Extends rev 1's "unexpected subjects" check, which only covered assessment subjects, to baseline/candidate *objects* too.)
16. **`DataSnapshot` shape**: every `assessedAgainstSnapshot` target has `a pkg:DataSnapshot` and a non-empty `rdfs:label`, and its IRI is one of the `(upstream_graph, run_token)` snapshot IRIs this run actually minted (§5.2) — not a stray reference.
17. **Zero `rebuildOf` triples anywhere in staging** — hard invariant enforcing the non-goal in §1.
18. Zero `lineageConfirmed=true` and zero `lineageEvidence` triples — this deriver never promotes lineage; a `true` here would indicate a code defect, not real data.
19. **Distribution coverage**: `COUNT(derivedFromDistribution)` == `report.distribution_count` (unchanged from prior design).

## 7. Unchanged from the prior design

Staged atomic replacement (`load_atomic`, `atomic_swap_update`, drop-before-load, drop-on-every-failure-path, run-token validation), fail-closed readiness checks (`check_readiness`, distinct-source-count floor via `aggregate_source_rows`), duplicate-rebuild-graph rejection, the pure/impure split (`derive()` does I/O, `build_report` is pure and unit-testable), and the CronJob staying `suspend: true` until prerequisites land.

## 8. Testing

- **Unit (pure):** `assess()` decision table — one test per case (A/B/C/D) plus: exact-tier ambiguity, vendor-tier ambiguity, modular-tier ambiguity (ties stop at the deciding tier, don't fall through), `drift-even` vs `drift-version-equivalent` disjointness (identical tuple vs. rpmvercmp-equal-but-different-string), confidence values per outcome, self-baseline is structurally impossible when rebuild/upstream graphs differ (documented, not testable in isolation — covered by the `rebuild_graph != rhel_graph` guard in `derive()`).
- **`build_report` integration (pure):** two-pair fixture (mirroring the prior design's coverage) exercising: `DataSnapshot` reuse across pairs sharing an upstream graph, assessment IRI construction, all four confidence values, an ambiguous case, a presence=false case, distribution dedup, and report tallies — asserting the exact emitted triples per §5.4's shapes.
- **Staging validation:** unit-test each new query builder's string shape (as before); no live-Fuseki test (consistent with existing project convention).
- **Manual integration:** re-run against the six existing collected graphs in the local Fuseki once ontology v0.13.0-reified is loaded; spot-check a known case (e.g. AlmaLinux 9 `openssl`) against the shapes in §5.4.

## 9. Ontology feedback (report on PR #5 before merge)

**Rev 1's items 1 and 2 are resolved** — the ontology (uncommitted local edits on top of the PR-#5 commit, as of this rev) now pins the full confidence table directly in `core.ttl`'s `assessmentConfidence` comment and DD-RB, and adds an explicit rule requiring a distinct method id (`rebuild-norm/v1-nostream`) with its own lower confidence when module-stream data is unavailable. This design complies with both (§3, §4) rather than flagging them as open questions. Recorded here for the historical record of what real-world implementation surfaced, not as pending asks.

Remaining/new items to raise before merge:

1. **`assessedAgainstSnapshot`'s reproducibility requirement has a subtle failure mode worth calling out explicitly in DD-RB**: a snapshot IRI keyed only on the upstream graph's *identity* (not a run/content marker) silently breaks reproducibility the moment that graph's contents change between runs, since the IRI doesn't change but what it denotes does. This design works around it by keying the snapshot (and the assessment) IRI on `(upstream_graph, run)` (§5.1, §5.2) — worth considering whether DD-RB should say this explicitly, since it's an easy trap for any other producer implementing this vocabulary.
2. *(Placeholder for anything else surfaced while implementing/testing — to be appended before the ontology PR is asked to merge.)*

## 10. Sequencing / rollout

1. This spec → implementation plan → subagent-driven execution on `alma-rocky-rhel` (same branch — the flat-model commits it replaces were never released to any consumer).
2. Ship platform code now, gated exactly as before: `check_ontology_terms` fails closed until ontology v0.13.0-reified is loaded, so merging platform code has no live effect until the ontology PR merges and syncs.
3. Before ontology PR #5 merges: report §9's findings (plus anything else surfaced during implementation) for the ontology author's consideration.
4. Once merged: sync ontology (`sync-ontology.sh` pin bump, already scaffolded from the prior design), load TBox, unsuspend the CronJob once RHEL/Alma/Rocky collect jobs also exist (tracked separately, unchanged from the prior design's residual follow-up).

## 11. Risks / open points

- **Ambiguity surfacing changes downstream query patterns**: consumers can no longer assume every matched fidelity has exactly one baseline without checking `ambiguousCandidate` — this is intentional (DD-RB's explicit goal) but is a real behavior change from the (unreleased) flat model's silent tie-break.
- **`DataSnapshot` minting is deriver-local**, not part of the broader deferred snapshot-publication initiative (v0.5.0 buildout, still unimplemented) — scoped narrowly to this deriver's own need, per §5.2's "cheap either way" rationale.
- **Self-baseline guard is structural, not just defensive**: rejecting `rebuild_graph == rhel_graph` pairs at `derive()` entry is the only place this is enforced in code; staging check 10 is a second, independent gate in case the structural guard is ever bypassed or the algorithm changes.
