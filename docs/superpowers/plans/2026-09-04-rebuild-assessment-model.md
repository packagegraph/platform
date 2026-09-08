# Rebuild Assessment Model Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the deriver's flat (never-released) `pkg:rebuildTrackingStatus` output with reified `pkg:RebuildAssessment` nodes matching ontology PR #5's presence/fidelity/drift model, implementing `rebuild-norm/v1-nostream`.

**Architecture:** `rpmver.rs`/`rebuild_norm.rs`/`sparql.rs` are untouched. `rebuild_classify.rs`'s `classify()` is replaced by `assess()`, returning three independent outcomes instead of one flat status. `derive_comparison.rs`'s emission, ontology gate, and staging validation are rewritten around the new shape; `load_atomic`'s growing parameter list is bundled into a `LoadExpectations` struct. CLI and deploy-job comment get small updates.

**Tech Stack:** Rust (`etl/pg-collect`), Apache Jena Fuseki (SPARQL 1.1), chrono for timestamps.

**Spec:** `docs/superpowers/specs/2026-09-04-rebuild-assessment-model-design.md` (rev 2)

## Global Constraints

- **Method id:** every assessment's `pkg:assessmentMethod` is the literal string `"rebuild-norm/v1-nostream"` — never `"rebuild-norm/v1"` (that name is reserved for a producer that verifies module-stream membership, which this pipeline cannot do).
- **Confidence values (fixed, exact decimal literals — never reformat as float)**: `fidelity-exact`=`"1.0"`, `fidelity-vendor-patched`=`"0.9"`, `fidelity-modular-equivalent`=`"0.6"`, `fidelity-unknown` (no candidates)=`"0.7"`, `fidelity-unknown` (ambiguous)=`"0.5"`.
- **`rebuildOf` is never emitted.** `lineageConfirmed` is always `false`; `lineageEvidence` is never written. Staging validation must fail closed if either invariant is violated.
- **Fidelity-tier matching uses canonical EVR equality** (epoch equal AND `rpmvercmp(...) == Ordering::Equal` on the tier-normalized version-release string) — **not** raw string equality.
- **IRIs are run-scoped**: both the `DataSnapshot` IRI (keyed on `(upstream_graph, run_token)`) and the `RebuildAssessment` IRI (keyed on `(rb.node_uri, run_token)`) must include `run_token` — a stable IRI reused across runs breaks `assessedAgainstSnapshot`'s reproducibility guarantee.
- **No AI attribution in commit messages.**
- **TDD:** every code change starts with a failing test. Run `cargo test` from `etl/pg-collect`; a task's own tests must pass and the full suite must show zero new failures/warnings before its commit.

---

### Task 1: Rewrite `rebuild_classify.rs` — `assess()` replaces `classify()`

**Files:**
- Modify: `etl/pg-collect/src/rebuild_classify.rs` (full rewrite — delete `Classification`/`Link`/`classify`, keep `Build`/`pick_newest`)

**Interfaces:**
- Consumes: `crate::rebuild_norm::{strip_vendor, module_base}`, `crate::rpmver::{rpmvercmp, evr_cmp}` (all unchanged, already `pub`).
- Produces (consumed by Task 2):
  ```rust
  pub struct Build { pub node_uri: String, pub epoch: i64, pub version: String, pub release: String } // unchanged
  pub const CONFIDENCE_EXACT: &str;               // "1.0"
  pub const CONFIDENCE_VENDOR_PATCHED: &str;      // "0.9"
  pub const CONFIDENCE_MODULAR_EQUIVALENT: &str;  // "0.6"
  pub const CONFIDENCE_UNKNOWN_NO_MATCH: &str;    // "0.7"
  pub const CONFIDENCE_AMBIGUOUS: &str;           // "0.5"
  pub struct FidelityOutcome { pub concept: &'static str, pub baseline: Option<String>, pub ambiguous_candidates: Vec<String>, pub confidence: &'static str }
  pub struct DriftOutcome { pub concept: &'static str, pub compared_against: String }
  pub struct Assessment { pub has_upstream_counterpart: bool, pub fidelity: Option<FidelityOutcome>, pub drift: Option<DriftOutcome> }
  pub fn assess(rb: &Build, upstream: &[Build]) -> Assessment
  ```

- [ ] **Step 1: Write the failing tests**

