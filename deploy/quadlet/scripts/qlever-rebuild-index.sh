#!/bin/bash
# Build a fresh QLever index from the published graph corpus in Minio and
# promote it to "latest" if it passes completeness gates. Equivalent of the
# rebuild-qlever-index CronJob (deploy/overlays/{dev,prod}/jobs/rebuild-qlever-index.yaml),
# minus the `kubectl rollout restart/status` steps at the end -- those are the
# host's job here, not the container's. See qlever-refresh-if-changed.sh,
# invoked via ExecStopPost= on qlever-rebuild-index.service, which reloads and
# bounces qlever.service whenever the promoted index differs from the one
# confirmed serving -- it decides from that comparison, not from this run's
# exit status or its last-run.json (#73).
#
# The corpus comes from two places and the rules for combining them are in
# docs/GRAPH-PUBLICATION.md: a per-graph commit manifest under graphs/ (the
# authority, verified by digest here) and the legacy stable-key .nt/.graph
# pairs under nt-output/ (a fallback for graphs nothing has re-published yet).
#
# Requires: mc, jq, qlever-index -- all present in the qlever-rebuild image.
# Env: MINIO_ENDPOINT, MINIO_ACCESS_KEY, MINIO_SECRET_KEY, MINIO_BUCKET.
# Bind-mounted into the qlever-rebuild-index container at
# /usr/local/bin/qlever-rebuild-index.sh (see qlever-rebuild-index.container).
set -euo pipefail

echo "=== QLever Index Rebuild ==="
START_TIME=$(date -Iseconds)
echo "Started: $START_TIME"
STATUS="failure"
CONTENT_HASH=""
TRIPLE_COUNT=0
INDEX_SIZE=""

write_status() {
  local end_time
  end_time=$(date -Iseconds)
  local duration=$SECONDS
  echo "{\"status\":\"$STATUS\",\"timestamp\":\"$end_time\",\"started\":\"$START_TIME\",\"content_hash\":\"$CONTENT_HASH\",\"triple_count\":$TRIPLE_COUNT,\"index_size\":\"$INDEX_SIZE\",\"duration_seconds\":$duration}" | \
    mc pipe "pgraph/${MINIO_BUCKET}/qlever-index/last-run.json" 2>/dev/null || true
  echo "Status: $STATUS (${duration}s)"
}
trap write_status EXIT

mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

# mc_cat_check: reads an object into MC_RESULT.
# Returns 0 (found), 1 (missing), 2 (fatal error).
# MUST NOT be called inside $(...) — exit would be swallowed.
MC_RESULT=""
mc_cat_check() {
  MC_RESULT=""
  local mc_err
  mc_err=$(mktemp)
  if MC_RESULT=$(mc cat "$1" 2>"$mc_err"); then
    rm -f "$mc_err"
    return 0
  fi
  local err_msg
  err_msg=$(cat "$mc_err")
  rm -f "$mc_err"
  if echo "$err_msg" | grep -q "NoSuchKey\|Object does not exist"; then
    return 1
  fi
  echo "ERROR: Minio read failed for $1: $err_msg" >&2
  return 2
}

# advance_corpus_marker: record the corpus fingerprint this run succeeded on.
# Call ONLY from a path that has produced or confirmed a live index, never
# earlier. The marker drives the early-exit skip at the top of the script, so
# writing it before conversion/build/gate/promotion made every failure in that
# window permanent: the next run saw an unchanged corpus, skipped, and left
# the index at whatever stale state the failure had frozen it in, forever --
# nothing short of a new upload to nt-output/ could break the cycle. Written
# last instead, a failure anywhere leaves the previous marker in place and the
# next run retries the full pipeline. See issue #75.
advance_corpus_marker() {
  if ! echo "$CORPUS_LISTING_HASH" | \
    mc pipe "pgraph/${MINIO_BUCKET}/qlever-index/last-corpus-listing-hash.txt"; then
    # Non-fatal: a missing marker costs one redundant rebuild, never
    # correctness. Failing here would discard a good, promoted index.
    echo "WARN: failed to record corpus marker — next run will rebuild redundantly" >&2
  fi
}

# Completeness gate: compare against last successful run
# || captures non-zero without triggering set -e
MC_RC=0; mc_cat_check "pgraph/${MINIO_BUCKET}/qlever-index/last-success.json" || MC_RC=$?
if [ "$MC_RC" -eq 2 ]; then
  echo "FATAL: Minio error reading completeness baseline — aborting"
  exit 1
elif [ "$MC_RC" -eq 0 ]; then
  PREV_RUN="$MC_RESULT"
  echo "Previous run found"
else
  PREV_RUN='{}'
  echo "No previous successful run found (first run)"
