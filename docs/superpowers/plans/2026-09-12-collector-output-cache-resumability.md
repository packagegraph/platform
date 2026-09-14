# Collector Output-Checkpoint Resumability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rpm-full` collectors resume after a timeout/kill instead of restarting from zero, by checkpointing each item's derived triples locally.

**Architecture:** A new `output_cache.rs` stores per-item derived N-Triples under
`<cache_dir>/output/<generation>/<stage>/<schema_version>/<digest>`. Each per-item
loop wraps its derivation in `get_or_compute`; on resume, completed items replay
from disk with no network call. A run generation, owned by the `rpm-full`
orchestrator and committed only after a successful upload, keeps checkpoints
scoped to one in-flight run rather than becoming permanent memoization.

**Tech Stack:** Rust (crate `pg-collect`), `sha2`, `once_cell`, `serde_json`;
tests with `tempfile` + `mockito`. Shell wrappers use `mc` (MinIO client).

**Spec:** `docs/superpowers/specs/2026-09-12-collector-output-cache-resumability-design.md`

## Global Constraints

- Checkpoints are **local-only**. Never synced to Minio. Every mirror in a
  `rpm-full` wrapper gets `--exclude 'output/*'`, both directions.
- No new crate dependencies. For randomness use the `RandomState`-seeded xorshift
  pattern already in `http_transport.rs` (the crate deliberately has no `rand`).
- Cacheability is an **explicit typed outcome**, never inferred from `Err` and
  never from whether `emit_dq_issue` was called.
- Generation ids match `^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{8}$` and are validated before
  being joined to any path. Never reused.
- Cache persistence is **best-effort**: a write failure warns, counts, and returns
  the computed output. It never fails the run and never drops derived triples.
- Only `rpm-full` injects a checkpoint. Standalone `enrich-koji` passes `None` and
  is otherwise unchanged, in both its discovery and `--srpm-list` modes.
- Adopters must not use `write_triple_once`/`write_literal_once`/
  `write_datetime_once` — cached fragments must render independently.
- There is **no workspace manifest at the repo root** — the only one is
  `etl/pg-collect/Cargo.toml`. Every cargo command carries
  `--manifest-path etl/pg-collect/Cargo.toml`, and the binary lands at
  `etl/pg-collect/target/debug/pg-collect`. Full suite must stay green.
- When a task creates a new module file, add its `pub mod` line to `lib.rs` **in
  the same step that creates the file**. Otherwise cargo never compiles it and the
  red phase reports "0 tests" — a false green that looks like a passing run.
- Collector iteration order is sorted (see Task 5 and Task 8), which is what makes
  "a replayed run is byte-identical" a real contract rather than a coincidence of
  `HashSet` ordering.

---

### Task 1: Genericize writer-taking emit helpers over `W: Write`

Capturing a fragment requires running derivation against a
`NTriplesWriter<Vec<u8>>`. Today the emit helpers take `&mut NTriplesWriter`,
which resolves to the default type parameter `NTriplesWriter<File>`
(`ntriples.rs:50`), so they cannot accept a scratch writer. This task is a pure
refactor: no behavior change, no new tests beyond proving a `Vec<u8>`-backed
writer now compiles through the helpers. Precedent for the generic form already
exists at `derive_comparison.rs:145`.

**Files:**
- Modify: `etl/pg-collect/src/forge.rs` (`emit_dq_issue:784`, `emit_forge_triples:915`, `emit_upstream_project:1006`)
- Modify: `etl/pg-collect/src/collect_spec.rs` (`process_spec:163`, `emit_ecosystem_triples:422`, `emit_buildrequires_triples:480`, `emit_changelog_triples:510`)
- Modify: `etl/pg-collect/src/enrich_koji.rs` (`get_build:231`, `query_rpm_signatures:316`, `emit_signature_triples:432`, `emit_build_triples:470`)
- Test: `etl/pg-collect/src/forge.rs` (append to existing `mod tests`)

**Interfaces:**
- Produces: all listed functions accept `&mut NTriplesWriter<W>` where `W: Write`.
  Existing callers passing a file-backed writer continue to infer `W = File`, so
  no call site changes.

- [ ] **Step 1: Write the failing test**

Append to `forge.rs`'s existing `mod tests`:

```rust
#[test]
fn emit_helpers_accept_an_in_memory_writer() {
    // Guards the genericization the checkpoint capture path depends on:
    // derivation must be able to run against a Vec<u8>-backed scratch writer.
    let mut w = NTriplesWriter::new(Vec::<u8>::new());
    let n = emit_dq_issue(&mut w, "test-detector", "field", "value", "issue", "warning")
        .expect("emit_dq_issue should accept an in-memory writer");
    assert!(n > 0, "expected emit_dq_issue to write at least one triple");
    let out = w.into_string().expect("valid utf8");
    assert!(out.contains("test-detector"), "got: {out}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib forge::tests::emit_helpers_accept_an_in_memory_writer`
Expected: FAIL to compile — `expected struct NTriplesWriter<File>, found NTriplesWriter<Vec<u8>>`.

- [ ] **Step 3: Genericize the three `forge.rs` helpers**

Change only the signatures; bodies are untouched. Ensure `use std::io::Write;` is
present in `forge.rs`.

```rust
pub fn emit_dq_issue<W: Write>(
    writer: &mut NTriplesWriter<W>,
    detector: &str,
    field: &str,
    raw_value: &str,
    issue_type: &str,
    severity: &str,
) -> Result<usize> {
```

```rust
pub fn emit_forge_triples<W: Write>(
    writer: &mut NTriplesWriter<W>,
    repo_uri: &str,
    repo_url: &str,
) -> Result<usize> {
```

```rust
pub fn emit_upstream_project<W: Write>(
    writer: &mut NTriplesWriter<W>,
    repo_url: &str,
) -> Result<usize> {
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib forge::tests::emit_helpers_accept_an_in_memory_writer`
Expected: PASS.

- [ ] **Step 5: Genericize the `collect_spec.rs` and `enrich_koji.rs` methods**

Same mechanical change — add `<W: Write>` to the method and take
`&mut NTriplesWriter<W>`. Add `use std::io::Write;` to each file if absent.

In `collect_spec.rs`: `process_spec`, `emit_ecosystem_triples`,
`emit_buildrequires_triples`, `emit_changelog_triples`. Also make `collect` generic
so it can pass its writer through:

```rust
pub fn collect<W: Write>(
    &self,
    writer: &mut NTriplesWriter<W>,
    srpm_names: &HashSet<String>,
    srpm_identity_map: &HashMap<String, Vec<String>>,
    existing_ecosystem_pkgs: &HashSet<String>,
    emit_buildrequires: bool,
    emit_maintainers: bool,
) -> Result<(usize, usize)> {
```

In `enrich_koji.rs`: `get_build`, `query_rpm_signatures`, `emit_signature_triples`,
`emit_build_triples`.

- [ ] **Step 6: Run the full suite**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml`
Expected: PASS, same test count as before plus one. Any failure here means a call
site needed an explicit type annotation — add `::<File>` at that call site rather
than reverting the signature.

- [ ] **Step 7: Commit**

```bash
git add etl/pg-collect/src/forge.rs etl/pg-collect/src/collect_spec.rs etl/pg-collect/src/enrich_koji.rs
git commit -m "refactor(etl): genericize writer-taking emit helpers over W: Write

Checkpoint capture runs derivation against a Vec<u8>-backed scratch writer,
which the default NTriplesWriter<File> parameter made impossible. Pure
signature change, no behavior change; derive_comparison.rs:145 already
established the generic form."
```

---

### Task 2: `OutputCache` core

The storage primitive: canonical key digest, versioned integrity-checked envelope,
atomic write, disabled mode, and the `get_or_compute` contract.

**Files:**
- Create: `etl/pg-collect/src/output_cache.rs`
- Modify: `etl/pg-collect/src/lib.rs` (add `pub mod output_cache;`)

**Interfaces:**
- Produces:
  - `CachedOutput { logical_triples: usize, skipped_invalid_iri: usize, auto_inverses: usize, text: String }`
  - `ComputeOutcome::{Complete(CachedOutput), Retryable(CachedOutput)}`
  - `CanonicalContext::new() -> Self`, `.field(name: &str, value: &str) -> Self`, `.flag(name: &str, value: bool) -> Self`, `.list(name: &str, values: &[String]) -> Self`
  - `OutputCache::new(cache_dir: &Path, generation: &str, stage: &str, schema_version: &str) -> io::Result<Self>`
  - `OutputCache::disabled() -> Self`
  - `OutputCache::get_or_compute<F>(&self, item_key: &str, context: &CanonicalContext, compute: F) -> io::Result<CachedOutput> where F: FnOnce() -> io::Result<ComputeOutcome>`
  - `OutputCache::stats(&self) -> CacheStats` with public fields `hits, misses, retryable, write_failures, integrity_failures: u64`
  - `pub const OUTPUT_CACHE_FORMAT_VERSION: u32 = 1;`

- [ ] **Step 1: Declare the module, then write the failing tests**

First add to `etl/pg-collect/src/lib.rs`, after `pub mod source_cache;`:

```rust
pub mod output_cache;
```

Declaring it now, not after the implementation, is what makes the red phase real:
without the `pub mod` line cargo never compiles the file, so the "failing" run
reports 0 tests and exits 0 — a false green.

Then create `etl/pg-collect/src/output_cache.rs` containing only the test module
(the file must exist for `lib.rs` to compile):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn out(text: &str) -> CachedOutput {
        CachedOutput { logical_triples: 1, skipped_invalid_iri: 0, auto_inverses: 0, text: text.to_string() }
    }
    fn ctx() -> CanonicalContext {
        CanonicalContext::new().field("distro", "fedora").field("release", "44")
    }
    fn cache(dir: &TempDir) -> OutputCache {
        OutputCache::new(dir.path(), "20260912T010203Z-abcdef01", "spec", "spec-v1").unwrap()
    }

    #[test]
    fn miss_then_hit_skips_the_closure() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let r1 = c.get_or_compute("pkg", &ctx(), || Ok(ComputeOutcome::Complete(out("<a> <b> <c> .\n")))).unwrap();
        assert_eq!(r1.text, "<a> <b> <c> .\n");
        let r2 = c.get_or_compute("pkg", &ctx(), || panic!("closure must not run on a hit")).unwrap();
        assert_eq!(r2.text, "<a> <b> <c> .\n");
        assert_eq!(c.stats().hits, 1);
        assert_eq!(c.stats().misses, 1);
    }

    #[test]
    fn counters_survive_a_round_trip() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let o = CachedOutput { logical_triples: 7, skipped_invalid_iri: 2, auto_inverses: 3, text: "x".into() };
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(o))).unwrap();
        let hit = c.get_or_compute("k", &ctx(), || panic!("hit expected")).unwrap();
        assert_eq!((hit.logical_triples, hit.skipped_invalid_iri, hit.auto_inverses), (7, 2, 3));
    }

    #[test]
    fn retryable_is_returned_but_never_persisted() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let r = c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Retryable(out("tmp")))).unwrap();
        assert_eq!(r.text, "tmp");
        let mut ran = false;
        let r2 = c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("real"))) }).unwrap();
        assert!(ran, "a Retryable result must not have been checkpointed");
        assert_eq!(r2.text, "real");
        assert_eq!(c.stats().retryable, 1);
    }

    #[test]
    fn err_propagates_and_writes_nothing() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let e = c.get_or_compute("k", &ctx(), || Err(io::Error::new(io::ErrorKind::Other, "boom")));
        assert!(e.is_err());
        let mut ran = false;
        let _ = c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v"))) }).unwrap();
        assert!(ran, "an Err must not have been checkpointed");
    }

    #[test]
    fn disabled_mode_never_touches_disk() {
        let d = TempDir::new().unwrap();
        let c = OutputCache::disabled();
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        let mut ran = false;
        c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v"))) }).unwrap();
        assert!(ran, "disabled cache must always recompute");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn corrupt_entry_is_a_miss_and_is_evicted() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        let path = c.entry_path("k", &ctx()).unwrap();
        std::fs::write(&path, b"{ not a valid envelope").unwrap();
        let mut ran = false;
        c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v2"))) }).unwrap();
        assert!(ran, "corrupt entry must be treated as a miss");
        assert_eq!(c.stats().integrity_failures, 1);
    }

    #[test]
    fn digest_mismatch_is_a_miss() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        let path = c.entry_path("k", &ctx()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // Valid JSON envelope, but the payload no longer matches its recorded digest.
        std::fs::write(&path, raw.replace("\"text\":\"v\"", "\"text\":\"TAMPERED\"")).unwrap();
        let mut ran = false;
        c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v")))}).unwrap();
        assert!(ran, "payload not matching its digest must be a miss");
    }

    #[test]
    fn unknown_format_version_is_a_miss() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        let path = c.entry_path("k", &ctx()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replace("\"format\":1", "\"format\":9999")).unwrap();
        let mut ran = false;
        c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v")))}).unwrap();
        assert!(ran, "an unknown envelope format must be a miss");
    }

    #[test]
    fn context_encoding_is_injective() {
        // ("ab","c") and ("a","bc") must not collide -- the ambiguity that
        // plain concatenation would introduce.
        let a = CanonicalContext::new().field("x", "ab").field("y", "c");
        let b = CanonicalContext::new().field("x", "a").field("y", "bc");
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn every_context_field_changes_the_digest() {
        let base = CanonicalContext::new()
            .field("distro", "fedora").field("release", "44")
            .flag("buildrequires", false)
            .list("identities", &["a".into(), "b".into()]);
        let variants = [
            CanonicalContext::new().field("distro", "rhel").field("release", "44")
                .flag("buildrequires", false).list("identities", &["a".into(), "b".into()]),
            CanonicalContext::new().field("distro", "fedora").field("release", "43")
                .flag("buildrequires", false).list("identities", &["a".into(), "b".into()]),
            CanonicalContext::new().field("distro", "fedora").field("release", "44")
                .flag("buildrequires", true).list("identities", &["a".into(), "b".into()]),
            CanonicalContext::new().field("distro", "fedora").field("release", "44")
                .flag("buildrequires", false).list("identities", &["a".into(), "c".into()]),
        ];
        for v in &variants {
            assert_ne!(base.digest(), v.digest());
        }
    }

    #[test]
    fn context_variants_are_domain_separated() {
        // Without a type tag these all collapse onto the same encoding, so
        // swapping field for flag during a refactor would silently reuse the
        // other's checkpoints.
        let as_field = CanonicalContext::new().field("x", "0");
        let as_flag = CanonicalContext::new().flag("x", false);
        let as_list = CanonicalContext::new().list("x", &["0".into()]);
        assert_ne!(as_field.digest(), as_flag.digest());
        assert_ne!(as_field.digest(), as_list.digest());
        assert_ne!(as_flag.digest(), as_list.digest());
        assert_ne!(
            CanonicalContext::new().field("x", "1").digest(),
            CanonicalContext::new().flag("x", true).digest()
        );
    }

    #[test]
    fn list_order_does_not_matter_but_contents_do() {
        let a = CanonicalContext::new().list("l", &["b".into(), "a".into()]);
        let b = CanonicalContext::new().list("l", &["a".into(), "b".into()]);
        assert_eq!(a.digest(), b.digest(), "lists are sorted before hashing");
        let c = CanonicalContext::new().list("l", &["a".into()]);
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn item_key_and_context_are_separately_distinguished() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let p1 = c.entry_path("ab", &CanonicalContext::new().field("f", "c")).unwrap();
        let p2 = c.entry_path("a", &CanonicalContext::new().field("f", "bc")).unwrap();
        assert_ne!(p1, p2);
    }

    #[test]
    fn write_failure_still_returns_the_computed_output() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        // Replace the entry directory with a file so the atomic write cannot succeed.
        let dir = c.entry_path("k", &ctx()).unwrap().parent().unwrap().to_path_buf();
        std::fs::remove_dir_all(&dir).ok();
        std::fs::write(&dir, b"blocker").unwrap();
        let r = c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        assert_eq!(r.text, "v", "a cache write failure must not discard derived output");
        assert_eq!(c.stats().write_failures, 1);
    }

    #[test]
    fn temp_files_are_unique_per_write() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let a = c.temp_path();
        let b = c.temp_path();
        assert_ne!(a, b);
    }

    #[test]
    fn tampering_with_any_counter_is_an_integrity_miss() {
        // The counters are restored into the parent writer and drive reported
        // totals, so they must be covered by the digest -- not just the RDF text.
        for field in ["logical_triples", "skipped_invalid_iri", "auto_inverses"] {
            let d = TempDir::new().unwrap();
            let c = cache(&d);
            c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(CachedOutput {
                logical_triples: 1, skipped_invalid_iri: 1, auto_inverses: 1, text: "v".into(),
            }))).unwrap();
            let path = c.entry_path("k", &ctx()).unwrap();
            let raw = std::fs::read_to_string(&path).unwrap();
            let tampered = raw.replace(&format!("\"{field}\":1"), &format!("\"{field}\":999"));
            assert_ne!(tampered, raw, "test must actually mutate {field}");
            std::fs::write(&path, tampered).unwrap();
            let mut ran = false;
            c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v"))) }).unwrap();
            assert!(ran, "a tampered {field} must be an integrity miss");
        }
    }

    #[test]
    fn constructor_rejects_unsafe_path_components() {
        // OutputCache is a shared public primitive: it enforces its own
        // invariant rather than trusting every caller to have validated.
        let d = TempDir::new().unwrap();
        let good = "20260912T010203Z-abcdef01";
        assert!(OutputCache::new(d.path(), "../escape", "spec", "spec-v1").is_err());
        assert!(OutputCache::new(d.path(), good, "..", "spec-v1").is_err());
        assert!(OutputCache::new(d.path(), good, "spec", "../x").is_err());
        assert!(OutputCache::new(d.path(), good, "a/b", "spec-v1").is_err());
        assert!(OutputCache::new(d.path(), good, "", "spec-v1").is_err());
        assert!(OutputCache::new(d.path(), good, "spec", "").is_err());
        assert!(OutputCache::new(d.path(), good, "spec", "spec-v1").is_ok());
    }

    #[test]
    fn constructor_rejects_an_invalid_generation_id() {
        let d = TempDir::new().unwrap();
        assert!(OutputCache::new(d.path(), "not-a-generation", "spec", "spec-v1").is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib output_cache::`
Expected: FAIL **to compile** — `cannot find type CachedOutput in this scope`, etc.
A run that reports "0 tests" passing means the `pub mod` line is missing; fix that
before continuing, since it would hide every test in this task.

- [ ] **Step 3: Implement the module**

Write above the test module in `etl/pg-collect/src/output_cache.rs`:

