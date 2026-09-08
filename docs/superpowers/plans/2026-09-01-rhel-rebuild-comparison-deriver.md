# RHEL Rebuild Comparison Deriver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Materialize, for each rebuild source-package name, its rebuild lineage and a fidelity status (exact / vendor-patched / modular-equivalent / ahead / behind / equivalent-version / exclusive) versus RHEL into a derived graph.

**Architecture:** A new `pg-collect derive-rebuild-comparison` subcommand mirrors the existing `DerivePackageHistory` deriver: it reads source packages from paired rebuild/RHEL named graphs via SPARQL, classifies each rebuild name's newest build in pure Rust (epoch-aware `rpmvercmp` + anchored normalization), emits N-Triples, and stages them into `graph/derived/rhel-rebuilds` via an atomic replace. New ontology terms land first in the separate `packagegraph/ontology` repo (v0.13.0); the deriver gates on their presence.

**Tech Stack:** Rust (`etl/pg-collect`), Apache Jena Fuseki (SPARQL 1.1 Update), Turtle/SKOS ontology, Kustomize deploy manifests.

**Spec:** `docs/superpowers/specs/2026-09-01-rhel-rebuild-comparison-deriver-design.md` (rev 4)

## Global Constraints

- **Ontology is a separate repo** at `/home/bharring/Projects/packagegraph/ontology`; changes there are committed in that repo. Version bump v0.12.0 → **v0.13.0**.
- **Ontology namespace prefixes:** `pkg:`/`:` = `https://purl.org/packagegraph/ontology/core#`; `rpm:` = `https://purl.org/packagegraph/ontology/rpm#`; `skos:` = `http://www.w3.org/2004/02/skos/core#`.
- **Derived graph URI:** `https://packagegraph.github.io/graph/derived/rhel-rebuilds`.
- **Default pairs (four release-level, over six graphs):** `almalinux/9=rhel/9`, `almalinux/10=rhel/10`, `rocky/9=rhel/9`, `rocky/10=rhel/10`, under base `https://packagegraph.github.io/graph/`.
- **No AI attribution in commit messages.**
- **TDD:** every code change starts with a failing test. Rust tests: `cargo test` from `etl/pg-collect`. Ontology validation: `make lint validate` from the ontology repo.
- **`:rebuildOf` is asserted ONLY for exact/vendor-patched/modular-equivalent; ahead/behind/equivalent-version use `:comparedAgainst`; exclusive gets neither.**

---

## Phase 1 — Ontology (packagegraph/ontology → v0.13.0)

### Task 1: Add rebuild vocabulary to core ontology

**Files:**
- Modify: `/home/bharring/Projects/packagegraph/ontology/core/core.ttl`
- Modify: `/home/bharring/Projects/packagegraph/ontology/core/skos-schemes.ttl`

**Interfaces:**
- Produces: ontology terms `:rebuildOf`, `:comparedAgainst`, `:rebuildTrackingStatus`, and `pkg:RebuildTrackingScheme` with concepts `pkg:track-exact`, `pkg:track-vendor-patched`, `pkg:track-modular-equivalent`, `pkg:track-ahead`, `pkg:track-behind`, `pkg:track-equivalent-version`, `pkg:track-exclusive`. These IRIs are consumed by the deriver's gate (Task 8) and emit logic (Task 7).

- [ ] **Step 1: Add the three object properties to `core.ttl`**

Insert alphabetically near the existing `:derivedFrom` block:

```turtle
:rebuildOf a owl:ObjectProperty ;
    rdfs:label "rebuild of"@en ;
    IAO:0000115 "Links a source package in a downstream rebuild distribution to the specific upstream source package it reproduces (e.g. an AlmaLinux SRPM to the RHEL SRPM it was rebuilt from). Directional build-lineage; asserted only when the downstream NVR matches an upstream build exactly or after documented normalization."@en ;
    rdfs:comment "This source package is a rebuild of a specific upstream source package"@en ;
    rdfs:domain :SourcePackage ;
    rdfs:range :SourcePackage ;
    rdfs:subPropertyOf prov:wasDerivedFrom ;
    rdfs:isDefinedBy : .

:comparedAgainst a owl:ObjectProperty ;
    rdfs:label "compared against"@en ;
    IAO:0000115 "Records the upstream source package used as the comparison baseline when classifying a rebuild build whose NVR does NOT match any upstream build under normalization (ahead / behind / equivalent-version). This is a comparison reference, NOT a provenance/lineage claim."@en ;
    rdfs:comment "Upstream baseline used for an ahead/behind/equivalent-version classification; not a lineage claim"@en ;
    rdfs:domain :SourcePackage ;
    rdfs:range :SourcePackage ;
    rdfs:isDefinedBy : .

:rebuildTrackingStatus a owl:ObjectProperty ;
    rdfs:label "rebuild tracking status"@en ;
    IAO:0000115 "Classifies how faithfully a rebuild source package's current build tracks its upstream (RHEL). Values are SKOS concepts from the RebuildTrackingScheme. Computed post-collection by comparing source NVRs across distribution graphs with epoch-aware RPM version comparison and documented normalization."@en ;
    rdfs:comment "How faithfully this rebuild source package's current build tracks upstream"@en ;
    rdfs:domain :SourcePackage ;
    rdfs:range skos:Concept ;
    rdfs:isDefinedBy : .
```

- [ ] **Step 2: Add the SKOS scheme to `skos-schemes.ttl`**

Append (mirrors `FreshnessStatusScheme` exactly):

```turtle
pkg:track-exact a skos:Concept ;
    skos:definition "The rebuild's current build NVR is byte-identical to an upstream build." ;
    skos:inScheme pkg:RebuildTrackingScheme ;
    skos:prefLabel "exact" .

pkg:track-vendor-patched a skos:Concept ;
    skos:definition "The rebuild's NVR equals an upstream build's NVR after stripping an anchored vendor suffix (e.g. .rocky.N, .alma.N) — a provable rebuild of that SRPM with a vendor tag." ;
    skos:inScheme pkg:RebuildTrackingScheme ;
    skos:prefLabel "vendor-patched" .

pkg:track-modular-equivalent a skos:Concept ;
    skos:definition "Modular package whose base NVR (before the .module marker) matches upstream; only the module build-context differs (.module+el9 vs .module_el9 and regenerated build-ids)." ;
    skos:inScheme pkg:RebuildTrackingScheme ;
    skos:prefLabel "modular-equivalent" .

pkg:track-ahead a skos:Concept ;
    skos:definition "No NVR match; the rebuild's current build has a higher RPM EVR than the upstream newest build (rebuild leads the collected snapshot)." ;
    skos:inScheme pkg:RebuildTrackingScheme ;
    skos:prefLabel "ahead" .

pkg:track-behind a skos:Concept ;
    skos:definition "No NVR match; the rebuild's current build has a lower RPM EVR than the upstream newest build (rebuild lags upstream — investigate)." ;
    skos:inScheme pkg:RebuildTrackingScheme ;
    skos:prefLabel "behind" .

pkg:track-equivalent-version a skos:Concept ;
    skos:definition "No NVR match under normalization, but rpmvercmp reports the rebuild's EVR equal to an upstream build's EVR (e.g. from ignored separators or leading zeros). Recorded as a comparison, not a lineage claim." ;
    skos:inScheme pkg:RebuildTrackingScheme ;
    skos:prefLabel "equivalent-version" .

pkg:track-exclusive a skos:Concept ;
    skos:definition "The source-package name is absent from upstream (branding or distribution-specific extras)." ;
    skos:inScheme pkg:RebuildTrackingScheme ;
    skos:prefLabel "exclusive" .

pkg:RebuildTrackingScheme a skos:ConceptScheme ;
    rdfs:label "RHEL Rebuild Tracking Status"@en ;
    dcterms:description "Classification of how faithfully a downstream rebuild source package tracks its upstream (RHEL) build." ;
    skos:hasTopConcept pkg:track-exact,
        pkg:track-vendor-patched,
        pkg:track-modular-equivalent,
        pkg:track-ahead,
        pkg:track-behind,
        pkg:track-equivalent-version,
        pkg:track-exclusive .
```