fi
PREV_TRIPLES=$(echo "$PREV_RUN" | jq -r '.triple_count // 0')
PREV_STATUS=$(echo "$PREV_RUN" | jq -r '.status // "none"')
MAX_LOSS_PCT=25


# ---------------------------------------------------------------------------
# Corpus discovery.
#
# Two prefixes carry graph data and a reader has to consider both:
#
#   graphs/<slug>/manifest.json   the commit point for one graph (#72), naming
#                                 an immutable generation and its digest
#   nt-output/<slug>.nt[.gz]      legacy stable-key payload + .graph sidecar
#
# A graph is manifest-backed or legacy, never both: every legacy candidate
# whose graph URI has a manifest is dropped below. That is what keeps a
# rebuild from unioning two copies of one named graph, which is not a
# hypothetical -- see the dedup comment further down for the 24 graphs and
# ~10GB of duplicate quads the old scheme actually produced.
#
# The full contract, and what each side may assume of the other, is in
# docs/GRAPH-PUBLICATION.md.
# ---------------------------------------------------------------------------

# The region between the markers below is the ONLY copy of the corpus
# discovery logic. It is generated verbatim into the Kubernetes readers
# (deploy/overlays/{dev,prod}/jobs/rebuild-qlever-index.yaml and
# rebuild-tdb2.yaml), minus the host-only part marked inside it, and
# deploy/quadlet/tests/test_reader_parity.py fails if any of them has drifted.
# Four readers that disagree about which generation of a graph is current is
# the class of bug #72 is about; they drifted once already, which is why the
# Kubernetes readers still had no dedup step long after this one grew one.
# >>> shared graph corpus discovery >>>
# mc_list_json: raw `mc ls -r --json` lines for a prefix.
#
# An absent prefix is empty, not an error: graphs/ does not exist until the
# first manifest-backed upload, and nt-output/ will eventually be emptied.
# Anything else is fatal, deliberately -- "the listing failed" and "nothing is
# published there" must never look alike. Swallowing a transport error as an
# empty manifest set would silently fall back to the legacy copy of every
# graph and rebuild a stale corpus while reporting success.
mc_list_json() {
  local target="$1"
  local out err rc
  out=$(mktemp)
  err=$(mktemp)
  rc=0
  mc ls -r --json "$target" >"$out" 2>"$err" || rc=$?
  if [ "$rc" -ne 0 ]; then
    if grep -qi "does not exist\|NoSuchKey\|no such file\|not found" "$err"; then
      rm -f "$out" "$err"
      return 0
    fi
    echo "ERROR: listing $target failed: $(cat "$err")" >&2
    rm -f "$out" "$err"
    return 1
  fi
  cat "$out"
  rm -f "$out" "$err"
}

# corpus_listing: one sorted fingerprint of everything a rebuild reads.
#
# nt-output/'s .json keys are run metadata (last-run.json and friends), not
# corpus, and are excluded as they always were. graphs/'s .json keys ARE the
# manifests -- a manifest changing mid-download is the single most important
# race to catch here, so nothing is excluded from that side.
corpus_listing() {
  mc_list_json "pgraph/${MINIO_BUCKET}/nt-output/" | \
    jq -r 'select(.status == "success") | select(.key | test("\\.json$") | not) | "nt-output/\(.key) \(.etag)"'
  mc_list_json "pgraph/${MINIO_BUCKET}/graphs/" | \
    jq -r 'select(.status == "success") | "graphs/\(.key) \(.etag)"'
}

echo "Taking pre-download listing snapshot..."
corpus_listing | sort > /tmp/etag-before.txt

# >>> host-only >>>
# Early-exit skip: this same listing is a cheap, complete fingerprint of the
# corpus's current contents. QLever's indexer isn't incremental --
# every rebuild pays the full download+convert+build cost regardless of
# how much actually changed, which is wasted work entirely on any night
# where no collector uploaded anything new. Compare against the listing
# hash saved by the last run that actually produced or confirmed the live
# index (see advance_corpus_marker); an identical corpus against a marker
# that only a successful run could have written means an identical index
# would result, so skip before doing any of that work rather than after
# (the existing content-hash check further
# down still catches the case where the corpus *did* change but happens to
# produce a byte-identical index).
CORPUS_LISTING_HASH=$(sha256sum /tmp/etag-before.txt | cut -d' ' -f1)
MC_RC=0; mc_cat_check "pgraph/${MINIO_BUCKET}/qlever-index/latest" || MC_RC=$?
if [ "$MC_RC" -eq 0 ]; then
  PREV_LISTING_HASH=""
  MC_RC2=0; mc_cat_check "pgraph/${MINIO_BUCKET}/qlever-index/last-corpus-listing-hash.txt" || MC_RC2=$?
  [ "$MC_RC2" -eq 0 ] && PREV_LISTING_HASH="$MC_RESULT"
  if [ "$PREV_LISTING_HASH" = "$CORPUS_LISTING_HASH" ]; then
    echo "corpus unchanged since the last completed run — skipping download/build entirely"
    STATUS="unchanged"
    exit 0
  fi
