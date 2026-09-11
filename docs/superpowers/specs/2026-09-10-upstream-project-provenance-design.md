# Cross-Ecosystem Upstream Project Provenance — Design

**Date:** 2026-09-10 (revised same day after audit)
**Status:** Design — awaiting review before implementation plan
**Related:** Slack thread with mizmo on multi-upstream drift (snakeyaml's bitbucket/codeberg/github mirror history); `etl/pg-collect/src/collect_openwrt_upstream.rs` (the existing implementation this design extends and partially migrates); `docs/superpowers/specs/2026-09-01-rhel-rebuild-comparison-deriver-design.md` (prior art for `DataSnapshot`)

## Revision note

The first draft of this design was audited twice. The first round returned
seven findings, all confirmed valid against the code. Two required an
architectural decision:

- **`UpstreamProject` keying.** The existing OpenWrt implementation
  (`collect_openwrt_upstream.rs`) mints its `UpstreamProject` node keyed on
  `openwrt/{package-name}`, not on the canonical repo URL — even in its
  git-source case, where a real forge URL is resolved. A repo-URI-keyed hub
  (this design's whole point) would not converge with OpenWrt's existing
  nodes for the same real project. **Decision: migrate OpenWrt's git-source
  case to the same repo-URI keying used everywhere else.** This changes the
  IRIs of already-published git-sourced OpenWrt `UpstreamProject` nodes; see
  §3.5.
- **Publication-model scope.** `DataSnapshot` correctness depends on
  whole-graph replacement (each rebuild sees exactly one snapshot per
  graph). Fuseki's default incremental load doesn't replace, so snapshots
  would accumulate ambiguously. **Decision: scope this design's correctness
  guarantee to the Minio+QLever replacement pipeline only** (the pipeline
  actually in production on the quadlet host) and declare Fuseki-incremental
  publication unsupported for this feature, rather than building an
  `isCurrent`/cleanup mechanism for a path nothing currently uses.

Most of the rest of round one was a direct, mechanical response to a
confirmed finding — see the inline "(audit finding N)" markers. Two of
round one's findings changed where in the pipeline this work happens:
`DataSnapshot` minting moves from the Rust collectors to `upload-nt.sh`
(finding 4), and `UpstreamProject` hub-linking centralizes in
`forge::emit_upstream_repo` rather than being patched at each of the
originally-listed "seven" call sites (finding 2) — an inventory that was
itself wrong.

**Round two** caught three more:

- **`normalize_forge_url` returns a PackageGraph node IRI
  (`.../d/repo/...`), not a canonical URL** — its three direct callers
  (`rpm.rs`, `maven.rs`, `emit/rdf.rs`) would key the hub off that node IRI
  while every `forge.rs`-routed collector keys off the plain canonical URL,
  so identical projects wouldn't converge. (§3.2)
- **`pkg:snapshotGraph` is an `owl:DatatypeProperty` with `rdfs:range
  xsd:anyURI`** (`core.ttl:823-829`) — a typed literal, not an object
  property. The round-one draft emitted it as an IRI reference and joined
  it as one in the sample query; both are fixed in §4 and §5. (Checking
  `derive_comparison.rs` for prior art here — the design's own claimed
  justification — turned up that it doesn't actually use `snapshotGraph` at
  all; it repurposes `snapshotSource` for the RHEL graph reference instead,
  as a plain untyped literal. That's a pre-existing quirk in that deriver,
  not something this design should copy; §4 is written directly against
  the ontology's actual declaration instead.)
- **The obvious fix for the first finding — carrying `normalize_forge_url`'s
  own match logic forward into a canonical-URL variant — would have
  preserved a real, independent bug in it:** its GitLab case truncates
  nested groups (`gitlab.com/group/subgroup/project` → `.../group/subgroup`,
  silently dropping the repo segment), while `forge.rs`'s GitLab handling
  keeps the full path. Two hand-maintained matchers is how findings 1 and 3
  both happened. **Decision: retire `uris.rs`'s independent matching logic
  entirely — `normalize_forge_url_canonical` delegates to
  `forge::extract_forge_url`, so there is exactly one matcher.** This also
  narrows away a false-positive-prone loose match (any URL containing the
  substring `"gitlab."`, not just a real GitLab host) — an intentional,
  documented behavior change. Because that loose match was also the only
  rule covering self-hosted GitLab instances outside the curated
  `GITLAB_HOSTS` list, a second, narrower host-prefix rule restores that
  coverage without reintroducing the false positive. (§3.2)

## 1. Overview

Two related gaps surfaced while discussing PackageGraph's forge/upstream-repo
tracking:

1. **Bitbucket isn't recognized as a forge, in either of the two places that
   matter.** `uris::normalize_forge_url` doesn't recognize it — nor does
   `forge.rs`'s independent extraction engine (`FORGE_HOSTS`/`GITLAB_HOSTS`/
   `GITEA_HOSTS`, checked by `is_high_confidence_host` and
   `normalize_direct_forge`), which is the path most collectors actually use
   (see §2). A package whose only recorded URL is Bitbucket produces **zero**
   `pkg:upstreamRepository` triples via either path — not a stale-but-present
   one, nothing at all.
2. **`pkg:upstreamRepository` is a bare, unattributed assertion**, written
   independently by three different mechanisms with no cross-ecosystem
   reconciliation and no "who asserted this, and when" signal (see §3.1 for
   the corrected inventory of all three).

Both were assumed to need new machinery. They mostly don't: the ontology
already has a working, tested pattern for exactly this — `pkg:UpstreamProject`,
`pkg:hasUpstreamProject`, `pkg:projectRepository`, `pkg:projectName` (all in
the pinned v0.13.0 ontology, already implemented once in
`collect_openwrt_upstream.rs`) — plus `pkg:DataSnapshot`
(`snapshotGraph`/`snapshotTimestamp`/`snapshotSource`), already proven by
`derive_comparison.rs`. This design activates both. The one piece of new
machinery is deciding *where* `DataSnapshot` gets minted, since the obvious
place (inside each Rust collector) turns out not to have the inputs it needs
in production (§4).

### Goals

- Recognize Bitbucket in both `uris::normalize_forge_url` and `forge.rs`.
- Extend the existing `UpstreamProject` hub pattern, keyed deterministically
  on the canonical repo URI, to every current `upstreamRepository` emitter,
  so independent ecosystems (and, after migration, OpenWrt) converge on
  shared identity with no explicit matching/merge step.
- Give every graph published through the replacement pipeline (Minio +
  QLever) an answerable "as of when," via one `DataSnapshot` per published
  graph.
- Answer, for any real upstream project: which ecosystems assert it, what
  repo each currently points at, and how recently each was last observed.

### Non-goals (this design)

- Removing or restructuring the existing `pkg:upstreamRepository` triple —
  it stays exactly as-is; the hub is additive.
- Any new ontology term or a `packagegraph/ontology` version bump — every
  class/property this design uses already exists in the pinned v0.13.0
  ontology.
- Per-triple reification of the `hasUpstreamProject` edge itself.
  `DataSnapshot` is graph-scoped by design, and every collector's output is
  already its own named graph, so graph membership plus a `DataSnapshot`
  join already answers "as of when."
- Retroactively resolving *existing* disagreements between ecosystems for
  projects that have already drifted. This design makes disagreement
  *visible and dated*, not resolved.
- Fuseki incremental-load support for `DataSnapshot`. Out of scope per the
  revision note above; using Fuseki's `pg-collect load` path with this
  feature requires accepting unbounded, ambiguous snapshot accumulation,
  which this design does not attempt to fix.
- Reconciling OpenWrt's archive-source `UpstreamProject` nodes (no
  resolvable repo URL exists for them) into the repo-URI-keyed scheme. They
  keep their current `openwrt/{name}` keying — there's nothing to key on
  otherwise.
- A change-log/delta model for `DataSnapshot` (only minting on an actual
  value change). Rejected in favor of one snapshot per published graph,
  deduplicated by deterministic IRI.

## 2. Bitbucket recognition

One fix, not two. Round one added Bitbucket to `uris::normalize_forge_url`
and separately to `forge.rs`'s host lists, because at the time those looked
like two extraction engines that each needed their own patch. Round two's
finding 3 (§3.2) established that maintaining two independent matchers by
hand is itself the bug — `uris.rs`'s GitLab handling silently diverged from
`forge.rs`'s and truncated nested groups. So `uris.rs` no longer has its own
matching logic to patch (§3.2); this fix lives in exactly one place:

**`forge.rs`** — add `"bitbucket.org"` to `FORGE_HOSTS` (line 40, alongside
`github.com`/`codeberg.org`/`sr.ht` — Bitbucket is `owner/repo`-shaped like
those three, not team-based like GitLab), and add a matching case to
`normalize_direct_forge`, in the same style as the adjacent GitHub case:

```rust
// Bitbucket: bitbucket.org/{owner}/{repo}
if path.starts_with("bitbucket.org/") {
    let rest = path.strip_prefix("bitbucket.org/")?;
    let caps = FORGE_OWNER_REPO_RE.captures(rest)?;
    let owner = caps.get(1)?.as_str();
    let repo = caps.get(2)?.as_str();
    if !owner.is_empty() && !repo.is_empty() {
        return Some(format!("https://bitbucket.org/{}/{}", owner, repo));
    }
}
```

Adding it to `FORGE_HOSTS` also makes `is_high_confidence_host` recognize it
automatically — no separate change needed there. Since `uris::normalize_forge_url`
now delegates to `forge::extract_forge_url` (§3.2), this one change makes
Bitbucket recognized everywhere at once — the prerequisite for §3, since a
Bitbucket-only upstream can't enter the hub until it can be normalized at
all.

## 3. The `UpstreamProject` hub, extended

### 3.1 Corrected inventory of `upstreamRepository` emitters

The original design's "seven emitters" list was wrong (audit finding 2).
There are three genuinely independent mechanisms:

| Mechanism | Used by |
|---|---|
| Direct `uris::normalize_forge_url` call | `rpm.rs`, `maven.rs` |
| `forge::extract_forge_url`/`extract_forge_url_with_field` → `forge::emit_upstream_repo` | `debian.rs`, `gentoo.rs`, `collect_salsa.rs`, `collect_spec.rs`, `cargo_collect.rs`, `yocto.rs`, `openwrt.rs` |
| Generic IR emitter (`emit/rdf.rs`), calling `normalize_forge_url` independently, used by `main.rs`'s `PackageIr`-based ecosystems | routed through `main.rs` |

Because the middle row is a single shared function (`forge::emit_upstream_repo`,
`forge.rs:906-928`) that already receives `identity_uri` and computes the
canonical repository IRI (`r_uri`) in one place, that's where hub-linking
belongs — not patched at each of its ~7 callers individually.

### 3.2 One shared canonical URL normalizer (round two, findings 1 and 3)

Round two surfaced two related bugs in the same area:

- **Finding 1:** `uris::normalize_forge_url` (`uris.rs:266`) doesn't return a
  canonical URL — every branch already applies `repo_uri(&format!(...))`
  before returning, so its result is the `.../d/repo/...` PackageGraph node
  IRI. Its three `pkg:upstreamRepository`-emitting callers (`rpm.rs:877`,
  `maven.rs:891`, `emit/rdf.rs:109`) bind that IRI to a variable named
  `upstream_uri`/`repo_uri` — a naming accident that made the first
  revision's `emit_upstream_project_link` call look consistent with these
  sites when it wasn't.
- **Finding 3:** the obvious fix for finding 1 — split `normalize_forge_url`
  into a canonical-URL matcher plus a thin `repo_uri()`-wrapping shim,
  reusing `normalize_forge_url`'s own existing match logic for the
  matcher — carries forward a real, independent bug in that logic:
  `uris.rs`'s GitLab case (`splitn(4, '/')`, using only `parts[1]`/`parts[2]`)
  truncates nested groups — `gitlab.com/group/subgroup/project` normalizes
  to `.../group/subgroup`, silently dropping `project` — while `forge.rs`'s
  GitLab handling (`forge.rs:236-246`, `/-/`-subpath stripping) keeps the
  full nested path. Two independently-maintained matchers is *how* this
  drifted; patching one to add Bitbucket while leaving the other's
  pre-existing GitLab bug in place would just create a third, differently-
  wrong matcher.

Fix: stop maintaining a second matcher in `uris.rs`. Delegate to
`forge::extract_forge_url` — already the richer, more carefully maintained
implementation (it also covers Gitea/Forgejo, sr.ht, and a generic `git.*`
fallback that `uris.rs` never had) — and keep `uris::normalize_forge_url`'s
existing public signature as a thin wrapper, so nothing outside this pair
of functions needs to change:

```rust
/// Match a URL against known forge patterns and return its canonical form
/// (e.g. "https://github.com/owner/repo") -- the plain URL, not a
/// PackageGraph node IRI. Delegates to forge::extract_forge_url so this
/// and the forge.rs-routed collectors share exactly one matcher --
/// see round-two finding 3 for why two independent copies drifted.
pub fn normalize_forge_url_canonical(url: &str) -> Option<String> {
    crate::forge::extract_forge_url(url).map(|extraction| extraction.repo_url)
}

/// Try to normalize a URL into a canonical forge repository URI.
/// Returns Some(repo_uri) if the URL matches a known forge pattern, None otherwise.
pub fn normalize_forge_url(url: &str) -> Option<String> {
    normalize_forge_url_canonical(url).map(|canonical| repo_uri(&canonical))
}
```

**One intentional behavior change** falls out of this delegation:
`uris.rs`'s old GitLab case matched *any* host containing the substring
`"gitlab."` anywhere in the URL (e.g. it would also match
`https://blog.example.com/tags/gitlab.html`) — a genuine false-positive
risk, and removing it was necessary. But that loose match was also the
*only* rule that covered self-hosted GitLab instances outside the curated
`GITLAB_HOSTS` list (`gitlab.kitware.com`, `gitlab.isc.org`,
`gitlab.torproject.org`, `gitlab.inria.fr`, `gitlab.alpinelinux.org`,
etc.) — packages depending on those hosts would have silently lost their
`pkg:upstreamRepository` triple. `forge.rs` therefore also gains a second,
narrower GitLab rule alongside the curated-list loop: a host-*prefix*
match (the host's first label is literally `gitlab.`), which restores
self-hosted-instance coverage without reintroducing the substring false
positive (`blog.example.com` doesn't start with `gitlab.`, so it still
correctly resolves to `None`). Confidence for these self-hosted instances
falls out as Medium (not High) via `is_high_confidence_host`, since only
the curated `GITLAB_HOSTS` list counts as High. §8 adds tests locking in
both the restored coverage and the substring-rejection behavior. Two
additional, positive side effects: the three direct writers now also
benefit from `extract_forge_url`'s archive-URL and FTP-mirror
normalization, which `normalize_forge_url` never had.

Each of the three direct-writer call sites changes from one call to one
call plus a derived value, e.g. `rpm.rs`:

```rust
// Before:
if let Some(upstream_uri) = normalize_forge_url(url) {
    writer.write_triple(&identity_uri, &format!("{PKG}upstreamRepository"), &upstream_uri)?;
}

// After:
if let Some(canonical_url) = normalize_forge_url_canonical(url) {
    let upstream_repo_iri = repo_uri(&canonical_url);
    writer.write_triple(&identity_uri, &format!("{PKG}upstreamRepository"), &upstream_repo_iri)?;
    emit_upstream_project_link(writer, &identity_uri, &canonical_url)?;
}
```

`maven.rs:890-896` (keying off `scm_url`) and `emit/rdf.rs:105-114` (keying
off `homepage`) change the same way.

**A fourth direct caller of the forge matcher exists and is deliberately
excluded from this migration:** `emit/debian_ext.rs:57` calls
`normalize_forge_url(vcs_url)` (the original wrapper, not
`normalize_forge_url_canonical`) to populate `pkg:packagingRepository`,
not `pkg:upstreamRepository`. It inherits the GitLab-nested-group fix,
the self-hosted-GitLab host-prefix rule, and the scheme-less-URL fix
automatically, since all three live in the shared matcher underneath —
but it does *not* get an `emit_upstream_project_link` call and must not
gain one. `packagingRepository` describes the packaging/VCS repo (e.g.
the Salsa repo hosting Debian's packaging metadata for a source package),
which is a distinct concept from `upstreamRepository` (the project's own
upstream source) — the two can differ (a Debian team's Salsa packaging
repo is not the upstream project's repo). This is a deliberate modeling
boundary, not an oversight; a future reader should not "fix" it into
calling the hub-link helper.

### 3.3 Identity: reuse the existing `upstream_uri` helper, don't add a new one

`uris.rs` already has:

```rust
/// Build an UpstreamProject URI.
pub fn upstream_uri(name: &str) -> String {
    format!("{DATA}upstream/{}", encode(name))
}
```

Two ecosystems whose metadata normalizes to the same canonical repo URL
converge automatically if both call `upstream_uri(repo_url)` with that same
URL string as the key — no new IRI-minting function is needed. (The
original design proposed a new `upstream_project_uri(repo_uri)` function;
it would have done nothing `upstream_uri` doesn't already do.)

### 3.4 `projectName` is required, and derivable without new parameters

`:UpstreamProject` has an OWL cardinality-1 restriction on `:projectName`
(`core.ttl:1319`), and the one existing implementation
(`collect_openwrt_upstream.rs:55-59`) emits it, explicitly commented `//
pkg:projectName (SHACL required)`, with a test enforcing it. The original
design's helper omitted this entirely (audit finding 6).

Rather than threading a `project_name` parameter through every call site
(most of which don't have an obviously "right" name handy — a Cargo crate
name and a Debian source package name for the same upstream can differ),
derive it from the same canonical repo URL already in hand — the
`owner/repo` slug, which is ecosystem-agnostic and stable regardless of
which collector happens to create the node first:

```rust
/// Derive a human-readable UpstreamProject name from its canonical repo
/// URL -- the owner/repo slug, e.g. "FasterXML/jackson-databind". Used as
/// pkg:projectName, which UpstreamProject requires (cardinality 1).
pub fn project_name_from_repo_url(repo_url: &str) -> String {
    let path = repo_url
        .strip_prefix("https://")
        .or_else(|| repo_url.strip_prefix("http://"))
        .unwrap_or(repo_url);
    path.splitn(2, '/')
        .nth(1)
        .unwrap_or(path)
        .to_string()
}
```

### 3.5 Emission

New helper in `forge.rs` (not `uris.rs` — it's called from
`emit_upstream_repo` and the three direct writers, all of which already
depend on `forge.rs` or `uris.rs`):

```rust
/// Link a PackageIdentity to its UpstreamProject hub, minting the hub node
/// (idempotently) if this is the first time any collector run has seen
/// this canonical repo. Keyed on the same repo_url normalize_forge_url /
/// extract_forge_url already produced -- no new identity scheme.
pub fn emit_upstream_project_link(
    writer: &mut NTriplesWriter,
    identity_uri: &str,
    repo_url: &str,
) -> Result<usize> {
    let project_uri = upstream_uri(repo_url);
    let mut triples = 0;

    if writer.write_triple_once(&project_uri, RDF_TYPE, &format!("{PKG}UpstreamProject"))? {
        triples += 1;
    }
    if writer.write_literal_once(&project_uri, &format!("{PKG}projectName"), &project_name_from_repo_url(repo_url))? {
        triples += 1;
    }
    if writer.write_triple_once(&project_uri, &format!("{PKG}projectRepository"), &repo_uri(repo_url))? {
        triples += 1;
    }
    writer.write_triple(identity_uri, &format!("{PKG}hasUpstreamProject"), &project_uri)?;
    triples += 1;

    Ok(triples)
}
```

**Call sites:**

- **Centralized (covers 7 collectors in one change):** `forge::emit_upstream_repo`
  gains one call to `emit_upstream_project_link(writer, identity_uri, repo_url)`
  right after its existing `upstreamRepository` write. This alone covers
  `debian.rs`, `gentoo.rs`, `collect_salsa.rs`, `collect_spec.rs`,
  `cargo_collect.rs`, `yocto.rs`, and `openwrt.rs`.
- **Direct writers (patched individually, since they don't go through
  `forge.rs`):** `rpm.rs`, `maven.rs`, `emit/rdf.rs` — each switches its
  existing `normalize_forge_url(url)` call to `normalize_forge_url_canonical(url)`
  (§3.2), derives the `upstreamRepository` object via `repo_uri(&canonical_url)`,
  and adds the `emit_upstream_project_link(writer, &identity_uri, &canonical_url)`
  call — see the exact before/after in §3.2.
- **`collect_openwrt_upstream.rs` (identity-keying migration, not a new call
  site):** its git-source branch currently mints
  `upstream_uri(&format!("openwrt/{}", effective_name))` unconditionally,
  *before* checking whether the source is git or archive. Restructure so
  the git-source branch instead calls `upstream_uri(&extraction.repo_url)`
  (i.e., the same keying as everywhere else) and gains a `projectName`
  triple derived the same way. The archive-source branch (no resolvable
  repo URL) is untouched — it keeps minting `upstream_uri("openwrt/{name}")`
  as its only option, since there's no repo URL to key on.

  This changes the IRI of already-published git-sourced OpenWrt
  `UpstreamProject` nodes. Nothing needs a manual migration step: the next
  full `openwrt-full` collector run publishes a fresh graph through the
  Minio+QLever replacement pipeline (§4), which replaces the old file
  wholesale — the old IRIs simply stop being asserted, consistent with how
  every other data change in this pipeline already propagates.

No collector's emitted `upstreamRepository` triple changes value — the
three direct writers' internal code changes shape (§3.2), but the object
they write is identical to today's. This is a pure addition alongside the
existing write (plus the one keying change to OpenWrt's git-source case,
called out above).

## 4. `DataSnapshot`, minted in `upload-nt.sh`, not in the Rust collectors

The original design proposed minting `DataSnapshot` inside each collector's
`collect()`, using its own `graph_uri`/timestamp/source. That doesn't work
in production (audit finding 4, confirmed two ways):

- `emit_upstream_repo`'s signature has no `graph_uri` parameter at all.
- More fundamentally: `main.rs` threads a global `--graph` CLI flag to
  every collector via `.with_graph(graph_uri.clone())` — the plumbing
  exists — but **none of the 41 deployed quadlet collector scripts ever
  pass `--graph`** (confirmed by grep across
  `deploy/quadlet/collectors/scripts/*.sh`). Every one of them instead
  passes the graph URI straight to `upload-nt.sh` as a separate shell
  argument (e.g. `alma-9-full.sh`: `pg-collect rpm-full ... -o
  /tmp/collection/alma-9.nt` with no `--graph`, then separately
  `upload-nt.sh /tmp/collection/alma-9.nt "$GRAPH_URI"`). In every real
  invocation, the Rust process's `graph_uri` is `None`.

`upload-nt.sh` is the one place that reliably has both the graph URI (its
existing required `$2`) and can trivially get a timestamp (`date -u`). It's
also the choke point for every graph published through the replacement
pipeline this design is scoped to (§1 non-goals) — so minting here means
the feature applies uniformly to every such graph, not just the ~10
forge-related collectors the original draft scoped it to.

**Change to `upload-nt.sh`:** add an optional third argument, the source
URL, and append a `DataSnapshot` describing the graph directly into the
`.nt` file before it's gzipped and uploaded — so the snapshot triples land
in the exact same named graph as the data they describe, with no separate
upload step:

```bash
# Usage: upload-nt.sh <local-file.nt> <graph-uri> [source-url]
...
LOCAL_FILE="$1"
GRAPH_URI="$2"
SOURCE_URL="${3:-}"

...

# Append a DataSnapshot describing this graph, before gzip/upload, so it
# lands in the same named graph as the rest of this file's content.
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)          # single capture -- both forms below derive from it
SNAPSHOT_TIMESTAMP_COMPACT=$(echo "$NOW" | tr -d ':-')
SNAPSHOT_IRI="https://packagegraph.github.io/d/snapshot/collector/${GRAPH_SLUG}/${SNAPSHOT_TIMESTAMP_COMPACT}"
{
  printf '<%s> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#DataSnapshot> .\n' "$SNAPSHOT_IRI"
  # snapshotGraph is owl:DatatypeProperty, range xsd:anyURI (core.ttl:823-829)
  # -- a typed literal, NOT an IRI reference. Getting this wrong makes the
  # join in the SPARQL query below silently return zero rows (an IRI and a
  # literal never test equal in SPARQL, even with identical string content).
  printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotGraph> "%s"^^<http://www.w3.org/2001/XMLSchema#anyURI> .\n' "$SNAPSHOT_IRI" "$GRAPH_URI"
  printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotTimestamp> "%s"^^<http://www.w3.org/2001/XMLSchema#dateTime> .\n' "$SNAPSHOT_IRI" "$NOW"
  if [ -n "$SOURCE_URL" ]; then
    printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotSource> "%s" .\n' "$SNAPSHOT_IRI" "$SOURCE_URL"
  fi
} >> "$LOCAL_FILE"
```

**Call-site changes:** each of the ~41 collector shell scripts gains a third
argument to its existing `upload-nt.sh` call, wherever a single canonical
source URL exists (e.g. `alma-9-full.sh` would pass
`https://repo.almalinux.org/almalinux/9/` or similar). **Multi-source
collectors** (e.g. `rpm-full` invoked with several `--url` mirrors for
different arches, as `alma-9-full.sh` already does) have no single
canonical source — `snapshotSource` is optional precisely for this reason;
these scripts simply omit the third argument rather than inventing an
artificial "primary" mirror. This is a real, accepted gap, not an oversight
(audit finding 4's "rules for multi-source collectors").

## 5. Fixed join query

Two independent fixes to the original example query:

- **Round one, finding 5:** it put the `DataSnapshot` pattern outside any
  `GRAPH` block, relying on default-graph-union semantics that don't hold
  here — `DataSnapshot` triples live inside the same named graph they
  describe, same as everything else in that file.
- **Round two, finding 2:** it joined `?g` (bound as a graph name — always
  an IRI) directly against `pkg:snapshotGraph`'s object. Since
  `snapshotGraph` is a typed literal (§4), that object is never term-equal
  to the IRI `?g`, so the join would silently return zero rows even after
  fix one. Bind the literal to its own variable and compare string forms
  with `FILTER(STR(...) = STR(...))`, as the ontology's declared datatype
  requires:

```sparql
SELECT ?repo ?snapshotTimestamp WHERE {
  GRAPH ?g {
    ?identity pkg:hasUpstreamProject/pkg:projectRepository ?repo .
    ?snapshot a pkg:DataSnapshot ;
              pkg:snapshotGraph ?graphLiteral ;
              pkg:snapshotTimestamp ?snapshotTimestamp .
    FILTER(STR(?graphLiteral) = STR(?g))
  }
}
```

## 6. Data flow

Collector run → per package: existing `pkg:upstreamRepository` write
(unchanged) + new `emit_upstream_project_link` call, wherever a `repo_url`
was already resolved → collector writes its `.nt` file as always, with no
snapshot content → `upload-nt.sh` appends this graph's `DataSnapshot` →
gzip + upload (unchanged) → next `qlever-rebuild-index` promotes it. A
cross-ecosystem query for "which upstream, asserted by which packaging, as
of when" becomes: find every identity sharing a `hasUpstreamProject` value,
note which graph each came from, join to that graph's `DataSnapshot`.

## 7. Error handling

- `normalize_forge_url`/`extract_forge_url` returning `None` (still-
  unrecognized forge): no `UpstreamProject` link and no plain
  `upstreamRepository` triple — unchanged behavior, now just covering
  Bitbucket too, via both extraction paths.
- A collector failing mid-run produces no output file (existing behavior)
  — no partial `UpstreamProject` state reaches Minio. Since `DataSnapshot`
  is appended by `upload-nt.sh` only after the collector already succeeded
  and produced a file, a failed collector run also produces no
  `DataSnapshot` — consistent, no partial-state case to handle.
- Two ecosystems asserting genuinely different repos for what a human
  would consider "the same project" is not resolved by this design — it
  becomes two distinct `UpstreamProject` nodes, each dated via its own
  graph's `DataSnapshot`. Making that disagreement visible is the goal;
  adjudicating it is out of scope.
- Fuseki-incremental publication (`COLLECTOR_FULL_RELOAD` unset): explicitly
  unsupported for `DataSnapshot` (§1 non-goals). `UpstreamProject`/
  `hasUpstreamProject` triples are unaffected either way — they're
  idempotent per canonical repo URL regardless of publication model.

## 8. Testing

- Unit tests for Bitbucket URL normalization in `forge.rs` (the only place
  the match logic now lives): happy path, trailing slash, `.git` suffix,
  extra subpath — matching the file's existing Codeberg/GitHub test shape.
- **Parity/regression tests for `uris::normalize_forge_url_canonical`**
  (round two, finding 3), proving it now agrees with `forge::extract_forge_url`
  by construction rather than by hand-maintained coincidence:
  - Nested GitLab groups: `https://gitlab.com/group/subgroup/project` (and
    a `gitlab.freedesktop.org` equivalent) normalizes to the full
    three-segment path, not truncated to `group/subgroup`.
  - Fragments and query strings: `https://github.com/owner/repo#readme` and
    `.../repo?tab=readme` both normalize to `https://github.com/owner/repo`.
  - `.git` suffix stripped: `https://codeberg.org/owner/repo.git` normalizes
    to `https://codeberg.org/owner/repo`.
  - Forge-specific paths still resolve correctly through the shared
    matcher: Salsa (`salsa.debian.org/{team}/{repo}`), Pagure
    (`pagure.io/{repo}`), kernel.org (`git.kernel.org/pub/scm/...`).
  - Intentional narrowing: a non-forge URL containing the substring
    `"gitlab."` outside a real host position (e.g.
    `https://blog.example.com/tags/gitlab.html`) no longer normalizes to
    anything — documents the deliberate behavior change from §3.2, so a
    future change that accidentally restores the old loose match gets
    caught.
- Unit test: two different collectors' URLs that normalize to the same
  canonical repo produce the identical `UpstreamProject` IRI via
  `upstream_uri`.
- Unit test: `project_name_from_repo_url` on a handful of real canonical
  URLs (GitHub, GitLab nested-group, Codeberg, Bitbucket) produces the
  expected `owner/repo` slug.
- Unit test: `emit_upstream_project_link` called twice in one run with the
  same `repo_url` writes the `UpstreamProject` type/`projectName`/
  `projectRepository` triples exactly once (`write_*_once`), and the
  `hasUpstreamProject` edge twice (once per distinct `identity_uri`).
- Unit test: `collect_openwrt_upstream.rs`'s migrated git-source branch
  produces the same `UpstreamProject` IRI as a non-OpenWrt collector given
  the same canonical repo URL; its archive-source branch is unchanged and
  still produces the `openwrt/{name}`-keyed IRI.
- Shell test (or manual invocation) of the updated `upload-nt.sh`: verify
  the appended `DataSnapshot` triples parse as valid N-Triples, land in the
  same graph as the rest of the file's content after a real
  `qlever-rebuild-index` load, and that omitting the optional
  source-URL argument produces no `snapshotSource` triple (not a triple
  with an empty-string object).
- Live verification post-deploy: SPARQL query (§5) joining
  `hasUpstreamProject` across two real ecosystems for a project known to be
  packaged in both, confirming both resolve to one `UpstreamProject` node
  with distinct, correctly-timestamped `DataSnapshot`s per graph.
