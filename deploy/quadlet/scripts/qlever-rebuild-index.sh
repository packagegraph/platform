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

echo "Downloading all .nt and .graph files..."
mc mirror --exclude '*.json' \
  "pgraph/${MINIO_BUCKET}/nt-output/" /tmp/nt-output/ --overwrite

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

# Validate complete pairs — every .graph must have its .nt
INCOMPLETE=0
for graph_file in /tmp/nt-output/*.graph; do
    nt_file="${graph_file%.graph}"
    if [ ! -f "$nt_file" ]; then
        echo "ERROR: orphan sidecar $(basename "$graph_file") — .nt missing (concurrent upload in progress?)"
        INCOMPLETE=$((INCOMPLETE + 1))
    fi
done
if [ "$INCOMPLETE" -gt 0 ]; then
  echo "ERROR: $INCOMPLETE incomplete pair(s) found — aborting to avoid partial rebuild"
  exit 1
fi

GRAPH_COUNT=0
for graph_file in /tmp/nt-output/*.graph; do
    nt_file="${graph_file%.graph}"
    filename=$(basename "$nt_file")
    graph_uri=$(cat "$graph_file" | tr -d '\n')
    nt_size=$(du -h "$nt_file" | cut -f1)
    echo "  $filename ($nt_size) → <$graph_uri>"
    echo "$graph_uri" >> /tmp/graph-uris.txt
    sed "s| \.$| <${graph_uri}> .|" "$nt_file" >> /tmp/packagegraph.nq
    rm -f "$nt_file" "$graph_file"
    GRAPH_COUNT=$((GRAPH_COUNT + 1))
done
rm -rf /tmp/nt-output

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
time qlever-index -i /tmp/index/packagegraph \
    -s /tmp/settings.json \
    -F nq -f /tmp/packagegraph.nq -p true

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