fi

# <<< host-only <<<

echo "Downloading legacy .nt/.nt.gz and .graph files..."
# --overwrite is required, not optional. /tmp is the persistent
# qlever-rebuild-scratch.volume (not tmpfs), so anything downloaded once
# stays cached across runs -- that part of the original reasoning was
# right. But without --overwrite, `mc mirror` treats ANY existing
# destination file that doesn't exactly match (by size/mtime) as a
# conflict and SKIPS it with a warning rather than updating it -- it does
# NOT sync changed files, only skip-if-same or download-if-new. Confirmed
# 2026-09-10: after fixing and re-uploading several graphs (cve-nvd,
# ubuntu-noble, centos-stream-10) mid-incident, this step logged
# "Overwrite not allowed for ... Use --overwrite to override this
# behavior" for every one of them and silently kept indexing the stale,
# broken bytes already sitting in the volume -- the exact same "Parse
# error at byte position 11860648860" recurred twice in a row because the
# fixed uploads never actually made it into /tmp/nt-output. With
# --overwrite, a file that already matches by size+mtime is still skipped
# (no needless re-transfer of genuinely-unchanged graphs); only an actual
# mismatch now gets synced instead of silently ignored.
#
# Only the legacy prefix is mirrored. graphs/ deliberately is not: its
# generations are immutable and never deleted, so mirroring it would
# accumulate every generation of every graph ever published on this volume.
# Each committed generation is fetched individually below and its superseded
# predecessors are pruned locally.
mc mirror --overwrite --exclude '*.json' \
  "pgraph/${MINIO_BUCKET}/nt-output/" /tmp/nt-output/

# ---------------------------------------------------------------------------
# Manifest-backed graphs.
# ---------------------------------------------------------------------------
PAYLOAD_DIR=/tmp/graph-payloads
mkdir -p "$PAYLOAD_DIR"

# Slug from ".../<slug>/manifest.json", taken as the path segment before the
# filename rather than by stripping a fixed prefix -- `mc ls --json` reports
# keys relative to the listed target, and this stays correct whether or not
# that relative key repeats the prefix.
awk '{ k = $1
       if (k ~ /\/manifest\.json$/) {
         sub(/\/manifest\.json$/, "", k)
         n = split(k, seg, "/")
         print seg[n]
       } }' /tmp/etag-before.txt | sort -u > /tmp/manifest-slugs.txt