```rust
//! Per-item derived-output checkpoints, so an interrupted collector run
//! resumes instead of restarting. Distinct from `source_cache.rs` (raw
//! upstream artifacts) and `cache.rs` (TTL'd API responses): this stores the
//! *triples we derived*, keyed by item + declared context + run generation.
//!
//! Local-only by design -- never synced to Minio. See the design spec at
//! docs/superpowers/specs/2026-09-12-collector-output-cache-resumability-design.md

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Serialization format of the on-disk envelope. Distinct from the stages'
/// RDF schema versions: bump this when the envelope layout changes, so a
/// serialization change does not force every adopter to bump its own
/// schema version. An unknown value is treated as a cache miss.
pub const OUTPUT_CACHE_FORMAT_VERSION: u32 = 1;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generation ids are `<UTC YYYYMMDDTHHMMSSZ>-<8 lowercase hex>`.
///
/// Defined here rather than in `checkpoint_generation` because this module is
/// the lower-level primitive: it must be able to reject an unsafe path
/// component without depending on the module that mints them.
/// `Generation::is_valid_id` delegates to this.
pub fn is_valid_generation_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.len() != 25 {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(|c| c.is_ascii_digit());
    digits(0..8)
        && b[8] == b'T'
        && digits(9..15)
        && b[15] == b'Z'
        && b[16] == b'-'
        && b[17..].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

/// One item's derived output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedOutput {
    /// The caller's logical tally. NOT derivable from `text`: write_triple
    /// auto-emits inverse statements, so output lines exceed logical writes
    /// by `auto_inverses` (see ntriples.rs).
    pub logical_triples: usize,
    /// Parent-writer counters, restored on replay rather than recomputed.
    pub skipped_invalid_iri: usize,
    pub auto_inverses: usize,
    /// Canonical N-Triples, no graph term. A String, so UTF-8 validity is
    /// settled once at capture rather than on every replay.
    pub text: String,
}

/// Whether a computation may be checkpointed. Never inferred from `Err`, and
/// never from whether a DQ issue was emitted -- a successful ecosystem
/// detection deliberately emits one (collect_spec.rs), so DQ emission says
/// nothing about transience.
pub enum ComputeOutcome {
    /// Deterministic result from a valid upstream response. Persist and replay.
    Complete(CachedOutput),
    /// Operationally inconclusive (transport error, API fault, malformed
    /// response). Use for this run's output, but never persist.
    Retryable(CachedOutput),
}

/// A stage's declared cache-context inputs. The stage owns *what* goes in;
/// this type owns *how* it is encoded, so two stages cannot disagree.
///
/// Encoding is length-prefixed per field (`<u32 len><bytes>` for both name and
/// value), which makes it injective: ("ab","c") and ("a","bc") cannot collide.
#[derive(Debug, Default, Clone)]
pub struct CanonicalContext {
    buf: Vec<u8>,
}

impl CanonicalContext {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// `tag` domain-separates the variants: without it `field("x", "0")` and
    /// `flag("x", false)` would encode identically, so swapping one for the
    /// other during a refactor would silently reuse the other's checkpoints.
    fn push(mut self, tag: u8, name: &str, value: &str) -> Self {
        self.buf.push(tag);
        self.buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(value.as_bytes());
        self
    }

    pub fn field(self, name: &str, value: &str) -> Self {
        self.push(b'F', name, value)
    }

    /// Booleans encode as "0"/"1" explicitly, never via Display -- a future
    /// type change must not be able to alter the encoding silently.
    pub fn flag(self, name: &str, value: bool) -> Self {
        self.push(b'B', name, if value { "1" } else { "0" })
    }

    /// Sorted before hashing, so element order is not significant, and
    /// length-prefixed element-wise so contents still are.
    pub fn list(mut self, name: &str, values: &[String]) -> Self {
        let mut sorted: Vec<&String> = values.iter().collect();
        sorted.sort();
        self = self.push(b'L', name, "");
        self.buf.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
        for v in sorted {
            self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(v.as_bytes());
        }
        self
    }

    pub fn digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(&self.buf);
        h.finalize().into()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub retryable: u64,
    pub write_failures: u64,
    pub integrity_failures: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: u32,
    logical_triples: usize,
    skipped_invalid_iri: usize,
    auto_inverses: usize,
    /// Hex SHA-256 over *every* field of the CachedOutput, not just `text`.
    /// The counters are restored into the parent writer and drive reported
    /// totals, so a readable corruption of one of them must invalidate the
    /// entry too. `fs::read` succeeding does not mean these are the bytes we
    /// wrote.
    digest: String,
    len: usize,
    text: String,
}

/// Canonical digest of a whole `CachedOutput`, length-prefixed so the
/// counters cannot be confused with each other or with the text.
fn output_digest(o: &CachedOutput) -> String {
    let mut h = Sha256::new();
    h.update((o.logical_triples as u64).to_le_bytes());
    h.update((o.skipped_invalid_iri as u64).to_le_bytes());
    h.update((o.auto_inverses as u64).to_le_bytes());
    h.update((o.text.len() as u64).to_le_bytes());
    h.update(o.text.as_bytes());
    hex(&h.finalize())
}

pub struct OutputCache {
    /// `None` when disabled (no --cache-dir): compute-through, no filesystem.
    base_dir: Option<PathBuf>,
    hits: Cell<u64>,
    misses: Cell<u64>,
    retryable: Cell<u64>,
    write_failures: Cell<u64>,
    integrity_failures: Cell<u64>,
}

impl OutputCache {
    pub fn new(
        cache_dir: &Path,
        generation: &str,
        stage: &str,
        schema_version: &str,
    ) -> io::Result<Self> {
        // This is a shared public primitive, so it enforces its own path
        // invariant rather than trusting every caller to have validated.
        if !is_valid_generation_id(generation) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid checkpoint generation id: {generation:?}"),
            ));
        }
        for (label, part) in [("stage", stage), ("schema_version", schema_version)] {
            let bad = part.is_empty()
                || part == "."
                || part == ".."
                || part.contains('/')
                || part.contains('\\');
            if bad {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{label} must be a single safe path component, got {part:?}"),
                ));
            }
        }

        let base = cache_dir
            .join("output")
            .join(generation)
            .join(stage)
            .join(schema_version);
        std::fs::create_dir_all(&base)?;
        Ok(Self {
            base_dir: Some(base),
            hits: Cell::new(0),
            misses: Cell::new(0),
            retryable: Cell::new(0),
            write_failures: Cell::new(0),
            integrity_failures: Cell::new(0),
        })
    }

    pub fn disabled() -> Self {
        Self {
            base_dir: None,
            hits: Cell::new(0),
            misses: Cell::new(0),
            retryable: Cell::new(0),
            write_failures: Cell::new(0),
            integrity_failures: Cell::new(0),
        }
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.get(),
            misses: self.misses.get(),
            retryable: self.retryable.get(),
            write_failures: self.write_failures.get(),
            integrity_failures: self.integrity_failures.get(),
        }
    }

    /// `sha256(sha256(context) || sha256(item_key))` over fixed-width binary
    /// digests -- field boundaries are unambiguous, unlike raw concatenation.
    pub fn entry_path(&self, item_key: &str, context: &CanonicalContext) -> Option<PathBuf> {
        let base = self.base_dir.as_ref()?;
        let mut item = Sha256::new();
        item.update(item_key.as_bytes());
        let item: [u8; 32] = item.finalize().into();

        let mut outer = Sha256::new();
        outer.update(context.digest());
        outer.update(item);
        let digest: [u8; 32] = outer.finalize().into();
        Some(base.join(hex(&digest)))
    }

    pub fn temp_path(&self) -> PathBuf {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".tmp.{}.{}", std::process::id(), n);
        match &self.base_dir {
            Some(b) => b.join(name),
            None => PathBuf::from(name),
        }
    }

    pub fn get_or_compute<F>(
        &self,
        item_key: &str,
        context: &CanonicalContext,
        compute: F,
    ) -> io::Result<CachedOutput>
    where
        F: FnOnce() -> io::Result<ComputeOutcome>,
    {
        let path = match self.entry_path(item_key, context) {
            Some(p) => p,
            // Disabled: compute-through, no filesystem, no counters beyond misses.
            None => {
                self.misses.set(self.misses.get() + 1);
                return Ok(match compute()? {
                    ComputeOutcome::Complete(o) | ComputeOutcome::Retryable(o) => o,
                });
            }
        };

        if let Some(hit) = self.read_entry(&path) {
            self.hits.set(self.hits.get() + 1);
            return Ok(hit);
        }
        self.misses.set(self.misses.get() + 1);

        match compute()? {
            ComputeOutcome::Retryable(o) => {
                self.retryable.set(self.retryable.get() + 1);
                Ok(o)
            }
            ComputeOutcome::Complete(o) => {
                // Best-effort: a persistence failure must never discard
                // derived triples or fail the run.
                if let Err(e) = self.write_entry(&path, &o) {
                    self.write_failures.set(self.write_failures.get() + 1);
                    eprintln!("Warning: checkpoint write failed for {}: {}", path.display(), e);
                }
                Ok(o)
            }
        }
    }

    /// `None` on any of: absent, unreadable, unparseable, unknown format
    /// version, length mismatch, digest mismatch. A corrupt entry is evicted
    /// so it cannot poison later lookups (mirrors source_cache.rs's
    /// self-healing manifest read).
    fn read_entry(&self, path: &Path) -> Option<CachedOutput> {
        let raw = std::fs::read_to_string(path).ok()?;
        let bad = |s: &Self| {
            s.integrity_failures.set(s.integrity_failures.get() + 1);
            let _ = std::fs::remove_file(path);
            None::<CachedOutput>
        };
        let env: Envelope = match serde_json::from_str(&raw) {
            Ok(e) => e,
            Err(_) => return bad(self),
        };
        if env.format != OUTPUT_CACHE_FORMAT_VERSION || env.len != env.text.len() {
            return bad(self);
        }
        let out = CachedOutput {
            logical_triples: env.logical_triples,
            skipped_invalid_iri: env.skipped_invalid_iri,
            auto_inverses: env.auto_inverses,
            text: env.text,
        };
        if output_digest(&out) != env.digest {
            return bad(self);
        }
        Some(out)
    }

    /// Temp file + fsync + rename, so an interrupted write can never leave a
    /// truncated entry that looks valid.
    fn write_entry(&self, path: &Path, o: &CachedOutput) -> io::Result<()> {
        let env = Envelope {
            format: OUTPUT_CACHE_FORMAT_VERSION,
            logical_triples: o.logical_triples,
            skipped_invalid_iri: o.skipped_invalid_iri,
            auto_inverses: o.auto_inverses,
            digest: output_digest(o),
            len: o.text.len(),
            text: o.text.clone(),
        };
        let body = serde_json::to_vec(&env)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let tmp = self.temp_path();
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
```

(The `pub mod output_cache;` line was already added in Step 1.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib output_cache::`
Expected: PASS, 18 tests.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/output_cache.rs etl/pg-collect/src/lib.rs
git commit -m "feat(etl): add OutputCache, the per-item derived-output checkpoint

Canonical length-prefixed context encoding (injective, unlike raw
concatenation), versioned integrity-checked envelope, atomic writes,
best-effort persistence that never discards derived triples, and an
explicit disabled mode for runs without --cache-dir."
```

---

### Task 3: Run generation lifecycle and `checkpoint commit`

Scopes checkpoints to one in-flight run. Without it, entries would be reused
across every future scheduled run, permanently bypassing spec re-fetch, the Koji
30-day TTL, and late-arriving signatures.

**Files:**
- Create: `etl/pg-collect/src/checkpoint_generation.rs`
- Modify: `etl/pg-collect/src/lib.rs` (add `pub mod checkpoint_generation;`)
- Modify: `etl/pg-collect/src/main.rs` (add `Checkpoint` subcommand + handler)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `AcquiredGeneration { id: String, reused: bool }`
  - `Generation::acquire(cache_dir: &Path) -> io::Result<AcquiredGeneration>` —
    the generation to use, minting+pruning or reusing as appropriate. `reused`
    distinguishes a resumed run from a fresh one, which the orchestrator logs.
  - `Generation::commit(cache_dir: &Path) -> io::Result<()>` — flips status to
    `complete`.
  - `Generation::is_valid_id(id: &str) -> bool`
  - CLI: `pg-collect checkpoint commit --cache-dir <dir>`

- [ ] **Step 1: Declare the module, then write the failing tests**

First add to `etl/pg-collect/src/lib.rs`:

```rust
pub mod checkpoint_generation;
```

As in Task 2, declaring it before the red run is what makes the red run real.

Then create `etl/pg-collect/src/checkpoint_generation.rs` with only this test
module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fresh_dir_mints_a_valid_generation() {
        let d = TempDir::new().unwrap();
        let g = Generation::acquire(d.path()).unwrap();
        assert!(Generation::is_valid_id(&g.id), "minted id must be valid: {}", g.id);
        assert!(d.path().join("output").join(&g.id).is_dir());
    }

    #[test]
    fn an_active_generation_is_reused() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        let b = Generation::acquire(d.path()).unwrap().id;
        assert_eq!(a, b, "an interrupted run must resume its own generation");
    }

    #[test]
    fn commit_then_acquire_mints_a_new_generation_and_prunes_the_old() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        std::fs::write(d.path().join("output").join(&a).join("leftover"), b"x").unwrap();
        Generation::commit(d.path()).unwrap();
        let b = Generation::acquire(d.path()).unwrap().id;
        assert_ne!(a, b);
        assert!(!d.path().join("output").join(&a).exists(), "old generation must be pruned");
        assert!(d.path().join("output").join(&b).is_dir());
    }

    #[test]
    fn two_mints_in_the_same_second_do_not_collide() {
        // The collision a bare one-second timestamp would allow: a completing
        // run and a new invocation would share a directory, and pruning would
        // preserve the old fragments as if they were the new run's.
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        Generation::commit(d.path()).unwrap();
        let b = Generation::acquire(d.path()).unwrap().id;
        Generation::commit(d.path()).unwrap();
        let c = Generation::acquire(d.path()).unwrap().id;
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn malformed_state_mints_fresh_instead_of_reusing() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        std::fs::write(d.path().join("output").join("GENERATION"), b"not json at all").unwrap();
        let b = Generation::acquire(d.path()).unwrap();
        assert_ne!(a, b.id, "unauthenticatable state must not be reused");
        assert!(Generation::is_valid_id(&b.id));
    }

    #[test]
    fn a_traversal_id_in_state_is_rejected() {
        let d = TempDir::new().unwrap();
        Generation::acquire(d.path()).unwrap();
        std::fs::write(
            d.path().join("output").join("GENERATION"),
            br#"{"id":"../../etc","status":"active"}"#,
        ).unwrap();
        let g = Generation::acquire(d.path()).unwrap();
        assert!(Generation::is_valid_id(&g.id));
        assert!(!g.id.contains(".."), "id is used as a path component");
    }

    #[test]
    fn id_validation_rejects_dangerous_and_malformed_values() {
        assert!(Generation::is_valid_id("20260912T010203Z-abcdef01"));
        for bad in ["", ".", "..", "../x", "a/b", "20260912T010203Z", "20260912T010203Z-ABCDEF01",
                    "20260912T010203Z-abcdef0", "20260912T010203Z-abcdef012"] {
            assert!(!Generation::is_valid_id(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn commit_without_a_generation_is_an_error_not_a_panic() {
        let d = TempDir::new().unwrap();
        assert!(Generation::commit(d.path()).is_err());
    }

    #[test]
    fn unknown_fields_in_state_invalidate_it() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        std::fs::write(
            d.path().join("output").join("GENERATION"),
            format!(r#"{{"id":"{a}","status":"active","injected":"surprise"}}"#),
        ).unwrap();
        let b = Generation::acquire(d.path()).unwrap();
        assert_ne!(a, b.id, "state with unknown fields must not be trusted");
    }

    #[test]
    fn unknown_status_invalidates_state() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        std::fs::write(
            d.path().join("output").join("GENERATION"),
            format!(r#"{{"id":"{a}","status":"halfway"}}"#),
        ).unwrap();
        let b = Generation::acquire(d.path()).unwrap();
        assert_ne!(a, b.id);
    }

    #[test]
    fn acquire_reports_whether_it_reused_or_minted() {
        let d = TempDir::new().unwrap();
        let first = Generation::acquire(d.path()).unwrap();
        assert!(!first.reused, "a fresh directory mints");
        let second = Generation::acquire(d.path()).unwrap();
        assert!(second.reused, "an active generation is reused");
        Generation::commit(d.path()).unwrap();
        let third = Generation::acquire(d.path()).unwrap();
        assert!(!third.reused, "a committed generation is retired, not reused");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib checkpoint_generation::`
Expected: FAIL to compile — `Generation` undefined.

- [ ] **Step 3: Implement the module**

Write above the test module in `etl/pg-collect/src/checkpoint_generation.rs`:

```rust
//! Run-generation lifecycle for output checkpoints.
//!
//! A generation scopes checkpoints to one in-flight run: retries of an
//! interrupted run reuse it, and a successful publication retires it. Without
//! this, checkpoints would be reused across every future scheduled run,
//! permanently bypassing spec re-fetch, the Koji 30-day TTL, and signatures
//! that appear after a build was first observed.

use serde::{Deserialize, Serialize};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// `deny_unknown_fields` is load-bearing, not stylistic: the design requires
/// state we cannot structurally authenticate to be discarded rather than
/// trusted, and serde accepts unknown fields by default.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    id: String,
    status: String,
}

/// The generation this run should use, and how we came by it.
pub struct AcquiredGeneration {
    pub id: String,
    /// True when we resumed an interrupted run's generation rather than
    /// minting. Logged, so an operator can tell a resume from a fresh start.
    pub reused: bool,
}

pub struct Generation;

impl Generation {
    /// Ids are `<UTC YYYYMMDDTHHMMSSZ>-<8 hex>`. The timestamp is for human
    /// legibility only; uniqueness comes from the random suffix plus the
    /// exclusive directory create in `acquire`.
    ///
    /// Delegates to `output_cache`, which owns the format because it must
    /// reject an unsafe path component without depending on this module.
    pub fn is_valid_id(id: &str) -> bool {
        crate::output_cache::is_valid_generation_id(id)
    }

    fn output_dir(cache_dir: &Path) -> PathBuf {
        cache_dir.join("output")
    }

    fn state_path(cache_dir: &Path) -> PathBuf {
        Self::output_dir(cache_dir).join("GENERATION")
    }

    /// Reads state only if it is structurally authenticatable. Anything else --
    /// unparseable, unknown status, an id that fails validation -- returns None
    /// so the caller mints fresh rather than trusting it.
    fn read_state(cache_dir: &Path) -> Option<State> {
        let path = Self::state_path(cache_dir);
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None, // first run
            Err(e) => {
                eprintln!("Warning: cannot read checkpoint state {}: {}", path.display(), e);
                return None;
            }
        };
        // Every invalidation is logged: a silently discarded state looks
        // identical to a first run, which would hide a recurring corruption.
        let s: State = match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Warning: ignoring unparseable checkpoint state (or unknown fields): {e}");
                return None;
            }
        };
        if !Self::is_valid_id(&s.id) {
            eprintln!("Warning: ignoring checkpoint state with invalid generation id");
            return None;
        }
        if s.status != "active" && s.status != "complete" {
            eprintln!("Warning: ignoring checkpoint state with unknown status {:?}", s.status);
            return None;
        }
        Some(s)
    }

    fn write_state(cache_dir: &Path, state: &State) -> io::Result<()> {
        let dir = Self::output_dir(cache_dir);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".GENERATION.{}.tmp", std::process::id()));
        let body = serde_json::to_vec(state)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, Self::state_path(cache_dir))
    }

    fn mint_id() -> String {
        let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        // The crate has no `rand` dependency by design; seed from RandomState
        // the same way http_transport.rs does for its jitter.
        let mut h = RandomState::new().build_hasher();
        h.write_u64(std::process::id() as u64);
        format!("{}-{:08x}", now, (h.finish() & 0xffff_ffff) as u32)
    }

    /// Returns the generation id this run should use.
    ///
    /// Reuses an `active` generation (a retry of an interrupted run); otherwise
    /// mints a new one and prunes every other generation directory. Pruning at
    /// mint time, not at commit, means a crash between publication and commit
    /// leaves the prior generation intact -- costing one redundant
    /// re-collection rather than losing data.
    pub fn acquire(cache_dir: &Path) -> io::Result<AcquiredGeneration> {
        if let Some(s) = Self::read_state(cache_dir) {
            if s.status == "active" && Self::output_dir(cache_dir).join(&s.id).is_dir() {
                return Ok(AcquiredGeneration { id: s.id, reused: true });
            }
        }

        // Exclusive create is the uniqueness authority: if the directory
        // already exists we lost a race (or collided), so re-mint.
        let dir = Self::output_dir(cache_dir);
        std::fs::create_dir_all(&dir)?;
        let mut id = String::new();
        for _ in 0..64 {
            let candidate = Self::mint_id();
            match std::fs::create_dir(dir.join(&candidate)) {
                Ok(()) => {
                    id = candidate;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        if id.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "could not mint a unique checkpoint generation id",
            ));
        }

        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_name() != std::ffi::OsStr::new(&id) && entry.path().is_dir() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }

        Self::write_state(cache_dir, &State { id: id.clone(), status: "active".into() })?;
        Ok(AcquiredGeneration { id, reused: false })
    }

    /// Marks the active generation complete. Called only after a successful
    /// publication, so a run that collected everything but failed to publish
    /// stays resumable.
    pub fn commit(cache_dir: &Path) -> io::Result<()> {
        let s = Self::read_state(cache_dir).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no valid checkpoint generation to commit")
        })?;
        Self::write_state(cache_dir, &State { id: s.id, status: "complete".into() })
    }
}
```

Add to `etl/pg-collect/src/lib.rs`:

```rust
pub mod checkpoint_generation;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib checkpoint_generation::`
Expected: PASS, 11 tests.

- [ ] **Step 5: Add the `checkpoint commit` subcommand**

In `etl/pg-collect/src/main.rs`, add to `enum Commands` (after the `RpmFull`
variant):

```rust
    /// Checkpoint lifecycle management for resumable collectors
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointAction,
    },
