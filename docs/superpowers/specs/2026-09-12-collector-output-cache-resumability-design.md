# Collector output-checkpoint resumability — design

## Problem

`pg-collect@<name>.service` is `systemd` `Type=oneshot` with `TimeoutStartSec=28800`
(8h — already raised once from 5h on 2026-09-10, after `fedora-43-full` lost a run
at 99.6% complete). `upload-nt.sh` runs exactly once, after the whole `pg-collect`
process exits successfully. So a timeout, a `SIGKILL`, or a manual restart at any
point discards 100% of that run's work — there is no partial credit.

`fedora-44-full` repeated this failure live on 2026-09-12: three restarts in one
day (two interrupted early during an image-update attempt, one ran the full 8h and
still didn't finish), zero completions, zero uploaded data, despite roughly 24h of
cumulative container runtime.

PR #49 (merged 2026-09-12) unified the crate's HTTP send path with retry, backoff,
and per-host pacing. That improves fault tolerance on individual fetches, but it's
an orthogonal problem: a collector that never fails a single HTTP request still
loses everything if it's killed before its one final upload. This spec addresses
that separate gap.

## What this is, and is not

This is a **resumability checkpoint mechanism**, not a general output memoization
cache. A checkpoint entry is valid only for *retries of one in-flight run*, and is
discarded once that run publishes successfully. Treating it as a durable cache
would silently freeze upstream data — a correctness bug, not merely staleness.

Checkpoints are **local to the collector host** and are deliberately not synced to
Minio. See "Storage location" for why.

## Scope decisions

- **Internal only.** Minio and `qlever-rebuild-index.sh` never see partial data:
  exactly one *committed publication* per completed run, as today. The
  whole-graph-replacement/dedup gate requires **no changes** and carries no new
  risk.
- **`rpm-full` only.** The storage primitive lives in shared crate plumbing
  (alongside `source_cache.rs`), but only `rpm-full` adopts it. Standalone
  `enrich-koji` is explicitly **unchanged** — see "Explicit injection".
- **Generation owned by the orchestrator.** `rpm-full` creates one run generation
  and shares it across both its spec and Koji stages.

## Architecture

`output_cache.rs` stores the **derived output** of a per-item processing step (as
opposed to `SourceCache`/`cache.rs`, which store the raw upstream fetch). A hit
means "this item, under this schema version, configuration, and run generation,
was already fully processed"; its output replays with no network call and no
re-derivation.

The local `.nt` file needs no special handling — it is still truncated and rebuilt
on every invocation, because rebuilding it is now cheap. No ledger file, no
append-across-restart, no torn-write recovery: what persists is the per-item
checkpoint, not the assembled output.

Stage 1 (per-arch RPM/SRPM enumeration) is out of scope — a bulk local parse of
already-downloaded metadata, with no per-item network calls to checkpoint.

### Cache identity

```
<cache_dir>/output/<generation>/<stage>/<schema_version>/<key_digest>
```

| Segment | Invalidates on |
|---|---|
| `generation` | A successful publication (upstream may have changed since) |
| `stage` | — namespaces `spec` vs `koji` so keys cannot collide |
| `schema_version` | A code change to that stage's emitted triples |
| `key_digest` | The item identity and its declared context (below) |

### Key digest — canonical and injective

Plain concatenation is ambiguous (`("ab","c")` and `("a","bc")` share a preimage).
The digest is over **fixed-width sub-digests**, never raw concatenated strings:

```
key_digest = hex(sha256( sha256(canonical_context) || sha256(item_key) ))
```

Both inner digests are fixed 32-byte binary, so field boundaries are unambiguous.
`canonical_context` is a length-prefixed encoding of the stage's declared context
(below): each field emitted as `<u32 length><utf8 bytes>`, in a fixed declared
order, with lists sorted and length-prefixed element-wise. Booleans encode as
`"0"`/`"1"` — never as Rust's `Display`, which would let a future type change alter
the encoding silently.

## Storage location: local only, not Minio

Checkpoints live on the `pg-collect-scratch` volume and are **excluded from the
Minio cache mirror** (`mc mirror --exclude 'output/*'` in both directions).

Three reasons, in order of weight:

1. **Remote deletion is impossible with the pipeline's credentials.** Verified
   empirically on 2026-09-12: `mc pipe` (PUT) succeeds, `mc rm` returns
   `Access Denied`. Generation pruning therefore *cannot* be implemented remotely.
   Mirrored checkpoints would accumulate forever, be re-downloaded on every
   startup warm, be pruned locally, and reappear on the next run. Adding a broad
   `mc mirror --remove` is not an acceptable workaround: a failed cache warm could
   then delete valid remote *source-cache* entries.
2. **Local storage is already durable enough for the failure mode this fixes.**
   `pg-collect-scratch` is a bind mount on the dedicated data disk
   (`/var/lib/packagegraph/pg-collect-scratch`), surviving container restarts and
   host reboots — verified: per-collector cache directories from prior days
   persist there today. Our actual failure is "systemd kills the run, systemd
   restarts it on the same host", which local storage covers completely.
3. **It avoids the cost problem entirely.** Tens of thousands of small objects per
   stage would materially change the five-minute mirror's cost profile. Not
   syncing them removes that concern rather than mitigating it.

**Accepted trade-off:** losing the host or the volume loses in-flight checkpoints,
costing one full re-collection. That is a rare event with a bounded,
already-familiar cost.

## Run generation (lifecycle)

Without this, an entry would be reused across *all future scheduled runs*,
permanently bypassing refresh mechanisms the collectors rely on: spec files are
re-fetched each run (`collect_spec.rs:385`), the Koji `FileCache` is *intended*
to expire after 30 days (`enrich_koji.rs:41`), and Koji signatures can appear
after a build was first observed. Schema versions invalidate *code* changes, not
*upstream data* changes.

**That Koji TTL does not currently work when Minio is configured.**
`FileCache::read_local` (`cache.rs:269`) checks file mtime against the TTL, but
the Minio fallback `read_minio` (`:295`) accepts any successful response with no
age check, and `get` (`:234`) then rewrites the local file — refreshing its
mtime. So the local copy ages out, the remote copy resurrects it, and the clock
restarts: no Minio-backed entry ever expires. This is a pre-existing `FileCache`
defect, tracked separately from this design; it strengthens rather than weakens
the case for run generations, since the generation is then the *only* mechanism
bounding reuse.

**State file:** `<cache_dir>/output/GENERATION` — generation id plus status
(`active` | `complete`), written atomically.

**Generation ids must be unique and never reused.** A bare one-second-precision
timestamp is insufficient: a run that completes and a new invocation that starts
within the same second would mint the *same* directory name, and pruning would
preserve that directory — letting the previous run's fragments masquerade as the
new generation's. The id is therefore `<UTC YYYYMMDDTHHMMSSZ>-<8 hex random>`, and
minting uses **exclusive directory creation** (`fs::create_dir`, which fails if the
path exists) as the authority: on collision, re-mint and retry. The timestamp is
retained only for human legibility when reading a directory listing; uniqueness
comes from the random suffix plus the exclusive create.

**Generation state is validated before use, not trusted.** The id is used as a
path component, so before it is joined to any path it must match
`^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{8}$`. Anything else — a malformed status, an
unparseable file, unknown fields, an id containing a separator or `.`/`..` — makes
the state unusable: log the invalidation and mint a fresh generation. A generation
whose state cannot be structurally authenticated is never reused.

**`rpm-full` startup** (the orchestrator owns this; stages receive the generation):
- absent, `complete`, or invalid → mint a new unique id, write `active`, and prune
  every other generation directory;
- `active` and valid → reuse it (this is a retry of an interrupted run).

**Completion:** a new `pg-collect checkpoint commit --cache-dir <dir>` subcommand
flips the status to `complete`. Wrappers call it after publication succeeds:

```sh
pg-collect rpm-full ... -o "$OUTPUT"
/app/scripts/upload-nt.sh "$OUTPUT" "$GRAPH_URI"
pg-collect checkpoint commit --cache-dir "$CACHE_DIR"    # only after upload succeeds
mc mirror --overwrite --exclude 'output/*' "${CACHE_DIR}/" "${MINIO_CACHE}/"
```

All ten `rpm-full` wrappers **will be updated** to this shape (none has a
checkpoint commit or a mirror exclusion today): `fedora-43-full`,
`fedora-44-full`, `centos-stream-9-full`, `centos-stream-10-full`, `rhel-9-full`,
`rhel-10-full`, `alma-9-full`, `alma-10-full`, `rocky-9-full`, `rocky-10-full`.
Note that `rhel-*` run under the separate `pg-collect-rhel@.container` template
but use the same wrapper shape.

Pruning at mint time (not at commit) means a crash between publish and commit
leaves the prior generation intact, costing one redundant re-collection rather
than losing data.

## Compute outcome: cacheability ≠ success, and ≠ absence of DQ

`io::Result` is the wrong signal, because both adopters return `Ok` after emitting
a data-quality issue. But **"emitted a DQ issue" is equally wrong as a proxy for
"transient"**: `collect_spec.rs:458` emits a DQ issue recording detection method
and confidence on *successful* ecosystem detection. Keying off `emit_dq_issue`
would make nearly every successfully-processed spec item non-checkpointable and
gut the feature.

Cacheability is therefore an **explicit, typed stage outcome**, never inferred:

```rust
pub struct CachedOutput {
    /// Caller's logical tally. NOT derivable from the payload: write_triple
    /// auto-emits inverse statements, so output lines exceed logical writes
    /// by `auto_inverses` (ntriples.rs:39).
    pub logical_triples: usize,
    /// Parent-writer counters, restored on replay rather than recomputed.
    pub skipped_invalid_iri: usize,
    pub auto_inverses: usize,
    /// Canonical N-Triples, no graph term. Stored as String so UTF-8 validity
    /// is established once at capture, not re-litigated on every replay.
    pub text: String,
}

pub enum ComputeOutcome {
    /// Deterministic result from a valid upstream response. Persist and replay.
    Complete(CachedOutput),
    /// Operationally inconclusive. Use for this run's output, never persist.
    Retryable(CachedOutput),
}
```

### Public API

```rust
impl OutputCache {
    /// `stage` and `schema_version` are fixed at construction; `generation` is
    /// supplied by the orchestrator so both stages share one.
    pub fn new(cache_dir: &Path, generation: &str, stage: &str, schema_version: &str)
        -> io::Result<Self>;

    /// Disabled mode: `get_or_compute` runs the closure and returns its output
    /// without touching the filesystem. Used when `--cache-dir` is absent.
    pub fn disabled() -> Self;

    pub fn get_or_compute<F>(
        &self,
        item_key: &str,
        context: &CanonicalContext,
        compute: F,
    ) -> io::Result<CachedOutput>
    where
        F: FnOnce() -> io::Result<ComputeOutcome>;
}
```

Ownership is explicit: the **caller** builds the `CanonicalContext` (it alone knows
its stage's declared inputs) and classifies its own result as `Complete` or
`Retryable`; the **cache** owns digesting, envelope integrity, atomic persistence,
and replay. The three outcomes of `get_or_compute`:

| Closure returns | Output for this run | Checkpoint written |
|---|---|---|
| `Ok(Complete(o))` | `o` | yes |
| `Ok(Retryable(o))` | `o` | no |
| `Err(e)` | none — `Err(e)` propagates to the caller | no |

A cache hit returns the stored `CachedOutput` without invoking the closure. A
persistence failure on the `Complete` path is best-effort: it is warned and
counted, and `o` is still returned (see "Failure and integrity semantics").

**Classification rule:**
- **Retryable** — transport errors, API faults, malformed/unparseable responses,
  and anything else operationally inconclusive.
- **Complete** — deterministic output derived from a valid upstream response,
  *including* semantic and data-quality observations (missing Source0,
  unexpanded macros, ecosystem-detection confidence, normalized or invalid
  maintainer email). These are real findings about real input, not failures.

### Both adopters need typed results to honor this

The current code destroys the information the rule depends on, in both stages.
Adding the outcome type without these refactors would leave transient failures
misclassified as `Complete`.

**Koji — all three RPCs, not just `getBuild`.** A per-NVR fragment is produced by
a chain: `getBuild` (`enrich_koji.rs:275`), `listBuildRPMs` (`:364`), and
`queryRPMSigs` (`:414`). All three collapse fault, malformed, and legitimately
empty responses into the same empty parse (`parse_xmlrpc_struct` /
`parse_xmlrpc_array`), so today none of them can tell "no such build" from "Koji
returned a fault". Each RPC parses into:

```rust
enum KojiRpcResult<T> { ValidNonempty(T), ValidEmpty, ApiFault(String), Malformed(String) }
```

Only valid responses may contribute to a `Complete` item. **Any inconclusive RPC
anywhere in the chain makes the whole per-NVR item `Retryable`** — a build whose
`getBuild` succeeded but whose `queryRPMSigs` faulted must not be checkpointed as
though it had no signature. A chain of `ValidNonempty`/`ValidEmpty` is `Complete`;
its staleness (including a signature that has not been published yet) is bounded
by the generation, i.e. one run.

**Spec — `fetch_spec` must stop collapsing everything to `NotFound`.** It tries
several candidate URLs and, on exhaustion, returns `ErrorKind::NotFound` for every
failure mode including transport errors (`collect_spec.rs:314-340`; the `last_err`
detail added in PR #38 is diagnostic text, not a machine-readable distinction). It
must return:

```rust
enum SpecFetchResult { Found(String), NotFound, RetryableFailure(String) }
```

- every candidate URL returned a definitive 404 → `NotFound`, and the item is
  `Complete` (a real, deterministic answer about this SRPM);
- any candidate failed inconclusively (transport error, 5xx, malformed) with no
  successful fallback → `RetryableFailure`, and the item is `Retryable`;
- a spec was fetched and parsed → `Complete`, including any deterministic DQ
  findings it produces.

## Replay contract

The derive functions an earlier draft assumed do not exist: `process_spec` and
`get_build` write into a shared `NTriplesWriter` and return a logical count. This
is a real refactor, not a call-site swap.

- **Capture:** derivation runs against a scratch `NTriplesWriter<Vec<u8>>` built
  *without* a graph URI, so the payload is canonical N-Triples. The pattern exists
  already (`ntriples.rs:30-33`).
- **Replay:** cached text is written line-by-line via
  `NTriplesWriter::write_raw_line`, which appends the caller's graph term in
  N-Quads mode (`ntriples.rs:286`). Graph URI stays out of both payload and key —
  graph selection happens at replay.
- **Counters:** the scratch writer's `skipped_invalid_iri` and `auto_inverses` fold
  into the parent on capture, and are restored from the cached record on replay
  (`write_raw_line` does not recompute inverses — they are already materialized).
- **Independence:** a cached item must render standalone. Neither adopter uses
  `write_triple_once`/`write_literal_once`/`write_datetime_once` (verified: zero
  call sites in either file), and adopters **must not** — those dedup against
  per-file writer state, making a fragment depend on what preceded it.

## Context fingerprint

Each stage declares its inputs by building a `CanonicalContext` — an ordered list
of named fields that the cache encodes length-prefixed and digests (see "Key
digest"). The stage owns *what* goes in; the cache owns *how* it is encoded, so
two stages cannot disagree about encoding.

**Spec** (`collect_spec.rs:116-121, 163, 251`): distro, release, that item's sorted
identity URIs from `srpm_identity_map`, that item's `existing_ecosystem_pkgs`
membership, `emit_buildrequires`, `emit_maintainers`.

**Koji** (`enrich_koji.rs:494`): distro, release, `koji_hub`.

Production wrappers happen to use collector-specific cache roots, which masks some
collision risk; the shared API must not depend on that, since changing a flag
within one wrapper already makes old entries incompatible.

## Explicit injection (no implicit adoption)

`enrich_from_nvrs` gains an explicit parameter:

```rust
pub fn enrich_from_nvrs(..., checkpoint: Option<&OutputCache>) -> Result<(usize, usize)>
```

Only `rpm-full` passes `Some`. Standalone `enrich-koji --srpm-list` passes `None`
and is otherwise untouched, because it has no wrapper that commits after
publication — implicit adoption would let its generation stay `active` forever and
reuse stale checkpoints across unrelated invocations. The deployed
`deploy/quadlet/enrichers/scripts/koji.sh:28` uses the SPARQL-discovery path
(`enrich_koji.rs:147`), which this spec excludes outright.

Standalone `enrich-koji` can opt in later, once its wrapper gains a
commit-after-publish lifecycle; at that point both its modes can adopt the shared
primitive deliberately.

## Failure and integrity semantics

- **Cache-write failure is best-effort.** A full disk or permission error must not
  discard derived triples: warn, count, return the fresh value, continue.
  Collection correctness never depends on checkpoint durability.
- **Versioned, integrity-checked envelope.** Each entry carries
  `OUTPUT_CACHE_FORMAT_VERSION` (governing envelope serialization, distinct from
  the stages' RDF schema versions, so a serialization change does not force every
  adopter to bump its schema version), payload length, and SHA-256. An unknown
  format version, a length or digest mismatch, or an unparseable envelope is a
  miss, and the entry is evicted — mirroring PR #38's self-healing manifest read.
  `fs::read` succeeding does not mean the bytes are the ones we wrote; atomic
  rename protects only against torn local writes.
- **Atomic writes** via uniquely-named temp file (pid + atomic counter, as in
  `source_cache.rs`'s `MANIFEST_TEMP_COUNTER`) + `fsync` + `rename`. Stale temp
  files from a killed write are removed at the next generation prune.
- **Concurrency is unsupported, not merely benign.** Two concurrent runs sharing
  one generation are out of contract: identical declared context does not
  guarantee identical output, since upstream state can change between two
  in-flight fetches. systemd already enforces one instance per unit name; no
  locking is added, and none is relied upon.
- **`--cache-dir` is optional** (`main.rs:1356`). `OutputCache` has an explicit
  disabled mode: `get_or_compute` invokes the closure and returns its value
  without touching the filesystem, so adopters need no `Option` handling beyond
  the injection parameter above.

## Observability

Per PR #49's transport-stats precedent (counters that exist but are never
aggregated hide problems for months), each stage logs one line at end of run:
hits, misses, retryable-not-cached, write failures, integrity failures, plus the
generation id and whether it was reused or freshly minted.

## Versioning

`SPEC_SCHEMA_VERSION` and `KOJI_SCHEMA_VERSION` are module-local constants.
Manual versions beat hashing source files: hashing invalidates on comments,
refactors and formatting, while still missing behavior changes in shared
serialization/ontology helpers unless the hash input grows unwieldy.

Each carries the rule in a doc comment:

> Bump this for any change to this stage's emitted triples, including changes to
> shared serialization or ontology helpers it calls. A stale checkpoint fragment
> is indistinguishable from a correct one — there is no automatic detection.

Enforced by review convention (emission change and version bump in the same
patch). A golden-output test catches *unintentional* drift but cannot force a bump
on an intentional change.

## Testing

**`OutputCache` unit tests:** hit; miss; disabled mode never touches disk;
integrity failure (truncated payload, wrong digest, unknown format version,
corrupt envelope) treated as a miss and evicted; `Retryable` never persisted;
write failure returns the computed value without failing the run; unique temp
filenames.

**Key-digest tests:** the canonical encoding is injective — `("ab","c")` and
`("a","bc")` produce different digests; reordering a sorted list does not change
the digest, but changing its contents does; each declared context field (every
flag, identity-URI list, distro/release, koji hub) changes the digest
independently.

**Generation lifecycle:** fresh dir mints; `active` is reused; `checkpoint commit`
flips to `complete`; the next startup mints a new generation and prunes the old
directory; a crash between publish and commit leaves the prior generation intact.
Two mints within the same clock second produce **different** ids and different
directories (the collision this design's random suffix exists to prevent).

**Generation state validation:** a malformed status, an unparseable state file,
unknown fields, and an id failing the `^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{8}$` pattern
each cause a fresh mint rather than reuse. Ids containing `/`, `.` or `..` are
rejected before being joined to a path — asserted directly, since this is the one
place external-ish state becomes a filesystem path.

**Wrapper validation** (a test over the wrapper scripts themselves, since
correctness here is an ordering property no Rust test can observe): for each of the
ten `rpm-full` wrappers, assert that `output/*` is excluded on the startup,
periodic, and final mirrors in both directions; that `checkpoint commit` appears
*after* `upload-nt.sh`; and that `set -e` plus that ordering means neither a
collection failure nor an upload failure can reach the commit. The
`--exclude 'output/*'` semantics were verified against the deployed `mc`
(RELEASE.2025-04-08) with `--dry-run`: `*` crosses `/`, so nested checkpoint files
and `output/GENERATION` are both excluded while sibling cache subtrees still sync.
The acceptance run re-verifies this against whatever `mc` is deployed at the time.

**Outcome classification:** a successful ecosystem detection that emits a DQ issue
is `Complete` and *is* checkpointed (the regression this rule exists to prevent);
a transport error is `Retryable` and is not. Per Koji RPC: `ApiFault` and
`Malformed` are `Retryable`, `ValidEmpty`/`ValidNonempty` are `Complete` — and a
chain where `getBuild` succeeds but `listBuildRPMs` or `queryRPMSigs` is
inconclusive makes the **whole item** `Retryable`. For spec: all-404 is `NotFound`
and `Complete`; any inconclusive candidate with no successful fallback is
`Retryable` (the case today's `ErrorKind::NotFound` collapse cannot express).

**Identity:** the two stages resolve to distinct paths *and* each path contains its
own expected version segment — asserting only that the paths differ is vacuous,
since the `stage` segment already differs, and would pass even if both stages
shared one version constant.

**Resume (the tests that prove the feature):** per stage, pre-populate a
checkpoint, wrap the compute closure in a call counter, run the loop, assert the
counter is zero for that key and that `logical_triples`, `skipped_invalid_iri` and
`auto_inverses` all match a non-resumed run exactly.

**Replay fidelity:** a fully-fresh run and a fully-replayed run produce identical
output files, in both N-Triples and N-Quads mode.

**Injection:** `enrich_from_nvrs(None)` writes no checkpoint files at all —
guarding the standalone-`enrich-koji` contract.

**Golden output** per stage, as drift detection.

## Operational acceptance criteria

Since checkpoints are local-only, the Minio cost questions do not arise. Measure
on one full `fedora-44-full` run:

- checkpoint bytes and inode count under `output/` after a full run, against free
  space on the data disk;
- that `output/` is genuinely absent from the Minio mirror in both directions;
- replay time for a resumed run versus fetch+derive time for a fresh one — the
  headline number this design exists to improve;
- that a mid-run `SIGKILL` followed by a restart resumes rather than restarts,
  with the completed-item count preserved.

## Adoption criteria

A collector may adopt this only if all of the following hold.

*Correctness:*
- work is independently keyed per item;
- output is deterministic from explicitly declared context;
- complete results are distinguishable from retryable failures by an explicit
  typed outcome (not inferred from `Err` or from DQ emission);
- replay does not depend on cross-item mutable writer state.

*Storage and lifecycle* — these follow from the local-only decision and are easy to
overlook when reading "shared primitive" as "safe to switch on anywhere":
- its cache dir is on a **persistent local volume** surviving process and container
  restarts (an ephemeral `/tmp` silently reduces this to a no-op that still costs
  disk writes);
- that volume is **namespaced per collector**, so two collectors cannot collide;
- its wrapper **excludes the checkpoint subtree** from every Minio mirror, in both
  directions;
- its wrapper **commits only after successful publication**.

Flat per-item collectors (npm, Repology) fit. Recursive collectors where
processing one item mutates traversal queues or shared discovery state (PyPI,
Maven) **do not** fit without further design.

## Out of scope

- Any change to `qlever-rebuild-index.sh`, the dedup/completeness gate, or
  `upload-nt.sh`.
- Stage 1 (RPM/SRPM enumeration).
- Standalone `enrich-koji`, in both its SPARQL-discovery (`enrich_koji.rs:147`) and
  `--srpm-list` modes — unchanged, passes `None`.
- Any collector other than `rpm-full`.
- Syncing checkpoints to Minio, and any remote retention mechanism.
- Raising `TimeoutStartSec` further, or any other systemd unit change.
