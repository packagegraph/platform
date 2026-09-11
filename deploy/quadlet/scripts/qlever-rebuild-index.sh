#!/bin/bash
# Build a fresh QLever index from the .nt/.graph corpus in Minio and promote
# it to "latest" if it passes completeness gates. Equivalent of the
# rebuild-qlever-index CronJob (deploy/overlays/{dev,prod}/jobs/rebuild-qlever-index.yaml),
# minus the `kubectl rollout restart/status` steps at the end -- those are the
# host's job here, not the container's. See qlever-refresh-if-changed.sh,
# invoked via ExecStartPost= on qlever-rebuild-index.service, which reloads
# and bounces qlever.service only when this script actually promotes a new
# index (STATUS=success in last-run.json).
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

# ETag-based snapshot before download to detect concurrent writes
echo "Taking pre-download ETag snapshot..."
mc ls -r --json "pgraph/${MINIO_BUCKET}/nt-output/" | \
  jq -r 'select(.status == "success") | select(.key | test("\\.json$") | not) | "\(.key) \(.etag)"' | \
  sort > /tmp/etag-before.txt

# Early-exit skip: this same listing is a cheap, complete fingerprint of
# nt-output/'s current contents. QLever's indexer isn't incremental --
# every rebuild pays the full download+convert+build cost regardless of
# how much actually changed, which is wasted work entirely on any night
# where no collector uploaded anything new. Compare against the listing
# hash saved after the last run that got this far (below); an identical
# corpus means an identical index would result, so skip before doing any
# of that work rather than after (the existing content-hash check further
# down still catches the case where the corpus *did* change but happens to
# produce a byte-identical index).
CORPUS_LISTING_HASH=$(sha256sum /tmp/etag-before.txt | cut -d' ' -f1)
MC_RC=0; mc_cat_check "pgraph/${MINIO_BUCKET}/qlever-index/latest" || MC_RC=$?
if [ "$MC_RC" -eq 0 ]; then
  PREV_LISTING_HASH=""
  MC_RC2=0; mc_cat_check "pgraph/${MINIO_BUCKET}/qlever-index/last-corpus-listing-hash.txt" || MC_RC2=$?
  [ "$MC_RC2" -eq 0 ] && PREV_LISTING_HASH="$MC_RESULT"
  if [ "$PREV_LISTING_HASH" = "$CORPUS_LISTING_HASH" ]; then
    echo "nt-output/ unchanged since the last completed run — skipping download/build entirely"
    STATUS="unchanged"
    exit 0
  fi
fi

echo "Downloading all .nt/.nt.gz and .graph files..."
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
mc mirror --overwrite --exclude '*.json' \
  "pgraph/${MINIO_BUCKET}/nt-output/" /tmp/nt-output/

# Verify no files changed during download (ETags catch same-size replacements)
mc ls -r --json "pgraph/${MINIO_BUCKET}/nt-output/" | \
  jq -r 'select(.status == "success") | select(.key | test("\\.json$") | not) | "\(.key) \(.etag)"' | \
  sort > /tmp/etag-after.txt
if ! diff -q /tmp/etag-before.txt /tmp/etag-after.txt >/dev/null 2>&1; then
  echo "ERROR: nt-output changed during download — concurrent write detected"
  diff /tmp/etag-before.txt /tmp/etag-after.txt || true
  exit 1
fi
rm -f /tmp/etag-before.txt /tmp/etag-after.txt

# Record this corpus state now that it's confirmed stable, so the next run's
# early-exit check (above) can skip if nothing changes before then. Recorded
# regardless of whether this run ends up promoting -- a crash before this
# point (script error, indexer failure, etc.) simply leaves the previous
# marker in place, so the next run correctly does NOT skip and retries the
# full pipeline instead of getting stuck skipping forever.
echo "$CORPUS_LISTING_HASH" | mc pipe "pgraph/${MINIO_BUCKET}/qlever-index/last-corpus-listing-hash.txt"

GRAPH_FILES=$(find /tmp/nt-output -name '*.graph' | wc -l | tr -d ' ')
echo "$GRAPH_FILES graphs discovered"

if [ "$GRAPH_FILES" -eq 0 ]; then
  echo "ERROR: no .graph sidecar files found"
  exit 1
fi

if [ "$GRAPH_FILES" -lt 10 ]; then
  echo "ERROR: only $GRAPH_FILES graphs found, minimum is 10"
  exit 1
fi
echo "Completeness: $GRAPH_FILES graphs, previous status=$PREV_STATUS ($PREV_TRIPLES triples)"

DOWNLOAD_SIZE=$(du -sh /tmp/nt-output | cut -f1)
echo "Downloaded $DOWNLOAD_SIZE to local disk"

echo "Converting to N-Quads..."
: > /tmp/packagegraph.nq
: > /tmp/graph-uris.txt

