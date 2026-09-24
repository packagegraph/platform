#!/bin/sh
# Collector: pypi
# Ported from deploy/overlays/dev/jobs/collect-pypi.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-pypi.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/pypi"

CACHE_DIR=/tmp/cache/pypi
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/pypi"

mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

mkdir -p "${CACHE_DIR}"
if mc ls "${MINIO_CACHE}/" >/dev/null 2>&1; then
  echo "Syncing cache from Minio..."
  mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" || echo "Cache warm failed, continuing without cache"
fi

# Periodically flush the cache back to Minio while the collect runs, so
# a timeout kill doesn't discard the run's cache progress -- see
# fedora-43-full.sh's 2026-09-10 timeout incident for why this matters.
( while sleep 300; do
    mc mirror --overwrite --exclude "*.tmp" --exclude "*.lock" "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
  done ) &
CACHE_SYNC_PID=$!
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true; rm -rf "$RUN_DIR"' EXIT

set +e
pg-collect pypi --endpoint "$FUSEKI_ENDPOINT" --cache-dir "${CACHE_DIR}" --cache-ttl-hours 24 -o "$RUN_DIR/pypi.nt"
COLLECT_EXIT=$?
set -e

if [ "$COLLECT_EXIT" -eq 0 ]; then
  echo "Syncing cache to Minio..."
  mc mirror --overwrite --exclude "*.tmp" --exclude "*.lock" "${CACHE_DIR}/" "${MINIO_CACHE}/" || echo "Cache save failed"
  /app/scripts/upload-nt.sh "$RUN_DIR/pypi.nt" "$GRAPH_URI"
fi
exit "$COLLECT_EXIT"