```

Add this enum next to `enum Commands`:

```rust
#[derive(clap::Subcommand)]
enum CheckpointAction {
    /// Mark the active run generation complete. Run only after a successful
    /// upload -- a run that collected everything but failed to publish must
    /// stay resumable.
    Commit {
        #[arg(long)]
        cache_dir: String,
    },
}
```

Add the handler alongside the other `Commands::` arms. `fn main()` returns `()`
(`main.rs:1434`), so `?` cannot be used directly in an arm — wrap it in an
immediately-invoked closure, matching how the existing compound arms (e.g.
`Commands::RpmFull`) do it:

```rust
        Commands::Checkpoint { action } => match action {
            CheckpointAction::Commit { cache_dir } => {
                (|| -> std::io::Result<(usize, usize)> {
                    match pg_collect::checkpoint_generation::Generation::commit(
                        std::path::Path::new(&cache_dir),
                    ) {
                        Ok(()) => eprintln!("Checkpoint generation committed for {}", cache_dir),
                        // "Nothing to commit" is a success, not a failure. When
                        // checkpoint setup degraded to disabled, the collection
                        // and upload still succeeded, and the wrapper runs this
                        // unconditionally under `set -e` -- erroring here would
                        // kill the script after a good publication and skip the
                        // final cache mirror.
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            eprintln!(
                                "No active checkpoint generation in {} — nothing to commit",
                                cache_dir
                            );
                        }
                        Err(e) => return Err(e),
                    }
                    Ok((0, 0))
                })()
            }
        },
```

The library keeps its precise `NotFound` signal; only the CLI, which is what the
wrapper invokes, treats it as a no-op.

- [ ] **Step 6: Verify the CLI works end to end, including the empty case**

```bash
cargo build --manifest-path etl/pg-collect/Cargo.toml
D=$(mktemp -d)
mkdir -p "$D/output/20260912T010203Z-abcdef01"
printf '{"id":"20260912T010203Z-abcdef01","status":"active"}' > "$D/output/GENERATION"
etl/pg-collect/target/debug/pg-collect checkpoint commit --cache-dir "$D"
cat "$D/output/GENERATION"

# The degraded-checkpointing path: nothing to commit must exit 0, or the
# wrapper's `set -e` would abort after a successful upload.
E=$(mktemp -d)
etl/pg-collect/target/debug/pg-collect checkpoint commit --cache-dir "$E"
echo "exit=$?"
```
Expected: the first prints `Checkpoint generation committed` and the state file
shows `"status":"complete"`; the second prints `nothing to commit` and `exit=0`.

- [ ] **Step 7: Run the full suite and commit**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml`

```bash
git add etl/pg-collect/src/checkpoint_generation.rs etl/pg-collect/src/lib.rs etl/pg-collect/src/main.rs
git commit -m "feat(etl): add run-generation lifecycle for output checkpoints

Scopes checkpoints to one in-flight run: retries reuse the active
generation, a successful publication retires it via 'pg-collect checkpoint
commit'. Ids carry a random suffix and are claimed by exclusive directory
creation, so two mints in the same clock second cannot collide -- a bare
timestamp would let a completing run and a new invocation share a
directory that pruning then preserves. Generation state is structurally
validated before its id is used as a path component."
```

---

### Task 4: `SpecFetchResult` — stop collapsing transport errors into `NotFound`

`fetch_spec` (`collect_spec.rs:314`) tries several candidate URLs and, on
exhaustion, returns `ErrorKind::NotFound` for *every* failure mode including
transport errors. The `last_err` detail added in PR #38 is diagnostic text, not a
machine-readable distinction. Without this, a transient spec failure would be
checkpointed as a definitive "this SRPM has no spec".

`fetch_url` already preserves the distinction (`collect_spec.rs:408-419`: only a
real 404 becomes `ErrorKind::NotFound`), so this task only has to stop discarding
it.

**Files:**
- Modify: `etl/pg-collect/src/collect_spec.rs` (`fetch_spec:314-341`)
- Test: `etl/pg-collect/src/collect_spec.rs` (append to existing `mod tests`)

**Interfaces:**
- Produces: `pub enum SpecFetchResult { Found(String), NotFound, RetryableFailure(String) }`
  and `fn fetch_spec(&self, source_name: &str) -> SpecFetchResult` (no longer
  `Result<String>`).

- [ ] **Step 1: Write the failing test**

Append to `collect_spec.rs`'s existing `mod tests`. `spec_urls` returns an empty
vec for unknown distros, which is the no-candidates case:

The aggregation rule — not the variants — is what must be tested, so extract it
into a pure helper that takes per-candidate outcomes. Constructing the enum
variants directly would assert nothing about behavior.

```rust
#[test]
fn all_404s_aggregate_to_not_found() {
    let outcomes = vec![CandidateOutcome::Missing, CandidateOutcome::Missing];
    assert!(matches!(aggregate_candidates("pkg", 2, outcomes), SpecFetchResult::NotFound));
}

#[test]
fn one_inconclusive_candidate_poisons_the_aggregate() {
    // The case that matters: a 404 on one branch plus a transport error on
    // another is NOT evidence that this SRPM has no spec.
    let outcomes = vec![
        CandidateOutcome::Missing,
        CandidateOutcome::Inconclusive("connection reset".into()),
    ];
    assert!(matches!(aggregate_candidates("pkg", 2, outcomes), SpecFetchResult::RetryableFailure(_)));
}

#[test]
fn a_later_success_wins_over_an_earlier_failure() {
    let outcomes = vec![
        CandidateOutcome::Inconclusive("5xx".into()),
        CandidateOutcome::Found("Name: pkg".into()),
    ];
    match aggregate_candidates("pkg", 2, outcomes) {
        SpecFetchResult::Found(c) => assert_eq!(c, "Name: pkg"),
        _ => panic!("a successful fallback must win"),
    }
}

#[test]
fn no_candidate_urls_is_not_found() {
    assert!(matches!(aggregate_candidates("pkg", 0, vec![]), SpecFetchResult::NotFound));
}

#[test]
fn fetch_spec_reports_an_unsupported_distro_as_not_found() {
    // spec_urls returns no candidates for an unknown distro.
    let c = SpecCollector::new("not-a-distro", "1", None).unwrap();
    assert!(matches!(c.fetch_spec("anything"), SpecFetchResult::NotFound));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib collect_spec::tests::all_404s_aggregate_to_not_found -- --exact`
Expected: FAIL to compile — `SpecFetchResult` / `aggregate_candidates` undefined.

- [ ] **Step 3: Implement**

Add near the top of `collect_spec.rs`, after the `SpecData` struct:

```rust
/// Outcome of trying every candidate spec URL for one SRPM.
///
/// The distinction matters for checkpointing: `NotFound` is a deterministic
/// answer about this SRPM and may be checkpointed, while `RetryableFailure`
/// is operationally inconclusive and must not be.
pub enum SpecFetchResult {
    Found(String),
    /// Every candidate URL returned a definitive 404.
    NotFound,
    /// At least one candidate failed inconclusively (transport error, 5xx,
    /// malformed) and none succeeded.
    RetryableFailure(String),
}
```

Add the per-candidate outcome type and the pure aggregation rule, so the logic is
testable without a network or an injectable transport:

```rust
/// What one candidate URL produced.
pub enum CandidateOutcome {
    Found(String),
    /// A definitive 404.
    Missing,
    /// Anything else: transport error, 5xx, malformed body.
    Inconclusive(String),
}

/// Reduce per-candidate outcomes to one answer for this SRPM.
///
/// A definitive `NotFound` requires EVERY candidate to have 404'd: one
/// inconclusive candidate means we cannot claim this SRPM has no spec, which
/// is exactly the distinction checkpointing depends on.
pub fn aggregate_candidates(
    source_name: &str,
    url_count: usize,
    outcomes: Vec<CandidateOutcome>,
) -> SpecFetchResult {
    let mut inconclusive: Option<String> = None;
    for o in outcomes {
        match o {
            CandidateOutcome::Found(c) => return SpecFetchResult::Found(c),
            CandidateOutcome::Missing => continue,
            CandidateOutcome::Inconclusive(d) => inconclusive = Some(d),
        }
    }
    match inconclusive {
        Some(detail) => SpecFetchResult::RetryableFailure(format!(
            "spec fetch inconclusive for {} across {} URLs: {}",
            source_name, url_count, detail
        )),
        None => SpecFetchResult::NotFound,
    }
}
```

Replace `fetch_spec` (`collect_spec.rs:314-341`) with a thin shell over it:

```rust
    fn fetch_spec(&self, source_name: &str) -> SpecFetchResult {
        let urls = self.spec_urls(source_name);
        // Stop at the first success, exactly as before. Eagerly mapping every
        // candidate would triple spec-fetch traffic (three URLs per SRPM,
        // tens of thousands of SRPMs per run).
        let mut outcomes = Vec::with_capacity(urls.len());
        for url in &urls {
            let outcome = match self.fetch_url(url, source_name) {
                Ok(content) => CandidateOutcome::Found(content),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => CandidateOutcome::Missing,
                Err(e) => CandidateOutcome::Inconclusive(format!("{} ({})", url, e)),
            };
            let done = matches!(outcome, CandidateOutcome::Found(_));
            outcomes.push(outcome);
            if done {
                break;
            }
        }
        aggregate_candidates(source_name, urls.len(), outcomes)
    }
```

Split `process_spec` into a fetching shell and a content-taking body. Task 5 needs
the split so it can classify the fetch and derive from the content **without
fetching twice** — without it, the disabled-cache path would make two real network
requests per SRPM.

Change `process_spec` to fetch and delegate. Everything from `let spec =
parse_spec(&spec_content);` (`collect_spec.rs:190`) to the end of the current body
moves verbatim into `process_spec_with_content`, unchanged:

```rust
    fn process_spec<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        source_name: &str,
        identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
    ) -> Result<usize> {
        let spec_content = match self.fetch_spec(source_name) {
            SpecFetchResult::Found(content) => content,
            SpecFetchResult::NotFound => {
                eprintln!("  {} → no spec in dist-git", source_name);
                return emit_dq_issue(
                    writer, "collect-spec", "spec-file", source_name,
                    "spec-fetch-failed", "warning",
                );
            }
            SpecFetchResult::RetryableFailure(detail) => {
                eprintln!("  {} → spec fetch inconclusive: {}", source_name, detail);
                return emit_dq_issue(
                    writer, "collect-spec", "spec-file", source_name,
                    "spec-fetch-failed", "warning",
                );
            }
        };
        self.process_spec_with_content(
            writer, source_name, &spec_content, identity_map,
            existing_ecosystem_pkgs, emit_buildrequires, emit_maintainers,
        )
    }

    /// Derivation only -- the caller has already fetched the spec. Split out so
    /// the checkpointed path can classify the fetch and derive from its result
    /// without issuing a second request.
    #[allow(clippy::too_many_arguments)]
    fn process_spec_with_content<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        source_name: &str,
        spec_content: &str,
        identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
    ) -> Result<usize> {
        let spec = parse_spec(spec_content);
        let mut triples = 0;
        // ... rest of the original process_spec body, verbatim ...
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib collect_spec::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/collect_spec.rs
git commit -m "feat(etl): give fetch_spec a typed result

It collapsed every failure -- transport errors included -- into
ErrorKind::NotFound, so a transient blip was indistinguishable from 'this
SRPM has no spec'. fetch_url already preserved the distinction; fetch_spec
now propagates it, which the checkpoint outcome model depends on."
```

---

### Task 5: Checkpoint the spec stage

**Files:**
- Modify: `etl/pg-collect/src/collect_spec.rs` (`collect`, `process_spec`, add `SPEC_SCHEMA_VERSION`)
- Test: `etl/pg-collect/src/collect_spec.rs`

**Interfaces:**
- Consumes: `OutputCache`, `CachedOutput`, `ComputeOutcome`, `CanonicalContext` (Task 2); `SpecFetchResult` (Task 4); generic writers (Task 1).
- Produces: `pub const SPEC_SCHEMA_VERSION: &str = "spec-v1";` and
  `SpecCollector::collect_checkpointed<W: Write>(&self, writer: &mut NTriplesWriter<W>, srpm_names, srpm_identity_map, existing_ecosystem_pkgs, emit_buildrequires, emit_maintainers, checkpoint: &OutputCache) -> Result<(usize, usize)>`.
  `collect` remains, delegating with `OutputCache::disabled()`.

- [ ] **Step 1: Write the failing tests**

Append to `collect_spec.rs`'s `mod tests`:

```rust
#[test]
fn spec_cache_path_carries_its_own_schema_version() {
    // Non-vacuous version check: assert the path actually contains THIS
    // stage's version segment, and that changing only the version changes
    // the path. Asserting merely that spec and koji paths differ would pass
    // even if both stages shared one constant, since the stage segment
    // already differs. Task 6 adds the mirror of this test for koji.
    let d = tempfile::TempDir::new().unwrap();
    let ctx = crate::output_cache::CanonicalContext::new();
    let mine = crate::output_cache::OutputCache::new(
        d.path(), "20260912T010203Z-abcdef01", "spec", SPEC_SCHEMA_VERSION).unwrap();
    let bumped = crate::output_cache::OutputCache::new(
        d.path(), "20260912T010203Z-abcdef01", "spec", "spec-vNEXT").unwrap();
    let p = mine.entry_path("k", &ctx).unwrap();
    assert!(p.to_string_lossy().contains(SPEC_SCHEMA_VERSION), "got {p:?}");
    assert_ne!(p, bumped.entry_path("k", &ctx).unwrap(),
        "a version bump must invalidate existing checkpoints");
}

#[test]
fn collect_checkpointed_replays_a_prepopulated_item_without_fetching() {
    // Drives the REAL loop, not OutputCache in isolation. Only this shape can
    // catch a wrong stage context, a fetch still happening on a hit, lost
    // counter propagation, or output not going through write_raw_line.
    //
    // Distro "offline-test" is deliberately unsupported: spec_urls returns no
    // candidates for it, so this test can never issue a network request. A hit
    // replays the checkpoint; a miss would produce a NotFound DQ fragment
    // instead. Either way the assertions below distinguish them deterministically,
    // with no dependency on the live Fedora service.
    use crate::output_cache::{CachedOutput, ComputeOutcome, OutputCache};
    let d = tempfile::TempDir::new().unwrap();
    let cache = OutputCache::new(d.path(), "20260912T010203Z-abcdef01", "spec", SPEC_SCHEMA_VERSION).unwrap();

    let c = SpecCollector::new("offline-test", "44", None).unwrap();
    let mut names = HashSet::new();
    names.insert("zlib".to_string());
    let identity_map: HashMap<String, Vec<String>> = HashMap::new();
    let existing: HashSet<String> = HashSet::new();

    // Pre-populate using the exact context the loop will compute, so the
    // lookup only hits if the stage builds its context identically.
    let ctx = crate::output_cache::CanonicalContext::new()
        .field("distro", "offline-test")
        .field("release", "44")
        .list("identities", &[])
        .flag("in_existing_ecosystem", false)
        .flag("emit_buildrequires", false)
        .flag("emit_maintainers", false);
    cache.get_or_compute("zlib", &ctx, || Ok(ComputeOutcome::Complete(CachedOutput {
        logical_triples: 4,
        skipped_invalid_iri: 1,
        auto_inverses: 2,
        text: "<s> <p> <o> .\n".into(),
    }))).unwrap();

    let mut w = crate::ntriples::NTriplesWriter::new(Vec::<u8>::new());
    let (specs, triples) = c.collect_checkpointed(
        &mut w, &names, &identity_map, &existing, false, false, &cache,
    ).unwrap();
    // A NotFound DQ fragment would have non-zero triples but different text,
    // so the assertions below are what separate a hit from a silent miss.

    assert_eq!((specs, triples), (1, 4), "totals must come from the checkpoint");
    assert_eq!(w.skipped_invalid_iri, 1, "counters must propagate on replay");
    assert_eq!(w.auto_inverses, 2);
    assert_eq!(w.into_string().unwrap(), "<s> <p> <o> .\n",
        "replayed text must reach the output writer verbatim");
    assert_eq!(cache.stats().hits, 1);
    assert_eq!(cache.stats().misses, 1, "only the pre-population miss");
}

#[test]
fn a_different_context_does_not_hit_the_checkpoint() {
    // Guards the context fingerprint: flipping a declared input must miss.
    // Same offline-only distro, so the miss cannot reach the network.
    use crate::output_cache::{CachedOutput, ComputeOutcome, OutputCache};
    let d = tempfile::TempDir::new().unwrap();
    let cache = OutputCache::new(d.path(), "20260912T010203Z-abcdef01", "spec", SPEC_SCHEMA_VERSION).unwrap();
    let ctx = crate::output_cache::CanonicalContext::new()
        .field("distro", "offline-test").field("release", "44")
        .list("identities", &[])
        .flag("in_existing_ecosystem", false)
        .flag("emit_buildrequires", false)   // <- differs from the call below
        .flag("emit_maintainers", false);
    cache.get_or_compute("zlib", &ctx, || Ok(ComputeOutcome::Complete(CachedOutput {
        logical_triples: 4, skipped_invalid_iri: 0, auto_inverses: 0, text: "<s> <p> <o> .\n".into(),
    }))).unwrap();

    let c = SpecCollector::new("offline-test", "44", None).unwrap();
    let mut names = HashSet::new();
    names.insert("zlib".to_string());
    let mut w = crate::ntriples::NTriplesWriter::new(Vec::<u8>::new());
    // emit_buildrequires = true this time.
    let _ = c.collect_checkpointed(
        &mut w, &names, &HashMap::new(), &HashSet::new(), true, false, &cache,
    );
    assert_eq!(cache.stats().hits, 0, "a changed declared input must not hit");
}

```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib collect_spec::tests::spec_cache_path_carries_its_own_schema_version -- --exact`
Expected: FAIL to compile — `SPEC_SCHEMA_VERSION` undefined.

- [ ] **Step 3: Add the schema version constant**

Near the top of `collect_spec.rs`:

```rust
/// Bump this for any change to this stage's emitted triples, including
/// changes to shared serialization or ontology helpers it calls. A stale
/// checkpoint fragment is indistinguishable from a correct one -- there is
/// no automatic detection. Emission changes and a bump belong in the same
/// patch.
pub const SPEC_SCHEMA_VERSION: &str = "spec-v1";
```

- [ ] **Step 4: Add `collect_checkpointed` and make `collect` delegate**

Replace the body of `collect` so both paths share one implementation:

```rust
    pub fn collect<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        srpm_names: &HashSet<String>,
        srpm_identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
    ) -> Result<(usize, usize)> {
        self.collect_checkpointed(
            writer, srpm_names, srpm_identity_map, existing_ecosystem_pkgs,
            emit_buildrequires, emit_maintainers,
            &crate::output_cache::OutputCache::disabled(),
        )
    }

    /// As `collect`, but replays per-item fragments from `checkpoint` so an
    /// interrupted run resumes instead of restarting.
    #[allow(clippy::too_many_arguments)]
    pub fn collect_checkpointed<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        srpm_names: &HashSet<String>,
        srpm_identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
        checkpoint: &crate::output_cache::OutputCache,
    ) -> Result<(usize, usize)> {
        use crate::output_cache::{CachedOutput, CanonicalContext, ComputeOutcome};

        let mut total_specs = 0;
        let mut total_triples = 0;

        // Sorted, not HashSet order. Two processes iterate a HashSet in
        // different randomized orders, which would make "a replayed run is
        // byte-identical" untestable and, worse, produce gratuitously
        // different .nt files between runs. Sorting makes byte identity a
        // real contract.
        let mut ordered: Vec<&String> = srpm_names.iter().collect();
        ordered.sort();

        for (idx, name) in ordered.into_iter().enumerate() {
            let identities = srpm_identity_map.get(name).cloned().unwrap_or_default();
            let ctx = CanonicalContext::new()
                .field("distro", &self.distro)
                .field("release", &self.release)
                .list("identities", &identities)
                .flag("in_existing_ecosystem", existing_ecosystem_pkgs.contains(name))
                .flag("emit_buildrequires", emit_buildrequires)
                .flag("emit_maintainers", emit_maintainers);

            let result = checkpoint.get_or_compute(name, &ctx, || {
                // Derive into a scratch writer with no graph term, so the
                // fragment is canonical N-Triples and graph selection stays
                // at replay time.
                let mut scratch = NTriplesWriter::new(Vec::<u8>::new());
                let (logical, cacheability) = self.process_spec_classified(
                    &mut scratch, name, srpm_identity_map, existing_ecosystem_pkgs,
                    emit_buildrequires, emit_maintainers,
                )?;
                let out = CachedOutput {
                    logical_triples: logical,
                    skipped_invalid_iri: scratch.skipped_invalid_iri,
                    auto_inverses: scratch.auto_inverses,
                    text: scratch.into_string()?,
                };
                Ok(match cacheability {
                    Cacheability::Complete => ComputeOutcome::Complete(out),
                    Cacheability::Retryable => ComputeOutcome::Retryable(out),
                })
            });

            match result {
                Ok(out) => {
                    for line in out.text.lines() {
                        writer.write_raw_line(line)?;
                    }
                    writer.skipped_invalid_iri += out.skipped_invalid_iri;
                    writer.auto_inverses += out.auto_inverses;
                    if out.logical_triples > 0 {
                        total_specs += 1;
                        total_triples += out.logical_triples;
                    }
                }
                Err(e) => eprintln!("  {} → error: {}", name, e),
            }

            if (idx + 1) % 100 == 0 {
                eprintln!(
                    "Processed {}/{} spec files ({} triples)",
                    idx + 1, srpm_names.len(), total_triples
                );
            }
        }

        let s = checkpoint.stats();
        eprintln!(
            "Spec collection complete: {} specs, {} triples \
             (checkpoint: {} hits, {} misses, {} retryable, {} write-fail, {} integrity-fail)",
            total_specs, total_triples,
            s.hits, s.misses, s.retryable, s.write_failures, s.integrity_failures
        );
        Ok((total_specs, total_triples))
    }
