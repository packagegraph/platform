# Graph publication contract

How a collector's N-Triples output becomes part of the index, and what every
writer and reader must agree on for that to be safe.

Five programs implement this contract and none of them run in the same process:

| Role | Implementation |
| --- | --- |
| writer | `etl/scripts/upload-nt.sh` (every collector and enricher) |
| writer | `etl/pg-collect/src/main.rs`, `load --write-backend minio` |
| reader | `deploy/quadlet/scripts/qlever-rebuild-index.sh` (the host rebuild) |
| reader | `deploy/overlays/{dev,prod}/jobs/rebuild-qlever-index.yaml` |
| reader | `deploy/overlays/{dev,prod}/jobs/rebuild-tdb2.yaml` |

`deploy/quadlet/tests/test_graph_manifest.py` drives the host reader for real
against a fake object store. The Kubernetes readers are not separate copies:
`deploy/quadlet/tests/reader_parity.py` generates their discovery block from
the host script's marked region, and `test_reader_parity.py` fails if any of
them has drifted. The writers are covered by
`deploy/quadlet/collectors/tests/test_upload_manifest.py` and the
`graph_publication_tests` module in `main.rs`.

## Why the old scheme was not a commit protocol

The original layout was a stable payload key plus a sidecar naming its graph:

```
nt-output/debian-trixie.nt.gz
nt-output/debian-trixie.nt.gz.graph     ← "commit marker"
```

The sidecar was described as a commit marker because it is written only after
the payload upload returns. That reasoning holds exactly once. On the *second*
upload for the same graph the sidecar is already there, so the moment the new
payload lands under the stable key it is discoverable — with no verification
having happened and nothing left to gate it. Whatever the sidecar step does
afterwards, succeed or fail, changes nothing: it re-writes bytes that were
already correct. The marker commits the first generation of a graph and no
other.

Two consequences followed from that, and both were observed in production:

- **Nothing verified the bytes.** A reader took whatever was under the key.
- **Readers could union two copies of one graph.** When the payload format
  changed (`.nt` → `.nt.gz`) the old key kept its own valid sidecar, so a
  single graph URI had two discoverable payloads. Both got indexed. Measured
  2026-09-11: 24 graphs affected, ~10 GB of duplicate quads. The host reader
  grew an mtime-based dedup heuristic to cope; that heuristic is a symptom.

See #72.

## The layout

```
graphs/<slug>/manifest.json                   ← the commit point
graphs/<slug>/generations/<gen>.nt.gz         ← immutable payload
```

`<slug>` is the graph URI with the PackageGraph graph prefix stripped and `/`
replaced by `-`, exactly as `upload-nt.sh` has always derived it
(`https://packagegraph.github.io/graph/debian/trixie` → `debian-trixie`). It is
a **display name for humans reading the bucket**, not an identifier: readers key
everything on the graph URI inside the manifest. The slug transform is not
injective (`graph/a/b` and `graph/a-b` both give `a-b`), so writers refuse to
publish when an existing manifest under the same slug names a different graph
URI, and readers refuse to build when two manifests claim the same graph URI.

`<gen>` is `YYYYMMDDTHHMMSSZ-<first 12 hex of the payload's sha256>`. Sortable,
unique per upload, and self-identifying. Generation objects are **never
overwritten and never deleted by this contract** — reclaiming them is a
separate concern with its own retention policy (see the comment above the dedup
loop in `qlever-rebuild-index.sh` for why reclamation must not run in front of
a build).

## The manifest

```json
{
  "schema": 1,
  "graph": "https://packagegraph.github.io/graph/debian/trixie",
  "generation": "20260924T031500Z-3f2a1c8d9e01",
  "key": "graphs/debian-trixie/generations/20260924T031500Z-3f2a1c8d9e01.nt.gz",
  "encoding": "gzip",
  "size_bytes": 128374651,
  "sha256": "3f2a1c8d9e01...",
  "data_triples": 1840233,
  "committed_at": "2026-09-24T03:15:00Z",
  "source_url": "http://deb.debian.org/debian"
}
```