Replace the entire `#[cfg(test)] mod tests` block (the old tests reference deleted `classify`/`Classification`/`Link` and will not compile once Step 3 lands — that is expected and is how you'll confirm RED):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn b(uri: &str, e: i64, v: &str, r: &str) -> Build {
        Build { node_uri: uri.into(), epoch: e, version: v.into(), release: r.into() }
    }

    #[test]
    fn no_upstream_candidates_is_no_counterpart() {
        let rb = b("rk:rocky-logos", 0, "90", "1.el9");
        let a = assess(&rb, &[]);
        assert!(!a.has_upstream_counterpart);
        assert!(a.fidelity.is_none());
        assert!(a.drift.is_none());
    }

    #[test]
    fn exact_match_unambiguous() {
        let rb = b("alma:openssl", 0, "3.5.5", "6.el9_8");
        let up = vec![b("rhel:openssl", 0, "3.5.5", "6.el9_8")];
        let a = assess(&rb, &up);
        assert!(a.has_upstream_counterpart);
        let f = a.fidelity.unwrap();
        assert_eq!(f.concept, "fidelity-exact");
        assert_eq!(f.baseline, Some("rhel:openssl".to_string()));
        assert!(f.ambiguous_candidates.is_empty());
        assert_eq!(f.confidence, CONFIDENCE_EXACT);
        let d = a.drift.unwrap();
        assert_eq!(d.concept, "drift-even");
        assert_eq!(d.compared_against, "rhel:openssl");
    }

    #[test]
    fn exact_tier_tolerates_rpmvercmp_quirks_not_just_raw_string_equality() {
        // "1.05" and "1.5" are rpmvercmp-equal (leading-zero difference) but not
        // string-equal. Canonical EVR equality means this is fidelity-exact, not
        // fidelity-unknown -- this is the fix for the raw-string-equality bug.
        let rb = b("alma:foo", 0, "1.05", "1.el9");
        let up = vec![b("rhel:foo", 0, "1.5", "1.el9")];
        let a = assess(&rb, &up);
        let f = a.fidelity.unwrap();
        assert_eq!(f.concept, "fidelity-exact");
        assert_eq!(f.baseline, Some("rhel:foo".to_string()));
    }

    #[test]
    fn vendor_suffix_is_vendor_patched_unambiguous() {
        let rb = b("rocky:cloud-init", 0, "24.4", "8.el9.rocky.0.1");
        let up = vec![b("rhel:cloud-init", 0, "24.4", "8.el9")];
        let f = assess(&rb, &up).fidelity.unwrap();
        assert_eq!(f.concept, "fidelity-vendor-patched");
        assert_eq!(f.baseline, Some("rhel:cloud-init".to_string()));
        assert_eq!(f.confidence, CONFIDENCE_VENDOR_PATCHED);
    }

    #[test]
    fn modular_build_id_diff_is_modular_equivalent_with_lowered_confidence() {
        let rb = b("alma:acl", 0, "1.9.0", "4.module_el9.6.0+148+fb6dc857");
        let up = vec![b("rhel:acl", 0, "1.9.0", "4.module+el9.8.0+24092+eb9f67d0")];
        let f = assess(&rb, &up).fidelity.unwrap();
        assert_eq!(f.concept, "fidelity-modular-equivalent");
        // 0.6, NOT v1's 0.85 -- this producer cannot verify module:stream.
        assert_eq!(f.confidence, CONFIDENCE_MODULAR_EQUIVALENT);
    }

    #[test]
    fn no_tier_matches_is_unknown_with_no_match_confidence() {
        let rb = b("rk:foo", 0, "9.9", "1.el9");
        let up = vec![b("rhel:foo", 0, "1.2", "1.el9")];
        let f = assess(&rb, &up).fidelity.unwrap();
        assert_eq!(f.concept, "fidelity-unknown");
        assert!(f.baseline.is_none());
        assert!(f.ambiguous_candidates.is_empty());
        assert_eq!(f.confidence, CONFIDENCE_UNKNOWN_NO_MATCH);
    }

    #[test]
    fn tied_exact_matches_are_ambiguous_not_tie_broken() {
        let rb = b("alma:x", 0, "1.0", "1.el9");
        let up = vec![
            b("rhel:x@b", 0, "1.0", "1.el9"),
            b("rhel:x@a", 0, "1.0", "1.el9"),
        ];
        let f = assess(&rb, &up).fidelity.unwrap();
        assert_eq!(f.concept, "fidelity-unknown");
        assert!(f.baseline.is_none());
        assert_eq!(f.confidence, CONFIDENCE_AMBIGUOUS);
        let mut cands = f.ambiguous_candidates.clone();
        cands.sort();
        assert_eq!(cands, vec!["rhel:x@a".to_string(), "rhel:x@b".to_string()]);
    }

    #[test]
    fn ambiguous_tier_does_not_fall_through_to_a_later_tier() {
        // Two exact-tier ties AND a vendor-tier candidate that would otherwise
        // match cleanly. The exact tier produced >=1 candidate, so it decides --
        // ambiguous, never falling through to vendor-patched.
        let rb = b("alma:x", 0, "1.0", "1.el9.alma.1");
        let up = vec![
            b("rhel:x@a", 0, "1.0", "1.el9.alma.1"),
            b("rhel:x@b", 0, "1.0", "1.el9.alma.1"),
            b("rhel:x@c", 0, "1.0", "1.el9"), // would match vendor-patched tier alone
        ];
        let f = assess(&rb, &up).fidelity.unwrap();
        assert_eq!(f.concept, "fidelity-unknown");
        assert_eq!(f.ambiguous_candidates.len(), 2);
    }

    #[test]
    fn drift_ahead_and_behind() {
        let up = vec![b("rhel:foo", 0, "1.2", "1.el9")];
        let ahead = assess(&b("rk:foo", 0, "1.3", "1.el9"), &up).drift.unwrap();
        assert_eq!(ahead.concept, "drift-ahead");
        let behind = assess(&b("rk:foo", 0, "1.0", "1.el9"), &up).drift.unwrap();
        assert_eq!(behind.concept, "drift-behind");
    }

    #[test]
    fn drift_even_and_version_equivalent_are_disjoint() {
        // Identical tuple -> even. rpmvercmp-equal but string-differs -> version-equivalent.
        let up = vec![b("rhel:foo", 0, "1.5", "1.el9")];
        let even = assess(&b("rk:foo", 0, "1.5", "1.el9"), &up).drift.unwrap();
        assert_eq!(even.concept, "drift-even");
        let ver_equiv = assess(&b("rk:foo", 0, "1.05", "1.el9"), &up).drift.unwrap();
        assert_eq!(ver_equiv.concept, "drift-version-equivalent");
    }

    #[test]
    fn differing_epoch_never_matches_at_any_fidelity_tier() {
        // Identical version-release, different epoch: canonical_evr_eq requires
        // epoch equality too, so this must be fidelity-unknown, not fidelity-exact.
        let rb = b("alma:foo", 1, "1.0", "1.el9");
        let up = vec![b("rhel:foo", 0, "1.0", "1.el9")];
        let f = assess(&rb, &up).fidelity.unwrap();
        assert_eq!(f.concept, "fidelity-unknown");
        let d = assess(&rb, &up).drift.unwrap();
        assert_eq!(d.concept, "drift-ahead"); // epoch 1 > epoch 0
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cd etl/pg-collect && cargo test --lib rebuild_classify 2>&1 | tail -30`
Expected: compile errors (`cannot find function 'assess'`, `cannot find type 'Assessment'`, etc.) — the old `classify`/`Classification`/`Link` still exist at this point but the new tests reference symbols that don't exist yet.

- [ ] **Step 3: Replace the implementation**

Replace everything above the `#[cfg(test)]` line with:

```rust
//! Pure rebuild-fidelity/drift assessment: one rebuild build vs the upstream
//! build set. Implements `rebuild-norm/v1-nostream` — see
//! docs/superpowers/specs/2026-09-04-rebuild-assessment-model-design.md.
use crate::rebuild_norm::{module_base, strip_vendor};
use crate::rpmver::{evr_cmp, rpmvercmp};
use std::cmp::Ordering;

#[derive(Clone)]
pub struct Build {
    pub node_uri: String,
    pub epoch: i64,
    pub version: String,
    pub release: String,
}

impl Build {
    fn nvr(&self) -> String { format!("{}-{}", self.version, self.release) }
}

/// `rebuild-norm/v1-nostream`'s pinned confidence values (design §4). Fixed
/// string literals, not floats: serialization must be exact, never reformatted.
pub const CONFIDENCE_EXACT: &str = "1.0";
pub const CONFIDENCE_VENDOR_PATCHED: &str = "0.9";
/// Lowered from v1's 0.85: this producer has no module:stream metadata and
/// cannot verify the modular-equivalent tier's stream-membership requirement.
pub const CONFIDENCE_MODULAR_EQUIVALENT: &str = "0.6";
pub const CONFIDENCE_UNKNOWN_NO_MATCH: &str = "0.7";
pub const CONFIDENCE_AMBIGUOUS: &str = "0.5";

pub struct FidelityOutcome {
    pub concept: &'static str,
    pub baseline: Option<String>,
    pub ambiguous_candidates: Vec<String>,
    pub confidence: &'static str,
}

pub struct DriftOutcome {
    pub concept: &'static str,
    pub compared_against: String,
}

pub struct Assessment {
    pub has_upstream_counterpart: bool,
    pub fidelity: Option<FidelityOutcome>,
    pub drift: Option<DriftOutcome>,
}

/// Deterministic pick: newest by EVR, ties broken by lexicographically smallest URI.
fn pick_newest<'a>(cands: &mut Vec<&'a Build>) -> &'a Build {
    cands.sort_by(|a, b| {
        evr_cmp(b.epoch, &b.version, &b.release, a.epoch, &a.version, &a.release)
            .then_with(|| a.node_uri.cmp(&b.node_uri))
    });
    cands[0]
}

/// Canonical EVR equality after epoch normalization: epoch equal AND `rpmvercmp`
/// on the (tier-normalized) version-release string returns `Equal`. This
/// tolerates RPM-ignored separator/leading-zero differences that raw string
/// equality incorrectly treats as a non-match.
fn canonical_evr_eq(epoch_a: i64, nvr_a: &str, epoch_b: i64, nvr_b: &str) -> bool {
    epoch_a == epoch_b && rpmvercmp(nvr_a, nvr_b) == Ordering::Equal
}

/// One fidelity tier's candidate search. `upstream_normalize` is applied to each
/// upstream build's NVR before comparing to `rb_nvr_normalized` (already
/// normalized by the caller): identity for exact/vendor tiers (upstream side
/// unmodified), `module_base` for the modular tier (both sides truncated).
fn tier_candidates<'a>(
    rb_epoch: i64,
    rb_nvr_normalized: &str,
    upstream: &'a [Build],
    upstream_normalize: impl Fn(&str) -> String,
) -> Vec<&'a Build> {
    upstream
        .iter()
        .filter(|u| canonical_evr_eq(rb_epoch, rb_nvr_normalized, u.epoch, &upstream_normalize(u.nvr())))
        .collect()
}

/// Resolve one fidelity tier's candidate set: exactly 1 -> matched with that
/// baseline; >=2 -> ambiguous (fidelity-unknown, all tied candidates recorded,
/// no baseline) -- ties are surfaced, never silently tie-broken.
fn resolve_tier(concept: &'static str, confidence: &'static str, mut candidates: Vec<&Build>) -> FidelityOutcome {
    if candidates.len() == 1 {
        FidelityOutcome {
            concept,
            baseline: Some(candidates[0].node_uri.clone()),
            ambiguous_candidates: Vec::new(),
            confidence,
        }
    } else {
        candidates.sort_by(|a, b| a.node_uri.cmp(&b.node_uri));
        FidelityOutcome {
            concept: "fidelity-unknown",
            baseline: None,
            ambiguous_candidates: candidates.iter().map(|c| c.node_uri.clone()).collect(),
            confidence: CONFIDENCE_AMBIGUOUS,
        }
    }
}

pub fn assess(rb: &Build, upstream: &[Build]) -> Assessment {
    if upstream.is_empty() {
        return Assessment { has_upstream_counterpart: false, fidelity: None, drift: None };
    }
    let rb_nvr = rb.nvr();

    // Fidelity ladder: first tier with >=1 candidate decides -- never fall
    // through past a tier that produced any candidates, ambiguous or not.
    let exact = tier_candidates(rb.epoch, &rb_nvr, upstream, |s| s.to_string());
    let fidelity = if !exact.is_empty() {
        resolve_tier("fidelity-exact", CONFIDENCE_EXACT, exact)
    } else {
        let rb_stripped = strip_vendor(&rb_nvr);
        let vendor = if rb_stripped != rb_nvr {
            tier_candidates(rb.epoch, &rb_stripped, upstream, |s| s.to_string())
        } else {
            Vec::new()
        };
        if !vendor.is_empty() {
            resolve_tier("fidelity-vendor-patched", CONFIDENCE_VENDOR_PATCHED, vendor)
        } else {
            let rb_mod = module_base(&rb_nvr);
            let modular = if rb_mod != rb_nvr {
                tier_candidates(rb.epoch, &rb_mod, upstream, |s| module_base(&s))
            } else {
                Vec::new()
            };
            if !modular.is_empty() {
                resolve_tier("fidelity-modular-equivalent", CONFIDENCE_MODULAR_EQUIVALENT, modular)
            } else {
                FidelityOutcome {
                    concept: "fidelity-unknown",
                    baseline: None,
                    ambiguous_candidates: Vec::new(),
                    confidence: CONFIDENCE_UNKNOWN_NO_MATCH,
                }
            }
        }
    };

    // Drift: always computed alongside fidelity, against upstream's newest
    // build (an independent baseline -- NOT the fidelity baseline). Unaffected
    // by the canonical-equality fix above: even/version-equivalent is
    // deliberately a raw-string-vs-rpmvercmp distinction on a different axis.
    let newest = pick_newest(&mut upstream.iter().collect());
    let drift_concept = if rb.epoch == newest.epoch && rb.version == newest.version && rb.release == newest.release {
        "drift-even"
    } else {
        match evr_cmp(rb.epoch, &rb.version, &rb.release, newest.epoch, &newest.version, &newest.release) {
            Ordering::Equal => "drift-version-equivalent",
            Ordering::Greater => "drift-ahead",
            Ordering::Less => "drift-behind",
        }
    };
    let drift = DriftOutcome { concept: drift_concept, compared_against: newest.node_uri.clone() };

    Assessment { has_upstream_counterpart: true, fidelity: Some(fidelity), drift: Some(drift) }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd etl/pg-collect && cargo test --lib rebuild_classify 2>&1 | tail -20`
Expected: all 11 tests pass, no warnings.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/rebuild_classify.rs
git commit -m "feat(deriver): replace flat classify() with reified assess()

Implements rebuild-norm/v1-nostream: independent presence/fidelity/drift
outcomes, canonical (rpmvercmp) EVR equality per tier instead of raw string
equality, ambiguity surfaced rather than tie-broken, and the -nostream
confidence table (0.6 for modular-equivalent, 0.7 for unmatched)."
```

---

### Task 2: `derive_comparison.rs` — assessment emission and IRI construction

**Files:**
- Modify: `etl/pg-collect/src/derive_comparison.rs` (lines 1–355, i.e. everything above `pub struct RebuildComparisonDeriver`)

**Interfaces:**
- Consumes: `crate::rebuild_classify::{assess, Assessment, Build}` (Task 1); `crate::uris::{PKG, DATA, RDF_TYPE, RDFS_LABEL, XSD, encode}` (all pre-existing, `pub`); `crate::ntriples::NTriplesWriter` (`write_triple`, `write_literal`, `write_typed_literal`, `write_boolean`, `write_datetime`, `write_triple_once`, `write_literal_once` — all pre-existing).
- Produces (consumed by Tasks 3, 4, 5):
  ```rust
  pub struct Pair { pub rebuild_graph: String, pub rhel_graph: String }  // unchanged
  pub const ASSESSMENT_METHOD: &str;  // "rebuild-norm/v1-nostream"
  pub fn assessment_iri(node_uri: &str, run_token: &str) -> String;
  pub fn snapshot_iri(upstream_graph: &str, run_token: &str) -> String;
  #[derive(Debug)]
  pub struct RebuildReport {
      pub pairs: usize,
      pub assessments: usize,
      pub presence_true: usize,
      pub presence_false: usize,
      pub fidelity_counts: BTreeMap<String, usize>,
      pub drift_counts: BTreeMap<String, usize>,
      pub ambiguous_count: usize,
      pub distribution_count: usize,
      pub assessment_subjects: BTreeSet<String>,
      pub snapshot_subjects: BTreeSet<String>,
      pub triples: usize,
  }
  pub(crate) struct PairData {
      pub rebuild_dist: String,
      pub rhel_dist: String,
      pub rhel_graph: String,   // NEW — needed for the snapshot IRI
      pub rebuild_builds: Vec<(String, Build)>,
      pub rhel_builds: Vec<(String, Build)>,
  }
  pub(crate) fn build_report(pairs_data: &[PairData], run_token: &str, assessed_at: &str) -> Result<(String, RebuildReport)>;
  ```
  `parse_pair`/`err`/`aggregate_source_rows` are unchanged (already correct for the new model — no edits needed).

- [ ] **Step 1: Write the failing tests**

Replace the module's existing `RebuildReport`/`PairData`/`build_report`/`emit_classification` tests (they reference `statuses`/`status_subjects`/old `emit_classification` and will not compile once Step 3 lands) with:

```rust
#[cfg(test)]
mod emission_tests {
    use super::*;
    use crate::rebuild_classify::Build;
    use std::io::Read;

    fn b(uri: &str, e: i64, v: &str, r: &str) -> (String, Build) {
        (
            uri.rsplit('/').nth(1).unwrap_or("x").to_string(), // crude name stand-in; real callers group by name separately
            Build { node_uri: uri.into(), epoch: e, version: v.into(), release: r.into() },
        )
    }

    #[test]
    fn iris_are_scoped_to_run_token() {
        let a1 = assessment_iri("https://x/src/rhel/9/foo/1.0-1", "run-1");
        let a2 = assessment_iri("https://x/src/rhel/9/foo/1.0-1", "run-2");
        assert_ne!(a1, a2);
        assert!(a1.starts_with("https://x/src/rhel/9/foo/1.0-1/assessment/"));

        let s1 = snapshot_iri("https://x/graph/rhel/9", "run-1");
        let s2 = snapshot_iri("https://x/graph/rhel/9", "run-2");
        assert_ne!(s1, s2);
        // same (graph, run) => same snapshot IRI (reused within a run)
        assert_eq!(snapshot_iri("https://x/graph/rhel/9", "run-1"), s1);
    }

    #[test]
    fn build_report_end_to_end_case_coverage() {
        // One pair: almalinux/9 -> rhel/9. Names: "openssl" (exact match, drift-even),
        // "foo" (no upstream candidates -> presence=false), "bar" (behind).
        let pairs_data = vec![PairData {
            rebuild_dist: "https://x/d/distro/almalinux".into(),
            rhel_dist: "https://x/d/distro/rhel".into(),
            rhel_graph: "https://x/graph/rhel/9".into(),
            rebuild_builds: vec![
                ("openssl".into(), Build { node_uri: "alma:openssl".into(), epoch: 0, version: "3.5.5".into(), release: "6.el9_8".into() }),
                ("bar".into(), Build { node_uri: "alma:bar".into(), epoch: 0, version: "1.0".into(), release: "1.el9".into() }),
                ("rocky-logos".into(), Build { node_uri: "alma:rocky-logos".into(), epoch: 0, version: "90".into(), release: "1.el9".into() }),
            ],
            rhel_builds: vec![
                ("openssl".into(), Build { node_uri: "rhel:openssl".into(), epoch: 0, version: "3.5.5".into(), release: "6.el9_8".into() }),
                ("bar".into(), Build { node_uri: "rhel:bar".into(), epoch: 0, version: "1.2".into(), release: "1.el9".into() }),
            ],
        }];

        let (nt, report) = build_report(&pairs_data, "run-abc", "2026-09-04T00:00:00Z").unwrap();

        assert_eq!(report.pairs, 1);
        assert_eq!(report.assessments, 3);
        assert_eq!(report.presence_true, 2); // openssl, bar
        assert_eq!(report.presence_false, 1); // rocky-logos
        assert_eq!(report.assessment_subjects.len(), 3);
        assert_eq!(report.assessment_subjects.len(), report.assessments);
        assert_eq!(report.fidelity_counts.values().sum::<usize>(), report.presence_true);
        assert_eq!(report.drift_counts.values().sum::<usize>(), report.presence_true);
        assert_eq!(report.distribution_count, 1);
        assert_eq!(report.snapshot_subjects.len(), 1); // one upstream graph this run

        // Exactly one DataSnapshot definition emitted despite 3 references to it.
        assert_eq!(nt.matches("a <https://purl.org/packagegraph/ontology/core#DataSnapshot>").count(), 1);
        // Never emit rebuildOf, never lineageConfirmed=true, never lineageEvidence.
        assert!(!nt.contains("#rebuildOf>"));
        assert!(!nt.contains("lineageConfirmed> \"true\""));
        assert!(!nt.contains("#lineageEvidence>"));
        // Method id is the -nostream variant everywhere.
        assert!(nt.contains("\"rebuild-norm/v1-nostream\""));
        assert!(!nt.contains("\"rebuild-norm/v1\""));
        // openssl: exact fidelity, drift-even, confidence 1.0.
        assert!(nt.contains("#fidelityBaseline> <rhel:openssl>"));
        assert!(nt.contains("#fidelity-exact>"));
        assert!(nt.contains("#drift-even>"));
        // rocky-logos: hasUpstreamCounterpart false, no fidelity/drift lines for it.
        let rocky_assessment_prefix = "alma:rocky-logos/assessment/";
        assert!(nt.lines().any(|l| l.contains(rocky_assessment_prefix) && l.contains("hasUpstreamCounterpart> \"false\"")));
    }

    #[test]
    fn dedup_writer_collapses_repeated_snapshot_definition() {
        // Two pairs citing the SAME upstream graph must share one DataSnapshot node.
        let make_pair = |rb_dist: &str| PairData {
            rebuild_dist: rb_dist.into(),
            rhel_dist: "https://x/d/distro/rhel".into(),
            rhel_graph: "https://x/graph/rhel/9".into(),
            rebuild_builds: vec![("foo".into(), Build { node_uri: format!("{rb_dist}:foo"), epoch: 0, version: "1.0".into(), release: "1.el9".into() })],
            rhel_builds: vec![("foo".into(), Build { node_uri: "rhel:foo".into(), epoch: 0, version: "1.0".into(), release: "1.el9".into() })],
        };
        let pairs_data = vec![make_pair("https://x/d/distro/almalinux"), make_pair("https://x/d/distro/rocky")];
        let (nt, report) = build_report(&pairs_data, "run-1", "2026-09-04T00:00:00Z").unwrap();
        assert_eq!(report.snapshot_subjects.len(), 1);
        assert_eq!(nt.matches("a <https://purl.org/packagegraph/ontology/core#DataSnapshot>").count(), 1);
        assert_eq!(nt.matches("http://www.w3.org/2000/01/rdf-schema#label>").count(), 1);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison 2>&1 | tail -30`
Expected: compile errors referencing `assessment_iri`, `snapshot_iri`, `report.assessments`, `report.presence_true`, `PairData { rhel_graph: ... }`, etc. — none of these exist in the current file.

- [ ] **Step 3: Replace lines 1–355 of `derive_comparison.rs`**

```rust
//! Deriver: assess each rebuild source name's newest build against upstream
//! (presence/fidelity/drift) and emit reified pkg:RebuildAssessment nodes into
//! a derived graph. Implements rebuild-norm/v1-nostream -- see
//! docs/superpowers/specs/2026-09-04-rebuild-assessment-model-design.md.
use crate::ntriples::NTriplesWriter;
use crate::rebuild_classify::{assess, Assessment, Build};
use crate::rpmver::evr_cmp;
use crate::sparql::SparqlClient;
use crate::uris::{encode, DATA, PKG, RDFS_LABEL, RDF_TYPE, XSD};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Result;

pub struct Pair {
    pub rebuild_graph: String,
    pub rhel_graph: String,
}

/// This deriver's versioned method id. Never "rebuild-norm/v1": that name is
/// reserved for a producer that verifies module-stream membership on the
/// modular-equivalent tier, which this pipeline's collected data cannot do.
pub const ASSESSMENT_METHOD: &str = "rebuild-norm/v1-nostream";

#[derive(Debug)]
pub struct RebuildReport {
    pub pairs: usize,
    /// Total pkg:RebuildAssessment nodes emitted (one per assessed rebuild name).
    pub assessments: usize,
    pub presence_true: usize,
    pub presence_false: usize,
    /// Only for presence_true assessments. Sum == presence_true.
    pub fidelity_counts: BTreeMap<String, usize>,
    /// Only for presence_true assessments. Sum == presence_true.
    pub drift_counts: BTreeMap<String, usize>,
    /// Count of assessments with >=1 pkg:ambiguousCandidate. Subset of
    /// fidelity_counts["fidelity-unknown"] (the other subset is "no candidates
    /// at any tier", which is also fidelity-unknown but not ambiguous).
    pub ambiguous_count: usize,
    /// Distinct pkg:derivedFromDistribution triples emitted.
    pub distribution_count: usize,
    /// The exact set of pkg:assessmentOf TARGET URIs (the rebuild source nodes
    /// assessed this run). Staging validation requires the staging graph's
    /// assessmentOf targets to equal this set exactly.
    pub assessment_subjects: BTreeSet<String>,
    /// The exact set of pkg:DataSnapshot IRIs minted this run (one per distinct
    /// upstream graph). Staging validation requires every
    /// pkg:assessedAgainstSnapshot value to be a member of this set.
    pub snapshot_subjects: BTreeSet<String>,
    pub triples: usize,
}

pub fn parse_pair(s: &str) -> Result<Pair> {
    let (rb, rh) = s
        .split_once('=')
        .ok_or_else(|| err(format!("--pair must be rebuild=rhel, got: {s}")))?;
    for iri in [rb, rh] {
        if !(iri.starts_with("http://") || iri.starts_with("https://")) {
            return Err(err(format!("--pair IRIs must be absolute, got: {iri}")));
        }
        if iri.chars().any(|c| {
            matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`')
                || c.is_ascii_whitespace()
        }) {
            return Err(err(format!(
                "--pair IRIs must not contain <, >, \", {{, }}, |, \\, ^, backtick, or whitespace, got: {iri}"
            )));
        }
    }
    Ok(Pair {
        rebuild_graph: rb.to_string(),
        rhel_graph: rh.to_string(),
    })
}

fn err(m: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, m)
}

/// Collapse the one-row-per-(source, binary) shape from `query_source_builds`
/// into one `Build` per distinct source `node_uri`, taking the MAX epoch across
/// its rows. `version`/`release` are constant per source, so first-seen wins.
pub(crate) fn aggregate_source_rows(
    rows: Vec<(String, String, i64, String, String)>,
) -> Vec<(String, Build)> {
    let mut by_uri: HashMap<String, (String, Build)> = HashMap::new();
    for (name, uri, epoch, version, release) in rows {
        by_uri
            .entry(uri.clone())
            .and_modify(|(_, b)| {
                if epoch > b.epoch {
                    b.epoch = epoch;
                }
            })
            .or_insert((
                name,
                Build {
                    node_uri: uri,
                    epoch,
                    version,
                    release,
                },
            ));
    }
    by_uri.into_values().collect()
}

/// This run's deterministic `RebuildAssessment` IRI for a rebuild source node.
/// Run-scoped (includes `run_token`), NOT stable across runs: `RebuildAssessment`
/// is documented as a timestamped, point-in-time observation, and the whole
/// derived graph is atomically replaced every run, so a stable IRI would only
/// let a later run silently overwrite the meaning of an earlier one.
pub fn assessment_iri(node_uri: &str, run_token: &str) -> String {
    format!("{node_uri}/assessment/{}", encode(run_token))
}

/// This run's deterministic `DataSnapshot` IRI for an upstream graph. Keyed on
/// `(upstream_graph, run_token)`, NOT `upstream_graph` alone: `assessedAgainstSnapshot`
/// exists so results are reproducible against a specific state, and a graph-only
/// key would let a later run against changed upstream data silently reuse the
/// same IRI for a different state. Reused across every pair in THIS run that
/// cites the same upstream graph.
pub fn snapshot_iri(upstream_graph: &str, run_token: &str) -> String {
    format!(
        "{DATA}snapshot/rebuild-norm/v1-nostream/{}/{}",
        encode(upstream_graph),
        encode(run_token)
    )
}

/// Per-pair inputs assembled by `derive` (all SPARQL I/O done by the caller) and
/// consumed by the pure `build_report`.
pub(crate) struct PairData {
    pub rebuild_dist: String,
    pub rhel_dist: String,
    /// The upstream graph IRI itself (e.g. ".../graph/rhel/9") -- needed to key
    /// the DataSnapshot, distinct from `rhel_dist` (the Distribution resource).
    pub rhel_graph: String,
    pub rebuild_builds: Vec<(String, Build)>,
    pub rhel_builds: Vec<(String, Build)>,
}

/// Emit one `RebuildAssessment` node's triples. Returns the triple count.
fn emit_assessment<W: std::io::Write>(
    w: &mut NTriplesWriter<W>,
    rb_node_uri: &str,
    snapshot: &str,
    assessed_at: &str,
    run_token: &str,
    a: &Assessment,
) -> Result<usize> {
    let assessment = assessment_iri(rb_node_uri, run_token);
    let mut n = 0;
    w.write_triple(&assessment, RDF_TYPE, &format!("{PKG}RebuildAssessment"))?;
    n += 1;
    w.write_triple(&assessment, &format!("{PKG}assessmentOf"), rb_node_uri)?;
    n += 1;
    w.write_datetime(&assessment, &format!("{PKG}assessedAt"), assessed_at)?;
    n += 1;
    w.write_literal(&assessment, &format!("{PKG}assessmentMethod"), ASSESSMENT_METHOD)?;
    n += 1;
    w.write_triple(&assessment, &format!("{PKG}assessedAgainstSnapshot"), snapshot)?;
    n += 1;
    w.write_boolean(&assessment, &format!("{PKG}hasUpstreamCounterpart"), a.has_upstream_counterpart)?;
    n += 1;
    w.write_boolean(&assessment, &format!("{PKG}lineageConfirmed"), false)?;
    n += 1;

    if let Some(fidelity) = &a.fidelity {
        w.write_triple(&assessment, &format!("{PKG}rebuildFidelity"), &format!("{PKG}{}", fidelity.concept))?;
        n += 1;
        w.write_typed_literal(&assessment, &format!("{PKG}assessmentConfidence"), fidelity.confidence, &format!("{XSD}decimal"))?;
        n += 1;
        if let Some(baseline) = &fidelity.baseline {
            w.write_triple(&assessment, &format!("{PKG}fidelityBaseline"), baseline)?;
            n += 1;
        }
        for cand in &fidelity.ambiguous_candidates {
            w.write_triple(&assessment, &format!("{PKG}ambiguousCandidate"), cand)?;
            n += 1;
        }
    }
    if let Some(drift) = &a.drift {
        w.write_triple(&assessment, &format!("{PKG}rebuildDrift"), &format!("{PKG}{}", drift.concept))?;
        n += 1;
        w.write_triple(&assessment, &format!("{PKG}comparedAgainst"), &drift.compared_against)?;
        n += 1;
    }
    Ok(n)
}

/// Pure orchestration core: given per-pair data (no SPARQL I/O) plus this run's
/// `run_token` and `assessed_at` timestamp, render the derived N-Triples and
/// tally the report. Deterministic; does distribution dedup, snapshot minting,
/// group-by-name, newest-per-name selection, assessment, and emission.
pub(crate) fn build_report(
    pairs_data: &[PairData],
    run_token: &str,
    assessed_at: &str,
) -> Result<(String, RebuildReport)> {
    let mut report = RebuildReport {
        pairs: pairs_data.len(),
        assessments: 0,
        presence_true: 0,
        presence_false: 0,
        fidelity_counts: BTreeMap::new(),
        drift_counts: BTreeMap::new(),
        ambiguous_count: 0,
        distribution_count: 0,
        assessment_subjects: BTreeSet::new(),
        snapshot_subjects: BTreeSet::new(),
        triples: 0,
    };
    let mut w = NTriplesWriter::new(Vec::<u8>::new());
    let mut emitted_dist: HashSet<(String, String)> = HashSet::new();

    for pd in pairs_data {
        // distribution-level lineage (deduped across release pairs) -- unchanged
        if emitted_dist.insert((pd.rebuild_dist.clone(), pd.rhel_dist.clone())) {
            w.write_triple(&pd.rebuild_dist, &format!("{PKG}derivedFromDistribution"), &pd.rhel_dist)?;
            report.triples += 1;
            report.distribution_count += 1;
        }

        // DataSnapshot for this pair's upstream graph: deterministic IRI, so
        // repeated references across pairs sharing an upstream graph collapse
        // via write_*_once regardless of call order.
        let snapshot = snapshot_iri(&pd.rhel_graph, run_token);
        report.snapshot_subjects.insert(snapshot.clone());
        if w.write_triple_once(&snapshot, RDF_TYPE, &format!("{PKG}DataSnapshot"))? {
            report.triples += 1;
        }
        let label = format!(
            "Upstream snapshot for {ASSESSMENT_METHOD}, graph {}, run {run_token}",
            pd.rhel_graph
        );
        if w.write_literal_once(&snapshot, RDFS_LABEL, &label)? {
            report.triples += 1;
        }

        let mut up: HashMap<String, Vec<Build>> = HashMap::new();
        for (name, build) in &pd.rhel_builds {
            up.entry(name.clone()).or_default().push(build.clone());
        }
        let mut rb: HashMap<String, Vec<Build>> = HashMap::new();
        for (name, build) in &pd.rebuild_builds {
            rb.entry(name.clone()).or_default().push(build.clone());
        }

        for (name, mut builds) in rb {
            builds.sort_by(|a, b| {
                evr_cmp(b.epoch, &b.version, &b.release, a.epoch, &a.version, &a.release)
                    .then_with(|| a.node_uri.cmp(&b.node_uri))
            });
            let newest = &builds[0];
            let upstream = up.get(&name).map(|v| v.as_slice()).unwrap_or(&[]);
            let a = assess(newest, upstream);

            let n = emit_assessment(&mut w, &newest.node_uri, &snapshot, assessed_at, run_token, &a)?;
            report.triples += n;
            report.assessments += 1;
            report.assessment_subjects.insert(newest.node_uri.clone());

            if a.has_upstream_counterpart {
                report.presence_true += 1;
                if let Some(f) = &a.fidelity {
                    *report.fidelity_counts.entry(f.concept.to_string()).or_insert(0) += 1;
                    if !f.ambiguous_candidates.is_empty() {
                        report.ambiguous_count += 1;
                    }
                }
                if let Some(d) = &a.drift {
                    *report.drift_counts.entry(d.concept.to_string()).or_insert(0) += 1;
                }
            } else {
                report.presence_false += 1;
            }
        }
    }
    let ntriples = w.into_string()?;
    Ok((ntriples, report))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison::emission_tests 2>&1 | tail -30`
Expected: both tests pass. The rest of `derive_comparison`'s test module (ontology gate, staging validation, `derive()`, `RebuildComparisonDeriver`) will still fail to compile at this point — that's expected; Tasks 3–4 fix the rest of the file. Do not attempt to fix compile errors outside the `emission_tests` module in this task.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/derive_comparison.rs
git commit -m "feat(deriver): emit reified RebuildAssessment nodes

Run-scoped DataSnapshot and RebuildAssessment IRIs (keyed on run_token, not
just graph identity, so assessedAgainstSnapshot stays reproducible across
runs). DataSnapshot definitions dedup via write_*_once when multiple pairs
share an upstream graph. rebuildOf/lineageEvidence are never emitted;
lineageConfirmed is always false."
```

---

### Task 3: Ontology gate + self-baseline guard

**Files:**
- Modify: `etl/pg-collect/src/derive_comparison.rs` (the section from the old `REBUILD_TERMS`/`term_type_query` through `check_ontology_terms`, plus `derive()`'s duplicate-check block)

**Interfaces:**
- Consumes: nothing new.
- Produces (consumed by Task 5's CLI, unchanged from before): `RebuildComparisonDeriver::check_ontology_terms(&self) -> Result<()>`.

- [ ] **Step 1: Write the failing test**

The existing gate test references the deleted `TRACK_CONCEPTS`/`REBUILD_TERMS`-old values; replace it (in the file's main `#[cfg(test)] mod tests` block, not `emission_tests`) with:

```rust
#[test]
fn ontology_gate_checks_the_reified_terms() {
    for term in REBUILD_TERMS {
        let q = term_type_query(term);
        assert!(q.contains(term), "gate query for {term} must reference it");
    }
    assert!(REBUILD_TERMS.contains(&"RebuildAssessment"));
    assert!(REBUILD_TERMS.contains(&"assessedAgainstSnapshot"));
    assert!(REBUILD_TERMS.contains(&"RebuildFidelityScheme"));
    assert!(REBUILD_TERMS.contains(&"RebuildDriftScheme"));
    assert!(!REBUILD_TERMS.contains(&"rebuildTrackingStatus")); // old term, gone
}

#[test]
fn derive_rejects_a_pair_whose_rebuild_and_upstream_graph_are_the_same() {
    // Constructed directly against a fake endpoint URL; check_readiness/
    // check_ontology_terms are not called by derive() itself, so this only
    // exercises the guard, no live Fuseki needed.
    let deriver = RebuildComparisonDeriver::new("http://127.0.0.1:1/unused");
    let pairs = vec![Pair {
        rebuild_graph: "https://x/graph/rhel/9".into(),
        rhel_graph: "https://x/graph/rhel/9".into(),
    }];
    let err = deriver.derive("/tmp/unused-output.nt", &pairs, "run-1", "2026-09-04T00:00:00Z").unwrap_err();
    assert!(err.to_string().contains("cannot be its own upstream"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison::tests::ontology_gate_checks_the_reified_terms derive_comparison::tests::derive_rejects_a_pair_whose_rebuild_and_upstream_graph_are_the_same 2>&1 | tail -30`
Expected: compile errors (old `REBUILD_TERMS` still has the flat-model 4 entries; `derive()` doesn't yet take `run_token`/`assessed_at`).

- [ ] **Step 3: Replace the term list and add the self-baseline guard**

Find the old block (currently around line 110–124):
```rust
pub(crate) const REBUILD_TERMS: [&str; 4] = [
    "rebuildOf",
    "comparedAgainst",
    "rebuildTrackingStatus",
    "RebuildTrackingScheme",
];

pub(crate) fn term_type_query(term: &str) -> String {
    format!("SELECT ?p WHERE {{ <{PKG}{term}> a ?p }} LIMIT 1")
}
```
Replace with:
```rust
/// Rebuild-vocabulary terms that must be declared in the loaded TBox before
/// the deriver may run. Deliberately references ONLY the reified model's terms
/// -- not the old flat rebuildTrackingStatus/RebuildTrackingScheme, which no
/// longer exist in the ontology, so gating on them would always fail closed
/// (safe, but with a useless error message).
pub(crate) const REBUILD_TERMS: [&str; 11] = [
    "RebuildAssessment",
    "assessmentOf",
    "hasUpstreamCounterpart",
    "rebuildFidelity",
    "rebuildDrift",
    "fidelityBaseline",
    "comparedAgainst",
    "assessedAgainstSnapshot",
    "lineageConfirmed",
    "RebuildFidelityScheme",
    "RebuildDriftScheme",
];

/// Per-term existence probe. `SparqlClient::query` is built for SELECT bindings,
/// so we use `SELECT ?p WHERE { <term> a ?p } LIMIT 1` (NOT ASK) and require
/// each term to return >= 1 row (works uniformly for properties and classes:
/// a class has its own rdf:type, e.g. owl:Class).
pub(crate) fn term_type_query(term: &str) -> String {
    format!("SELECT ?p WHERE {{ <{PKG}{term}> a ?p }} LIMIT 1")
}
```

Then find `check_ontology_terms`'s body and update its error message (keep the function signature and per-term loop structure identical):
```rust
pub fn check_ontology_terms(&self) -> Result<()> {
    for term in REBUILD_TERMS {
        let rows = self.sparql.query(&term_type_query(term))?;
        if rows.is_empty() {
            return Err(err(format!(
                "rebuild-assessment vocabulary term pkg:{term} is not declared. Sync ontology PR #5 (v0.13.0-reified) and load the TBox before running this deriver."
            )));
        }
    }
    Ok(())
}
```

Finally, in `derive()`, change the signature to accept `run_token`/`assessed_at` and add the self-baseline guard immediately after the existing duplicate-rebuild-graph check:

```rust
pub fn derive(
    &self,
    output_path: &str,
    pairs: &[Pair],
    run_token: &str,
    assessed_at: &str,
) -> Result<RebuildReport> {
    let mut seen: HashSet<&str> = HashSet::new();
    for p in pairs {
        if !seen.insert(p.rebuild_graph.as_str()) {
            return Err(err(format!(
                "rebuild graph {} appears in more than one pair; a rebuild graph must map to exactly one upstream",
                p.rebuild_graph
            )));
        }
    }
    // A rebuild graph must never be paired with itself: it would make a
    // package its own fidelity/drift baseline, which RebuildAssessmentSelfBaselineShape
    // forbids -- reject it structurally before any I/O, not just via the
    // staging self-baseline check (belt and suspenders).
    for p in pairs {
        if p.rebuild_graph == p.rhel_graph {
            return Err(err(format!(
                "pair rebuild_graph and rhel_graph are the same graph ({}); a rebuild graph cannot be its own upstream",
                p.rebuild_graph
            )));
        }
    }

    let mut pairs_data: Vec<PairData> = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let rebuild_dist = self.sparql.resolve_distribution(&pair.rebuild_graph)?;
        let rhel_dist = self.sparql.resolve_distribution(&pair.rhel_graph)?;
        let rebuild_builds = aggregate_source_rows(self.sparql.query_source_builds(&pair.rebuild_graph)?);
        let rhel_builds = aggregate_source_rows(self.sparql.query_source_builds(&pair.rhel_graph)?);
        pairs_data.push(PairData {
            rebuild_dist,
            rhel_dist,
            rhel_graph: pair.rhel_graph.clone(),
            rebuild_builds,
            rhel_builds,
        });
    }

    let (ntriples, report) = build_report(&pairs_data, run_token, assessed_at)?;
    std::fs::write(output_path, ntriples)?;
    Ok(report)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison::tests::ontology_gate_checks_the_reified_terms derive_comparison::tests::derive_rejects_a_pair_whose_rebuild_and_upstream_graph_are_the_same 2>&1 | tail -20`
Expected: both pass. (The crate will still not fully build — Task 4 replaces `validate_staging`/`load_atomic`, which currently reference the old `RebuildReport.statuses`/`status_subjects` fields removed in Task 2. That's expected; don't chase those errors here.)

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/derive_comparison.rs
git commit -m "feat(deriver): gate on reified ontology terms; reject self-paired graphs

REBUILD_TERMS now names the RebuildAssessment vocabulary (11 terms); the old
4-term flat-model list is gone. derive() also rejects a pair whose
rebuild_graph equals its rhel_graph before any SPARQL I/O."
```

---

### Task 4: Staging validation rewrite

**Files:**
- Modify: `etl/pg-collect/src/derive_comparison.rs` (everything from the old `atomic_swap_update`/`TRACK_CONCEPTS`/`StagingValidationQueries`/`staging_validation_queries`/`staging_status_subjects_query` through `validate_staging` and `load_atomic`)

**Interfaces:**
- Consumes: `RebuildReport` (Task 2), `Pair` (unchanged).
- Produces (consumed by Task 5's CLI):
  ```rust
  pub struct LoadExpectations<'a> {
      pub assessments: usize,
      pub distribution_count: usize,
      pub subjects: &'a BTreeSet<String>,
      pub snapshots: &'a BTreeSet<String>,
      pub rhel_graphs: &'a [String],
  }
  impl RebuildComparisonDeriver {
      pub fn load_atomic(&self, nt_path: &str, prod_graph: &str, run_token: &str, expect: LoadExpectations) -> Result<()>;
  }
  ```
  `atomic_swap_update`, `valid_run_token` are unchanged (keep verbatim — do not touch them in this task).

This is the largest task in the plan. Work through it in the sub-steps below; each is independently testable.

- [ ] **Step 1: Write the failing tests**

Add to the file's main `#[cfg(test)] mod tests` block:

```rust
#[test]
fn exactly_one_and_at_most_one_checks_reference_the_predicate() {
    let (_, q1) = exactly_one_check("urn:staging", "assessmentOf");
    assert!(q1.contains("pkg:assessmentOf"));
    assert!(q1.contains("HAVING(?n != 1)"));
    let (_, q2) = at_most_one_check("urn:staging", "rebuildFidelity");
    assert!(q2.contains("pkg:rebuildFidelity"));
    assert!(q2.contains("HAVING(?n > 1)"));
}

#[test]
fn datatype_check_references_predicate_and_type() {
    let (_, q) = datatype_check("urn:staging", "assessedAt", &format!("{XSD}dateTime"));
    assert!(q.contains("pkg:assessedAt"));
    assert!(q.contains("xsd") || q.contains("XMLSchema"));
}

#[test]
fn staging_checks_cover_the_new_shape() {
    let checks = staging_checks("urn:staging", &["urn:rhel9".to_string()]);
    let descs: Vec<&str> = checks.iter().map(|c| c.description.as_str()).collect();
    for expected in [
        "concept outside RebuildFidelityScheme",
        "concept outside RebuildDriftScheme",
        "hasUpstreamCounterpart=false",
        "hasUpstreamCounterpart=true",
        "rebuildDrift present",
        "matched fidelity",
        "fidelity-unknown",
        "ambiguousCandidate",
        "self-baseline",
        "unexpected",
        "pkg:DataSnapshot",
        "rdfs:label",
        "pkg:rebuildOf",
        "lineageConfirmed",
        "lineageEvidence",
    ] {
        assert!(
            descs.iter().any(|d| d.contains(expected)),
            "missing a staging check whose description mentions {expected:?}; got {descs:?}"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison 2>&1 | tail -30`
Expected: `cannot find function 'exactly_one_check'` etc. — none of these exist yet.

- [ ] **Step 3: Replace the validation section**

Delete everything from the old `TRACK_CONCEPTS` const through the old `staging_status_subjects_query` function (i.e. keep `atomic_swap_update` and `valid_run_token`, which sit just before/after this block — verify their exact boundaries by re-reading the file before deleting, since Tasks 2–3 may have shifted line numbers). Replace the deleted block with:

```rust
/// The four `pkg:fidelity-*` concepts of RebuildFidelityScheme. Every emitted
/// `pkg:rebuildFidelity` object MUST be one of these.
const FIDELITY_CONCEPTS: [&str; 4] = [
    "fidelity-exact",
    "fidelity-vendor-patched",
    "fidelity-modular-equivalent",
    "fidelity-unknown",
];

/// The four `pkg:drift-*` concepts of RebuildDriftScheme. Every emitted
/// `pkg:rebuildDrift` object MUST be one of these.
const DRIFT_CONCEPTS: [&str; 4] = [
    "drift-even",
    "drift-ahead",
    "drift-behind",
    "drift-version-equivalent",
];

/// Matched (non-unknown) fidelity tiers: these MUST carry a fidelityBaseline.
const MATCHED_FIDELITY_CONCEPTS: [&str; 3] = [
    "fidelity-exact",
    "fidelity-vendor-patched",
    "fidelity-modular-equivalent",
];

fn concept_list(concepts: &[&str]) -> String {
    concepts.iter().map(|c| format!("pkg:{c}")).collect::<Vec<_>>().join(", ")
}

/// A single fail-closed staging check: `query` is a `SELECT (COUNT(...) AS ?c)`
/// against the staging graph. `MustBeZero` requires `?c == 0`; `MustEqual(n)`
/// requires `?c == n` (used for the two coverage counts, which are compared
/// against a caller-supplied expectation at validation time, not a literal
/// baked into the query).
pub(crate) enum StagingCheckKind {
    MustBeZero,
    MustEqual(usize),
}

pub(crate) struct StagingCheck {
    pub description: String,
    pub query: String,
    pub kind: StagingCheckKind,
}

/// One assessment must carry exactly one `pkg:{predicate}` (required fields:
/// assessmentOf, assessedAt, assessmentMethod, assessedAgainstSnapshot,
/// hasUpstreamCounterpart, lineageConfirmed). Catches both missing (n=0) and
/// duplicate (n>1) in one query via GROUP BY ... HAVING.
pub(crate) fn exactly_one_check(staging: &str, predicate: &str) -> (String, String) {
    (
        format!("assessment(s) not carrying exactly one pkg:{predicate}"),
        format!(
            "PREFIX pkg: <{PKG}>\nSELECT (COUNT(?a) AS ?c) WHERE {{ SELECT ?a (COUNT(?v) AS ?n) WHERE {{ GRAPH <{staging}> {{ ?a a pkg:RebuildAssessment . OPTIONAL {{ ?a pkg:{predicate} ?v }} }} }} GROUP BY ?a HAVING(?n != 1) }}"
        ),
    )
}

/// An assessment may carry AT MOST one `pkg:{predicate}` (optional fields:
/// rebuildFidelity, rebuildDrift, fidelityBaseline, comparedAgainst,
/// assessmentConfidence, lineageEvidence).
pub(crate) fn at_most_one_check(staging: &str, predicate: &str) -> (String, String) {
    (
        format!("assessment(s) carrying more than one pkg:{predicate}"),
        format!(
            "PREFIX pkg: <{PKG}>\nSELECT (COUNT(?a) AS ?c) WHERE {{ SELECT ?a (COUNT(?v) AS ?n) WHERE {{ GRAPH <{staging}> {{ ?a a pkg:RebuildAssessment . OPTIONAL {{ ?a pkg:{predicate} ?v }} }} }} GROUP BY ?a HAVING(?n > 1) }}"
        ),
    )
}

/// Every present `pkg:{predicate}` value must be typed `expected_datatype`.
pub(crate) fn datatype_check(staging: &str, predicate: &str, expected_datatype: &str) -> (String, String) {
    (
        format!("pkg:{predicate} value(s) not typed as the required datatype"),
        format!(
            "PREFIX pkg: <{PKG}>\nSELECT (COUNT(?v) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:{predicate} ?v }} FILTER(!isLiteral(?v) || datatype(?v) != <{expected_datatype}>) }}"
        ),
    )
}

/// Build the full fail-closed staging check list. `rhel_graphs` is the distinct
/// set of upstream graphs configured for this run (used by the "unexpected
/// baseline/candidate object" check to verify every referenced baseline is a
/// real SourcePackage that actually exists in one of them).
pub(crate) fn staging_checks(staging: &str, rhel_graphs: &[String]) -> Vec<StagingCheck> {
    // xsd: is declared even though most checks reference full datatype IRIs
    // directly (`<{XSD}dateTime>`) -- it's needed for the confidence-range
    // check's numeric-literal FILTER and is harmless (an unused PREFIX) elsewhere.
    let prefix = format!("PREFIX pkg: <{PKG}>\nPREFIX xsd: <{XSD}>\n");
    let mut checks = Vec::new();
    let push = |checks: &mut Vec<StagingCheck>, description: &str, query: String, kind: StagingCheckKind| {
        checks.push(StagingCheck { description: description.to_string(), query, kind });
    };

    // Required-field cardinality (exactly 1 each).
    for predicate in [
        "assessmentOf", "assessedAt", "assessmentMethod",
        "assessedAgainstSnapshot", "hasUpstreamCounterpart", "lineageConfirmed",
    ] {
        let (description, query) = exactly_one_check(staging, predicate);
        checks.push(StagingCheck { description, query, kind: StagingCheckKind::MustBeZero });
    }
    // Optional-field max-cardinality (<=1 each).
    for predicate in [
        "rebuildFidelity", "rebuildDrift", "fidelityBaseline",
        "comparedAgainst", "assessmentConfidence", "lineageEvidence",
    ] {
        let (description, query) = at_most_one_check(staging, predicate);
        checks.push(StagingCheck { description, query, kind: StagingCheckKind::MustBeZero });
    }
    // Datatype correctness.
    for (predicate, dt) in [
        ("assessedAt", format!("{XSD}dateTime")),
        ("assessmentMethod", format!("{XSD}string")),
        ("lineageEvidence", format!("{XSD}string")),
        ("hasUpstreamCounterpart", format!("{XSD}boolean")),
        ("lineageConfirmed", format!("{XSD}boolean")),
        ("assessmentConfidence", format!("{XSD}decimal")),
    ] {
        let (description, query) = datatype_check(staging, predicate, &dt);
        checks.push(StagingCheck { description, query, kind: StagingCheckKind::MustBeZero });
    }

    // Direct numeric FILTER comparison on an xsd:decimal-typed literal applies
    // SPARQL's built-in numeric type promotion -- no explicit xsd:decimal(?v)
    // cast function is needed (or correct to add: casting an already-decimal
    // literal is redundant, and the datatype_check above already rejects any
    // non-decimal value before this comparison would even run against real data).
    push(&mut checks, "confidence value(s) outside [0.0, 1.0]",
        format!("{prefix}SELECT (COUNT(?v) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessmentConfidence ?v }} FILTER(?v < 0.0 || ?v > 1.0) }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "rebuildFidelity concept outside RebuildFidelityScheme",
        format!("{prefix}SELECT (COUNT(?v) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildFidelity ?v }} FILTER(?v NOT IN ({})) }}", concept_list(&FIDELITY_CONCEPTS)),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "rebuildDrift concept outside RebuildDriftScheme",
        format!("{prefix}SELECT (COUNT(?v) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildDrift ?v }} FILTER(?v NOT IN ({})) }}", concept_list(&DRIFT_CONCEPTS)),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "hasUpstreamCounterpart=false assessment(s) also carrying fidelity/drift/baselines/ambiguousCandidate",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:hasUpstreamCounterpart false . {{ ?a pkg:rebuildFidelity ?x }} UNION {{ ?a pkg:rebuildDrift ?x }} UNION {{ ?a pkg:fidelityBaseline ?x }} UNION {{ ?a pkg:comparedAgainst ?x }} UNION {{ ?a pkg:ambiguousCandidate ?x }} }} }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "hasUpstreamCounterpart=true assessment(s) missing rebuildFidelity or rebuildDrift",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:hasUpstreamCounterpart true . FILTER(NOT EXISTS {{ ?a pkg:rebuildFidelity ?f }} || NOT EXISTS {{ ?a pkg:rebuildDrift ?d }}) }} }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "rebuildDrift present without a comparedAgainst baseline",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildDrift ?d . FILTER NOT EXISTS {{ ?a pkg:comparedAgainst ?c }} }} }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "matched fidelity (exact/vendor-patched/modular-equivalent) without a fidelityBaseline",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildFidelity ?f FILTER(?f IN ({})) . FILTER NOT EXISTS {{ ?a pkg:fidelityBaseline ?b }} }} }}", concept_list(&MATCHED_FIDELITY_CONCEPTS)),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "fidelity-unknown assessment(s) carrying a fidelityBaseline",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildFidelity pkg:fidelity-unknown ; pkg:fidelityBaseline ?b }} }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "ambiguousCandidate present without rebuildFidelity = fidelity-unknown",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:ambiguousCandidate ?c . FILTER NOT EXISTS {{ ?a pkg:rebuildFidelity pkg:fidelity-unknown }} }} }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "ambiguousCandidate present with fewer than two distinct candidates",
        format!("{prefix}SELECT (COUNT(?a) AS ?c) WHERE {{ SELECT ?a (COUNT(DISTINCT ?c) AS ?n) WHERE {{ GRAPH <{staging}> {{ ?a pkg:ambiguousCandidate ?c }} }} GROUP BY ?a HAVING(?n < 2) }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "self-baseline: assessmentOf target equals its own fidelityBaseline or comparedAgainst",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessmentOf ?p . {{ ?a pkg:fidelityBaseline ?p }} UNION {{ ?a pkg:comparedAgainst ?p }} }} }}"),
        StagingCheckKind::MustBeZero);

    // Unexpected baseline/candidate objects: every fidelityBaseline/
    // comparedAgainst/ambiguousCandidate must be a real SourcePackage that
    // exists in one of this run's configured upstream graphs.
    let graph_membership = rhel_graphs
        .iter()
        .map(|g| format!("{{ GRAPH <{g}> {{ ?b a pkg:SourcePackage }} }}"))
        .collect::<Vec<_>>()
        .join(" UNION ");
    push(&mut checks, "unexpected baseline/candidate object not found as a SourcePackage in a configured upstream graph",
        format!("{prefix}SELECT (COUNT(DISTINCT ?b) AS ?c) WHERE {{ GRAPH <{staging}> {{ {{ ?a pkg:fidelityBaseline ?b }} UNION {{ ?a pkg:comparedAgainst ?b }} UNION {{ ?a pkg:ambiguousCandidate ?b }} }} FILTER NOT EXISTS {{ {graph_membership} }} }}"),
        StagingCheckKind::MustBeZero);

    push(&mut checks, "assessedAgainstSnapshot value missing rdf:type pkg:DataSnapshot",
        format!("{prefix}SELECT (COUNT(DISTINCT ?s) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessedAgainstSnapshot ?s }} FILTER NOT EXISTS {{ ?s a pkg:DataSnapshot }} }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "assessedAgainstSnapshot value missing rdfs:label",
        format!("PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>\n{prefix}SELECT (COUNT(DISTINCT ?s) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessedAgainstSnapshot ?s }} FILTER NOT EXISTS {{ ?s rdfs:label ?l }} }}"),
        StagingCheckKind::MustBeZero);

    push(&mut checks, "pkg:rebuildOf triple present (this deriver never promotes lineage)",
        format!("{prefix}SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?s pkg:rebuildOf ?o }} }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "lineageConfirmed=true assessment(s) (this deriver never promotes lineage)",
        format!("{prefix}SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:lineageConfirmed true }} }}"),
        StagingCheckKind::MustBeZero);
    push(&mut checks, "lineageEvidence triple present (this deriver never promotes lineage)",
        format!("{prefix}SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:lineageEvidence ?e }} }}"),
        StagingCheckKind::MustBeZero);

    checks
}

/// The exact set of pkg:RebuildAssessment count and set-equality queries that
/// need a caller-supplied expectation rather than a fixed zero.
pub(crate) fn assessment_count_query(staging: &str) -> String {
    format!("PREFIX pkg: <{PKG}>\nSELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a a pkg:RebuildAssessment }} }}")
}

pub(crate) fn distribution_count_query(staging: &str) -> String {
    format!("PREFIX pkg: <{PKG}>\nSELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:derivedFromDistribution ?b }} }}")
}

pub(crate) fn assessment_subjects_query(staging: &str) -> String {
    format!("PREFIX pkg: <{PKG}>\nSELECT DISTINCT ?s WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessmentOf ?s }} }}")
}

pub(crate) fn snapshot_subjects_query(staging: &str) -> String {
    format!("PREFIX pkg: <{PKG}>\nSELECT DISTINCT ?s WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessedAgainstSnapshot ?s }} }}")
}
```

- [ ] **Step 4: Rewrite `validate_staging` and `load_atomic`**

First, add this as a new **top-level** struct — same nesting level as `Pair`/`RebuildReport`/`PairData`, declared *before* `pub struct RebuildComparisonDeriver { ... }`, **not** inside its `impl` block (it has no `&self`, so it cannot be a method):

```rust
/// Everything `load_atomic` needs to know about what THIS run's `derive()`
/// produced, so staging can be checked against it before the swap. Bundled
/// into a struct because the list of expectations has grown past what reads
/// cleanly as positional arguments.
pub struct LoadExpectations<'a> {
    pub assessments: usize,
    pub distribution_count: usize,
    pub subjects: &'a BTreeSet<String>,
    pub snapshots: &'a BTreeSet<String>,
    pub rhel_graphs: &'a [String],
}
```

Then, **inside** `impl RebuildComparisonDeriver` (leave `atomic_swap_update` and `valid_run_token` — both free functions declared just outside this `impl` block — exactly as they were; do not move or edit them), replace the old `validate_staging`/`load_atomic` methods with:

```rust
fn staging_count(&self, query: &str) -> Result<usize> {
    let rows = self.sparql.query(query)?;
    rows.first()
        .and_then(|r| r.get("c"))
        .and_then(|v| v.parse::<usize>().ok())
        .ok_or_else(|| err("staging validation query returned no numeric ?c binding".into()))
}

fn staging_subjects(&self, query: &str) -> Result<BTreeSet<String>> {
    let rows = self.sparql.query(query)?;
    Ok(rows.iter().filter_map(|r| r.get("s").cloned()).collect())
}

/// Fail-closed structural validation of the STAGING graph, run BEFORE the
/// swap. Not a substitute for running the ontology's actual SHACL shapes (out
/// of scope for this deriver) -- a targeted, enumerated set of checks against
/// the highest-risk classes of staging corruption: truncation, malformed
/// literals, axis-coupling violations, contamination/substitution, and stray
/// snapshot/baseline references.
fn validate_staging(&self, staging: &str, expect: &LoadExpectations) -> Result<()> {
    let assessments = self.staging_count(&assessment_count_query(staging))?;
    if assessments != expect.assessments {
        return Err(err(format!(
            "staging graph has {assessments} pkg:RebuildAssessment nodes but derive emitted {} (truncation/partial-load guard); aborting, prod untouched",
            expect.assessments
        )));
    }
    let distribution = self.staging_count(&distribution_count_query(staging))?;
    if distribution != expect.distribution_count {
        return Err(err(format!(
            "staging graph has {distribution} pkg:derivedFromDistribution triples but derive emitted {} (distribution-coverage guard); aborting, prod untouched",
            expect.distribution_count
        )));
    }

    for check in staging_checks(staging, expect.rhel_graphs) {
        let n = self.staging_count(&check.query)?;
        let ok = match check.kind {
            StagingCheckKind::MustBeZero => n == 0,
            StagingCheckKind::MustEqual(expected) => n == expected,
        };
        if !ok {
            return Err(err(format!(
                "staging graph failed check ({n}): {}; aborting, prod untouched",
                check.description
            )));
        }
    }

    let actual_subjects = self.staging_subjects(&assessment_subjects_query(staging))?;
    if actual_subjects != *expect.subjects {
        let unexpected: Vec<&String> = actual_subjects.difference(expect.subjects).take(5).collect();
        let missing: Vec<&String> = expect.subjects.difference(&actual_subjects).take(5).collect();
        return Err(err(format!(
            "staging assessmentOf subjects do not match the set derive emitted ({} in staging vs {} expected); unexpected: {:?}; missing: {:?}; aborting, prod untouched",
            actual_subjects.len(), expect.subjects.len(), unexpected, missing
        )));
    }

    let actual_snapshots = self.staging_subjects(&snapshot_subjects_query(staging))?;
    if actual_snapshots != *expect.snapshots {
        let unexpected: Vec<&String> = actual_snapshots.difference(expect.snapshots).take(5).collect();
        let missing: Vec<&String> = expect.snapshots.difference(&actual_snapshots).take(5).collect();
        return Err(err(format!(
            "staging assessedAgainstSnapshot values do not match the set derive emitted ({} in staging vs {} expected); unexpected: {:?}; missing: {:?}; aborting, prod untouched",
            actual_snapshots.len(), expect.snapshots.len(), unexpected, missing
        )));
    }

    Ok(())
}

/// Load `nt_path` into a per-run staging graph, run fail-closed structural
/// validation against the STAGING graph, then atomically swap it into
/// `prod_graph`. Prod is left untouched unless staging passed every check and
/// the swap succeeded; staging is dropped on every failure path (no orphans).
pub fn load_atomic(
    &self,
    nt_path: &str,
    prod_graph: &str,
    run_token: &str,
    expect: LoadExpectations,
) -> Result<()> {
    if !valid_run_token(run_token) {
        return Err(err(format!(
            "run token {run_token:?} is invalid; must match ^[A-Za-z0-9._-]+$"
        )));
    }
    let staging = format!("{prod_graph}-staging-{run_token}");
    self.sparql.drop_graph(&staging)?;
    if let Err(e) = self.sparql.load_file(nt_path, &staging, 10_000) {
        self.sparql.drop_graph(&staging).ok();
        return Err(e);
    }
    if let Err(e) = self.validate_staging(&staging, &expect) {
        self.sparql.drop_graph(&staging).ok();
        return Err(e);
    }
    match self.sparql.update(&atomic_swap_update(prod_graph, &staging)) {
        Ok(()) => Ok(()),
        Err(e) => {
            self.sparql.drop_graph(&staging).ok();
            Err(e)
        }
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison 2>&1 | tail -40`
Expected: the whole `derive_comparison` module now compiles and all its tests pass (this is the first point since Task 2 where the full file compiles cleanly — Tasks 2–3 intentionally left it non-compiling as a whole).

- [ ] **Step 6: Run the full workspace suite**

Run: `cd etl/pg-collect && cargo build --lib 2>&1 | grep -c warning` (record the baseline count) then `cargo test 2>&1 | grep -E "test result|FAILED|error\["`.
Expected: green, and warning count is unchanged from before this task started (`main.rs` will still fail to compile at this point, since Task 5 hasn't updated its call sites yet — if `cargo test` fails to build the `pg-collect` binary target, that is expected here; confirm at minimum that `cargo test --lib` and `cargo check --lib` succeed).

- [ ] **Step 7: Commit**

```bash
git add etl/pg-collect/src/derive_comparison.rs
git commit -m "feat(deriver): rewrite staging validation for the reified shape

Replaces the flat model's ~13 checks with ~24 fail-closed checks against the
RebuildAssessment shape: concept-scheme allowlists, datatype correctness,
full max-cardinality, axis-coupling rules, self-baseline rejection,
unexpected baseline/candidate objects (live-joined against the configured
upstream graphs), and DataSnapshot shape/set-equality. load_atomic's growing
parameter list is bundled into LoadExpectations."
```

---

### Task 5: CLI wiring (`main.rs`)

**Files:**
- Modify: `etl/pg-collect/src/main.rs:2494-2532` (the `Commands::DeriveRebuildComparison` handler)

**Interfaces:**
- Consumes: `RebuildComparisonDeriver::{derive, load_atomic, check_ontology_terms, check_readiness}` (Tasks 3–4), `LoadExpectations` (Task 4), `RebuildReport`'s new fields (Task 2).

- [ ] **Step 1: Update the handler**

The CLI flags themselves (`DeriveRebuildComparison`'s struct fields at `main.rs:1140-1159`) do not change. Replace the handler body (currently `main.rs:2494-2532`) with:

```rust
Commands::DeriveRebuildComparison { endpoint, output, pairs, min_sources, run_token, load } => {
    use pg_collect::derive_comparison::{parse_pair, LoadExpectations, Pair, RebuildComparisonDeriver};
    eprintln!("=== PackageGraph RHEL Rebuild Assessment Deriver ===");

    (|| -> std::io::Result<(usize, usize)> {
        let base = "https://packagegraph.github.io/graph";
        let pairs: Vec<Pair> = if pairs.is_empty() {
            ["almalinux/9=rhel/9", "almalinux/10=rhel/10", "rocky/9=rhel/9", "rocky/10=rhel/10"]
                .iter()
                .map(|p| { let (a, b) = p.split_once('=').unwrap();
                    parse_pair(&format!("{base}/{a}={base}/{b}")).unwrap() })
                .collect()
        } else {
            pairs.iter().map(|p| parse_pair(p)).collect::<std::io::Result<_>>()?
        };
        let deriver = RebuildComparisonDeriver::new(&endpoint);
        deriver.check_ontology_terms()?;
        deriver.check_readiness(&pairs, min_sources)?;

        let assessed_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let report = deriver.derive(&output, &pairs, &run_token, &assessed_at)?;

        eprintln!("Pairs: {}  Assessments: {}  Triples: {}", report.pairs, report.assessments, report.triples);
        eprintln!("  presence: true={} false={}", report.presence_true, report.presence_false);
        for (concept, n) in &report.fidelity_counts { eprintln!("  fidelity {concept}: {n}"); }
        for (concept, n) in &report.drift_counts { eprintln!("  drift {concept}: {n}"); }
        eprintln!("  ambiguous: {}", report.ambiguous_count);

        let prod = "https://packagegraph.github.io/graph/derived/rhel-rebuilds";
        if load {
            let mut rhel_graphs: Vec<String> = pairs.iter().map(|p| p.rhel_graph.clone()).collect();
            rhel_graphs.sort();
            rhel_graphs.dedup();
            deriver.load_atomic(
                &output,
                prod,
                &run_token,
                LoadExpectations {
                    assessments: report.assessments,
                    distribution_count: report.distribution_count,
                    subjects: &report.assessment_subjects,
                    snapshots: &report.snapshot_subjects,
                    rhel_graphs: &rhel_graphs,
                },
            )?;
            eprintln!("Loaded into {prod}");
        }
        Ok((report.pairs, report.triples))
    })()
}
```

- [ ] **Step 2: Build and run the full suite**

Run: `cd etl/pg-collect && cargo build 2>&1 | tail -20`
Expected: clean build, no new warnings.

Run: `cargo test 2>&1 | grep -E "test result|FAILED|error\["`
Expected: every suite green.

- [ ] **Step 3: Sanity-check the CLI**

Run: `./target/debug/pg-collect derive-rebuild-comparison --help 2>&1 | head -20`
Expected: unchanged flag list (`--endpoint`, `--output`/`-o`, `--pair`, `--min-sources`, `--run-token`, `--load`).

- [ ] **Step 4: Commit**

```bash
git add etl/pg-collect/src/main.rs
git commit -m "feat(cli): wire assess()-based reporting and LoadExpectations

assessed_at is captured once per invocation via chrono and threaded into
derive(); the --load path builds LoadExpectations (including the distinct
upstream graph list) instead of five loose arguments."
```

---

### Task 6: Deploy job comment

**Files:**
- Modify: `deploy/overlays/dev/jobs/derive-rhel-rebuilds.yaml:7-9`

**Interfaces:** none (comment-only change).

- [ ] **Step 1: Update the prerequisite comment**

The current comment (lines 7-9) reads:
```yaml
  # Prerequisites before unsuspending: ontology v0.13.0 TBox loaded (rebuild
  # ...
  # graphs exist. Unsuspend (set suspend: false / remove) once BOTH are in place.
```
Replace with:
```yaml
  # Prerequisites before unsuspending: ontology PR #5 (v0.13.0, the reified
  # RebuildAssessment model) merged and its TBox loaded, AND the Alma/Rocky/RHEL
  # collect jobs exist in this overlay. The deriver's own gates fail closed if
  # either is missing (check_ontology_terms / check_readiness), so a premature
  # unsuspend degrades to a clean job failure, never bad data. Unsuspend (set
  # suspend: false / remove) once BOTH are in place.
```

- [ ] **Step 2: Validate the manifest**

Run: `cd deploy/overlays/dev && kubectl kustomize . >/dev/null && echo OK` (or `kustomize build .` if the `kustomize` binary is present instead)
Expected: `OK`.

- [ ] **Step 3: Commit**

```bash
git add deploy/overlays/dev/jobs/derive-rhel-rebuilds.yaml
git commit -m "docs(deploy): update rebuild-deriver prerequisite comment for PR #5"
```

---

## Self-Review

**Spec coverage:**
- §3 algorithm (method id, canonical EVR equality, ambiguity-not-tie-break, drift computation unaffected) → Task 1. ✓
- §4 confidence table (1.0/0.9/0.6/0.7/0.5) → Task 1's constants, tested directly. ✓
- §5.1/§5.2 IRI schemes (run-scoped assessment + snapshot IRIs, write_*_once dedup) → Task 2, tested in `dedup_writer_collapses_repeated_snapshot_definition`. ✓
- §5.3 timestamp captured once per invocation, threaded not generated in pure code → Task 5 (chrono in the CLI handler) feeding Task 2's `build_report(..., assessed_at)`. ✓
- §5.4 four emission cases (A/B/C/D) → Task 2's `emit_assessment`, exercised by `build_report_end_to_end_case_coverage`. ✓
- §6 code structure (rebuild_classify.rs rewrite, derive_comparison.rs rewrite, sparql.rs/rpmver.rs/rebuild_norm.rs untouched, self-baseline guard, new term list) → Tasks 1-3. ✓
- §6.1 nineteen-item staging validation, explicitly scoped (not SHACL-equivalent) → Task 4. ✓
- §7 unchanged mechanics (atomic_swap_update, valid_run_token, check_readiness) → explicitly preserved verbatim in Task 4's instructions. ✓
- §9 ontology feedback item (already-resolved status) → no code task; this is a communication action for you, not the implementer — flagging here so it isn't lost: raise remaining item 1 (the snapshot-reproducibility trap) on PR #5 once this plan lands.
- Non-goals (rebuildOf never emitted, module:stream out of scope, no SHACL engine) → enforced by Task 4's checks 17-18 and the explicit scope statement in `validate_staging`'s doc comment. ✓

**Placeholder scan:** no TBD/TODO; every step has complete code, not descriptions of code. The one deliberately-vague instruction ("re-read the file before deleting" in Task 4 Step 3) is flagged because Tasks 2-3 shift line numbers ahead of Task 4 and a stale line-range would silently delete the wrong span — the implementer is told exactly what to preserve (`atomic_swap_update`, `valid_run_token`) as the anchor, not given a placeholder.

**Type consistency:** `Assessment`/`FidelityOutcome`/`DriftOutcome` (Task 1) match their use in Task 2's `emit_assessment`/`build_report` exactly (field names, `Option` wrapping). `RebuildReport`'s fields (Task 2) match Task 5's CLI reporting loop and Task 4's `LoadExpectations` construction. `PairData.rhel_graph` (added in Task 2) is populated in Task 3's rewritten `derive()`. `LoadExpectations` (Task 4) matches Task 5's construction site field-for-field. `REBUILD_TERMS` (Task 3) and `staging_checks`/`FIDELITY_CONCEPTS`/`DRIFT_CONCEPTS` (Task 4) reference the same ontology term spelling used in Task 2's `emit_assessment` (`rebuildFidelity`, `rebuildDrift`, `fidelityBaseline`, `comparedAgainst`, `assessedAgainstSnapshot`, `hasUpstreamCounterpart`, `lineageConfirmed`, `assessmentConfidence`, `assessmentMethod`, `assessedAt`, `assessmentOf`).