: > /tmp/manifest-winners.txt
: > /tmp/manifest-uris.txt
MANIFEST_COUNT=0
PARTIAL_COUNT=0
while read -r slug; do
  [ -n "$slug" ] || continue
  # The slug becomes a local directory name. Keep it to what the uploaders
  # actually produce rather than trusting a bucket key to be well behaved.
  case "$slug" in
    .|..|*[!A-Za-z0-9._-]*)
      echo "ERROR: refusing manifest slug '$slug' — not a plain name"
      exit 1 ;;
  esac

  MC_RC=0; mc_cat_check "pgraph/${MINIO_BUCKET}/graphs/${slug}/manifest.json" || MC_RC=$?
  if [ "$MC_RC" -eq 1 ]; then
    echo "ERROR: graphs/${slug}/manifest.json was listed but is gone — concurrent write detected"
    exit 1
  elif [ "$MC_RC" -ne 0 ]; then
    exit 1
  fi
  manifest="$MC_RESULT"

  if ! printf '%s' "$manifest" | jq -e . >/dev/null 2>&1; then
    echo "ERROR: graphs/${slug}/manifest.json is not valid JSON"
    exit 1
  fi

  graph_uri=$(printf '%s' "$manifest" | jq -r '.graph // empty')
  generation=$(printf '%s' "$manifest" | jq -r '.generation // empty')
  gen_key=$(printf '%s' "$manifest" | jq -r '.key // empty')
  encoding=$(printf '%s' "$manifest" | jq -r '.encoding // empty')
  want_size=$(printf '%s' "$manifest" | jq -r '.size_bytes // empty')
  want_sha=$(printf '%s' "$manifest" | jq -r '.sha256 // empty')
  # A field that resolved to empty leaves its label with a trailing colon.
  for field in "graph:$graph_uri" "generation:$generation" "key:$gen_key" \
               "encoding:$encoding" "size_bytes:$want_size" "sha256:$want_sha"; do
    case "$field" in
      *:) echo "ERROR: graphs/${slug}/manifest.json is missing required field '${field%%:*}'"
          exit 1 ;;
    esac
  done

  # A manifest may only vouch for bytes under its own graph. Without this a
  # manifest could name another graph's generation, and the digest check
  # would pass -- both the key and the digest it is compared against come
  # from the same manifest, so they agree on the wrong object happily.
  case "$gen_key" in
    "graphs/${slug}/generations/"*) ;;
    *) echo "ERROR: graphs/${slug}/manifest.json points outside its own generations: $gen_key"
       exit 1 ;;
  esac
  case "$generation" in
    .|..|*[!A-Za-z0-9._-]*)
      echo "ERROR: graphs/${slug}/manifest.json has an unusable generation name '$generation'"
      exit 1 ;;
  esac

  case "$encoding" in
    gzip) ext=".nt.gz" ;;
    none) ext=".nt" ;;
    *) echo "ERROR: graphs/${slug}/manifest.json declares unknown encoding '$encoding'"
       exit 1 ;;
  esac

  if grep -qxF "$graph_uri" /tmp/manifest-uris.txt; then
    echo "ERROR: two manifests claim <$graph_uri> — refusing to guess which one is current"
    exit 1
  fi
  echo "$graph_uri" >> /tmp/manifest-uris.txt

  local_dir="$PAYLOAD_DIR/$slug"
  mkdir -p "$local_dir"
  local_payload="$local_dir/${slug}.${generation}${ext}"
  # Generations are immutable and never deleted upstream, so every past
  # download of this graph is dead weight on a volume that has to hold the
  # whole corpus plus the index being built. Keep exactly the committed one.
  find "$local_dir" -maxdepth 1 -type f \
    ! -name "$(basename "$local_payload")" \
    ! -name "$(basename "$local_payload").graph" -delete
  if [ ! -f "$local_payload" ]; then
    mc cp "pgraph/${MINIO_BUCKET}/${gen_key}" "$local_payload"
  fi

  have_size=$(stat -c '%s' "$local_payload")
  if [ "$have_size" != "$want_size" ]; then
    rm -f "$local_payload"
    echo "ERROR: ${gen_key} is $have_size bytes, manifest says $want_size"
    exit 1
  fi
  have_sha=$(sha256sum "$local_payload" | cut -d' ' -f1)
  if [ "$have_sha" != "$want_sha" ]; then
    # Drop the bad copy: leaving it cached would make every retry fail the
    # same way without ever re-fetching it.
    rm -f "$local_payload"
    echo "ERROR: ${gen_key} digest $have_sha does not match manifest $want_sha"
    exit 1
  fi

  # Give the verified payload the same .graph sidecar shape the legacy files
  # have, so everything downstream -- pair validation, conversion, fragment
  # accounting -- handles one kind of input rather than two.
  printf '%s' "$graph_uri" > "${local_payload}.graph"
  echo "${local_payload}.graph" >> /tmp/manifest-winners.txt
  MANIFEST_COUNT=$((MANIFEST_COUNT + 1))
  # Surface recorded stage completeness where an operator already looks (#70).
  # "unknown" is the honest reading of a manifest with no quality block --
  # every graph published before that existed has none, and absence must never
  # be reported as completeness.
  quality=$(printf '%s' "$manifest" | jq -r '
    if .quality == null then "completeness unrecorded"
    elif .quality.complete == true then "complete"
    else "PARTIAL" end')
  echo "  manifest: $slug → <$graph_uri> ($generation, $have_size bytes, verified, $quality)"
  if [ "$quality" = "PARTIAL" ]; then
    PARTIAL_COUNT=$((PARTIAL_COUNT + 1))
  fi
done < /tmp/manifest-slugs.txt
rm -f /tmp/manifest-slugs.txt

# Verify no files changed during download (ETags catch same-size replacements,
# and the graphs/ half of this listing is what catches a manifest committed
# while we were reading the corpus it describes).
corpus_listing | sort > /tmp/etag-after.txt
if ! diff -q /tmp/etag-before.txt /tmp/etag-after.txt >/dev/null 2>&1; then
  echo "ERROR: the corpus changed during download — concurrent write detected"
  diff /tmp/etag-before.txt /tmp/etag-after.txt || true
  exit 1
fi
rm -f /tmp/etag-before.txt /tmp/etag-after.txt

# Validate complete pairs — every legacy .graph must have its .nt/.nt.gz. The
# sidecar filename always embeds the real extension of its pair (whatever
# upload-nt.sh wrote at upload time), so this works unchanged for both
# older uncompressed .nt uploads and current .nt.gz ones -- no format
# migration needed, the two coexist until each graph's own collector
# naturally re-uploads it in the new format.
INCOMPLETE=0
for graph_file in /tmp/nt-output/*.graph; do
    [ -e "$graph_file" ] || continue
    nt_file="${graph_file%.graph}"
    if [ ! -f "$nt_file" ]; then
        echo "ERROR: orphan sidecar $(basename "$graph_file") — .nt/.nt.gz missing (concurrent upload in progress?)"
        INCOMPLETE=$((INCOMPLETE + 1))
    fi
done
if [ "$INCOMPLETE" -gt 0 ]; then
  echo "ERROR: $INCOMPLETE incomplete pair(s) found — aborting to avoid partial rebuild"
  exit 1
fi

# Dedup the LEGACY sidecars by graph URI: upload-nt.sh migrated from uploading
# <slug>.nt to <slug>.nt.gz, but never deletes the old key when a collector
# re-uploads under the new name (nothing in this pipeline has Minio delete
# permission -- confirmed live, `mc rm` returns Access Denied). Without this
# step, both the stale <slug>.nt.graph and the current <slug>.nt.gz.graph get
# discovered by the glob above and BOTH get converted below, silently
# duplicating that graph's triples under the same graph URI with stale
# package data mixed into the current data -- confirmed live 2026-09-11
# against the production bucket: 24 graphs (arch, conda-forge, fedora-43,
# debian-trixie, maven, npm, pypi, ... ) had both an old raw upload and a
# newer .gz upload coexisting, accounting for ~10GB of pure duplicate
# quads actually being indexed, not just idle storage.
#
# Group every discovered sidecar by the graph URI in its contents (not by
# filename -- naming schemes can and have changed) and keep only the
# newest data file per URI. Selection only: losing duplicates are skipped
# for this build and left untouched in Minio. This step used to overwrite
# the loser's SOURCE key with a 0-byte payload to reclaim its storage, and
# that is why it no longer does: the reclaim ran here, before conversion,
# the build, the gates or promotion, against an unversioned bucket. Any
# failure downstream therefore destroyed bytes the retry needed and could
# not get back. Storage reclamation is a separate concern from index
# construction and does not belong in front of it.
#
# A graph with a committed manifest is excluded from this entirely. The
# manifest is the authority for that URI, and mtime is a guess -- publishing
# the legacy copy alongside it would union two generations of one named
# graph, which is the exact failure the manifest exists to end. The heuristic
# below should disappear with the last legacy sidecar.
echo "Deduplicating legacy graphs by URI (keep newest non-empty upload per graph)..."
: > /tmp/graph-candidates.txt
SUPERSEDED_COUNT=0
for graph_file in /tmp/nt-output/*.graph; do
    [ -e "$graph_file" ] || continue
    nt_file="${graph_file%.graph}"
    graph_uri=$(tr -d '\n' < "$graph_file")
    if grep -qxF "$graph_uri" /tmp/manifest-uris.txt; then
        echo "  legacy (superseded by manifest): $(basename "$nt_file") <$graph_uri>"
        SUPERSEDED_COUNT=$((SUPERSEDED_COUNT + 1))
        continue
    fi
    mtime=$(stat -c '%Y' "$nt_file")
    size=$(stat -c '%s' "$nt_file")
    # A reclaimed (zeroed) file's mtime is refreshed to the time of the
    # reclaim PUT, making it look newer than the real data it replaced.
    # Without this guard, the *next* run sees the now-empty file as
    # "newest" and reclaims the actual good copy instead -- a two-run
    # oscillation that destroyed ubuntu-noble and ~20 other graphs'
    # only good copies in production on 2026-09-11. A size-0 file must
    # never outrank a non-empty one, so the nonzero flag sorts ahead of
    # mtime; only among candidates in the same zero/nonzero class does
    # mtime decide.
    nonzero=1
    [ "$size" -eq 0 ] && nonzero=0
    printf '%s\t%s\t%s\t%s\n' "$graph_uri" "$nonzero" "$mtime" "$graph_file" >> /tmp/graph-candidates.txt
done
sort -t "$(printf '\t')" -k1,1 -k2,2nr -k3,3nr /tmp/graph-candidates.txt > /tmp/graph-candidates-sorted.txt
rm -f /tmp/graph-candidates.txt

: > /tmp/winning-graphs.txt
STALE_COUNT=0
LAST_URI=""
while IFS=$'\t' read -r graph_uri _nonzero _mtime graph_file; do
    if [ "$graph_uri" != "$LAST_URI" ]; then
        # Newest non-empty entry for this URI (nonzero-first, then
        # mtime-descending) -- winner.
        echo "$graph_file" >> /tmp/winning-graphs.txt
        LAST_URI="$graph_uri"
    else
        # Older duplicate of an already-won URI -- excluded from this build,
        # but deliberately NOT reclaimed. Zeroing it here destroyed the only
        # recoverable copy of a losing payload before the winning candidate
        # had been converted, gated or promoted: the bucket is unversioned,
        # so a build that then failed left neither a new index nor the bytes
        # needed to retry. Several graphs in this corpus exist only as a
        # single legacy object, so the loser is not always redundant.
        # Reclamation, if it returns, belongs after a validated index is
        # accepted and needs its own retention policy.
        nt_file="${graph_file%.graph}"
        echo "  duplicate (not used, retained): $(basename "$nt_file") <$graph_uri>"
        STALE_COUNT=$((STALE_COUNT + 1))
    fi
done < /tmp/graph-candidates-sorted.txt
rm -f /tmp/graph-candidates-sorted.txt
LEGACY_COUNT=$(wc -l < /tmp/winning-graphs.txt | tr -d ' ')
echo "Dedup complete: $STALE_COUNT duplicate(s) excluded from this build, all source objects retained"
echo "Superseded by a manifest: $SUPERSEDED_COUNT legacy sidecar(s) ignored"

# Manifest-backed graphs go in ahead of the legacy winners; from here down
# there is no distinction between them.
cat /tmp/manifest-winners.txt /tmp/winning-graphs.txt > /tmp/all-winners.txt
mv /tmp/all-winners.txt /tmp/winning-graphs.txt
rm -f /tmp/manifest-winners.txt /tmp/manifest-uris.txt

GRAPH_FILES=$(wc -l < /tmp/winning-graphs.txt | tr -d ' ')
echo "$GRAPH_FILES graphs selected ($MANIFEST_COUNT manifest-backed, $LEGACY_COUNT legacy)"
# Not a gate. A partial graph is the availability trade working as intended
# (#70); an index quietly built from a growing number of them is what should
# reach an operator, so it is counted rather than merely mentioned per graph.
if [ "$PARTIAL_COUNT" -gt 0 ]; then
  echo "NOTE: $PARTIAL_COUNT graph(s) published a knowingly partial snapshot"
fi
# <<< shared graph corpus discovery <<<

if [ "$GRAPH_FILES" -eq 0 ]; then
  echo "ERROR: no graphs found — neither a committed manifest nor a .graph sidecar"
  exit 1
fi

if [ "$GRAPH_FILES" -lt 10 ]; then
  echo "ERROR: only $GRAPH_FILES graphs found, minimum is 10"
  exit 1
fi
echo "Completeness: $GRAPH_FILES graphs, previous status=$PREV_STATUS ($PREV_TRIPLES triples)"

DOWNLOAD_SIZE=$(du -sch /tmp/nt-output "$PAYLOAD_DIR" 2>/dev/null | tail -1 | cut -f1)
echo "Corpus on local disk: $DOWNLOAD_SIZE"

# Per-graph gunzip+sed is independent work (no shared state, output order
# doesn't matter -- qlever-index sorts everything internally regardless of
# input quad order), so it runs in parallel across all available cores
# instead of one graph at a time. Confirmed live 2026-09-11: this loop was
# the actual bottleneck, not CPU contention from other containers --
# `podman stats` showed qlever-rebuild-index using only ~1.4 of its 8
# allotted cores during this exact phase, with 10+ host cores sitting idle.
# Each graph converts into its own fragment under /tmp/nq-parts/, then all
# fragments concatenate into packagegraph.nq once every job finishes.
echo "Converting to N-Quads..."
NQ_PARTS=/tmp/nq-parts
rm -rf "$NQ_PARTS"
mkdir -p "$NQ_PARTS"

convert_one_graph() {
    # Each invocation is a fresh `bash -c` process (spawned by xargs) that
    # does not inherit the outer script's `set -euo pipefail` -- without
    # this, a failed gunzip feeding a still-successful sed would silently
    # write truncated data into this graph's fragment instead of failing.
    set -euo pipefail
    local graph_file="$1"
    local nt_file="${graph_file%.graph}"
    local filename
    filename=$(basename "$nt_file")
    local graph_uri
    graph_uri=$(tr -d '\n' < "$graph_file")
    local nt_size
    nt_size=$(du -h "$nt_file" | cut -f1)
    echo "  $filename ($nt_size) → <$graph_uri>"
    echo "$graph_uri" > "$NQ_PARTS/${filename}.uri"
    case "$nt_file" in
        *.gz) gunzip -c "$nt_file" | sed "s| \.$| <${graph_uri}> .|" > "$NQ_PARTS/${filename}.nq" ;;
        *)    sed "s| \.$| <${graph_uri}> .|" "$nt_file" > "$NQ_PARTS/${filename}.nq" ;;
    esac
}
export -f convert_one_graph
export NQ_PARTS

xargs -a /tmp/winning-graphs.txt -d '\n' -P "$(nproc)" -I{} bash -c 'convert_one_graph "$@"' _ {}

# xargs runs every job regardless of earlier failures, so a mid-batch error
# doesn't fail fast the way the old serial loop did -- verify the expected
# fragment count landed before trusting the concatenation below. Compare
# against the deduped winner list, not a raw glob of /tmp/nt-output -- only
# winners were ever dispatched to convert_one_graph above.
GRAPH_COUNT=$(wc -l < /tmp/winning-graphs.txt | tr -d ' ')
FRAGMENT_COUNT=$(find "$NQ_PARTS" -name '*.nq' | wc -l | tr -d ' ')
if [ "$FRAGMENT_COUNT" -ne "$GRAPH_COUNT" ]; then
  echo "ERROR: expected $GRAPH_COUNT converted fragments, found $FRAGMENT_COUNT -- a parallel conversion job failed"
  exit 1
fi

cat "$NQ_PARTS"/*.nq > /tmp/packagegraph.nq
cat "$NQ_PARTS"/*.uri > /tmp/graph-uris.txt
rm -rf "$NQ_PARTS"
rm -f /tmp/winning-graphs.txt
# Deliberately not deleting /tmp/nt-output or /tmp/graph-payloads: they live
# on qlever-rebuild-scratch.volume (a real disk, not tmpfs -- see its own
# comment), and keeping them around is what lets next run's mc mirror above
# skip re-downloading every graph that hasn't changed since tonight, and lets
# an unchanged manifest reuse its already-verified generation. Unbounded
# growth is prevented per graph rather than by wiping: each graph's payload
# directory is pruned to its committed generation above.

NQ_SIZE=$(du -sh /tmp/packagegraph.nq | cut -f1)
TRIPLE_COUNT=$(wc -l < /tmp/packagegraph.nq)
echo "Converted $GRAPH_COUNT graphs: $TRIPLE_COUNT quads ($NQ_SIZE)"

cat > /tmp/settings.json << 'SETTINGS'
{
  "ascii-prefixes-only": false,
  "num-triples-per-batch": 5000000,
  "prefixes-external": [],
  "languages-internal": [],
  "locale": { "language": "en", "country": "US", "ignore-punctuation": true }
}
SETTINGS

echo "Building QLever index..."
mkdir -p /tmp/index
# -m (stxxl-memory) was never set before, so the index build used QLever's
# small built-in default for its external sort -- confirmed live
# 2026-09-10 that the whole build peaked at 81MB of this container's 8g
# limit, meaning the sort phase never got to use memory that's already
# reserved for it. 4G leaves comfortable headroom under the 8g cap for
# parsing buffers and everything else running in parallel above; qlever-index
# writes a resource-usage TSV next to the index by default (see --help),
# so a future rebuild's actual RSS is easy to check before tuning this
# further.
time qlever-index -i /tmp/index/packagegraph \
    -s /tmp/settings.json \
    -F nq -f /tmp/packagegraph.nq -p true -m 4G

INDEX_SIZE=$(du -sh /tmp/index | cut -f1)
echo "Index size: $INDEX_SIZE"

CONTENT_HASH=$(find /tmp/index -type f | sort | xargs sha256sum | sha256sum | cut -c1-16)
echo "Content hash: $CONTENT_HASH"

# Idempotency: if the freshly built index is byte-identical to what is
# already promoted, skip archive/upload/promotion (and, via
# qlever-refresh-if-changed.sh reading STATUS below, the qlever restart).
# Safe to skip the completeness gate here: an identical hash means the index
# already matches the live, previously-vetted one.
MC_RC=0; mc_cat_check "pgraph/${MINIO_BUCKET}/qlever-index/latest" || MC_RC=$?
if [ "$MC_RC" -eq 2 ]; then
  echo "FATAL: Minio error reading latest pointer — aborting"
  exit 1
elif [ "$MC_RC" -eq 0 ] && [ "$MC_RESULT" = "$CONTENT_HASH" ]; then
  echo "QLever index ${CONTENT_HASH} already promoted — no changes, skipping promotion"
  # A success exit: this corpus demonstrably builds the index that is live.
  advance_corpus_marker
  STATUS="unchanged"
  exit 0
fi

echo "Archiving index..."
tar czf /tmp/index.tar.gz -C /tmp/index .
ARCHIVE_SIZE=$(du -h /tmp/index.tar.gz | cut -f1)
echo "Archive size: ${ARCHIVE_SIZE}"

if [ "$PREV_STATUS" = "success" ] && [ "$PREV_TRIPLES" -gt 0 ]; then
  # Triple count gate
  MIN_TRIPLES=$(( PREV_TRIPLES * (100 - MAX_LOSS_PCT) / 100 ))
  echo "Previous: $PREV_TRIPLES triples, allowing ${MAX_LOSS_PCT}% loss (min: $MIN_TRIPLES)"
  if [ "$TRIPLE_COUNT" -lt "$MIN_TRIPLES" ]; then
    echo "ERROR: $TRIPLE_COUNT triples is below ${MAX_LOSS_PCT}% threshold of previous $PREV_TRIPLES"
    echo "Index not promoted — data may be incomplete"
    exit 1
  fi

  # Graph identity gate — every previously-present graph must still exist.
  # `comm` is a line-by-line multiset merge, not a set-membership test: a
  # duplicate entry in last-success.json's graphs[] with no matching
  # duplicate in the current run's list produces a spurious "missing"
  # report on the extra copy even though the graph is genuinely present.
  # `sort -u` on both sides makes this a true set comparison.
  PREV_GRAPHS=$(echo "$PREV_RUN" | jq -r '.graphs[]? // empty' 2>/dev/null | sort -u)
  CURR_GRAPHS=$(sort -u /tmp/graph-uris.txt)
  MISSING=$(comm -23 <(echo "$PREV_GRAPHS") <(echo "$CURR_GRAPHS"))
  if [ -n "$MISSING" ]; then
    echo "ERROR: graphs present in previous successful run are missing:"
    echo "$MISSING"
    echo "Index not promoted — graph set incomplete"
    exit 1
  fi
else
  echo "No previous successful run — using absolute minimum (1M triples)"
  if [ "$TRIPLE_COUNT" -lt 1000000 ]; then
    echo "ERROR: only $TRIPLE_COUNT triples, minimum is 1,000,000"
    exit 1
  fi
fi

echo "Uploading staged index to Minio..."
mc cp /tmp/index.tar.gz "pgraph/${MINIO_BUCKET}/qlever-index/${CONTENT_HASH}/index.tar.gz"

DATE_TAG=$(date +%Y-%m-%d)
echo "${CONTENT_HASH}" | mc pipe "pgraph/${MINIO_BUCKET}/qlever-index/tags/${DATE_TAG}"

echo "Completeness check passed ($GRAPH_COUNT graphs, $TRIPLE_COUNT triples)"

# Save previous latest — logged only. qlever-refresh-if-changed.sh does not
# auto-rollback on a failed reload; if that happens, restore manually with:
#   echo "$PREV_HASH" | mc pipe pgraph/$MINIO_BUCKET/qlever-index/latest
#   systemctl restart qlever-index-load.service qlever.service
MC_RC=0; mc_cat_check "pgraph/${MINIO_BUCKET}/qlever-index/latest" || MC_RC=$?
if [ "$MC_RC" -eq 2 ]; then
  echo "FATAL: Minio error reading latest pointer — aborting"
  exit 1
elif [ "$MC_RC" -eq 0 ]; then
  PREV_HASH="$MC_RESULT"
  echo "Previous latest found: ${PREV_HASH}"
else
  PREV_HASH=""
  echo "No previous latest pointer — first promotion"
fi

echo "Promoting ${CONTENT_HASH} to latest..."
echo "${CONTENT_HASH}" | mc pipe "pgraph/${MINIO_BUCKET}/qlever-index/latest"

echo "✓ Index ${CONTENT_HASH} promoted to latest"
advance_corpus_marker
STATUS="success"
# Multiple physical files can legitimately share one graph URI (e.g.
# multi-arch parts fanning into a single named graph), so graph-uris.txt
# can contain duplicate lines by design -- dedup here so last-success.json
# stores a true set, not a multiset (a multiset baseline breaks the graph
# identity gate's `comm -23` set comparison on the next run).
GRAPHS_JSON=$(sort -u /tmp/graph-uris.txt | jq -R -s 'split("\n") | map(select(length > 0))')
if ! echo "{\"status\":\"success\",\"timestamp\":\"$(date -Iseconds)\",\"content_hash\":\"$CONTENT_HASH\",\"triple_count\":$TRIPLE_COUNT,\"index_size\":\"$INDEX_SIZE\",\"graphs\":$GRAPHS_JSON}" | \
  mc pipe "pgraph/${MINIO_BUCKET}/qlever-index/last-success.json"; then
  echo "ERROR: failed to persist last-success.json — next rebuild may use stale baseline"
  STATUS="success-baseline-stale"
  exit 1
fi
rm -f /tmp/graph-uris.txt

echo "=== Rebuild complete: $(date -Iseconds) ==="