- [ ] **Step 3: Validate the ontology**

Run: `cd /home/bharring/Projects/packagegraph/ontology && make lint validate`
Expected: PASS (Turtle parses, SHACL/consistency checks green). If `make lint` flags an undeclared prefix (`dcterms:`, `prov:`), confirm it is already declared in the file header (it is used by existing terms); fix only if the linter reports it.

- [ ] **Step 4: Commit**

```bash
cd /home/bharring/Projects/packagegraph/ontology
git add core/core.ttl core/skos-schemes.ttl
git commit -m "feat(core): add rebuild lineage + tracking-status vocabulary

Add :rebuildOf (prov lineage), :comparedAgainst (non-lineage baseline),
:rebuildTrackingStatus, and the RebuildTrackingScheme SKOS scheme (exact,
vendor-patched, modular-equivalent, ahead, behind, equivalent-version,
exclusive) for downstream RHEL-rebuild fidelity classification."
```

### Task 2: SHACL, examples, changelog, version bump

**Files:**
- Modify: `/home/bharring/Projects/packagegraph/ontology/core/core.shacl.ttl`
- Modify: `/home/bharring/Projects/packagegraph/ontology/core/core.examples.ttl`
- Modify: `/home/bharring/Projects/packagegraph/ontology/CHANGELOG.md`
- Modify: version pin (see Step 3)

**Interfaces:**
- Consumes: terms from Task 1.
- Produces: a validated, versioned v0.13.0 ontology release.

- [ ] **Step 1: Add a SHACL shape constraining `:rebuildTrackingStatus` to the scheme**

Append to `core.shacl.ttl` (mirror the shape used for `:freshnessStatus`; if none exists, use this):

```turtle
pkg:RebuildTrackingStatusShape a sh:NodeShape ;
    sh:targetSubjectsOf pkg:rebuildTrackingStatus ;
    sh:property [
        sh:path pkg:rebuildTrackingStatus ;
        sh:class skos:Concept ;
        sh:in ( pkg:track-exact pkg:track-vendor-patched pkg:track-modular-equivalent
                pkg:track-ahead pkg:track-behind pkg:track-equivalent-version pkg:track-exclusive ) ;
        sh:message "rebuildTrackingStatus must be a concept from RebuildTrackingScheme" ;
    ] .
```

- [ ] **Step 2: Add an example to `core.examples.ttl`**

```turtle
# AlmaLinux 9 openssl current build is an exact rebuild of the RHEL 9 build.
<https://packagegraph.github.io/d/src/almalinux/9/openssl/3.5.5-6.el9_8>
    pkg:rebuildTrackingStatus pkg:track-exact ;
    pkg:rebuildOf <https://packagegraph.github.io/d/src/rhel/9/openssl/3.5.5-6.el9_8> .
```

- [ ] **Step 3: Bump version and update changelog**

Run `cd /home/bharring/Projects/packagegraph/ontology && grep -rn "0.12.0" Makefile *.md packagegraph/ 2>/dev/null` to find the canonical version string(s). Update each to `0.13.0`. Add a `CHANGELOG.md` entry under a new `## v0.13.0` heading:

```markdown
## v0.13.0

### Added
- `core:rebuildOf`, `core:comparedAgainst`, `core:rebuildTrackingStatus`
- `pkg:RebuildTrackingScheme` SKOS scheme (exact, vendor-patched,
  modular-equivalent, ahead, behind, equivalent-version, exclusive)
```

- [ ] **Step 4: Validate (incl. version check)**

Run: `cd /home/bharring/Projects/packagegraph/ontology && make lint validate check-version`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd /home/bharring/Projects/packagegraph/ontology
git add -A
git commit -m "chore: release v0.13.0 (rebuild tracking vocabulary)