```

- [ ] **Step 5: Add `process_spec_classified`**

Fetches exactly once, then reports both the triple count and whether the result
may be checkpointed. It uses `process_spec_with_content` (Task 4) rather than
`process_spec`, so the spec is not fetched a second time.

```rust
    /// Derivation plus an explicit cacheability verdict.
    ///
    /// Cacheability is NOT "did we emit a DQ issue" -- a successful ecosystem
    /// detection deliberately emits one (see the DQ call after detection in
    /// this file). Only an inconclusive *fetch* is non-cacheable; a definitive
    /// 404 is a real answer about this SRPM and may be checkpointed.
    #[allow(clippy::too_many_arguments)]
    fn process_spec_classified<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        source_name: &str,
        identity_map: &HashMap<String, Vec<String>>,
        existing_ecosystem_pkgs: &HashSet<String>,
        emit_buildrequires: bool,
        emit_maintainers: bool,
    ) -> Result<(usize, Cacheability)> {
        let content = match self.fetch_spec(source_name) {
            SpecFetchResult::Found(c) => c,
            SpecFetchResult::NotFound => {
                eprintln!("  {} → no spec in dist-git", source_name);
                let t = emit_dq_issue(
                    writer, "collect-spec", "spec-file", source_name,
                    "spec-fetch-failed", "warning",
                )?;
                return Ok((t, Cacheability::Complete)); // definitive answer
            }
            SpecFetchResult::RetryableFailure(detail) => {
                eprintln!("  {} → spec fetch inconclusive: {}", source_name, detail);
                let t = emit_dq_issue(
                    writer, "collect-spec", "spec-file", source_name,
                    "spec-fetch-failed", "warning",
                )?;
                return Ok((t, Cacheability::Retryable));
            }
        };
        let triples = self.process_spec_with_content(
            writer, source_name, &content, identity_map,
            existing_ecosystem_pkgs, emit_buildrequires, emit_maintainers,
        )?;
        Ok((triples, Cacheability::Complete))
    }
```

Add the enum next to `SpecFetchResult`. A named type rather than a `bool`: the
design's rule is an *explicit typed outcome*, and `(usize, bool)` makes an
inverted argument silently compile.

```rust
/// Whether a derived item may be checkpointed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cacheability {
    Complete,
    Retryable,
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib collect_spec::`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add etl/pg-collect/src/collect_spec.rs
git commit -m "feat(etl): checkpoint the spec stage per SRPM

Derivation runs against a scratch writer with no graph term, so fragments
are canonical N-Triples and graph mode is applied at replay via
write_raw_line. Writer counters are folded back on capture and restored on
replay. Only an inconclusive fetch is non-cacheable -- DQ emission is not
a transience signal, since successful ecosystem detection emits one."
```

---

### Task 6: `KojiRpcResult` — typed parsing for the whole RPC chain

A per-NVR fragment comes from three RPCs: `getBuild` (in `get_build`,
`enrich_koji.rs:231`), then `listBuildRPMs` and `queryRPMSigs` — both issued
*inline inside* `query_rpm_signatures` (`:316`; there is no `list_build_rpms`
function). All three feed `parse_xmlrpc_struct` (`:551`) or `parse_xmlrpc_array`
(`:611`), whose event loop ends with `Err(_) => break` (`:679`) — so a malformed
body yields whatever was parsed before the error, usually an empty collection,
indistinguishable from a legitimately empty result.

Validation cannot be bolted onto the outside of those parsers. Each layer of
outer checking just moves the leak inward: a substring check misses a truncated
body; a depth/tag census accepts `<params/>`; an outer payload-type check still
accepts `<array><bogus/></array>` and an array of strings, and accepts a build
struct with no `build_id`. In every case the lossy parser then returns an empty
collection that reads as a conclusive answer.

So this task **replaces** the scan-plus-lossy-parse arrangement with one typed
recursive-descent parser that validates the whole shape — root, payload type,
inner element grammar, and the fields the enrichment chain requires — and cannot
turn a structural problem into an empty-but-valid result.

Grammar validity alone is still not enough, because each RPC's *records* have
their own contract and each caller reads a different field:

- `listBuildRPMs` (`:366-370`) picks the first record whose `arch` is not
  `"src"` and reads its `id`. A record with no `arch` is treated as non-src
  and selected; a record with no `id` makes `rpm_id` `None`, which the very
  next line reads as the conclusive "this build has no binary RPMs."
- `queryRPMSigs` (`:416-420`) scans for a non-empty `sigkey`. A record with no
  `sigkey` key at all reads as the conclusive "this RPM is unsigned." An
  *empty* `sigkey` is Koji's own representation of unsigned and is a real
  answer — so this schema requires presence, not non-emptiness.
- `getBuild` (`:299`) reads `build_id` and uses it as the `<int>` argument to
  the next call, so a non-numeric value is as unusable as a missing one. Koji
  signals "no such build" by returning `None`, which the hub serializes as
  `<nil/>` — so `<nil/>` is the conclusive absence and *every* struct,
  including an empty one, must carry a usable `build_id`. The old
  `data.is_empty()` check (`:267`) called `<struct/>` "not found", which is
  precisely how a fault body became a conclusive answer.

A structurally valid but schema-invalid record — `<struct/>` as an array
element, say — therefore has exactly the same effect as the old lossy parser's
empty vec. Every array call site names the schema its records must satisfy, so
a future third array RPC cannot inherit "no validation" by default, and a
non-empty-but-invalid record is `Malformed` (retryable), never a conclusive
answer.

There are also seven `return Ok(0)` branches (`:267, :285, :357, :374, :407, :424,
:440`), several of them transport failures that must mark the item retryable.

**The 30-day source cache must be renamed, or the new parser is bypassed for
existing entries.** `get_build` consults `koji-build-<NVR>` (`:233`) and
`query_rpm_signatures` consults `koji-sigs-<build_id>` (`:322`) *before* any
HTTP response is parsed; this task only replaces parsing on the cache-miss
path. Those keys already hold output of the lossy parser: its event loop ends
`Err(_) => break` (`:598`), so a body truncated after a valid `build_id` yields
a non-empty partial map, which `cache_put` then persists locally and to Minio
for 30 days. A fresh output generation would reuse that entry, never parse
anything, and checkpoint the result as `Complete`.

Validating the cached JSON instead would not help: a truncated document can
contain every required field, so the cached map is indistinguishable from a
good one. The only sound fix is to stop reading the old entries, so both key
namespaces move under a `koji-rpc-<KOJI_RPC_CACHE_VERSION>-` prefix that the
old keys could never produce. `FileCache::key_path` hashes the whole key string
and `minio_key` derives the object name from it (`cache.rs:262`, `:288`), so a
changed key retires the old entry in both tiers at once.

**Files:**
- Modify: `etl/pg-collect/src/enrich_koji.rs`
- Test: `etl/pg-collect/src/enrich_koji.rs`

**Interfaces:**
- Produces:
  - `pub enum KojiRpcResult<T> { ValidNonempty(T), ValidEmpty, ApiFault(String), Malformed(String) }`
  - `pub const KOJI_SCHEMA_VERSION: &str = "koji-v1";`
  - `KojiRpcResult::is_conclusive(&self) -> bool` — true for `ValidNonempty`/`ValidEmpty`.
  - `pub enum RpcValue { Struct(HashMap<String, String>), Array(Vec<HashMap<String, String>>), Nil }`
  - `pub fn parse_response(xml: &str) -> KojiRpcResult<RpcValue>` — the full
    grammar-validating parser
  - `pub fn parse_build_response(xml: &str) -> KojiRpcResult<HashMap<String, String>>`
    — `<nil/>` is the conclusive no-build answer; every struct must carry a
    numeric `build_id`
  - `pub const KOJI_RPC_CACHE_VERSION: &str = "v2";` — namespaces the
    `koji-build-*` / `koji-sigs-*` source-cache keys, retiring entries the old
    lossy parser wrote
  - `pub struct ArraySchema { pub method: &'static str, pub validate: fn(&HashMap<String, String>) -> std::result::Result<(), String> }`
  - `pub const LIST_BUILD_RPMS: ArraySchema` / `pub const QUERY_RPM_SIGS: ArraySchema`
    — the per-RPC record contracts
  - `pub fn parse_array_response(xml: &str, schema: &ArraySchema) -> KojiRpcResult<Vec<HashMap<String, String>>>`
    — array of structs, or nil, with every record checked against `schema`
  - Test helpers Task 7 reuses: `ok_build()`, `ok_rpms()`, `ok_sigs()`,
    `fault()`, and
    `mock_hub(&mut mockito::Server, [(&str, &str); 3], [usize; 3]) -> Vec<mockito::Mock>`
  - **Removes** `parse_xmlrpc_struct` (`:551`) and `parse_xmlrpc_array`
    (`:611`), which the new parser fully supersedes, along with the four
    `test_parse_xmlrpc_*` tests that call them.
  - Leaves `enrich_from_nvrs`'s three-argument signature untouched; Task 7
    changes it.

- [ ] **Step 1: Write the failing tests**

Append to `enrich_koji.rs`'s `mod tests`:

