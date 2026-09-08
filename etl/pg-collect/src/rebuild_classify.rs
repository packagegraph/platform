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
        .filter(|u| canonical_evr_eq(rb_epoch, rb_nvr_normalized, u.epoch, &upstream_normalize(&u.nvr())))
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