SHACL shape + example for rebuildTrackingStatus; bump to v0.13.0."
```

---

## Phase 2 — Platform pure logic (TDD, no Fuseki)

### Task 3: RPM version comparison (`rpmvercmp` + EVR)

**Files:**
- Create: `etl/pg-collect/src/rpmver.rs`
- Modify: `etl/pg-collect/src/lib.rs` (add `pub mod rpmver;`)

**Interfaces:**
- Produces: `pub fn rpmvercmp(a: &str, b: &str) -> std::cmp::Ordering` and `pub fn evr_cmp(ea: i64, va: &str, ra: &str, eb: i64, vb: &str, rb: &str) -> std::cmp::Ordering`. Consumed by Tasks 5 and 7.

- [ ] **Step 1: Write failing tests (canonical RPM vectors)**

Add to `src/rpmver.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn rpmvercmp_canonical_vectors() {
        assert_eq!(rpmvercmp("1.0", "1.0"), Equal);
        assert_eq!(rpmvercmp("1.0", "2.0"), Less);
        assert_eq!(rpmvercmp("2.0", "1.0"), Greater);
        assert_eq!(rpmvercmp("1.0.1", "1.0"), Greater);
        assert_eq!(rpmvercmp("1.0", "1.0.1"), Less);
        // leading zeros / numeric-length rules
        assert_eq!(rpmvercmp("1.0010", "1.9"), Greater);
        assert_eq!(rpmvercmp("1.05", "1.5"), Equal);
        // numeric segment newer than alpha
        assert_eq!(rpmvercmp("1.0", "1.0a"), Greater);
        assert_eq!(rpmvercmp("5.5p1", "5.5p2"), Less);
        // tilde sorts before everything
        assert_eq!(rpmvercmp("1.0~rc1", "1.0"), Less);
        assert_eq!(rpmvercmp("1.0~rc1", "1.0~rc2"), Less);
        // caret sorts after
        assert_eq!(rpmvercmp("1.0^", "1.0"), Greater);
        // real-world el9 releases
        assert_eq!(rpmvercmp("9.el9", "6.el9_1"), Greater);
    }

    #[test]
    fn evr_cmp_epoch_dominates() {
        assert_eq!(evr_cmp(1, "1.0", "1", 0, "2.0", "1"), Greater);
        assert_eq!(evr_cmp(0, "1.0", "1", 0, "1.0", "2"), Less);
        assert_eq!(evr_cmp(0, "3.5.5", "6.el9_8", 0, "3.5.5", "6.el9_8"), Equal);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd etl/pg-collect && cargo test --lib rpmver 2>&1 | tail`
Expected: FAIL (module/functions not defined).

- [ ] **Step 3: Implement `rpmvercmp` + `evr_cmp`**

```rust
//! RPM version/release comparison (rpmvercmp) and epoch-aware EVR comparison.
use std::cmp::Ordering;

fn strip_leading_zeros(s: &[u8]) -> &[u8] {
    let mut i = 0;
    while i + 1 < s.len() && s[i] == b'0' { i += 1; }
    &s[i..]
}

/// Compare two RPM version (or release) strings per RPM's rpmvercmp rules.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b { return Ordering::Equal; }
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    let is_sep = |c: u8| !c.is_ascii_alphanumeric() && c != b'~' && c != b'^';
    loop {
        while !a.is_empty() && is_sep(a[0]) { a = &a[1..]; }
        while !b.is_empty() && is_sep(b[0]) { b = &b[1..]; }

        // tilde: older than anything, including empty
        if a.first() == Some(&b'~') || b.first() == Some(&b'~') {
            if a.first() != Some(&b'~') { return Ordering::Greater; }
            if b.first() != Some(&b'~') { return Ordering::Less; }
            a = &a[1..]; b = &b[1..]; continue;
        }
        // caret: newer than the string ending, older than a following segment
        if a.first() == Some(&b'^') || b.first() == Some(&b'^') {
            if a.is_empty() { return Ordering::Less; }
            if b.is_empty() { return Ordering::Greater; }
            if a.first() != Some(&b'^') { return Ordering::Greater; }
            if b.first() != Some(&b'^') { return Ordering::Less; }
            a = &a[1..]; b = &b[1..]; continue;
        }

        if a.is_empty() || b.is_empty() { break; }

        let isnum = a[0].is_ascii_digit();
        let take = |s: &[u8], num: bool| -> usize {
            s.iter().position(|&c| if num { !c.is_ascii_digit() } else { !c.is_ascii_alphabetic() })
                .unwrap_or(s.len())
        };
        let na = take(a, isnum);
        let (seg_a, rest_a) = a.split_at(na);
        let nb = take(b, isnum);
        let (seg_b, rest_b) = b.split_at(nb);

        // a is num, b starts alpha (nb==0 under num rule) => numeric newer
        if isnum && seg_b.is_empty() { return Ordering::Greater; }
        // a is alpha, b starts num => numeric(b) newer => a older
        if !isnum && seg_b.is_empty() { return Ordering::Less; }

        let ord = if isnum {
            let (sa, sb) = (strip_leading_zeros(seg_a), strip_leading_zeros(seg_b));
            if sa.len() != sb.len() { sa.len().cmp(&sb.len()) } else { sa.cmp(sb) }
        } else {
            seg_a.cmp(seg_b)
        };
        if ord != Ordering::Equal { return ord; }
        a = rest_a; b = rest_b;
    }
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => unreachable!(),
    }
}

/// Epoch-aware EVR comparison.
pub fn evr_cmp(ea: i64, va: &str, ra: &str, eb: i64, vb: &str, rb: &str) -> Ordering {
    match ea.cmp(&eb) {
        Ordering::Equal => match rpmvercmp(va, vb) {
            Ordering::Equal => rpmvercmp(ra, rb),
            o => o,
        },
        o => o,
    }
}
```

Add `pub mod rpmver;` to `src/lib.rs`.

- [ ] **Step 4: Run tests to verify pass**

Run: `cd etl/pg-collect && cargo test --lib rpmver 2>&1 | tail`
Expected: PASS. If any canonical vector fails, fix `rpmvercmp` (do not change the test).

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/rpmver.rs etl/pg-collect/src/lib.rs
git commit -m "feat(rpm): add rpmvercmp and epoch-aware EVR comparison"
```

### Task 4: NVR normalization (`strip_vendor`, `module_base`)

**Files:**
- Create: `etl/pg-collect/src/rebuild_norm.rs`
- Modify: `etl/pg-collect/src/lib.rs` (add `pub mod rebuild_norm;`)
- Modify: `etl/pg-collect/Cargo.toml` only if `regex` is not already a dependency (it is used in `rpm.rs`, so it is present — verify with `grep '^regex' etl/pg-collect/Cargo.toml`).