```rust
#[test]
fn only_valid_rpc_results_are_conclusive() {
    assert!(KojiRpcResult::ValidNonempty(1).is_conclusive());
    assert!(KojiRpcResult::<i32>::ValidEmpty.is_conclusive());
    assert!(!KojiRpcResult::<i32>::ApiFault("500".into()).is_conclusive());
    assert!(!KojiRpcResult::<i32>::Malformed("bad xml".into()).is_conclusive());
}

#[test]
fn an_xmlrpc_fault_is_not_an_empty_result() {
    // The bug this type exists to fix: today a fault body parses to an empty
    // struct and is indistinguishable from "no such build".
    let fault = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
      <member><name>faultCode</name><value><int>1000</int></value></member>
      <member><name>faultString</name><value><string>no such build</string></value></member>
    </struct></value></fault></methodResponse>"#;
    assert!(matches!(parse_build_response(fault), KojiRpcResult::ApiFault(_)));
    assert!(matches!(parse_array_response(fault, &LIST_BUILD_RPMS), KojiRpcResult::ApiFault(_)));
}

#[test]
fn truncated_xml_is_malformed_not_empty() {
    // The case a substring classifier cannot catch: this CONTAINS
    // "<methodResponse" and no "<fault>", yet the parser aborts partway and
    // would otherwise yield an empty collection that looks conclusive.
    let truncated = "<?xml version=\"1.0\"?><methodResponse><params><param><value><struct>\
                     <member><name>id</name><value><int>7</int";
    assert!(matches!(parse_build_response(truncated), KojiRpcResult::Malformed(_)),
        "a body that fails to parse must never be reported as empty");
    assert!(matches!(parse_array_response(truncated, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
}

#[test]
fn non_xml_is_malformed() {
    assert!(matches!(parse_build_response("not xml at all"), KojiRpcResult::Malformed(_)));
    assert!(matches!(parse_array_response("<html>503</html>", &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
}

#[test]
fn a_genuinely_empty_result_is_valid_empty() {
    // getBuild's conclusive "no such build" is <nil/>: koji's own getBuild
    // returns None, which the hub serializes with allow_none. An empty array
    // is likewise a real answer for the list RPCs.
    let nil = r#"<?xml version="1.0"?><methodResponse><params><param>
      <value><nil/></value></param></params></methodResponse>"#;
    assert!(matches!(parse_build_response(nil), KojiRpcResult::ValidEmpty));
    let empty_array = r#"<?xml version="1.0"?><methodResponse><params><param>
      <value><array><data></data></array></value></param></params></methodResponse>"#;
    assert!(matches!(parse_array_response(empty_array, &LIST_BUILD_RPMS), KojiRpcResult::ValidEmpty));
}

#[test]
fn an_empty_build_struct_is_malformed_not_absence() {
    // The old code's `data.is_empty()` check (`:267`) called this "not
    // found", which is exactly how a fault or a truncated body became a
    // conclusive answer. Koji signals absence with <nil/>, never <struct/>.
    let empty_struct = r#"<?xml version="1.0"?><methodResponse><params><param>
      <value><struct></struct></value></param></params></methodResponse>"#;
    assert!(matches!(parse_build_response(empty_struct), KojiRpcResult::Malformed(_)),
        "an empty struct is not koji's way of saying 'no such build'");
}

#[test]
fn eof_with_unclosed_elements_is_malformed() {
    // quick-xml's pull parser reaches EOF happily with elements still open,
    // so EOF alone proves nothing. Each of these contains "<methodResponse",
    // has no "<fault>", and would otherwise parse to an empty result.
    for truncated in [
        "<?xml version=\"1.0\"?><methodResponse><params>",
        "<?xml version=\"1.0\"?><methodResponse><params><param><value><struct>",
        "<?xml version=\"1.0\"?><methodResponse>",
    ] {
        assert!(matches!(parse_build_response(truncated), KojiRpcResult::Malformed(_)),
            "unclosed document must be Malformed: {truncated:?}");
        assert!(matches!(parse_array_response(truncated, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
    }
}

#[test]
fn a_self_closing_fault_is_still_a_fault() {
    // <fault/> is Event::Empty, not Event::Start. A scanner that only watches
    // Start would report this as a valid empty result.
    let x = r#"<?xml version="1.0"?><methodResponse><fault/></methodResponse>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::ApiFault(_)));
    assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::ApiFault(_)));
}

#[test]
fn a_self_closing_method_response_carries_no_payload() {
    let x = r#"<?xml version="1.0"?><methodResponse/>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)));
}

#[test]
fn a_response_with_no_params_payload_is_malformed() {
    let x = r#"<?xml version="1.0"?><methodResponse></methodResponse>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)),
        "a response carrying neither params nor fault is not a conclusive empty");
}

#[test]
fn a_well_formed_but_empty_params_element_is_malformed() {
    // Balanced, has a methodResponse, has exactly one <params> -- a tag
    // census accepts it, yet it carries no value at all.
    let x = r#"<?xml version="1.0"?><methodResponse><params/></methodResponse>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)),
        "<params/> carries no payload and must not read as a conclusive empty");
    assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
}

#[test]
fn a_method_response_nested_in_another_document_is_malformed() {
    // e.g. an HTML error page that happens to embed the text. The response
    // must BE the root, not merely appear somewhere inside.
    let x = r#"<html><methodResponse><params><param><value><struct/>
      </value></param></params></methodResponse></html>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)));
}

#[test]
fn an_unexpected_payload_type_is_malformed() {
    let x = r#"<?xml version="1.0"?><methodResponse><params><param>
      <value><boolean>1</boolean></value></param></params></methodResponse>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)),
        "koji answers with struct, array or nil; anything else is unexpected");
}

#[test]
fn a_nil_payload_is_a_conclusive_empty() {
    // XML-RPC's explicit "no value" -- a real answer, so checkpointable.
    // Accepted by both parsers.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param>
      <value><nil/></value></param></params></methodResponse>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::ValidEmpty));
    assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::ValidEmpty));
}

#[test]
fn a_struct_payload_is_rejected_by_the_array_parser() {
    // Well-formed, no fault, balanced -- but parse_xmlrpc_array would return
    // an empty vec for it, and that empty would be checkpointed as a
    // conclusive "this build has no RPMs / no signatures".
    let struct_body = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><int>7</int></value></member>
    </struct></value></param></params></methodResponse>"#;
    assert!(matches!(parse_array_response(struct_body, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)),
        "an array RPC handed a struct must not read as an empty array");
    // ...while the struct parser accepts it.
    assert!(matches!(parse_build_response(struct_body), KojiRpcResult::ValidNonempty(_)));
}

#[test]
fn an_array_payload_is_rejected_by_the_struct_parser() {
    let array_body = r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
      <value><struct>
        <member><name>id</name><value><int>5</int></value></member>
        <member><name>arch</name><value><string>x86_64</string></value></member>
      </struct></value>
    </data></array></value></param></params></methodResponse>"#;
    assert!(matches!(parse_build_response(array_body), KojiRpcResult::Malformed(_)),
        "getBuild handed an array must not read as an empty struct");
    assert!(matches!(parse_array_response(array_body, &LIST_BUILD_RPMS), KojiRpcResult::ValidNonempty(_)));
}

#[test]
fn more_than_one_payload_is_malformed() {
    let x = r#"<?xml version="1.0"?><methodResponse><params>
      <param><value><struct/></value></param>
      <param><value><struct/></value></param>
    </params></methodResponse>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)));
}

#[test]
fn an_array_without_a_data_element_is_malformed() {
    // Outer type is right, inner content is nonsense. The lossy parser would
    // return an empty vec and this would read as "no RPMs for this build".
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value>
      <array><bogus/></array>
    </value></param></params></methodResponse>"#;
    assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
}

#[test]
fn array_elements_that_are_not_structs_are_malformed() {
    // Same trap one level deeper: well-formed array>data, wrong element type.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value>
      <array><data><value><string>wrong element type</string></value></data></array>
    </value></param></params></methodResponse>"#;
    assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)),
        "an array of scalars is a schema violation, not an empty array");
}

#[test]
fn an_empty_array_is_a_conclusive_empty() {
    // The legitimate counterpart: a build really can have no signatures.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value>
      <array><data></data></array>
    </value></param></params></methodResponse>"#;
    assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::ValidEmpty));
}

#[test]
fn a_build_struct_without_build_id_is_malformed() {
    // Non-empty, well-formed, but missing the field the enrichment chain
    // keys on (enrich_koji.rs:299). Checkpointing it would cache a build
    // that can never produce complete enrichment.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>name</name><value><string>zlib</string></value></member>
    </struct></value></param></params></methodResponse>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)),
        "a build struct with no build_id cannot complete enrichment");
}

#[test]
fn a_build_struct_with_build_id_is_valid() {
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><int>7</int></value></member>
      <member><name>name</name><value><string>zlib</string></value></member>
    </struct></value></param></params></methodResponse>"#;
    match parse_build_response(x) {
        KojiRpcResult::ValidNonempty(m) => {
            assert_eq!(m.get("build_id").map(String::as_str), Some("7"));
            assert_eq!(m.get("name").map(String::as_str), Some("zlib"));
        }
        other => panic!("expected ValidNonempty, got {other:?}"),
    }
}

#[test]
fn a_non_numeric_build_id_is_malformed() {
    // Key present, value unusable: this gets interpolated into the next
    // call's <int> argument, so "presence" is not the property that matters.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><string>n/a</string></value></member>
    </struct></value></param></params></methodResponse>"#;
    assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)));
}

/// Wraps array records in a full methodResponse, so the record-level tests
/// below differ only in the records themselves.
fn array_of(records: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
        {records}
        </data></array></value></param></params></methodResponse>"#
    )
}

const RPM_RECORD: &str = r#"<value><struct>
  <member><name>id</name><value><int>5</int></value></member>
  <member><name>arch</name><value><string>x86_64</string></value></member>
</struct></value>"#;

const SIG_RECORD: &str = r#"<value><struct>
  <member><name>sigkey</name><value><string>abc123</string></value></member>
</struct></value>"#;

#[test]
fn an_empty_struct_array_element_is_malformed() {
    // Grammatically perfect and utterly unreadable. Under the old parser and
    // under grammar-only validation alike, this array reaches `rpm_id == None`
    // at :372 and is treated as the conclusive "this build has no RPMs".
    let x = array_of("<value><struct/></value>");
    assert!(matches!(parse_array_response(&x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)),
        "a record the caller cannot read is not an answer");
}

#[test]
fn an_rpm_record_missing_a_required_field_is_malformed() {
    // Missing id -> rpm_id is None -> "no binary RPMs".
    let no_id = array_of(
        "<value><struct><member><name>arch</name>\
         <value><string>x86_64</string></value></member></struct></value>");
    assert!(matches!(parse_array_response(&no_id, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));

    // Missing arch -> `:368`'s map_or(true, ...) treats it as non-src and
    // selects it, so absence is not neutral here.
    let no_arch = array_of(
        "<value><struct><member><name>id</name>\
         <value><int>5</int></value></member></struct></value>");
    assert!(matches!(parse_array_response(&no_arch, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));

    // Present but unusable.
    let bad_id = array_of(
        "<value><struct><member><name>id</name><value><string>x</string></value></member>\
         <member><name>arch</name><value><string>x86_64</string></value></member></struct></value>");
    assert!(matches!(parse_array_response(&bad_id, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));

    // A record that meets the contract still passes.
    assert!(matches!(
        parse_array_response(&array_of(RPM_RECORD), &LIST_BUILD_RPMS),
        KojiRpcResult::ValidNonempty(_)));
}

#[test]
fn a_signature_record_without_a_sigkey_is_malformed() {
    let x = array_of(
        "<value><struct><member><name>rpm_id</name>\
         <value><int>5</int></value></member></struct></value>");
    assert!(matches!(parse_array_response(&x, &QUERY_RPM_SIGS), KojiRpcResult::Malformed(_)),
        "a record with no sigkey would read as a conclusive 'unsigned'");
}

#[test]
fn an_empty_sigkey_is_a_real_unsigned_answer() {
    // The deliberate asymmetry with the rpm schema: Koji reports unsigned
    // RPMs as an empty sigkey, and :419 already filters for non-empty. That
    // is a conclusive answer, not a schema violation.
    let x = array_of(
        "<value><struct><member><name>sigkey</name>\
         <value><string></string></value></member></struct></value>");
    assert!(matches!(parse_array_response(&x, &QUERY_RPM_SIGS), KojiRpcResult::ValidNonempty(_)));
}

#[test]
fn each_array_rpc_validates_against_its_own_schema() {
    // The assertion that proves the schema argument is actually consulted
    // rather than decorative: the same body passes under one and fails under
    // the other, in both directions.
    let rpms = array_of(RPM_RECORD);
    let sigs = array_of(SIG_RECORD);
    assert!(matches!(parse_array_response(&rpms, &LIST_BUILD_RPMS), KojiRpcResult::ValidNonempty(_)));
    assert!(matches!(parse_array_response(&rpms, &QUERY_RPM_SIGS), KojiRpcResult::Malformed(_)));
    assert!(matches!(parse_array_response(&sigs, &QUERY_RPM_SIGS), KojiRpcResult::ValidNonempty(_)));
    assert!(matches!(parse_array_response(&sigs, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
}

#[test]
fn one_bad_record_among_good_ones_is_malformed() {
    // Validation is over every record, not just the first. The old selection
    // logic scans the list, so a later broken record matters just as much.
    let x = array_of(&format!("{RPM_RECORD}<value><struct/></value>"));
    assert!(matches!(parse_array_response(&x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
}

#[test]
fn cdata_text_is_not_dropped() {
    // quick-xml reports CDATA as Event::CData, not Event::Text. A catch-all
    // that ignores it turns a valid string field into an empty one -- and an
    // empty field is precisely what gets checkpointed as missing metadata.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><int>7</int></value></member>
      <member><name>name</name><value><string><![CDATA[zlib]]></string></value></member>
    </struct></value></param></params></methodResponse>"#;
    match parse_build_response(x) {
        KojiRpcResult::ValidNonempty(m) => {
            assert_eq!(m.get("name").map(String::as_str), Some("zlib"))
        }
        other => panic!("expected ValidNonempty, got {other:?}"),
    }
}

#[test]
fn text_interleaved_with_cdata_is_concatenated() {
    // Three adjacent text runs. Taking only the first truncates the value
    // without erroring -- the same silent-loss shape as dropping CDATA.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><int>7</int></value></member>
      <member><name>name</name><value><string>a<![CDATA[b]]>c</string></value></member>
    </struct></value></param></params></methodResponse>"#;
    match parse_build_response(x) {
        KojiRpcResult::ValidNonempty(m) => {
            assert_eq!(m.get("name").map(String::as_str), Some("abc"))
        }
        other => panic!("expected ValidNonempty, got {other:?}"),
    }
}

#[test]
fn a_nested_array_of_scalars_is_legal() {
    // The array-of-records rule belongs to the top-level payload, not to the
    // grammar. Koji build structs carry things like an array of tag names in
    // members we do not even store; rejecting them here would make an
    // ordinary response retryable forever.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><int>7</int></value></member>
      <member><name>tags</name><value><array><data>
        <value><string>f44</string></value>
        <value><string>f44-updates</string></value>
      </data></array></value></member>
    </struct></value></param></params></methodResponse>"#;
    match parse_build_response(x) {
        KojiRpcResult::ValidNonempty(m) => {
            assert_eq!(m.get("build_id").map(String::as_str), Some("7"));
            assert!(!m.contains_key("tags"), "nested containers are not stored as scalars");
        }
        other => panic!("expected ValidNonempty, got {other:?}"),
    }
    // ...while a TOP-LEVEL array of scalars is still a schema violation.
    let top = r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
      <value><string>f44</string></value>
    </data></array></value></param></params></methodResponse>"#;
    assert!(matches!(parse_array_response(top, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
}

#[test]
fn an_empty_value_element_is_the_empty_string() {
    // `<value></value>` is XML-RPC's empty string. Treating it as a grammar
    // violation would make a legitimate response permanently retryable.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><int>7</int></value></member>
      <member><name>note</name><value></value></member>
    </struct></value></param></params></methodResponse>"#;
    match parse_build_response(x) {
        KojiRpcResult::ValidNonempty(m) => {
            assert_eq!(m.get("note").map(String::as_str), Some(""))
        }
        other => panic!("expected ValidNonempty, got {other:?}"),
    }
}

#[test]
fn the_fault_subtree_is_parsed_not_scanned() {
    // The detail comes out of a parsed struct, so it survives a faultString
    // whose value merely *contains* the word, and a malformed fault body is
    // reported as such rather than passed off as a hub fault.
    let ok = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
      <member><name>faultCode</name><value><int>1000</int></value></member>
      <member><name>faultString</name><value><string>no such build</string></value></member>
    </struct></value></fault></methodResponse>"#;
    match parse_build_response(ok) {
        KojiRpcResult::ApiFault(d) => assert_eq!(d, "no such build"),
        other => panic!("expected ApiFault, got {other:?}"),
    }
    // Truncated inside the fault: not a readable fault, and certainly not a
    // conclusive result.
    let truncated = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
      <member><name>faultCode</name><value><int>1000</int></value></member>"#;
    assert!(matches!(parse_build_response(truncated), KojiRpcResult::Malformed(_)));
}

#[test]
fn a_nested_container_member_is_validated_but_not_stored() {
    // Koji structs can carry nested values. They must not break parsing, and
    // must not be silently treated as scalars either.
    let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><int>7</int></value></member>
      <member><name>extra</name><value><struct>
        <member><name>inner</name><value><string>v</string></value></member>
      </struct></value></member>
    </struct></value></param></params></methodResponse>"#;
    match parse_build_response(x) {
        KojiRpcResult::ValidNonempty(m) => {
            assert_eq!(m.get("build_id").map(String::as_str), Some("7"));
            assert!(!m.contains_key("extra"), "nested containers are not stored as scalars");
        }
        other => panic!("expected ValidNonempty, got {other:?}"),
    }
}

#[test]
fn a_populated_response_parses_to_valid_nonempty() {
    let ok = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><int>7</int></value></member>
    </struct></value></param></params></methodResponse>"#;
    match parse_build_response(ok) {
        KojiRpcResult::ValidNonempty(m) => {
            assert_eq!(m.get("build_id").map(String::as_str), Some("7"))
        }
        _ => panic!("a well-formed populated response must be ValidNonempty"),
    }
}

#[test]
fn the_two_stage_schema_versions_are_distinct_literals() {
    // The only assertion that actually catches a copy-paste sharing one
    // constant. Both path tests pass regardless, because the stage segment
    // already differs.
    assert_ne!(
        KOJI_SCHEMA_VERSION,
        crate::collect_spec::SPEC_SCHEMA_VERSION,
        "each stage must version independently, or bumping one silently \
         invalidates the other's checkpoints"
    );
}

#[test]
fn koji_cache_path_carries_its_own_schema_version() {
    // Mirror of the spec-stage test: this stage's path must contain THIS
    // stage's version, and a bump must invalidate.
    let d = tempfile::TempDir::new().unwrap();
    let ctx = crate::output_cache::CanonicalContext::new();
    let mine = crate::output_cache::OutputCache::new(
        d.path(), "20260912T010203Z-abcdef01", "koji", KOJI_SCHEMA_VERSION).unwrap();
    let bumped = crate::output_cache::OutputCache::new(
        d.path(), "20260912T010203Z-abcdef01", "koji", "koji-vNEXT").unwrap();
    let p = mine.entry_path("k", &ctx).unwrap();
    assert!(p.to_string_lossy().contains(KOJI_SCHEMA_VERSION), "got {p:?}");
    assert_ne!(p, bumped.entry_path("k", &ctx).unwrap());
    // And it must not collide with the spec stage for the same key.
    let spec = crate::output_cache::OutputCache::new(
        d.path(), "20260912T010203Z-abcdef01", "spec",
        crate::collect_spec::SPEC_SCHEMA_VERSION).unwrap();
    assert_ne!(p, spec.entry_path("k", &ctx).unwrap());
}
// ---- shared mock fixtures -------------------------------------------------
// All three RPCs POST to the same hub URL, so the mocks are distinguished by
// methodName in the body. Task 7's whole-chain tests reuse these.

use mockito::Matcher;

fn ok_build() -> &'static str {
    r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
      <member><name>build_id</name><value><int>1</int></value></member>
      <member><name>name</name><value><string>zlib</string></value></member>
      <member><name>owner_name</name><value><string>freshowner</string></value></member>
    </struct></value></param></params></methodResponse>"#
}
/// Each array RPC needs a body that satisfies ITS schema -- one shared
/// `ok_array()` would fail `QUERY_RPM_SIGS` validation and make the
/// "successful chain" test assert the opposite of its name.
fn ok_rpms() -> &'static str {
    r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
      <value><struct>
        <member><name>id</name><value><int>5</int></value></member>
        <member><name>arch</name><value><string>x86_64</string></value></member>
      </struct></value>
    </data></array></value></param></params></methodResponse>"#
}
fn ok_sigs() -> &'static str {
    r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
      <value><struct>
        <member><name>sigkey</name><value><string>abc123</string></value></member>
      </struct></value>
    </data></array></value></param></params></methodResponse>"#
}
fn fault() -> &'static str {
    r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
      <member><name>faultCode</name><value><int>1000</int></value></member>
      <member><name>faultString</name><value><string>backend down</string></value></member>
    </struct></value></fault></methodResponse>"#
}

/// One mock per RPC on a shared hub, with exact expected call counts.
///
/// The counts are the point: mockito 1.7.2's `Server::new()` sets
/// `assert_on_drop = false`, so a mock that is never called fails nothing on
/// its own. The caller must `.assert()` every returned mock, including the
/// ones expecting zero.
fn mock_hub(
    server: &mut mockito::Server,
    bodies: [(&str, &str); 3],
    expected_calls: [usize; 3],
) -> Vec<mockito::Mock> {
    bodies
        .into_iter()
        .zip(expected_calls)
        .map(|((method, body), n)| {
            server
                .mock("POST", "/kojihub")
                .match_body(Matcher::Regex(format!("<methodName>{method}</methodName>")))
                .with_body(body)
                .expect(n)
                .create()
        })
        .collect()
}

/// Writes a source-cache entry under a key spelled out literally, as the
/// pre-`KOJI_RPC_CACHE_VERSION` code would have written it.
fn seed_legacy_cache(dir: &std::path::Path, key: &str, value: serde_json::Value) {
    let c = crate::cache::FileCache::new(dir.to_str().unwrap(), "koji", 720, None).unwrap();
    c.put(key, &value);
}

/// Drives `get_build` directly rather than `enrich_from_nvrs`, whose signature
/// changes in Task 7. These two tests are about the source cache, not the
/// checkpoint, and must keep compiling across that change.
fn run_get_build(hub: &str, cache_dir: &std::path::Path, out: &std::path::Path) -> String {
    let e = KojiEnricher::new_standalone(hub, "fedora", "44", Some(cache_dir.to_str().unwrap()));
    let mut w = NTriplesWriter::new(std::fs::File::create(out).unwrap());
    e.get_build("zlib-1.3-1.fc44", &mut w).unwrap();
    w.flush().unwrap();
    std::fs::read_to_string(out).unwrap()
}

#[test]
fn a_legacy_koji_build_cache_entry_is_not_reused() {
    // The old parser's `Err(_) => break` (:598) persisted partial maps built
    // from truncated bodies. This seeded entry is nonempty and even carries a
    // build_id, so validating the cached JSON would wave it through -- only
    // retiring the key namespace stops it being read.
    let mut server = mockito::Server::new();
    let mocks = mock_hub(
        &mut server,
        [("getBuild", ok_build()), ("listBuildRPMs", ok_rpms()), ("queryRPMSigs", ok_sigs())],
        [1, 1, 1],
    );

    let d = tempfile::TempDir::new().unwrap();
    seed_legacy_cache(
        d.path(),
        "koji-build-zlib-1.3-1.fc44",
        serde_json::json!({"build_id": "1", "owner_name": "legacyowner"}),
    );

    let hub = format!("{}/kojihub", server.url());
    let text = run_get_build(&hub, d.path(), &d.path().join("out.nt"));

    // Without the key change getBuild is never called and this fails.
    for m in &mocks {
        m.assert();
    }
    assert!(
        text.contains("agent/koji/freshowner"),
        "the refetched response must be what reaches the output"
    );
    assert!(!text.contains("legacyowner"), "the legacy entry must not be read");
}

#[test]
fn a_colliding_legacy_key_is_not_reused() {
    // Pins the namespace shape, not just the version bump. Under the obvious
    // spelling `koji-build-{VERSION}-{nvr}` this seeded legacy entry -- a real
    // package whose NVR starts with the version segment -- IS the new key for
    // "zlib-1.3-1.fc44", so it would be read as another package's metadata.
    let mut server = mockito::Server::new();
    let mocks = mock_hub(
        &mut server,
        [("getBuild", ok_build()), ("listBuildRPMs", ok_rpms()), ("queryRPMSigs", ok_sigs())],
        [1, 1, 1],
    );

    let d = tempfile::TempDir::new().unwrap();
    seed_legacy_cache(
        d.path(),
        &format!("koji-build-{}-zlib-1.3-1.fc44", KOJI_RPC_CACHE_VERSION),
        serde_json::json!({"build_id": "1", "owner_name": "legacyowner"}),
    );

    let hub = format!("{}/kojihub", server.url());
    let text = run_get_build(&hub, d.path(), &d.path().join("out.nt"));

    for m in &mocks {
        m.assert();
    }
    assert!(!text.contains("legacyowner"), "the new namespace must not overlap the old one");
}

#[test]
fn a_legacy_koji_sigs_cache_entry_is_not_reused() {
    // The second namespace, reached only after getBuild succeeds. Its key is
    // built from ok_build()'s build_id, so the pre-version spelling is
    // "koji-sigs-1".
    let mut server = mockito::Server::new();
    let mocks = mock_hub(
        &mut server,
        [("getBuild", ok_build()), ("listBuildRPMs", ok_rpms()), ("queryRPMSigs", ok_sigs())],
        [1, 1, 1],
    );

    let d = tempfile::TempDir::new().unwrap();
    seed_legacy_cache(d.path(), "koji-sigs-1", serde_json::json!({"sigkey": "deadbeef"}));

    let hub = format!("{}/kojihub", server.url());
    let text = run_get_build(&hub, d.path(), &d.path().join("out.nt"));

    // Without the key change the cached sigkey short-circuits :324, so
    // neither listBuildRPMs nor queryRPMSigs is called and both fail here.
    for m in &mocks {
        m.assert();
    }
    assert!(text.contains("abc123"), "the refetched sigkey must be what reaches the output");
    assert!(!text.contains("deadbeef"), "the legacy entry must not be read");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib enrich_koji::tests::only_valid_rpc_results_are_conclusive`