`source_url` is optional and omitted when the collector has no single canonical
upstream. `data_triples` is the count `upload-nt.sh` already computes for its
empty-graph floor (#58) — carried here so a reader can report it without
decompressing. Readers must ignore fields they do not know: `schema` is bumped
only for a change that would make an old reader wrong.

`encoding` is `gzip` or `none`. Readers that see any other value must fail
rather than guess.

### `quality`, the completeness record

Optional, and present only when the run that produced the payload recorded it:

```json
"quality": {
  "schema": 1,
  "complete": false,
  "stages": [
    {"stage": "rpm",  "required": true,  "attempted": 2,    "completed": 2,    "retryable": 0,  "failed": 0},
    {"stage": "spec", "required": false, "attempted": 4200, "completed": 4200, "retryable": 0,  "failed": 0},
    {"stage": "koji", "required": false, "attempted": 1200, "completed": 1182, "retryable": 18, "failed": 0}
  ]
}
```

Optional enrichment stages are allowed to fail item by item without failing
the run — one unreachable Koji hub should not cost a distribution's entire
package graph for the night. The cost is that a published graph can be a
knowingly partial snapshot, and nothing downstream could tell: a graph
enriched for 1,200 of 1,200 builds and one enriched for 400 of 1,200 arrived
identical, both green (#70).

Three rules make the field worth having:

- **Absence means unknown, never complete.** Every graph published before this
  existed has no `quality` block. A reader that filled one in, or that treated
  a missing one as clean, would make the field meaningless.
- **Any retryable or failed item means `complete: false`**, even though the
  run exited 0 and the graph published. Those are the same run.
- **`attempted` must equal `completed`** for a stage to be complete. An item
  that falls out of the loop unclassified is a hole in the accounting, and a
  hole reads as incomplete.

`required` stages abort the run on failure, so they can never be the reason a
published graph is partial. They are recorded so that is visible rather than
implied.

`pg-collect` writes this beside its N-Triples output as
`<output>.nt.quality.json` (see `etl/pg-collect/src/stage_report.rs`), and
`upload-nt.sh` carries it into the manifest. A sidecar that is present but
unparseable **fails the upload**: the collector wrote it moments earlier, so
an unreadable one is a bug, and publishing without it would launder a
knowingly partial graph into the indistinguishable "unknown" pile.

## Writer protocol

1. Compute the payload, its `size_bytes` and its `sha256` locally.
2. Read `graphs/<slug>/manifest.json`. If it exists and its `graph` is not the
   graph being published, **stop** — that is a slug collision, not a rewrite.
3. `PUT` the payload to `graphs/<slug>/generations/<gen>.<ext>`. The key is new
   on every upload, so this can never replace anything.
4. **Verify the stored object** by listing it: its size must equal
   `size_bytes`, and when the object store returns a plain MD5 ETag (single-part
   upload, no `-N` suffix) that must equal the payload's MD5. A multipart ETag
   is not an MD5 and is checked for presence only; the reader's SHA-256 check is
   what makes that safe.
5. `PUT` the manifest. **This is the commit.** A single small `PUT` is atomic in
   S3: a reader sees the whole previous manifest or the whole new one.

The failure modes fall out of the ordering:

- Payload upload or verification fails → the manifest still points at the
  previous generation, which is still present and still correct.
- Manifest write fails → the new generation is an orphan. Nothing references
  it, so nothing reads it, and the next run writes another one.

### Legacy mirror

After the commit, writers also refresh the old `nt-output/<slug>.nt.gz` pair,
best-effort, warning but not failing if it does not work. This exists **only**
so that a reader which has not been upgraded yet keeps seeing fresh data; it is
the same unsafe stable-key write this document exists to replace, and it is
harmless solely because upgraded readers ignore legacy copies of any graph that
has a manifest.

Set `PG_UPLOAD_LEGACY_MIRROR=0` to turn it off. Delete the code path once every
reader in the table above is deployed — the host readers are installed files
under `/etc/containers/systemd/scripts/`, updated by a repo sync, while writers
ship inside the collector image, so the two do not move together.

## Reader protocol

1. Snapshot `mc ls -r --json` for **both** `nt-output/` and `graphs/` before
   downloading. The `graphs/` snapshot must include `.json` keys: a manifest
   changing mid-download is precisely the race being detected.
2. Read every `graphs/*/manifest.json`. Reject a manifest that is unparseable,
   is missing a required field, carries an unknown `encoding`, or claims a
   graph URI another manifest already claimed. Reject one whose `key` is not
   under its own `graphs/<slug>/generations/` prefix: the key and the digest
   it is checked against both come from the same manifest, so they will agree
   perfectly well about another graph's object. Only the key's scope catches
   that.
3. Download each committed generation. Keep only the committed generation of
   each graph on local scratch — generations are immutable and unbounded in
   number, so mirroring the prefix wholesale would grow without limit.
4. Verify each payload's size and SHA-256 against its manifest. A mismatch is
   a hard failure, not a skip: a silently dropped graph is how a rebuild
   quietly publishes a smaller corpus.
5. Discover legacy `nt-output/*.graph` sidecars as before, then **drop every
   legacy candidate whose graph URI has a manifest.** A graph is either
   manifest-backed or legacy, never both, so the two copies can never be
   unioned into one named graph.
6. Re-snapshot both prefixes and fail if either changed.

Legacy graphs keep the existing mtime/non-empty dedup among themselves. That
heuristic is not extended to manifest-backed graphs and should disappear with
the last legacy sidecar.