# Validate complete pairs — every .graph must have its .nt/.nt.gz. The
# sidecar filename always embeds the real extension of its pair (whatever
# upload-nt.sh wrote at upload time), so this works unchanged for both
# older uncompressed .nt uploads and current .nt.gz ones -- no format
# migration needed, the two coexist until each graph's own collector
# naturally re-uploads it in the new format.
INCOMPLETE=0
for graph_file in /tmp/nt-output/*.graph; do
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

# Dedup by graph URI: upload-nt.sh migrated from uploading <slug>.nt to
# <slug>.nt.gz, but never deletes the old key when a collector re-uploads
# under the new name (nothing in this pipeline has Minio delete permission
# -- confirmed live, `mc rm` returns Access Denied). Without this step,
# both the stale <slug>.nt.graph and the current <slug>.nt.gz.graph get
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
# newest data file per URI. The older duplicate's SOURCE key in Minio is
# then overwritten with a 0-byte payload (confirmed live: `mc pipe` with
# empty stdin succeeds even without delete permission, since this is a
# plain PUT to an existing key, a different S3 permission than
# DeleteObject -- and the bucket is unversioned, so the old bytes are
# actually freed, not retained under a hidden version) so the exact same
# duplication doesn't get rediscovered and reclaimed on every future run.
echo "Deduplicating graphs by URI (keep newest upload per graph)..."
: > /tmp/graph-candidates.txt
for graph_file in /tmp/nt-output/*.graph; do
    nt_file="${graph_file%.graph}"
    graph_uri=$(tr -d '\n' < "$graph_file")
    mtime=$(stat -c '%Y' "$nt_file")
    printf '%s\t%s\t%s\n' "$graph_uri" "$mtime" "$graph_file" >> /tmp/graph-candidates.txt
done
sort -t "$(printf '\t')" -k1,1 -k2,2nr /tmp/graph-candidates.txt > /tmp/graph-candidates-sorted.txt
rm -f /tmp/graph-candidates.txt

: > /tmp/winning-graphs.txt
STALE_COUNT=0
LAST_URI=""
while IFS=$'\t' read -r graph_uri _mtime graph_file; do
    if [ "$graph_uri" != "$LAST_URI" ]; then
        # Newest entry for this URI (sorted mtime-descending) -- winner.
        echo "$graph_file" >> /tmp/winning-graphs.txt
        LAST_URI="$graph_uri"
    else
        # Older duplicate of an already-won URI -- reclaim its storage.
        nt_file="${graph_file%.graph}"
        stale_key="nt-output/$(basename "$nt_file")"
        echo "  stale duplicate: $(basename "$nt_file") <$graph_uri> — reclaiming $stale_key"
        STALE_COUNT=$((STALE_COUNT + 1))
        if ! printf '' | mc pipe "pgraph/${MINIO_BUCKET}/${stale_key}"; then
            echo "WARN: failed to reclaim $stale_key — will retry next run" >&2
        fi
    fi
done < /tmp/graph-candidates-sorted.txt
rm -f /tmp/graph-candidates-sorted.txt
echo "Dedup complete: $STALE_COUNT stale duplicate(s) reclaimed"

# Per-graph gunzip+sed is independent work (no shared state, output order
# doesn't matter -- qlever-index sorts everything internally regardless of
# input quad order), so it runs in parallel across all available cores
# instead of one graph at a time. Confirmed live 2026-09-11: this loop was
# the actual bottleneck, not CPU contention from other containers --
# `podman stats` showed qlever-rebuild-index using only ~1.4 of its 8
# allotted cores during this exact phase, with 10+ host cores sitting idle.
# Each graph converts into its own fragment under /tmp/nq-parts/, then all
# fragments concatenate into packagegraph.nq once every job finishes.
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
# Deliberately not deleting /tmp/nt-output or its contents: it lives on
# qlever-rebuild-scratch.volume (a real disk, not tmpfs -- see its own
# comment), and keeping it around is what lets next run's mc mirror above
# skip re-downloading every graph that hasn't changed since tonight.

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

  # Graph identity gate — every previously-present graph must still exist
  PREV_GRAPHS=$(echo "$PREV_RUN" | jq -r '.graphs[]? // empty' 2>/dev/null | sort)
  CURR_GRAPHS=$(sort /tmp/graph-uris.txt)
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
STATUS="success"
GRAPHS_JSON=$(jq -R -s 'split("\n") | map(select(length > 0))' /tmp/graph-uris.txt)
if ! echo "{\"status\":\"success\",\"timestamp\":\"$(date -Iseconds)\",\"content_hash\":\"$CONTENT_HASH\",\"triple_count\":$TRIPLE_COUNT,\"index_size\":\"$INDEX_SIZE\",\"graphs\":$GRAPHS_JSON}" | \
  mc pipe "pgraph/${MINIO_BUCKET}/qlever-index/last-success.json"; then
  echo "ERROR: failed to persist last-success.json — next rebuild may use stale baseline"
  STATUS="success-baseline-stale"
  exit 1
fi
rm -f /tmp/graph-uris.txt

echo "=== Rebuild complete: $(date -Iseconds) ==="