Expected: FAIL to compile — `KojiRpcResult` undefined.

- [ ] **Step 3: Implement the type and classifier**

Add near the top of `enrich_koji.rs`:

```rust
/// Bump this for any change to this stage's emitted triples, including
/// changes to shared serialization or ontology helpers it calls. A stale
/// checkpoint fragment is indistinguishable from a correct one.
pub const KOJI_SCHEMA_VERSION: &str = "koji-v1";

/// Namespaces the 30-day `koji-build-*` / `koji-sigs-*` source-cache keys.
///
/// Distinct from `KOJI_SCHEMA_VERSION`, which versions *emitted triples*; this
/// versions *parsed RPC responses*. Bump it whenever a parser change makes
/// previously-stored entries untrustworthy -- as this task's does, since the
/// old lossy parser persisted partial maps built from truncated bodies.
///
/// It is also a field of the Koji stage's `CanonicalContext` (Task 7), so a
/// bump invalidates output checkpoints as well as source-cache entries. Those
/// are two different caches and a bump has to reach both: the source cache
/// sits *behind* the output checkpoint, so an interrupted generation would
/// otherwise replay fragments the old parser produced without ever consulting
/// the source cache at all.
pub const KOJI_RPC_CACHE_VERSION: &str = "v2";

/// Outcome of one Koji XML-RPC call.
///
/// `parse_xmlrpc_struct`/`parse_xmlrpc_array` return an empty collection for
/// a fault, a malformed body, and a legitimately empty result alike. Only
/// conclusive results may contribute to a checkpointable item.
///
/// `Debug` is required, not cosmetic: the tests below print the unexpected
/// variant with `{other:?}` when an assertion fails, and without it they do
/// not compile.
#[derive(Debug)]
pub enum KojiRpcResult<T> {
    ValidNonempty(T),
    ValidEmpty,
    ApiFault(String),
    Malformed(String),
}

impl<T> KojiRpcResult<T> {
    pub fn is_conclusive(&self) -> bool {
        matches!(self, KojiRpcResult::ValidNonempty(_) | KojiRpcResult::ValidEmpty)
    }
}

/// A parsed XML-RPC value, restricted to the shapes Koji actually returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcValue {
    /// Top-level scalar members only, matching what the callers read. A
    /// nested container inside a member is grammar-legal and gets validated,
    /// but is not stored -- the same fields the enricher read before.
    Struct(HashMap<String, String>),
    /// Koji's arrays are arrays of structs (listBuildRPMs, queryRPMSigs).
    Array(Vec<HashMap<String, String>>),
    Nil,
}

/// One XML token. Tokenizing first, then doing recursive descent over a
/// slice, is far easier to get right (and to read) than threading quick-xml's
/// borrow-checked buffer through a recursive parser.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Open(String),
    Close(String),
    Text(String),
}

/// A self-closing `<x/>` becomes `Open(x), Close(x)`, so the grammar below
/// never has to special-case it -- forgetting that is how `<fault/>` slipped
/// through an earlier revision of this code.
fn tokenize(xml: &str) -> std::result::Result<Vec<Tok>, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;

    let mut toks = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                toks.push(Tok::Open(String::from_utf8_lossy(e.name().as_ref()).into_owned()))
            }
            Ok(Event::End(e)) => {
                toks.push(Tok::Close(String::from_utf8_lossy(e.name().as_ref()).into_owned()))
            }
            Ok(Event::Empty(e)) => {
                let n = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                toks.push(Tok::Open(n.clone()));
                toks.push(Tok::Close(n));
            }
            Ok(Event::Text(e)) => match e.unescape() {
                Ok(t) => {
                    let t = t.to_string();
                    if !t.is_empty() {
                        toks.push(Tok::Text(t));
                    }
                }
                // A body we cannot decode is not a body we may trust.
                Err(err) => return Err(format!("undecodable text: {err}")),
            },
            // CDATA is text that is raw by definition -- no unescaping. It
            // must be tokenized, not dropped: a `<string>` delivered as CDATA
            // would otherwise parse to "", and an empty field is exactly the
            // shape that gets checkpointed as missing metadata.
            Ok(Event::CData(e)) => {
                let t = String::from_utf8_lossy(&e).into_owned();
                if !t.is_empty() {
                    toks.push(Tok::Text(t));
                }
            }
            Ok(Event::Eof) => break,
            // Truncated or malformed input. This must never reach the caller
            // as an empty result.
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(toks)
}

/// Cursor over the token slice.
struct Cur<'a> {
    t: &'a [Tok],
    i: usize,
}

impl<'a> Cur<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.t.get(self.i)
    }
    fn open(&mut self, name: &str) -> std::result::Result<(), String> {
        match self.peek() {
            Some(Tok::Open(n)) if n == name => {
                self.i += 1;
                Ok(())
            }
            other => Err(format!("expected <{name}>, found {other:?}")),
        }
    }
    fn close(&mut self, name: &str) -> std::result::Result<(), String> {
        match self.peek() {
            Some(Tok::Close(n)) if n == name => {
                self.i += 1;
                Ok(())
            }
            other => Err(format!("expected </{name}>, found {other:?}")),
        }
    }
    fn at_open(&self, name: &str) -> bool {
        matches!(self.peek(), Some(Tok::Open(n)) if n == name)
    }
    /// Consumes every adjacent text run and concatenates them. A mixed body
    /// such as `a<![CDATA[b]]>c` tokenizes to three `Text`s; taking only the
    /// first would silently truncate the value to "a".
    fn text(&mut self) -> String {
        let mut out = String::new();
        while let Some(Tok::Text(s)) = self.peek() {
            out.push_str(s);
            self.i += 1;
        }
        out
    }
}

const SCALARS: [&str; 7] =
    ["int", "i4", "string", "double", "boolean", "dateTime.iso8601", "base64"];

/// `<value>` content. Returns `None` for a container we validate but do not
/// store (a nested struct/array inside a member), preserving exactly the
/// fields the enricher read before this change.
fn parse_value(c: &mut Cur) -> std::result::Result<Option<String>, String> {
    let name = match c.peek() {
        Some(Tok::Open(n)) => n.clone(),
        // <value>text</value> with no type element is a string in XML-RPC.
        Some(Tok::Text(_)) => return Ok(Some(c.text())),
        // <value></value> is the empty string, not a grammar violation.
        // Rejecting it would make a legitimate response retryable forever.
        Some(Tok::Close(n)) if n == "value" => return Ok(Some(String::new())),
        other => return Err(format!("expected a value, found {other:?}")),
    };

    if SCALARS.contains(&name.as_str()) {
        c.open(&name)?;
        let v = c.text();
        c.close(&name)?;
        return Ok(Some(v));
    }
    match name.as_str() {
        "nil" => {
            c.open("nil")?;
            c.close("nil")?;
            Ok(None)
        }
        "struct" => {
            parse_struct(c)?;
            Ok(None)
        }
        "array" => {
            parse_array(c)?;
            Ok(None)
        }
        other => Err(format!("unexpected value type <{other}>")),
    }
}

fn parse_struct(c: &mut Cur) -> std::result::Result<HashMap<String, String>, String> {
    c.open("struct")?;
    let mut out = HashMap::new();
    while c.at_open("member") {
        c.open("member")?;
        c.open("name")?;
        let key = c.text();
        c.close("name")?;
        c.open("value")?;
        let val = parse_value(c)?;
        c.close("value")?;
        c.close("member")?;
        if let Some(v) = val {
            out.insert(key, v);
        }
    }
    c.close("struct")?;
    Ok(out)
}

/// Grammar-level array: `<array><data>` holding any sequence of values.
///
/// Element *type* is the caller's business, not the grammar's. Each element
/// yields `Some(map)` when it was a struct and `None` otherwise; the
/// top-level payload in `parse_response` rejects the `None`s, while a nested
/// array reached through `parse_value` accepts them. Folding the
/// array-of-records rule in here would make a perfectly valid nested array of
/// scalars -- inside a member we do not even store -- turn the whole response
/// retryable.
fn parse_array(c: &mut Cur) -> std::result::Result<Vec<Option<HashMap<String, String>>>, String> {
    c.open("array")?;
    // Exactly one <data>. `<array><bogus/></array>` fails here rather than
    // yielding an empty vec.
    c.open("data")?;
    let mut out = Vec::new();
    while c.at_open("value") {
        c.open("value")?;
        if c.at_open("struct") {
            out.push(Some(parse_struct(c)?));
        } else {
            // Validated, but not a record.
            parse_value(c)?;
            out.push(None);
        }
        c.close("value")?;
    }
    c.close("data")?;
    c.close("array")?;
    Ok(out)
}

/// Parse a whole methodResponse. Any grammar violation is `Malformed`; a
/// `<fault>` is `ApiFault`. There is no path from a structural problem to an
/// empty-but-valid result.
pub fn parse_response(xml: &str) -> KojiRpcResult<RpcValue> {
    let toks = match tokenize(xml) {
        Ok(t) => t,
        Err(e) => return KojiRpcResult::Malformed(e),
    };
    let mut c = Cur { t: &toks, i: 0 };

    // The response must BE the root, not merely appear inside some other
    // document (e.g. an HTML error page that embeds it).
    if let Err(e) = c.open("methodResponse") {
        return KojiRpcResult::Malformed(format!("root: {e}"));
    }

    // The fault subtree is parsed, not scanned: "the whole document is
    // validated" has to be true of this branch too, and reading faultString
    // out of a parsed struct beats hunting for a Text token that happens to
    // equal "faultString".
    if c.at_open("fault") {
        return match parse_fault(&mut c) {
            Ok(d) => KojiRpcResult::ApiFault(d),
            // Either way the item is inconclusive; `Malformed` just names the
            // real problem instead of blaming the hub for a fault we could
            // not read.
            Err(e) => KojiRpcResult::Malformed(format!("fault: {e}")),
        };
    }

    let parsed = (|| -> std::result::Result<RpcValue, String> {
        c.open("params")?;
        c.open("param")?;
        c.open("value")?;
        let v = match c.peek() {
            Some(Tok::Open(n)) if n == "struct" => RpcValue::Struct(parse_struct(&mut c)?),
            Some(Tok::Open(n)) if n == "array" => {
                // Here -- and only here -- Koji's arrays must be arrays of
                // records. A scalar element is a schema violation, not an
                // empty array.
                let mut recs = Vec::new();
                for (i, e) in parse_array(&mut c)?.into_iter().enumerate() {
                    match e {
                        Some(m) => recs.push(m),
                        None => return Err(format!("array element {i} is not a <struct>")),
                    }
                }
                RpcValue::Array(recs)
            }
            Some(Tok::Open(n)) if n == "nil" => {
                c.open("nil")?;
                c.close("nil")?;
                RpcValue::Nil
            }
            other => return Err(format!("payload is {other:?}, expected struct, array or nil")),
        };
        c.close("value")?;
        c.close("param")?;
        // A second <param> would be a second payload.
        if c.at_open("param") {
            return Err("more than one <param> payload".into());
        }
        c.close("params")?;
        c.close("methodResponse")?;
        if c.peek().is_some() {
            return Err(format!("trailing content after </methodResponse>: {:?}", c.peek()));
        }
        Ok(v)
    })();

    match parsed {
        Ok(v) => KojiRpcResult::ValidNonempty(v),
        Err(e) => KojiRpcResult::Malformed(e),
    }
}

/// `<fault><value><struct>faultCode, faultString</struct></value></fault>`,
/// through to the end of the document. `<fault/>` carries no detail but is
/// still unambiguously a fault.
fn parse_fault(c: &mut Cur) -> std::result::Result<String, String> {
    c.open("fault")?;
    let detail = if c.at_open("value") {
        c.open("value")?;
        let m = parse_struct(c)?;
        c.close("value")?;
        m.get("faultString")
            .cloned()
            .unwrap_or_else(|| "unknown fault".to_string())
    } else {
        "unknown fault".to_string()
    };
    c.close("fault")?;
    c.close("methodResponse")?;
    if c.peek().is_some() {
        return Err(format!(
            "trailing content after </methodResponse>: {:?}",
            c.peek()
        ));
    }
    Ok(detail)
}

/// A Koji identifier must be a non-empty run of digits. `build_id` and `id`
/// arrive as `<int>` or `<string>` depending on hub version, and both are
/// interpolated straight into the next call's `<int>` argument, so the check
/// belongs on the value rather than on the XML type that carried it.
fn require_numeric_id(
    r: &HashMap<String, String>,
    field: &str,
) -> std::result::Result<(), String> {
    match r.get(field) {
        Some(v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => Ok(()),
        Some(v) => Err(format!("{field} is {v:?}, not a numeric id")),
        None => Err(format!("record has no {field}")),
    }
}

/// The record contract for one array-returning RPC. Naming a schema is
/// mandatory at every call site: a parser that defaults to "no record
/// validation" is how a `<struct/>` element becomes a conclusive empty.
pub struct ArraySchema {
    pub method: &'static str,
    pub validate: fn(&HashMap<String, String>) -> std::result::Result<(), String>,
}

fn validate_rpm_record(r: &HashMap<String, String>) -> std::result::Result<(), String> {
    require_numeric_id(r, "id")?;
    match r.get("arch") {
        Some(a) if !a.is_empty() => Ok(()),
        // Absent arch is not neutral: `:368` treats it as non-src and picks
        // the record.
        _ => Err("rpm record has no arch; the src filter would misread it".into()),
    }
}

fn validate_sig_record(r: &HashMap<String, String>) -> std::result::Result<(), String> {
    // Presence, not non-emptiness -- an empty sigkey is Koji's own way of
    // saying "unsigned", and that is a real answer worth checkpointing.
    if r.contains_key("sigkey") {
        Ok(())
    } else {
        Err("signature record has no sigkey; absence would read as unsigned".into())
    }
}

pub const LIST_BUILD_RPMS: ArraySchema = ArraySchema {
    method: "listBuildRPMs",
    validate: validate_rpm_record,
};

pub const QUERY_RPM_SIGS: ArraySchema = ArraySchema {
    method: "queryRPMSigs",
    validate: validate_sig_record,
};

/// `getBuild`: a struct, or nil.
///
/// Koji's own `getBuild` returns `None` -- serialized by the hub as `<nil/>`
/// -- when no such build exists, and returns a map *containing* `build_id`
/// when one does. So `<nil/>` is the conclusive no-build answer, and **every**
/// struct must carry a numeric `build_id`, including an empty one.
///
/// Treating `<struct/>` as "not found" is the old code's behaviour
/// (`data.is_empty()` at `enrich_koji.rs:267`) and it is what let a fault or a
/// truncated body read as conclusive absence. Koji does not produce it; a hub
/// that did is telling us something we cannot act on, which is retryable.
pub fn parse_build_response(xml: &str) -> KojiRpcResult<HashMap<String, String>> {
    match parse_response(xml) {
        KojiRpcResult::ValidNonempty(RpcValue::Struct(m)) => {
            // An early return rather than if/else: an `if let Err(_) =
            // require_numeric_id(&m, ..)` holds the borrow of `m` across the
            // else branch under edition 2021, which then cannot move `m`.
            if let Err(e) = require_numeric_id(&m, "build_id") {
                return KojiRpcResult::Malformed(format!(
                    "getBuild: {e}; cannot complete enrichment"
                ));
            }
            KojiRpcResult::ValidNonempty(m)
        }
        KojiRpcResult::ValidNonempty(RpcValue::Nil) => KojiRpcResult::ValidEmpty,
        KojiRpcResult::ValidNonempty(other) => KojiRpcResult::Malformed(format!(
            "getBuild returned {other:?}, expected a struct or nil"
        )),
        KojiRpcResult::ValidEmpty => KojiRpcResult::ValidEmpty,
        KojiRpcResult::ApiFault(d) => KojiRpcResult::ApiFault(d),
        KojiRpcResult::Malformed(d) => KojiRpcResult::Malformed(d),
    }
}

/// `listBuildRPMs` / `queryRPMSigs`: an array of structs (or nil), every
/// record of which must satisfy `schema`. An empty array is a real answer; an
/// array of records the caller cannot read is not -- a `<struct/>` element
/// reaches the same "no RPMs" / "unsigned" conclusion the old lossy parser's
/// empty vec did.
pub fn parse_array_response(
    xml: &str,
    schema: &ArraySchema,
) -> KojiRpcResult<Vec<HashMap<String, String>>> {
    match parse_response(xml) {
        KojiRpcResult::ValidNonempty(RpcValue::Array(v)) => {
            if v.is_empty() {
                return KojiRpcResult::ValidEmpty;
            }
            for (i, r) in v.iter().enumerate() {
                if let Err(e) = (schema.validate)(r) {
                    return KojiRpcResult::Malformed(format!(
                        "{} record {}: {}",
                        schema.method, i, e
                    ));
                }
            }
            KojiRpcResult::ValidNonempty(v)
        }
        KojiRpcResult::ValidNonempty(RpcValue::Nil) => KojiRpcResult::ValidEmpty,
        KojiRpcResult::ValidNonempty(other) => {
            KojiRpcResult::Malformed(format!("expected an array payload, got {other:?}"))
        }
        KojiRpcResult::ValidEmpty => KojiRpcResult::ValidEmpty,
        KojiRpcResult::ApiFault(d) => KojiRpcResult::ApiFault(d),
        KojiRpcResult::Malformed(d) => KojiRpcResult::Malformed(d),
    }
}
```

- [ ] **Step 4: Add the item-level inconclusive flag**

Add to the `KojiEnricher` struct:

```rust
    /// Set when ANY RPC in this item's chain was inconclusive, so the whole
    /// per-NVR fragment is Retryable -- a build whose getBuild succeeded but
    /// whose queryRPMSigs faulted must not be checkpointed as unsigned.
    item_inconclusive: std::cell::Cell<bool>,
