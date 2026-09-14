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
- Collector iteration order is sorted (`nvr_list` in `main.rs`, and
  `collect_checkpointed`'s own `ordered` vector), which is what makes a
  *checkpointed stage's fragment*
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
| 8. `rpm-full` wiring (Koji half) | `5ddbf48` | One generation per run, `open_cache` helper, sorted `nvr_list`, Koji stage checkpointed. |
| 9. Wrapper contract | `5a73b0e` | All ten wrappers exclude `output/*` and commit after upload, plus `test-wrapper-checkpoint-contract.sh`. |
| Coordinated host cutover | `a8b79a1` | `checkpoint-cutover.md`, `checkpoint-release.py`, the real-wrapper lifecycle rehearsal, the fail-closed mutation runner, and a CI step exercising the image's pinned `mc`. Images carry `org.opencontainers.image.revision`. |
| Fail-open fixes | `c858284` | `MAX_GENERATION_AGE_DAYS`, unparseable-timestamp rejection, duration-not-day-count comparison, and systemd-shaped unit-key matching in `checkpoint-release.py`. |
| A. `SpecFetchResult` | `f157ef1` | `fetch_spec` returns `Found`/`NotFound`/`RetryableFailure` via the pure `aggregate_candidates` reducer; one inconclusive candidate poisons the aggregate. `process_spec` split into fetch shell + `process_spec_with_content`. |
| B. Spec stage checkpointing | `2f6c965` | `collect_checkpointed`, `process_spec_classified`, `Cacheability`. `collect` delegates with `OutputCache::disabled()`; iteration is sorted. `process_spec` deleted as a superseded near-duplicate. |
| C. Spec half of `rpm-full` wiring | `8deb045` | Spec stage runs through the generation's cache. `PG_COLLECT_DIST_GIT_BASE` origin override + local spec fixture; stage-scoped rehearsal assertions; spec replay and spec-transience coverage. |
| D. Documentation | see below | `deploy/quadlet/README.md` "Collector checkpoints (rpm-full)". |

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
  CI validates repository artifacts, not installed host scripts. Follow
  `deploy/quadlet/collectors/checkpoint-cutover.md`: freeze before publishing,
  install and verify a digest-pinned release, then resume. Checkpointing remains
  automatic; there is no compatibility opt-in flag.
- **Whole-file byte-identity is NOT the replay contract.** Stage 1 is not
  checkpointed and emits a `dq#detectedAt` wall-clock timestamp, so `diff`ing
  two full runs always differs. The contract is that each *checkpointed
  stage's fragment* replays byte-identically, in the same order.
- **DQ emission is not a transience signal.** Successful ecosystem detection
  emits a DQ issue, so cacheability is decided by the *fetch* classification
  alone. A definitive 404 is a real answer and is checkpointed; a 503 or
  transport error is not. Reclassifying the inconclusive branch as `Complete`
  fails `test_inconclusive_spec_fetch_is_not_checkpointed_and_retries_on_resume`.
- **Generation reuse is age-bounded.** The cutover prevents the
  image/wrapper mispairing; `MAX_GENERATION_AGE_DAYS` bounds the damage when
  the cutover is skipped anyway. Age is compared as a duration, because
  `num_days` truncates toward zero and would let a 31-day-old generation
  read as 30 under sub-second clock drift.
- **Stage progress assertions must be stage-scoped.** Both checkpointed
  stages print `<n> hits, <n> misses`, so a whole-output substring assertion
  silently matches the wrong stage.
- **The dist-git origin override is not part of checkpoint identity.**
  `PG_COLLECT_DIST_GIT_BASE` names where a spec was fetched from, not which
  spec, so switching to a mirror must not invalidate fragments. It also
  cannot invent candidate URLs for an unsupported distro, which is what keeps
  the offline unit tests offline.

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
