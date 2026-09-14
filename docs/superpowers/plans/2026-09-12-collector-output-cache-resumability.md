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
- Collector iteration order is sorted (`nvr_list` already is; the spec stage in
  Task C still needs it), which is what makes a *checkpointed stage's fragment*
  replay identically rather than by coincidence of `HashSet` ordering. Note this
  is a per-stage property, not a whole-file one — see the invariants below.

---

## Implemented (2026-09-14)

These tasks are done and merged into `feat/collector-resumability`. Their
step-by-step snippets have been removed: the code and its tests are now the
source of truth, and stale snippets actively mislead — one had already
reintroduced a "30-day TTL" claim that the code does not honour, and another
still instructed implementers to build hub-blind source-cache keys, the exact
defect the implementation exists to fix.

To change any of this, read the code and change it there.

| Task | Commit | What it established |
|---|---|---|
| 1. Genericize emit helpers | `a5eacea` | `forge.rs`/`collect_spec.rs`/`enrich_koji.rs` emit helpers take `&mut NTriplesWriter<W>`, so derivation can run against a `Vec<u8>` scratch writer. No call site changed. |
| 2. `OutputCache` core | `b2a2959` | `etl/pg-collect/src/output_cache.rs`. Injective length-prefixed context encoding, integrity-checked versioned envelope, atomic writes, best-effort persistence, explicit disabled mode. 18 tests. |
| 3. Run-generation lifecycle | `b93a673` | `etl/pg-collect/src/checkpoint_generation.rs` + `pg-collect checkpoint commit`. Ids claimed by exclusive directory creation; the CLI treats "nothing to commit" as success so a degraded run cannot abort its wrapper. 11 tests. |
| 6. Typed Koji RPC parsing | `1ee11ce` | `KojiRpcResult`, the recursive-descent parser, per-RPC `ArraySchema`s, and `RpcCacheKey`. Replaces `parse_xmlrpc_struct`/`parse_xmlrpc_array`, which are deleted. 50 tests. |
| 7. Koji stage checkpointing | `c76629d` | `enrich_from_nvrs(.., checkpoint: Option<&OutputCache>)`, explicit injection, plus the hub-swap integration test. |
| 8. `rpm-full` wiring (Koji half) | `5ddbf48` | One generation per run, `open_cache` helper, sorted `nvr_list`, Koji stage checkpointed. **The spec half is still open — see Task C.** |
| 9. Wrapper contract | `5a73b0e` | All ten wrappers exclude `output/*` and commit after upload, plus `test-wrapper-checkpoint-contract.sh`. |

### Invariants these establish (verified, not assumed)

- **Source-cache identity includes the hub.** Koji ids are hub-relative, so
  `queryRPMSigs(1)` means different things on different hubs. Both the
  `RpcCacheKey` and the output checkpoint's `CanonicalContext` carry
  `koji_hub`; dropping either one independently fails
  `changing_hubs_neither_replays_nor_reuses_the_other_hubs_data`.
- **`KOJI_RPC_CACHE_VERSION` invalidates both tiers.** It is both a key
  segment and a context field, because the source cache sits *behind* the
  output checkpoint and would never be consulted on a fragment hit.
- **The new key namespace is disjoint from the legacy one.** `koji-rpc-…`
  cannot be produced by appending to `koji-build-`/`koji-sigs-`; a version
  segment appended to the old prefix would not be, since RPM names contain
  hyphens.
- **A retryable item persists nothing.** Transport failures, faults and
  malformed responses mark the whole per-NVR chain retryable.
- **Checkpoints never reach Minio.** Verified against the deployed image's
  `mc` (RELEASE.2025-04-08): `--exclude 'output/*'` excludes at every depth
  while siblings still sync. Do not verify this with `--dry-run` — that mc
  prints a summary table with no paths, so any grep of it passes vacuously.
- **Binary activation and wrapper contract are one unit.** A generation is
  retired only by an explicit `checkpoint commit`. Enabling checkpointing in
  the binary without the wrapper change leaves a generation active forever and
  replays its fragments across every later scheduled run.
- **Whole-file byte-identity is NOT the replay contract.** Stage 1 is not
  checkpointed and emits a `dq#detectedAt` wall-clock timestamp, so `diff`ing
  two full runs always differs. The contract is that each *checkpointed
  stage's fragment* replays byte-identically, in the same order.

---

### Task A: `SpecFetchResult` — stop collapsing transport errors into `NotFound`

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

Split `process_spec` into a fetching shell and a content-taking body. Task B needs
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

### Task B: Checkpoint the spec stage

**Files:**
- Modify: `etl/pg-collect/src/collect_spec.rs` (`collect`, `process_spec`, add `SPEC_SCHEMA_VERSION`)
- Test: `etl/pg-collect/src/collect_spec.rs`

**Interfaces:**
- Consumes: `OutputCache`, `CachedOutput`, `ComputeOutcome`, `CanonicalContext`, and the generic writers — all already implemented, see above; `SpecFetchResult` (Task A).
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
    // already differs. enrich_koji.rs has the mirror of this test.
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
may be checkpointed. It uses `process_spec_with_content` (Task A) rather than
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

### Task C: Wire the spec stage into `rpm-full`

The remaining half of the `rpm-full` wiring. The generation and `open_cache` helper already exist
in the `Commands::RpmFull` handler (`main.rs`); this only adds the spec side.

Replace the `spec_collector.collect(...)` call with `collect_checkpointed`
(Task B), passing `open_cache("spec", SPEC_SCHEMA_VERSION)`. Sort `srpm_names`
where the spec stage iterates it, for the same reason `nvr_list` is sorted:
it comes out of a `HashSet`, so fragment order would otherwise vary between
processes.

Then extend the rehearsal at
`/home/bharring/.claude/jobs/e22de645/tmp/rehearsal/run.sh` to assert
non-zero spec hits on replay, and that the spec fragment replays
byte-identically — the same shape as the Koji assertions already there.

**Do not assert whole-file byte-identity.** Stage 1 emits a `dq#detectedAt`
wall-clock timestamp, so a full-output `diff` between two runs always differs
for reasons that have nothing to do with checkpointing.

---

### Task D: Documentation

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

`KOJI_RPC_CACHE_VERSION` is separate: it retires the Koji stage's cached RPC
responses when a parser change makes previously-stored ones untrustworthy. It
is also part of that stage's checkpoint identity, so bumping it invalidates
both caches at once — necessary, because the source cache sits behind the
checkpoint and would otherwise never be consulted.

Note that the Koji `FileCache`'s nominal 30-day TTL is not enforced for
Minio-backed entries (`cache.rs:295` does not check age), so bumping the
version — not expiry — is what actually retires a bad entry.
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

This mattered to the Koji source-cache rename in one direction only, and that direction is already
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