```

Initialize it to `std::cell::Cell::new(false)` in all three constructors
(`new:32`, `new_standalone:79`, `new_standalone_with_minio:90`).

- [ ] **Step 5: Version the cache keys, and route every inconclusive path through the flag**

First, retire the entries the old parser wrote. In `get_build` (`:233`):

```rust
        let cache_key = format!("koji-rpc-{}-build-{}", KOJI_RPC_CACHE_VERSION, nvr);
```

and in `query_rpm_signatures` (`:322`):

```rust
        let cache_key = format!("koji-rpc-{}-sigs-{}", KOJI_RPC_CACHE_VERSION, build_id);
```

**The new prefix must not be reachable by appending to the old one.** The
obvious spelling, `format!("koji-build-{}-{}", VERSION, nvr)`, is *not*
disjoint: a legacy entry for a package whose NVR happens to be
`v2-zlib-1.3-1.fc44` sits at `koji-build-v2-zlib-1.3-1.fc44`, which is
byte-identical to the new key for NVR `zlib-1.3-1.fc44`. RPM names contain
hyphens, so the version segment cannot be distinguished from the start of a
name. Moving the version into a prefix the old keys never had
(`koji-rpc-` vs `koji-build-` / `koji-sigs-`) makes the two namespaces
disjoint by construction rather than by luck.

Do **not** delete the old entries: the Minio credentials cannot `mc rm`. They
become unreachable, which is what correctness requires here; see
`a_colliding_legacy_key_is_not_reused` in Step 1. They do **not** reliably go
away on their own — `read_minio` (`cache.rs:295`) never checks age — so the
objects accumulate. That is a pre-existing `FileCache` defect, recorded under
*Out of scope* below.

Then the flag itself.

Two categories, and both must set it — marking only the parse sites would leave
transport failures checkpointed, which is the more common failure in practice.

**(a) Transport failures.** There are seven `return Ok(0)` branches
(`:267, :285, :357, :374, :407, :424, :440`). For each one reached because a
request or decode *failed* (as opposed to a legitimate "no data" outcome such as
`rpm_id` being `None` at `:374`), add `self.item_inconclusive.set(true);`
immediately before the `return`. Concretely, the `Err(_) =>` arms around the
`self.transport.post(...)` calls in `get_build` and `query_rpm_signatures`, and
the `String::from_utf8` failure paths.

**(b) Parse results.** Replace each `parse_xmlrpc_struct`/`parse_xmlrpc_array`
call with its typed wrapper and match. In `get_build`, replace
`let data = parse_xmlrpc_struct(&body); if data.is_empty() { ... }` with:

```rust
        let data = match parse_build_response(&body) {
            KojiRpcResult::ValidNonempty(d) => d,
            KojiRpcResult::ValidEmpty => {
                // A real answer: Koji has no such build. Checkpointable.
                emit_dq_issue(writer, "koji-enricher", "getBuild", nvr,
                              "koji-build-not-found", "info")?;
                return Ok(0);
            }
            KojiRpcResult::ApiFault(d) | KojiRpcResult::Malformed(d) => {
                eprintln!("  {} → koji response inconclusive: {}", nvr, d);
                emit_dq_issue(writer, "koji-enricher", "getBuild", nvr,
                              "koji-api-error", "warning")?;
                self.item_inconclusive.set(true);
                return Ok(0);
            }
        };
```

Note that `ValidEmpty` now arrives only from `<nil/>` — Koji's actual "no such
build" answer. An empty struct no longer reaches this arm; it is `Malformed`,
and the item becomes retryable.

Apply the same shape to both `parse_xmlrpc_array` sites inside
`query_rpm_signatures` — the `listBuildRPMs` response (`:364`) becomes
`parse_array_response(&body, &LIST_BUILD_RPMS)` and the `queryRPMSigs` response
(`:414`) becomes `parse_array_response(&body, &QUERY_RPM_SIGS)` — using field
names `"listBuildRPMs"` and `"queryRPMSigs"` respectively in the DQ issues. The
schemas are not interchangeable: passing the wrong one rejects every valid
response, which the `each_array_rpc_validates_against_its_own_schema` test in
Step 1 pins down in both directions.

For those two, `ValidEmpty` is a legitimate outcome (a build with no binary
RPMs, or an RPM with no signature rows): return normally *without* setting the
flag.

- [ ] **Step 6: Retire the superseded parsers and their tests**

`parse_xmlrpc_struct` (`:551`) and `parse_xmlrpc_array` (`:611`) now have no
callers. Delete both. Leaving a lossy parser in the file invites a future call
site to reintroduce the exact defect this task removes — and leaving it means
`cargo test` still fails, because four existing tests call them.

Delete those four tests too. Each is already covered by a stricter replacement
from Step 1, so nothing is lost:

| Delete | Covered by |
|---|---|
| `test_parse_xmlrpc_struct` | `a_populated_response_parses_to_valid_nonempty` |
| `test_parse_xmlrpc_struct_ignores_nested` | `a_nested_container_member_is_validated_but_not_stored`, `a_nested_array_of_scalars_is_legal` |
| `test_parse_xmlrpc_array` | the positive case in `an_rpm_record_missing_a_required_field_is_malformed` |
| `test_parse_xmlrpc_array_empty` | `an_empty_array_is_a_conclusive_empty` |

Confirm nothing else references them:

```bash
grep -rn 'parse_xmlrpc_struct\|parse_xmlrpc_array' etl/pg-collect/src/
```

Expected: no matches.

Leave `test_enrich_from_nvrs_processes_list` alone — it still compiles against
the three-argument `enrich_from_nvrs`, and Task 7 updates it when that
signature changes.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib enrich_koji::`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add etl/pg-collect/src/enrich_koji.rs
git commit -m "feat(etl): distinguish koji faults from empty results

getBuild, listBuildRPMs and queryRPMSigs all collapsed fault, malformed
and legitimately-empty responses into the same empty parse, so none could
tell 'no such build' from 'Koji returned a fault'. Any inconclusive RPC
now marks the whole per-NVR item, since a build whose signature lookup
faulted must not be checkpointed as unsigned."
```

---

### Task 7: Checkpoint the Koji stage, with explicit injection

**Files:**
- Modify: `etl/pg-collect/src/enrich_koji.rs` (`enrich_from_nvrs:192`)
- Modify: `etl/pg-collect/src/main.rs` (the `enrich_from_nvrs` call site, ~3650)
- Test: `etl/pg-collect/src/enrich_koji.rs`

**Interfaces:**
- Consumes: `OutputCache` (Task 2), `KojiRpcResult`/`KOJI_SCHEMA_VERSION` and
  the `ok_build`/`ok_rpms`/`ok_sigs`/`fault`/`mock_hub` test helpers (Task 6).
- Produces: `enrich_from_nvrs(&self, nvrs: &[String], output_path: &str, limit: Option<usize>, checkpoint: Option<&OutputCache>) -> Result<(usize, usize)>`.
- Updates every existing caller of `enrich_from_nvrs` to the new arity.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn passing_no_checkpoint_writes_no_checkpoint_files() {
    // Guards the standalone-enrich-koji contract: it has no
    // commit-after-publish lifecycle, so it must never leave checkpoint state.
    // A real NVR against an unroutable host, so the code path is exercised
    // rather than skipped by an empty list.
    let d = tempfile::TempDir::new().unwrap();
    let out = d.path().join("out.nt");
    let e = KojiEnricher::new_standalone("https://koji.invalid/kojihub", "fedora", "44", None);
    let _ = e.enrich_from_nvrs(&["zlib-1.3-1.fc44".to_string()], out.to_str().unwrap(), None, None);
    assert!(!d.path().join("output").exists(), "no checkpoint tree may be created");
}

#[test]
fn a_checkpointed_nvr_replays_without_calling_koji() {
    // Drives the real loop. The hub is unroutable, so if the checkpoint were
    // not consulted the item would emit a DQ fragment instead of this text.
    use crate::output_cache::{CachedOutput, CanonicalContext, ComputeOutcome, OutputCache};
    let d = tempfile::TempDir::new().unwrap();
    let cache = OutputCache::new(d.path(), "20260912T010203Z-abcdef01", "koji", KOJI_SCHEMA_VERSION).unwrap();
    let hub = "https://koji.invalid/kojihub";

    // This context must match the production one field for field, which is
    // what pins `rpc_cache_version` into `enrich_from_nvrs`: drop it there and
    // this test stops hitting.
    let ctx = CanonicalContext::new()
        .field("distro", "fedora")
        .field("release", "44")
        .field("koji_hub", hub)
        .field("rpc_cache_version", KOJI_RPC_CACHE_VERSION);
    cache.get_or_compute("zlib-1.3-1.fc44", &ctx, || Ok(ComputeOutcome::Complete(CachedOutput {
        logical_triples: 3, skipped_invalid_iri: 0, auto_inverses: 1,
        text: "<b> <p> <o> .\n".into(),
    }))).unwrap();

    let out = d.path().join("out.nt");
    let e = KojiEnricher::new_standalone(hub, "fedora", "44", None);
    let (builds, triples) = e.enrich_from_nvrs(
        &["zlib-1.3-1.fc44".to_string()], out.to_str().unwrap(), None, Some(&cache),
    ).unwrap();

    assert_eq!((builds, triples), (1, 3), "totals must come from the checkpoint");
    assert_eq!(cache.stats().hits, 1);
    assert!(std::fs::read_to_string(&out).unwrap().contains("<b> <p> <o>"));
}

#[test]
fn an_rpc_cache_version_change_misses_existing_output_checkpoints() {
    // The scenario: a generation is interrupted, someone fixes a parser bug
    // and bumps KOJI_RPC_CACHE_VERSION, and the generation resumes. Fragments
    // the old parser wrote must not replay.
    use crate::output_cache::{CachedOutput, CanonicalContext, ComputeOutcome, OutputCache};
    let d = tempfile::TempDir::new().unwrap();
    let cache = OutputCache::new(
        d.path(), "20260912T010203Z-abcdef01", "koji", KOJI_SCHEMA_VERSION).unwrap();

    let ctx_for = |v: &str| CanonicalContext::new()
        .field("distro", "fedora")
        .field("release", "44")
        .field("koji_hub", "https://koji.invalid/kojihub")
        .field("rpc_cache_version", v);
    let fragment = || Ok(ComputeOutcome::Complete(CachedOutput {
        logical_triples: 1, skipped_invalid_iri: 0, auto_inverses: 0,
        text: "<b> <p> <o> .\n".into(),
    }));

    cache.get_or_compute("zlib-1.3-1.fc44", &ctx_for(KOJI_RPC_CACHE_VERSION), fragment).unwrap();
    assert_eq!(cache.stats().hits, 0, "first call is a miss");

    // Same key, same generation, same KOJI_SCHEMA_VERSION -- only the parser
    // version differs.
    cache.get_or_compute("zlib-1.3-1.fc44", &ctx_for("vNEXT"), fragment).unwrap();
    assert_eq!(cache.stats().hits, 0, "an RPC-version change must not replay old fragments");

    // ...and the original context still hits, so the miss above is the
    // version and not a context that never matches anything.
    cache.get_or_compute("zlib-1.3-1.fc44", &ctx_for(KOJI_RPC_CACHE_VERSION), fragment).unwrap();
    assert_eq!(cache.stats().hits, 1);
}

#[test]
fn an_unreachable_hub_is_retryable_not_checkpointed() {
    // A transport failure must leave nothing behind, or the next run would
    // replay an empty fragment as though the build genuinely had no data.
    use crate::output_cache::OutputCache;
    let d = tempfile::TempDir::new().unwrap();
    let cache = OutputCache::new(d.path(), "20260912T010203Z-abcdef01", "koji", KOJI_SCHEMA_VERSION).unwrap();
    let out = d.path().join("out.nt");
    let e = KojiEnricher::new_standalone("https://koji.invalid/kojihub", "fedora", "44", None);
    let _ = e.enrich_from_nvrs(
        &["zlib-1.3-1.fc44".to_string()], out.to_str().unwrap(), None, Some(&cache),
    );
    assert_eq!(cache.stats().retryable, 1, "a transport failure must be Retryable");
    assert_eq!(cache.stats().hits, 0);
}

// The whole-chain tests below reuse Task 6's `ok_build`/`ok_rpms`/`ok_sigs`/
// `fault` fixtures and its `mock_hub` helper. They live in THIS task, not
// Task 6, because they call the four-argument `enrich_from_nvrs` introduced
// here — in Task 6 they would not compile.

/// Runs one NVR with scripted responses.
///
/// Every mock's exact call count is asserted, including the zeroes. That is
/// what makes these tests meaningful: without exact counts, a "later RPC"
/// test could pass merely because an earlier unmatched request produced a
/// transport failure, proving nothing about the RPC it claims to exercise.
fn run_chain(
    bodies: [(&str, &str); 3],
    expected_calls: [usize; 3],
) -> (crate::output_cache::CacheStats, std::io::Result<(usize, usize)>) {
    let mut server = mockito::Server::new();
    let mocks = mock_hub(&mut server, bodies, expected_calls);

    let d = tempfile::TempDir::new().unwrap();
    let cache = crate::output_cache::OutputCache::new(
        d.path(), "20260912T010203Z-abcdef01", "koji", KOJI_SCHEMA_VERSION).unwrap();
    let hub = format!("{}/kojihub", server.url());
    let out = d.path().join("out.nt");
    let e = KojiEnricher::new_standalone(&hub, "fedora", "44", None);
    let result = e.enrich_from_nvrs(
        &["zlib-1.3-1.fc44".to_string()], out.to_str().unwrap(), None, Some(&cache),
    );

    // Exact call counts, including the zeroes.
    for m in &mocks {
        m.assert();
    }
    (cache.stats(), result)
}

#[test]
fn a_fault_in_get_build_makes_the_item_retryable() {
    // The later RPCs must not run at all once getBuild has faulted.
    let (s, r) = run_chain(
        [("getBuild", fault()), ("listBuildRPMs", ok_rpms()), ("queryRPMSigs", ok_sigs())],
        [1, 0, 0],
    );
    assert!(r.is_ok(), "a faulted item is reported, not an error: {r:?}");
    assert_eq!(s.retryable, 1, "a getBuild fault must not be checkpointed");
    assert_eq!(s.hits, 0);
}

#[test]
fn a_fault_in_list_build_rpms_makes_the_whole_item_retryable() {
    // getBuild succeeded, so the chain reached listBuildRPMs. The item must
    // STILL be retryable -- the whole-chain rule, and the case a per-RPC
    // check would miss. queryRPMSigs must not be reached.
    let (s, r) = run_chain(
        [("getBuild", ok_build()), ("listBuildRPMs", fault()), ("queryRPMSigs", ok_sigs())],
        [1, 1, 0],
    );
    assert!(r.is_ok());
    assert_eq!(s.retryable, 1, "an inconclusive later RPC must poison the item");
}

#[test]
fn a_fault_in_query_rpm_sigs_makes_the_whole_item_retryable() {
    // A build whose signature lookup faulted must not be checkpointed as
    // though it were unsigned. All three RPCs run.
    let (s, r) = run_chain(
        [("getBuild", ok_build()), ("listBuildRPMs", ok_rpms()), ("queryRPMSigs", fault())],
        [1, 1, 1],
    );
    assert!(r.is_ok());
    assert_eq!(s.retryable, 1);
}

#[test]
fn a_fully_successful_chain_is_checkpointed() {
    let (s, r) = run_chain(
        [("getBuild", ok_build()), ("listBuildRPMs", ok_rpms()), ("queryRPMSigs", ok_sigs())],
        [1, 1, 1],
    );
    let (builds, triples) = r.expect("a conclusive chain must succeed");
    assert_eq!(builds, 1);
    assert!(triples > 0, "a found build must emit triples");
    assert_eq!(s.retryable, 0, "a conclusive chain must be Complete");
    assert_eq!(s.misses, 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib enrich_koji::tests::passing_no_checkpoint_writes_no_checkpoint_files`
Expected: FAIL to compile — `enrich_from_nvrs` takes 3 arguments.

- [ ] **Step 3: Implement**

Change the signature and wrap the per-NVR body:

```rust
    /// `checkpoint` is injected explicitly rather than constructed here:
    /// standalone `enrich-koji --srpm-list` shares this method but has no
    /// wrapper that commits after publication, so it must pass `None` and
    /// never leave an active generation behind.
    pub fn enrich_from_nvrs(
        &self,
        nvrs: &[String],
        output_path: &str,
        limit: Option<usize>,
        checkpoint: Option<&crate::output_cache::OutputCache>,
    ) -> Result<(usize, usize)> {
```

Inside the existing `for nvr in nvrs` loop, replace the
`match self.get_build(nvr, &mut writer)` block with:

```rust
            use crate::output_cache::{CachedOutput, CanonicalContext, ComputeOutcome, OutputCache};
            let disabled = OutputCache::disabled();
            let cache = checkpoint.unwrap_or(&disabled);

            // `rpc_cache_version` is in the context so the two versions cannot
            // drift apart by hand. Bumping KOJI_RPC_CACHE_VERSION stops old
            // *source-cache* entries being read, but an interrupted generation
            // already holds *output* fragments the old parser produced, and
            // those hit before any source lookup happens. Carrying the version
            // here invalidates both in one move.
            let ctx = CanonicalContext::new()
                .field("distro", &self.distro)
                .field("release", &self.release)
                .field("koji_hub", &self.koji_hub)
                .field("rpc_cache_version", KOJI_RPC_CACHE_VERSION);

            let result = cache.get_or_compute(nvr, &ctx, || {
                self.item_inconclusive.set(false);
                let mut scratch = NTriplesWriter::new(Vec::<u8>::new());
                let logical = self.get_build(nvr, &mut scratch)?;
                let out = CachedOutput {
                    logical_triples: logical,
                    skipped_invalid_iri: scratch.skipped_invalid_iri,
                    auto_inverses: scratch.auto_inverses,
                    text: scratch.into_string()?,
                };
                Ok(if self.item_inconclusive.get() {
                    ComputeOutcome::Retryable(out)
                } else {
                    ComputeOutcome::Complete(out)
                })
            });

            match result {
                Ok(out) => {
                    for line in out.text.lines() {
                        writer.write_raw_line(line)?;
                    }
                    writer.skipped_invalid_iri += out.skipped_invalid_iri;
                    writer.auto_inverses += out.auto_inverses;
                    if out.logical_triples > 0 {
                        total_builds += 1;
                        total_triples += out.logical_triples;
                        eprintln!("  {} → {} triples", nvr, out.logical_triples);
                    } else {
                        eprintln!("  {} → not found", nvr);
                    }
                }
                Err(e) => eprintln!("  {} → error: {}", nvr, e),
            }
```

Before the final `Ok((total_builds, total_triples))`, add the summary:

```rust
        if let Some(c) = checkpoint {
            let s = c.stats();
            eprintln!(
                "Koji checkpoint: {} hits, {} misses, {} retryable, {} write-fail, {} integrity-fail",
                s.hits, s.misses, s.retryable, s.write_failures, s.integrity_failures
            );
        }
```

- [ ] **Step 4: Update all three existing `enrich_from_nvrs` call sites**

There are three, and **all** get `None` in this task:

1. `main.rs:2906` — the standalone `EnrichKoji` command. Stays `None` forever.
2. `main.rs:3651` — `rpm-full`. `None` *here*; Task 8 changes this one argument
   to `Some(&koji_cache)`.
3. `enrich_koji.rs:1000` — `test_enrich_from_nvrs_processes_list`, a
   pre-existing test. Stays `None`.

Site 2 is the one that is easy to skip, and skipping it leaves the **binary**
uncompilable between this commit and Task 8's: `cargo test --lib` in Step 5
builds the library only, so nothing here would catch it. Hence the `--bins`
check below.

```bash
grep -rn 'enrich_from_nvrs(' etl/pg-collect/src/
cargo build --manifest-path etl/pg-collect/Cargo.toml --bins
```

Expected: three call sites, each passing four arguments with `None` as the
fourth, and a clean binary build.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml --lib enrich_koji::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add etl/pg-collect/src/enrich_koji.rs etl/pg-collect/src/main.rs
git commit -m "feat(etl): checkpoint the koji stage behind explicit injection

enrich_from_nvrs takes Option<&OutputCache>; only rpm-full passes Some.
Standalone enrich-koji --srpm-list shares the method but has no
commit-after-publish lifecycle, so implicit adoption would let its
generation stay active forever and reuse stale checkpoints across
unrelated invocations."
```

---

### Task 8: Wire the generation into `rpm-full`

The orchestrator creates one generation and shares it across both stages.

**Files:**
- Modify: `etl/pg-collect/src/main.rs` (`Commands::RpmFull` handler, 3544-3665)

**Interfaces:**
- Consumes: `Generation::acquire` (Task 3), `OutputCache::new`/`disabled` (Task 2),
  `collect_checkpointed` (Task 5), `enrich_from_nvrs(..., Some(&cache))` (Task 7),
  `SPEC_SCHEMA_VERSION` (Task 5), `KOJI_SCHEMA_VERSION` (Task 6).

- [ ] **Step 1: Acquire the generation once, before Stage 1**

Inside the `(|| -> std::io::Result<(usize, usize)> { ... })()` closure, immediately
after `let mut writer = ...`.

Checkpoint *setup* failure degrades to disabled rather than aborting: this is a
resilience feature, so a permission problem on the checkpoint directory must not
be able to fail a run that would otherwise succeed. That matches the best-effort
policy already applied to individual checkpoint writes.

```rust
                // One generation per run, shared by both checkpointed stages.
                // Absent --cache-dir, or if setup fails, both stages get a
                // disabled cache and the run proceeds exactly as before.
                let generation = match cache_dir.as_deref() {
                    Some(dir) => {
                        match pg_collect::checkpoint_generation::Generation::acquire(
                            std::path::Path::new(dir),
                        ) {
                            Ok(g) => {
                                eprintln!(
                                    "Checkpoint generation: {} ({})",
                                    g.id,
                                    if g.reused { "resumed" } else { "new" }
                                );
                                Some((dir.to_string(), g.id))
                            }
                            Err(e) => {
                                eprintln!(
                                    "Warning: checkpointing disabled ({}); this run cannot resume",
                                    e
                                );
                                None
                            }
                        }
                    }
                    None => None,
                };

                // Same policy for the per-stage caches: warn and disable.
                let open_cache = |stage: &str, version: &str| match &generation {
                    Some((dir, gen)) => pg_collect::output_cache::OutputCache::new(
                        std::path::Path::new(dir), gen, stage, version,
                    )
                    .unwrap_or_else(|e| {
                        eprintln!("Warning: {} checkpointing disabled ({})", stage, e);
                        pg_collect::output_cache::OutputCache::disabled()
                    }),
                    None => pg_collect::output_cache::OutputCache::disabled(),
                };
```

- [ ] **Step 2: Use it in Stage 2 (spec)**

Replace the `spec_collector.collect(...)` call:

```rust
                    let spec_cache = open_cache("spec", pg_collect::collect_spec::SPEC_SCHEMA_VERSION);
                    let (specs, triples) = spec_collector.collect_checkpointed(
                        &mut writer,
                        &srpm_names,
                        &srpm_identity_map,
                        &existing_ecosystem,
                        with_buildrequires,
                        with_maintainers,
                        &spec_cache,
                    )?;
```

- [ ] **Step 3: Use it in Stage 3 (Koji)**

Task 7 already gave this call its fourth argument as `None`. Only that argument
changes here:

```rust
                    let koji_cache = open_cache("koji", pg_collect::enrich_koji::KOJI_SCHEMA_VERSION);
                    let (builds, triples) = koji_enricher.enrich_from_nvrs(
                        &nvr_list, &koji_tmp, limit, Some(&koji_cache),
                    )?;
```

Also sort `nvr_list` where it is built (`main.rs:3629`), for the same reason the
spec stage sorts: it currently comes straight out of a `HashSet`, so iteration
order varies between processes and byte-identical output could not be a contract.

```rust
                    let mut nvr_list: Vec<String> = srpm_nvrs.into_iter().collect();
                    nvr_list.sort();
```

- [ ] **Step 4: Build and smoke-test resume + replay fidelity**

A fully-replayed run must produce byte-identical output to the run that derived
it — the property the whole design rests on.

Both checkpointed stages must be enabled, or the Koji half of the feature is never
exercised end to end — hence `--with-koji` alongside `--with-spec`.

```bash
cargo build --manifest-path etl/pg-collect/Cargo.toml
D=$(mktemp -d)
RUN="etl/pg-collect/target/debug/pg-collect rpm-full \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/43/Everything/x86_64/os/ \
  --distro fedora --release 43 --with-spec --with-koji --cache-dir $D --limit 5"

set -e   # every check below must be able to fail the script

$RUN -o "$D/fresh.nt"
GEN=$(ls "$D/output" | grep -v GENERATION)
[ "$(printf '%s\n' "$GEN" | wc -l)" -eq 1 ] || { echo "FAIL: expected one generation dir"; exit 1; }

# Count real ENTRIES, not directories: OutputCache::new creates each
# stage/version directory whether or not anything is ever persisted, so
# `test -d` would pass even if the stage checkpointed nothing at all.
for stage in spec koji; do
  n=$(find "$D/output/$GEN/$stage" -type f 2>/dev/null | wc -l)
  [ "$n" -gt 0 ] || { echo "FAIL: $stage wrote no checkpoint entries"; exit 1; }
  echo "$stage checkpoint entries: $n"
done

# Re-run WITHOUT committing: the generation is still active, so this must
# replay rather than re-derive.
$RUN -o "$D/replayed.nt" 2> "$D/replay.log"
[ "$GEN" = "$(ls "$D/output" | grep -v GENERATION)" ] || { echo "FAIL: generation changed"; exit 1; }
diff -q "$D/fresh.nt" "$D/replayed.nt" || { echo "FAIL: replay not byte-identical"; exit 1; }

# The run must actually have replayed, not silently re-derived everything.
# Each pattern is anchored to its own stage's summary line: a bare
# 'checkpoint: N hits' also matches the Koji line, so both checks would pass
# with zero spec hits.
grep -qE 'Koji checkpoint: [1-9][0-9]* hits' "$D/replay.log" \
  || { echo "FAIL: no koji checkpoint hits on replay"; cat "$D/replay.log"; exit 1; }
grep -qE 'Spec collection complete:.*\(checkpoint: [1-9][0-9]* hits' "$D/replay.log" \
  || { echo "FAIL: no spec checkpoint hits on replay"; cat "$D/replay.log"; exit 1; }
echo "resume + replay fidelity: OK"
```
Expected: each stage reports a non-zero entry count, the generation directory name
is unchanged, `diff` reports no differences, and the replay log shows non-zero hits
for both stages.

Then confirm the generation actually retires:

```bash
etl/pg-collect/target/debug/pg-collect checkpoint commit --cache-dir "$D"
$RUN -o "$D/third.nt"
NEW=$(ls "$D/output" | grep -v GENERATION)
[ "$GEN" != "$NEW" ] || { echo "FAIL: generation was reused after commit"; exit 1; }
[ ! -d "$D/output/$GEN" ] || { echo "FAIL: old generation not pruned"; exit 1; }
echo "retire + prune after commit: OK"
```
Expected: a *different* generation directory, and the old one gone.

- [ ] **Step 5: Run the full suite and commit**

Run: `cargo test --manifest-path etl/pg-collect/Cargo.toml`

```bash
git add etl/pg-collect/src/main.rs
git commit -m "feat(etl): wire the checkpoint generation into rpm-full

One generation per run, acquired by the orchestrator and shared by the
spec and koji stages. Without --cache-dir both stages get a disabled
cache, so behavior is unchanged."
```

---

### Task 9: Update the ten `rpm-full` wrappers and validate their ordering

Correctness here is an ordering property no Rust test can observe: the commit must
follow a successful upload, and checkpoints must never reach Minio.

**Files:**
- Modify: `deploy/quadlet/collectors/scripts/{fedora-43-full,fedora-44-full,centos-stream-9-full,centos-stream-10-full,rhel-9-full,rhel-10-full,alma-9-full,alma-10-full,rocky-9-full,rocky-10-full}.sh`
- Create: `deploy/quadlet/collectors/scripts/test-wrapper-checkpoint-contract.sh`

- [ ] **Step 1: Write the failing validation script**

Create `deploy/quadlet/collectors/scripts/test-wrapper-checkpoint-contract.sh`:

```bash
#!/bin/bash
# Asserts the checkpoint contract every rpm-full wrapper must satisfy.
# These are ordering and exclusion properties no Rust test can observe.
set -uo pipefail
cd "$(dirname "$0")"
fail=0

for f in *-full.sh; do
  grep -q 'pg-collect rpm-full' "$f" || continue

  # Executable lines only -- a contract satisfied by a comment is not
  # satisfied at all.
  code=$(grep -vE '^\s*#' "$f")

  # Exactly three mirrors: startup warm, periodic loop, final sync.
  n_mirror=$(printf '%s\n' "$code" | grep -c 'mc mirror')
  [ "$n_mirror" -eq 3 ] || { echo "FAIL $f: expected 3 mc mirror calls, found $n_mirror"; fail=1; }

  # Every one of them must exclude the checkpoint subtree, both directions.
  n_excl=$(printf '%s\n' "$code" | grep 'mc mirror' | grep -c -- "--exclude 'output/\*'")
  [ "$n_excl" -eq "$n_mirror" ] || { echo "FAIL $f: $((n_mirror-n_excl)) mirror(s) missing output/* exclusion"; fail=1; }

  # Direction: startup pulls remote->local; the other two push local->remote.
  printf '%s\n' "$code" | grep 'mc mirror' | head -1 | grep -q '"${MINIO_CACHE}/" "${CACHE_DIR}/"' \
    || { echo "FAIL $f: first mirror is not remote->local (startup warm)"; fail=1; }
  n_push=$(printf '%s\n' "$code" | grep 'mc mirror' | grep -c '"${CACHE_DIR}/" "${MINIO_CACHE}/"')
  [ "$n_push" -eq 2 ] || { echo "FAIL $f: expected 2 local->remote mirrors, found $n_push"; fail=1; }

  # Commit appears exactly once, is executable, and follows the upload.
  n_ck=$(printf '%s\n' "$code" | grep -c 'checkpoint commit')
  [ "$n_ck" -eq 1 ] || { echo "FAIL $f: expected exactly 1 'checkpoint commit', found $n_ck"; fail=1; }

  up=$(grep -nE '^[^#]*upload-nt\.sh' "$f" | head -1 | cut -d: -f1)
  ck=$(grep -nE '^[^#]*checkpoint commit' "$f" | head -1 | cut -d: -f1)
  if [ -z "$up" ]; then
    echo "FAIL $f: no executable upload-nt.sh line"; fail=1
  elif [ -z "$ck" ]; then
    echo "FAIL $f: no executable 'checkpoint commit' line"; fail=1
  elif [ "$ck" -lt "$up" ]; then
    echo "FAIL $f: commit (line $ck) precedes upload (line $up)"; fail=1
  fi

  # The upload must be able to fail the script. '|| true' would let a failed
  # publication reach the commit and retire a generation that never shipped.
  printf '%s\n' "$code" | grep 'upload-nt.sh' | grep -qE '\|\|\s*true' \
    && { echo "FAIL $f: upload-nt.sh is guarded by '|| true'"; fail=1; }

  # set -e is what makes the ordering load-bearing.
  grep -qE '^set -[a-z]*e' "$f" || { echo "FAIL $f: no 'set -e'"; fail=1; }
done

[ "$fail" -eq 0 ] && echo "All rpm-full wrappers satisfy the checkpoint contract."
exit "$fail"
```

```bash
chmod +x deploy/quadlet/collectors/scripts/test-wrapper-checkpoint-contract.sh
```

- [ ] **Step 2: Run it to verify it fails**

Run: `deploy/quadlet/collectors/scripts/test-wrapper-checkpoint-contract.sh`
Expected: FAIL — ten `no 'checkpoint commit'` lines plus mirror-exclusion failures.

- [ ] **Step 3: Update each of the ten wrappers**

In every file, add `--exclude 'output/*'` to all three `mc mirror` invocations
(startup warm, the 5-minute background loop, and the final sync), and insert the
commit between the upload and the final mirror. Using `fedora-44-full.sh` as the
model, the startup warm becomes:

```sh
mc mirror --overwrite --exclude 'output/*' "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true
```

the background loop becomes:

```sh
( while sleep 300; do
    mc mirror --overwrite --exclude 'output/*' "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
  done ) &
```

and the tail of the script becomes:

```sh
/app/scripts/upload-nt.sh /tmp/collection/fedora-44.nt "$GRAPH_URI"

# Only after a successful publication: retire this run's checkpoint
# generation so the next scheduled run starts fresh. `set -e` means a
# failed collect or upload never reaches this line, leaving the run
# resumable.
pg-collect checkpoint commit --cache-dir "${CACHE_DIR}"

echo "Syncing cache to Minio..."
mc mirror --overwrite --exclude 'output/*' "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
```

- [ ] **Step 4: Run the validation script to verify it passes**

Run: `deploy/quadlet/collectors/scripts/test-wrapper-checkpoint-contract.sh`
Expected: `All rpm-full wrappers satisfy the checkpoint contract.`

- [ ] **Step 5: Verify the exclusion works against the deployed `mc`**

The design depends on `mc`'s `*` crossing `/` (unlike a shell glob). Confirm
against the image actually deployed rather than assuming:

```bash
podman run --rm --entrypoint /bin/bash ghcr.io/packagegraph/etl:devel-latest -c '
  mkdir -p /tmp/s/output/g1/spec/v1 /tmp/s/spec/f/44 /tmp/d
  echo x > /tmp/s/output/g1/spec/v1/deep; echo x > /tmp/s/output/GENERATION
  echo y > /tmp/s/spec/f/44/keep.spec
  out=$(mc mirror --overwrite --dry-run --exclude "output/*" /tmp/s/ /tmp/d/ 2>&1)
  echo "$out" | grep -q "output/" && { echo "FAIL: a checkpoint path was transferred"; exit 1; }
  echo "$out" | grep -q "keep.spec" || { echo "FAIL: sibling cache file was NOT transferred"; exit 1; }
  echo "PASS: checkpoints excluded, sibling cache still syncs" '
```
Expected: `PASS`. Asserting both halves matters — an exclusion that accidentally
matched everything would also produce zero `output/` hits while silently breaking
the source cache.

- [ ] **Step 6: Commit**

```bash
git add deploy/quadlet/collectors/scripts/
git commit -m "feat(deploy): commit checkpoint generations in rpm-full wrappers

Adds --exclude 'output/*' to every cache mirror (checkpoints are
local-only: the pipeline's Minio credentials can PUT but not rm, verified
live, so mirrored checkpoints could never be pruned), and retires the
generation only after a successful upload. Adds a contract test for the
ordering, which no Rust test can observe."
```

---

### Task 10: Documentation

**Files:**
- Modify: `deploy/quadlet/README.md`

- [ ] **Step 1: Document the checkpoint lifecycle**

Add a subsection under the collectors section:

```markdown
### Collector checkpoints (rpm-full)

`rpm-full` collectors checkpoint each item's derived triples under
`<cache_dir>/output/<generation>/`, so a run killed by `TimeoutStartSec`
resumes instead of restarting. A generation is created at run start, reused
by retries of an interrupted run, and retired by `pg-collect checkpoint
commit` — which the wrapper runs only after `upload-nt.sh` succeeds, so a
run that collected everything but failed to publish stays resumable.

Checkpoints are deliberately **local-only** and excluded from every Minio
mirror with `--exclude 'output/*'`. The pipeline's Minio credentials can PUT
but not delete (verified 2026-09-12: `mc rm` returns `Access Denied`), so
mirrored checkpoints could never be pruned and would accumulate forever. The
scratch volume is a bind mount on the data disk and already survives
container restarts and host reboots, which is what the failure mode this
feature addresses actually needs. Losing the host costs one full
re-collection.

Bumping `SPEC_SCHEMA_VERSION` or `KOJI_SCHEMA_VERSION` invalidates that
stage's checkpoints. Bump one whenever that stage's emitted triples change,
including via shared serialization or ontology helpers — a stale fragment is
indistinguishable from a correct one.

`KOJI_RPC_CACHE_VERSION` is separate: it retires the Koji stage's 30-day
source-cache entries when a parser change makes previously-stored responses
untrustworthy. It is also part of that stage's checkpoint identity, so
bumping it invalidates both caches at once — necessary, because the source
cache sits behind the checkpoint and would otherwise never be consulted.
```

- [ ] **Step 2: Commit**

```bash
git add deploy/quadlet/README.md
git commit -m "docs(deploy): document the rpm-full checkpoint lifecycle"
```

---

## Acceptance (run once on the host after deploying)

Per the spec's operational criteria, measure on one full `fedora-44-full` run:

- checkpoint bytes and inode count under `output/`, against free space on the
  data disk;
- that `output/` is absent from Minio in both directions
  (`mc ls --recursive pgraph/$MINIO_BUCKET/collector-cache/fedora-44-full/ | grep -c output/` → `0`);
- replay time for a resumed run versus fetch+derive for a fresh one — the headline
  number;
- that a mid-run `SIGKILL` followed by a restart resumes rather than restarts,
  with the completed-item count preserved.

---

## Out of scope (found while planning; not fixed here)

**`FileCache` does not enforce its TTL for Minio-backed entries.**
`read_local` (`cache.rs:269`) checks file mtime against the TTL, but the Minio
fallback `read_minio` (`:295`) accepts any successful JSON response with no age
check at all — and `get` (`:234`) then writes the value back to the local file,
refreshing its mtime. So for any collector configured with Minio, a cache entry
never expires: the local copy ages out, the remote copy resurrects it, and the
clock restarts. The `30 days TTL` comment at `enrich_koji.rs:41` is not what the
code does.

This matters to Task 6 in one direction only, and that direction is already
handled: the retired `koji-build-*` / `koji-sigs-*` objects are unreachable
under the new prefix regardless of age, so correctness does not depend on them
expiring. What remains is that they accumulate, along with every other
collector's, and that no Minio-backed entry is ever refreshed.

Not fixed here because the blast radius is wrong for this plan: `FileCache` is
shared by every enricher, and making TTLs suddenly bite would trigger a
simultaneous refetch across all of them. It wants its own change, with either a
stored creation timestamp in the envelope (the `http_cache.rs` envelope pattern
already does this) or a bucket lifecycle rule — plus a deliberate decision about
the refetch storm.