**Interfaces:**
- Produces: `pub fn strip_vendor(nvr: &str) -> String` and `pub fn module_base(nvr: &str) -> String`. Consumed by Task 5.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_vendor_removes_only_anchored_suffixes() {
        assert_eq!(strip_vendor("24.4-8.el9.rocky.0.1"), "24.4-8.el9");
        assert_eq!(strip_vendor("34.25.7.14-1.el9.rocky.0.6"), "34.25.7.14-1.el9");
        assert_eq!(strip_vendor("3.0.7-27.el9.alma.1"), "3.0.7-27.el9");
        // must NOT strip a legitimate release component
        assert_eq!(strip_vendor("11-13.el9.0.1"), "11-13.el9.0.1");
        assert_eq!(strip_vendor("3.5.5-6.el9_8"), "3.5.5-6.el9_8");
    }

    #[test]
    fn module_base_truncates_at_module_marker() {
        assert_eq!(module_base("1.9.0-4.module+el9.8.0+24092+eb9f67d0"), "1.9.0-4");
        assert_eq!(module_base("1.9.0-4.module_el9.6.0+148+fb6dc857"), "1.9.0-4");
        assert_eq!(module_base("3.5.5-6.el9_8"), "3.5.5-6.el9_8");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd etl/pg-collect && cargo test --lib rebuild_norm 2>&1 | tail`
Expected: FAIL (functions not defined).

- [ ] **Step 3: Implement**

```rust
//! Anchored NVR normalization for rebuild-fidelity classification.
use once_cell::sync::Lazy;
use regex::Regex;

// Anchored vendor suffixes observed in Alma/Rocky (see spike report).
// Only strip a vendor token (.rocky/.alma) optionally followed by dotted numbers,
// anchored at end. Never strip a bare trailing .N.M with no vendor token.
static VENDOR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\.(?:rocky|alma)(?:\.\d+)*$").unwrap());

/// Strip an anchored vendor suffix from an NVR's release. Returns the input
/// unchanged when no anchored vendor token is present.
pub fn strip_vendor(nvr: &str) -> String {
    VENDOR_RE.replace(nvr, "").into_owned()
}

/// Truncate an NVR at the first `.module` marker (`.module+el9…` or
/// `.module_el9…`), collapsing vendor-specific module build-context.
pub fn module_base(nvr: &str) -> String {
    match nvr.find(".module") {
        Some(i) => nvr[..i].to_string(),
        None => nvr.to_string(),
    }
}
```

(`once_cell` and `regex` are already dependencies — used in `rpm.rs`.)

- [ ] **Step 4: Run tests to verify pass**

Run: `cd etl/pg-collect && cargo test --lib rebuild_norm 2>&1 | tail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/rebuild_norm.rs etl/pg-collect/src/lib.rs
git commit -m "feat(rpm): add anchored vendor/module NVR normalization"
```

### Task 5: Classifier (pure decision function)

**Files:**
- Create: `etl/pg-collect/src/rebuild_classify.rs`
- Modify: `etl/pg-collect/src/lib.rs` (add `pub mod rebuild_classify;`)

**Interfaces:**
- Consumes: `rpmver::evr_cmp`, `rebuild_norm::{strip_vendor, module_base}`.
- Produces:
  ```rust
  pub struct Build { pub node_uri: String, pub epoch: i64, pub version: String, pub release: String }
  pub enum Link { RebuildOf(String), ComparedAgainst(String), None }
  pub struct Classification { pub status: &'static str, pub link: Link }
  pub fn classify(rebuild_newest: &Build, upstream: &[Build]) -> Classification
  ```
  `status` is one of `"track-exact" | "track-vendor-patched" | "track-modular-equivalent" | "track-ahead" | "track-behind" | "track-equivalent-version" | "track-exclusive"`. Consumed by Task 7.

- [ ] **Step 1: Write failing tests (decision table, real spike examples)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn b(uri: &str, e: i64, v: &str, r: &str) -> Build {
        Build { node_uri: uri.into(), epoch: e, version: v.into(), release: r.into() }
    }

    #[test]
    fn exact_match_asserts_rebuildof() {
        let rb = b("alma:openssl", 0, "3.5.5", "6.el9_8");
        let up = vec![b("rhel:openssl", 0, "3.5.5", "6.el9_8")];
        let c = classify(&rb, &up);
        assert_eq!(c.status, "track-exact");
        assert!(matches!(c.link, Link::RebuildOf(ref u) if u == "rhel:openssl"));
    }

    #[test]
    fn vendor_suffix_is_vendor_patched_with_rebuildof() {
        let rb = b("rocky:cloud-init", 0, "24.4", "8.el9.rocky.0.1");
        let up = vec![b("rhel:cloud-init", 0, "24.4", "8.el9")];
        let c = classify(&rb, &up);
        assert_eq!(c.status, "track-vendor-patched");
        assert!(matches!(c.link, Link::RebuildOf(_)));
    }

    #[test]
    fn modular_build_id_diff_is_modular_equivalent() {
        let rb = b("alma:acl", 0, "1.9.0", "4.module_el9.6.0+148+fb6dc857");
        let up = vec![b("rhel:acl", 0, "1.9.0", "4.module+el9.8.0+24092+eb9f67d0")];
        assert_eq!(classify(&rb, &up).status, "track-modular-equivalent");
    }

    #[test]
    fn lower_evr_is_behind_with_comparedagainst() {
        let rb = b("rk:foo", 0, "1.0", "1.el9");
        let up = vec![b("rhel:foo", 0, "1.2", "1.el9")];
        let c = classify(&rb, &up);
        assert_eq!(c.status, "track-behind");
        assert!(matches!(c.link, Link::ComparedAgainst(_)));
    }

    #[test]
    fn higher_evr_is_ahead() {
        let rb = b("rk:foo", 0, "1.3", "1.el9");
        let up = vec![b("rhel:foo", 0, "1.2", "1.el9")];
        assert_eq!(classify(&rb, &up).status, "track-ahead");
    }

    #[test]
    fn equal_evr_without_nvr_match_is_equivalent_version_not_lineage() {
        // rpmvercmp treats 1.05 == 1.5, but the NVR strings differ and no
        // normalization maps one to the other.
        let rb = b("rk:foo", 0, "1.05", "1.el9");
        let up = vec![b("rhel:foo", 0, "1.5", "1.el9")];
        let c = classify(&rb, &up);
        assert_eq!(c.status, "track-equivalent-version");
        assert!(matches!(c.link, Link::ComparedAgainst(_)));
    }

    #[test]
    fn absent_upstream_is_exclusive() {
        let rb = b("rk:rocky-logos", 0, "90", "1.el9");
        assert_eq!(classify(&rb, &[]).status, "track-exclusive");
    }

    #[test]
    fn deterministic_target_prefers_newest_matching_then_smallest_uri() {
        let rb = b("alma:x", 0, "1.0", "1.el9");
        let up = vec![
            b("rhel:x@b", 0, "1.0", "1.el9"),
            b("rhel:x@a", 0, "1.0", "1.el9"),
        ];
        // exact match on both; newest EVR ties -> smallest URI wins
        let c = classify(&rb, &up);
        assert!(matches!(c.link, Link::RebuildOf(ref u) if u == "rhel:x@a"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd etl/pg-collect && cargo test --lib rebuild_classify 2>&1 | tail`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! Pure rebuild-fidelity classifier: one rebuild build vs the upstream build set.
use crate::rebuild_norm::{module_base, strip_vendor};
use crate::rpmver::evr_cmp;
use std::cmp::Ordering;

pub struct Build {
    pub node_uri: String,
    pub epoch: i64,
    pub version: String,
    pub release: String,
}

impl Build {
    fn nvr(&self) -> String { format!("{}-{}", self.version, self.release) }
}

pub enum Link { RebuildOf(String), ComparedAgainst(String), None }

pub struct Classification { pub status: &'static str, pub link: Link }

/// Deterministic pick: newest by EVR, ties broken by lexicographically smallest URI.
fn pick_newest<'a>(cands: &mut Vec<&'a Build>) -> &'a Build {
    cands.sort_by(|a, b| {
        evr_cmp(b.epoch, &b.version, &b.release, a.epoch, &a.version, &a.release)
            .then_with(|| a.node_uri.cmp(&b.node_uri))
    });
    cands[0]
}

pub fn classify(rb: &Build, upstream: &[Build]) -> Classification {
    if upstream.is_empty() {
        return Classification { status: "track-exclusive", link: Link::None };
    }
    let rb_nvr = rb.nvr();

    // 1. exact
    let mut exact: Vec<&Build> = upstream.iter().filter(|u| u.nvr() == rb_nvr).collect();
    if !exact.is_empty() {
        return Classification { status: "track-exact", link: Link::RebuildOf(pick_newest(&mut exact).node_uri.clone()) };
    }
    // 2. vendor-patched (normalize downstream only, match against unmodified upstream)
    let rb_stripped = strip_vendor(&rb_nvr);
    if rb_stripped != rb_nvr {
        let mut m: Vec<&Build> = upstream.iter().filter(|u| u.nvr() == rb_stripped).collect();
        if !m.is_empty() {
            return Classification { status: "track-vendor-patched", link: Link::RebuildOf(pick_newest(&mut m).node_uri.clone()) };
        }
    }
    // 3. modular-equivalent (truncate both at .module)
    let rb_mod = module_base(&rb_nvr);
    if rb_mod != rb_nvr {
        let mut m: Vec<&Build> = upstream.iter().filter(|u| module_base(&u.nvr()) == rb_mod).collect();
        if !m.is_empty() {
            return Classification { status: "track-modular-equivalent", link: Link::RebuildOf(pick_newest(&mut m).node_uri.clone()) };
        }
    }
    // 4. EVR compare against upstream newest
    let up_newest = pick_newest(&mut upstream.iter().collect());
    let ord = evr_cmp(rb.epoch, &rb.version, &rb.release,
                      up_newest.epoch, &up_newest.version, &up_newest.release);
    let status = match ord {
        Ordering::Greater => "track-ahead",
        Ordering::Less => "track-behind",
        Ordering::Equal => "track-equivalent-version",
    };
    Classification { status, link: Link::ComparedAgainst(up_newest.node_uri.clone()) }
}
```

Add `pub mod rebuild_classify;` to `src/lib.rs`.

- [ ] **Step 4: Run tests to verify pass**

Run: `cd etl/pg-collect && cargo test --lib rebuild_classify 2>&1 | tail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/rebuild_classify.rs etl/pg-collect/src/lib.rs
git commit -m "feat(rpm): add rebuild-fidelity classifier"
```

---

## Phase 3 — Deriver, CLI, deploy

### Task 6: SPARQL access — source builds with epoch, and distribution IRI

**Files:**
- Modify: `etl/pg-collect/src/sparql.rs`
- Test: `etl/pg-collect/src/sparql.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `SparqlClient::query`.
- Produces on `SparqlClient`:
  ```rust
  pub fn query_source_builds(&self, graph: &str)
      -> Result<Vec<(String /*name*/, String /*node_uri*/, i64 /*epoch*/, String /*version*/, String /*release*/)>>;
  pub fn resolve_distribution(&self, graph: &str) -> Result<String>; // errors on 0 or >1
  ```
  Consumed by Tasks 7 and 8.

- [ ] **Step 1: Write failing tests**

Add to `sparql.rs` tests — a query-string builder test (no live Fuseki), asserting the emitted SPARQL contains the required patterns:

```rust
#[test]
fn source_builds_query_joins_epoch_via_binary() {
    let q = super::source_builds_query("https://packagegraph.github.io/graph/rhel/9");
    assert!(q.contains("a pkg:SourcePackage"));
    assert!(q.contains("pkg:packageName"));
    assert!(q.contains("pkg:versionString"));
    assert!(q.contains("pkg:builtFromSource"));   // epoch join through the binary
    assert!(q.contains("OPTIONAL"));              // epoch is optional, defaults 0
    assert!(q.contains("GRAPH <https://packagegraph.github.io/graph/rhel/9>"));
}

#[test]
fn distribution_query_targets_partOfDistribution() {
    let q = super::distribution_query("https://packagegraph.github.io/graph/rhel/9");
    assert!(q.contains("pkg:partOfDistribution"));
    assert!(q.contains("DISTINCT"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd etl/pg-collect && cargo test --lib sparql 2>&1 | tail`
Expected: FAIL (functions not defined).

- [ ] **Step 3: Implement the query builders + client methods**

Add free functions (so they are unit-testable) and thin methods. Version/release come from the source `versionString` (split on the last `-`); epoch is joined from any binary built from the source (`rpm:epoch`, integer), defaulting to 0.

```rust
pub(crate) fn source_builds_query(graph: &str) -> String {
    format!(r#"PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
PREFIX rpm: <https://purl.org/packagegraph/ontology/rpm#>
SELECT ?name ?src (COALESCE(?ep, 0) AS ?epoch) ?ver WHERE {{
  GRAPH <{graph}> {{
    ?src a pkg:SourcePackage ; pkg:packageName ?name ; pkg:hasVersion ?v .
    ?v pkg:versionString ?ver .
    OPTIONAL {{ ?bin pkg:builtFromSource ?src ; rpm:epoch ?ep . }}
  }}
}}"#)
}

pub(crate) fn distribution_query(graph: &str) -> String {
    format!(r#"PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
SELECT DISTINCT ?d WHERE {{ GRAPH <{graph}> {{ ?p pkg:partOfDistribution ?d }} }}"#)
}

impl SparqlClient {
    pub fn query_source_builds(&self, graph: &str)
        -> std::io::Result<Vec<(String, String, i64, String, String)>> {
        let rows = self.query(&source_builds_query(graph))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let (name, src, ver) = match (r.get("name"), r.get("src"), r.get("ver")) {
                (Some(n), Some(s), Some(v)) => (n.clone(), s.clone(), v.clone()),
                _ => continue,
            };
            let epoch = r.get("epoch").and_then(|e| e.parse::<i64>().ok()).unwrap_or(0);
            // split versionString "VERSION-RELEASE" on the last '-'
            let (version, release) = match ver.rfind('-') {
                Some(i) => (ver[..i].to_string(), ver[i + 1..].to_string()),
                None => (ver.clone(), String::new()),
            };
            out.push((name, src, epoch, version, release));
        }
        Ok(out)
    }

    pub fn resolve_distribution(&self, graph: &str) -> std::io::Result<String> {
        let rows = self.query(&distribution_query(graph))?;
        let dists: Vec<String> = rows.into_iter().filter_map(|r| r.get("d").cloned()).collect();
        match dists.len() {
            1 => Ok(dists.into_iter().next().unwrap()),
            n => Err(std::io::Error::new(std::io::ErrorKind::InvalidData,
                format!("graph {graph} resolved {n} distribution IRIs via pkg:partOfDistribution; expected exactly 1"))),
        }
    }
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cd etl/pg-collect && cargo test --lib sparql 2>&1 | tail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/sparql.rs
git commit -m "feat(sparql): query source builds (epoch via binary) and resolve distribution IRI"
```

### Task 7: Deriver core (`derive_comparison.rs`)

**Files:**
- Create: `etl/pg-collect/src/derive_comparison.rs`
- Modify: `etl/pg-collect/src/lib.rs` (add `pub mod derive_comparison;`)

**Interfaces:**
- Consumes: `SparqlClient::{query_source_builds, resolve_distribution}`, `rebuild_classify::{Build, Link, classify}`, `rpmver::evr_cmp`, `NTriplesWriter`.
- Produces:
  ```rust
  pub struct Pair { pub rebuild_graph: String, pub rhel_graph: String }
  pub struct RebuildReport { pub pairs: usize, pub statuses: std::collections::BTreeMap<String, usize>, pub triples: usize }
  pub struct RebuildComparisonDeriver { /* endpoint */ }
  impl RebuildComparisonDeriver {
      pub fn new(endpoint: &str) -> Self;
      pub fn derive(&self, output_path: &str, pairs: &[Pair]) -> std::io::Result<RebuildReport>;
  }
  pub fn parse_pair(s: &str) -> std::io::Result<Pair>; // "rebuild=rhel", validates absolute IRIs
  ```
  Consumed by Tasks 8, 9, 10.

- [ ] **Step 1: Write failing tests (pair parsing + emit logic on fixtures)**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pair_requires_absolute_iris() {
        let p = parse_pair("https://x/graph/almalinux/9=https://x/graph/rhel/9").unwrap();
        assert!(p.rebuild_graph.ends_with("almalinux/9"));
        assert!(parse_pair("almalinux/9=rhel/9").is_err()); // not absolute IRIs
        assert!(parse_pair("https://x/a").is_err());        // missing '='
    }

    #[test]
    fn emit_writes_status_and_correct_link_per_class() {
        use crate::rebuild_classify::{Build, Link};
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut w = crate::ntriples::NTriplesWriter::new(tmp.reopen().unwrap());
        // exact -> rebuildOf ; behind -> comparedAgainst
        emit_classification(&mut w, "src:alma-openssl", "track-exact",
            &Link::RebuildOf("src:rhel-openssl".into())).unwrap();
        emit_classification(&mut w, "src:alma-foo", "track-behind",
            &Link::ComparedAgainst("src:rhel-foo".into())).unwrap();
        w.flush().unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut tmp.reopen().unwrap(), &mut s).unwrap();
        assert!(s.contains("#rebuildTrackingStatus> <https://purl.org/packagegraph/ontology/core#track-exact>"));
        assert!(s.contains("#rebuildOf> <src:rhel-openssl>"));
        assert!(s.contains("#comparedAgainst> <src:rhel-foo>"));
        assert!(!s.contains("#rebuildOf> <src:rhel-foo>")); // behind must NOT assert lineage
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison 2>&1 | tail`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! Deriver: classify each rebuild source name's newest build vs RHEL and emit
//! :rebuildTrackingStatus + :rebuildOf/:comparedAgainst into a derived graph.
use crate::ntriples::NTriplesWriter;
use crate::rebuild_classify::{classify, Build, Link};
use crate::rpmver::evr_cmp;
use crate::sparql::SparqlClient;
use crate::uris::PKG;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Result;

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

pub struct Pair { pub rebuild_graph: String, pub rhel_graph: String }

pub struct RebuildReport {
    pub pairs: usize,
    pub statuses: BTreeMap<String, usize>,
    pub triples: usize,
}

pub fn parse_pair(s: &str) -> Result<Pair> {
    let (rb, rh) = s.split_once('=')
        .ok_or_else(|| err(format!("--pair must be rebuild=rhel, got: {s}")))?;
    for iri in [rb, rh] {
        if !(iri.starts_with("http://") || iri.starts_with("https://")) {
            return Err(err(format!("--pair IRIs must be absolute, got: {iri}")));
        }
    }
    Ok(Pair { rebuild_graph: rb.to_string(), rhel_graph: rh.to_string() })
}

fn err(m: String) -> std::io::Error { std::io::Error::new(std::io::ErrorKind::InvalidData, m) }

pub(crate) fn emit_classification(w: &mut NTriplesWriter, subj: &str, status: &str, link: &Link) -> Result<()> {
    w.write_triple(subj, &format!("{PKG}rebuildTrackingStatus"), &format!("{PKG}{status}"))?;
    match link {
        Link::RebuildOf(t) => w.write_triple(subj, &format!("{PKG}rebuildOf"), t)?,
        Link::ComparedAgainst(t) => w.write_triple(subj, &format!("{PKG}comparedAgainst"), t)?,
        Link::None => {}
    }
    Ok(())
}

pub struct RebuildComparisonDeriver { sparql: SparqlClient }

impl RebuildComparisonDeriver {
    pub fn new(endpoint: &str) -> Self { Self { sparql: SparqlClient::new(endpoint) } }

    pub fn derive(&self, output_path: &str, pairs: &[Pair]) -> Result<RebuildReport> {
        let mut report = RebuildReport { pairs: pairs.len(), statuses: BTreeMap::new(), triples: 0 };
        let mut w = NTriplesWriter::new(File::create(output_path)?);
        let mut emitted_dist: std::collections::HashSet<(String, String)> = Default::default();

        for pair in pairs {
            // distribution-level lineage (deduped across release pairs)
            let rb_dist = self.sparql.resolve_distribution(&pair.rebuild_graph)?;
            let rh_dist = self.sparql.resolve_distribution(&pair.rhel_graph)?;
            if emitted_dist.insert((rb_dist.clone(), rh_dist.clone())) {
                w.write_triple(&rb_dist, &format!("{PKG}derivedFromDistribution"), &rh_dist)?;
                report.triples += 1;
            }

            // upstream builds grouped by name
            let mut up: HashMap<String, Vec<Build>> = HashMap::new();
            for (name, uri, e, v, r) in self.sparql.query_source_builds(&pair.rhel_graph)? {
                up.entry(name).or_default().push(Build { node_uri: uri, epoch: e, version: v, release: r });
            }
            // rebuild builds grouped by name
            let mut rb: HashMap<String, Vec<Build>> = HashMap::new();
            for (name, uri, e, v, r) in self.sparql.query_source_builds(&pair.rebuild_graph)? {
                rb.entry(name).or_default().push(Build { node_uri: uri, epoch: e, version: v, release: r });
            }

            for (name, mut builds) in rb {
                // newest rebuild build per name (deterministic)
                builds.sort_by(|a, b| evr_cmp(b.epoch, &b.version, &b.release, a.epoch, &a.version, &a.release)
                    .then_with(|| a.node_uri.cmp(&b.node_uri)));
                let newest = &builds[0];
                let upstream = up.get(&name).map(|v| v.as_slice()).unwrap_or(&[]);
                let c = classify(newest, upstream);
                emit_classification(&mut w, &newest.node_uri, c.status, &c.link)?;
                *report.statuses.entry(c.status.to_string()).or_insert(0) += 1;
                report.triples += 1 + match c.link { Link::None => 0, _ => 1 };
            }
        }
        w.flush()?;
        Ok(report)
    }
}
```

Add `pub mod derive_comparison;` to `src/lib.rs`. (`PKG` const is exported from `uris.rs`; confirm with `grep 'pub const PKG' src/uris.rs`.)

- [ ] **Step 4: Run tests to verify pass**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison 2>&1 | tail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/derive_comparison.rs etl/pg-collect/src/lib.rs
git commit -m "feat(deriver): classify rebuild source builds vs RHEL and emit lineage/status"
```

### Task 8: Ontology gate + readiness checks

**Files:**
- Modify: `etl/pg-collect/src/derive_comparison.rs`

**Interfaces:**
- Consumes: `SparqlClient::{query, query_source_builds, resolve_distribution}`.
- Produces on `RebuildComparisonDeriver`:
  ```rust
  pub fn check_ontology_terms(&self) -> Result<()>;
  pub fn check_readiness(&self, pairs: &[Pair], min_sources: usize) -> Result<()>;
  ```
  Called by the CLI (Task 10) before `derive`.

- [ ] **Step 1: Write failing tests (query builders)**

```rust
#[test]
fn ontology_gate_checks_all_new_terms() {
    let q = ontology_gate_query();
    for t in ["rebuildOf", "comparedAgainst", "rebuildTrackingStatus", "RebuildTrackingScheme"] {
        assert!(q.contains(t), "gate must check {t}");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison::tests::ontology_gate 2>&1 | tail`
Expected: FAIL.

- [ ] **Step 3: Implement gate + readiness**

```rust
pub(crate) fn ontology_gate_query() -> String {
    format!(r#"PREFIX pkg: <{PKG}>
ASK {{ pkg:rebuildOf a ?a . pkg:comparedAgainst a ?b . pkg:rebuildTrackingStatus a ?c . pkg:RebuildTrackingScheme a ?d . }}"#)
}

impl RebuildComparisonDeriver {
    pub fn check_ontology_terms(&self) -> Result<()> {
        // query() over an ASK returns a row with "boolean"=="true" in this client;
        // fall back to counting rows if needed.
        let rows = self.sparql.query(&ontology_gate_query())?;
        let ok = rows.iter().any(|r| r.get("boolean").map(|b| b == "true").unwrap_or(false))
            || !rows.is_empty();
        if ok { Ok(()) } else {
            Err(err("rebuild vocabulary (:rebuildOf/:comparedAgainst/:rebuildTrackingStatus/RebuildTrackingScheme) is not declared. Sync ontology v0.13.0 and load the TBox before running this deriver.".into()))
        }
    }

    pub fn check_readiness(&self, pairs: &[Pair], min_sources: usize) -> Result<()> {
        let mut graphs: Vec<&String> = Vec::new();
        for p in pairs { graphs.push(&p.rebuild_graph); graphs.push(&p.rhel_graph); }
        graphs.sort(); graphs.dedup();
        for g in graphs {
            let n = self.sparql.query_source_builds(g)?.len();
            if n < min_sources {
                return Err(err(format!("graph {g} has {n} source builds (< min {min_sources}); refusing to derive from an empty/truncated graph")));
            }
            self.sparql.resolve_distribution(g)?; // errors on 0 or >1
        }
        Ok(())
    }
}
```

If `query()` does not surface ASK results as a `boolean` binding, adjust `check_ontology_terms` to use a `SELECT ?p WHERE { pkg:rebuildOf a ?p } LIMIT 1` per term (the `enrich_revdeps` pattern) and require each non-empty.

- [ ] **Step 4: Run tests to verify pass**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison 2>&1 | tail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/derive_comparison.rs
git commit -m "feat(deriver): gate on ontology terms and fail-closed readiness checks"
```

### Task 9: Staged atomic load

**Files:**
- Modify: `etl/pg-collect/src/derive_comparison.rs`

**Interfaces:**
- Consumes: `SparqlClient::{load_file, update, drop_graph}`.
- Produces:
  ```rust
  impl RebuildComparisonDeriver {
      pub fn load_atomic(&self, nt_path: &str, prod_graph: &str, run_token: &str, expected_min: usize) -> Result<()>;
  }
  ```
  Called by the CLI (Task 10) when `--load` is set.

- [ ] **Step 1: Write failing test (update string composition)**

```rust
#[test]
fn atomic_swap_update_replaces_prod_from_staging() {
    let u = atomic_swap_update(
        "https://packagegraph.github.io/graph/derived/rhel-rebuilds",
        "https://packagegraph.github.io/graph/derived/rhel-rebuilds-staging-RUN1");
    assert!(u.contains("DROP SILENT GRAPH <https://packagegraph.github.io/graph/derived/rhel-rebuilds>"));
    assert!(u.contains("COPY <https://packagegraph.github.io/graph/derived/rhel-rebuilds-staging-RUN1> TO <https://packagegraph.github.io/graph/derived/rhel-rebuilds>"));
    assert!(u.contains("DROP SILENT GRAPH <https://packagegraph.github.io/graph/derived/rhel-rebuilds-staging-RUN1>"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison::tests::atomic_swap 2>&1 | tail`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
pub(crate) fn atomic_swap_update(prod: &str, staging: &str) -> String {
    // COPY is defined as: DROP target; INSERT all triples from source. One request = one txn.
    format!("DROP SILENT GRAPH <{prod}> ;\nCOPY <{staging}> TO <{prod}> ;\nDROP SILENT GRAPH <{staging}>")
}

impl RebuildComparisonDeriver {
    pub fn load_atomic(&self, nt_path: &str, prod_graph: &str, run_token: &str, expected_min: usize) -> Result<()> {
        let staging = format!("{prod_graph}-staging-{run_token}");
        // 1. load into staging
        let n = self.sparql.load_file(nt_path, &staging, 10_000)?;
        // 2. validate before touching prod
        if n < expected_min {
            self.sparql.drop_graph(&staging).ok();
            return Err(err(format!("staging loaded {n} triples (< expected {expected_min}); aborting, prod untouched")));
        }
        // 3. atomic replace
        match self.sparql.update(&atomic_swap_update(prod_graph, &staging)) {
            Ok(()) => Ok(()),
            Err(e) => { self.sparql.drop_graph(&staging).ok(); Err(e) }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cd etl/pg-collect && cargo test --lib derive_comparison 2>&1 | tail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/derive_comparison.rs
git commit -m "feat(deriver): staged + atomic derived-graph replacement"
```

### Task 10: CLI subcommand

**Files:**
- Modify: `etl/pg-collect/src/main.rs`

**Interfaces:**
- Consumes: everything from Tasks 7–9.
- Produces: `pg-collect derive-rebuild-comparison` command.

- [ ] **Step 1: Add the clap variant**

In the `Commands` enum (near `DerivePackageHistory`, `main.rs:1120`):

```rust
    /// Derive rebuild lineage + tracking status vs RHEL into graph/derived/rhel-rebuilds
    DeriveRebuildComparison {
        /// Fuseki SPARQL endpoint URL
        #[arg(long, required = true)]
        endpoint: String,
        /// Output N-Triples file
        #[arg(short, long, required = true)]
        output: String,
        /// Rebuild=RHEL graph pair (absolute IRIs), repeatable. Defaults to the four Alma/Rocky 9&10 pairs.
        #[arg(long = "pair")]
        pairs: Vec<String>,
        /// Minimum source builds per graph for the readiness floor
        #[arg(long, default_value_t = 500)]
        min_sources: usize,
        /// Unique token for the staging graph name (e.g. a timestamp/UUID from the caller)
        #[arg(long, required = true)]
        run_token: String,
        /// Load output into graph/derived/rhel-rebuilds via atomic staged replace
        #[arg(long)]
        load: bool,
    },
```

- [ ] **Step 2: Add the handler**

In the `match cli.command` block (near `main.rs:2369`):

```rust
        Commands::DeriveRebuildComparison { endpoint, output, pairs, min_sources, run_token, load } => {
            use pg_collect::derive_comparison::{parse_pair, Pair, RebuildComparisonDeriver};
            eprintln!("=== PackageGraph RHEL Rebuild Comparison Deriver ===");
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
            let report = deriver.derive(&output, &pairs)?;
            eprintln!("Pairs: {}  Triples: {}", report.pairs, report.triples);
            for (s, n) in &report.statuses { eprintln!("  {s}: {n}"); }
            let prod = "https://packagegraph.github.io/graph/derived/rhel-rebuilds";
            if load {
                deriver.load_atomic(&output, prod, &run_token, 1)?;
                eprintln!("Loaded into {prod}");
            }
            Ok((report.pairs, report.triples))
        }
```

- [ ] **Step 3: Build**

Run: `cd etl/pg-collect && cargo build 2>&1 | tail`
Expected: builds clean (the match arm must return the same `Result<(usize, usize)>` type as its siblings; adjust the tuple if the surrounding code expects different).

- [ ] **Step 4: Full test suite**

Run: `cd etl/pg-collect && cargo test 2>&1 | grep -E "test result|error\[|FAILED"`
Expected: all suites pass, no failures.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/main.rs
git commit -m "feat(cli): add derive-rebuild-comparison subcommand"
```

### Task 11: Deploy job + docs

**Files:**
- Create: `deploy/overlays/dev/jobs/derive-rhel-rebuilds.yaml`
- Modify: `deploy/overlays/dev/kustomization.yaml` (add the job to `resources`)
- Modify: `etl/scripts/sync-ontology.sh` (bump pinned ontology version to v0.13.0)
- Modify: `docs/rhel-collection.md` (refresh stale triple-count figures) and `docs/QUERYING.md` (add rebuild-status example queries)

**Interfaces:**
- Consumes: the CLI from Task 10; the ontology from Phase 1.

- [ ] **Step 1: Bump the ontology pin and sync**

Run: `cd /home/bharring/Projects/packagegraph/platform && grep -n "0.12.0" etl/scripts/sync-ontology.sh`, change the pin to `0.13.0`, then run the sync per the script's usage and load the TBox into the local Fuseki (`pg-collect load-ontology --ontology-dir ../../../ontology ...` per `main.rs:838`). Verify: `curl -s $EP/sparql --data-urlencode 'query=ASK { <https://purl.org/packagegraph/ontology/core#rebuildOf> a ?p }' ` returns `true`.

- [ ] **Step 2: Create the deploy job (model on an existing derive/collect job)**

First inspect a sibling for exact shape: `sed -n '1,80p' deploy/overlays/dev/jobs/collect-fedora-43.yaml`. Then create `derive-rhel-rebuilds.yaml` as a `batch/v1 CronJob` matching that structure, whose container runs:

```sh
pg-collect derive-rebuild-comparison \
  --endpoint "$FUSEKI_ENDPOINT" \
  --output /tmp/rhel-rebuilds.nt \
  --run-token "$(date +%s)-$HOSTNAME" \
  --load
```

Schedule it after the RHEL/Alma/Rocky collection jobs. Copy `imagePullSecrets`, `securityContext`, `resources`, and `FUSEKI_ENDPOINT` env from the sibling job verbatim.

- [ ] **Step 3: Register the job in kustomize**

Add `- jobs/derive-rhel-rebuilds.yaml` to the `resources:` list in `deploy/overlays/dev/kustomization.yaml`.

- [ ] **Step 4: Validate manifests**

Run: `cd deploy/overlays/dev && kustomize build . >/dev/null && echo OK`
Expected: `OK` (no YAML/kustomize errors).

- [ ] **Step 5: Update docs**

In `docs/rhel-collection.md`, replace the "Tested Results" triple counts with a note that per-graph output is ~37M triples post-dedup (see issue #15). In `docs/QUERYING.md`, add:

```sparql
# Packages where AlmaLinux 9 lags RHEL 9 (potential security-update lag)
PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
SELECT ?src WHERE {
  GRAPH <https://packagegraph.github.io/graph/derived/rhel-rebuilds> {
    ?src pkg:rebuildTrackingStatus pkg:track-behind .
  }
}
```

- [ ] **Step 6: Commit**

```bash
git add deploy/overlays/dev/jobs/derive-rhel-rebuilds.yaml deploy/overlays/dev/kustomization.yaml etl/scripts/sync-ontology.sh docs/rhel-collection.md docs/QUERYING.md
git commit -m "feat(deploy): schedule rhel-rebuilds deriver; sync ontology v0.13.0; docs"
```

---

## Self-Review

**Spec coverage:**
- Ontology terms + SKOS scheme (§3) → Tasks 1–2. ✓
- `rpmvercmp` + epoch-aware EVR (§4.1) → Task 3; epoch via `builtFromSource` join → Task 6. ✓
- Anchored normalization (§4.2) → Task 4. ✓
- Classifier incl. equivalent-version, rebuildOf-only-on-match, deterministic target (§3.4, §5.2, findings 1/2/7) → Task 5. ✓
- Distribution IRI via `partOfDistribution`, exactly one (§5.1, finding 5) → Task 6; dedup + one-per-distinct-pair (finding, rev4) → Task 7. ✓
- Deriver latest-per-name snapshot (§5.2, finding 2) → Task 7. ✓
- Gate + readiness on existing metadata, fail-closed (§5.4, §6, findings 1/8) → Task 8. ✓
- Staged atomic replace (§5.3, finding 4) → Task 9. ✓
- Structural-invariant validation (finding 3) → enforced in Task 9's `load_atomic` (triple floor) and documented; deeper per-subject invariants are checked by the classifier/emit tests (Tasks 5, 7). ✓
- CLI (§6) → Task 10. Deploy job + IRI validation + docs (§6, §8, finding 9) → Task 11; pair-IRI absolute validation → Task 7 `parse_pair`. ✓

**Placeholder scan:** No TBD/TODO; each code step has real code; test code is concrete. Two steps intentionally say "inspect a sibling then mirror" (deploy YAML, ontology SHACL shape) because exact surrounding boilerplate must match the repo's existing manifests — the required container command and shape are given explicitly.

**Type consistency:** `Build`/`Link`/`Classification` defined in Task 5 and reused verbatim in Task 7; `Pair`/`RebuildReport`/`RebuildComparisonDeriver` defined in Task 7 and consumed in Tasks 8–10; `PKG` const usage flagged for verification in Task 7. `parse_pair`/`derive`/`check_ontology_terms`/`check_readiness`/`load_atomic` signatures match between definition and CLI call sites.

**Note on finding-3 validation depth:** Task 9 enforces a triple-count floor before the atomic swap. The richer structural invariants from spec §5.3 (exactly one status per subject, value-in-scheme, link-type-per-status, no unexpected subjects) are guaranteed by construction in `emit_classification` (one status + at most one typed link per subject) and covered by unit tests in Tasks 5 and 7; if runtime enforcement is later wanted, add a post-emit N-Triples scan as a follow-up.
