#!/bin/sh
# Collector: maven
# Ported from deploy/overlays/dev/jobs/collect-maven.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/maven"

CACHE_DIR=/tmp/cache/maven
mkdir -p "${CACHE_DIR}"
MINIO_CACHE="pgraph/${MINIO_BUCKET:-}/collector-cache/maven"

CACHE_AVAILABLE=false
if mc alias set pgraph "${MINIO_ENDPOINT:-}" "${MINIO_ACCESS_KEY:-}" "${MINIO_SECRET_KEY:-}" --api S3v4 2>/dev/null; then
  CACHE_AVAILABLE=true
  echo "Warming cache from Minio..."
  mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || echo "Cache warm failed, continuing"

  # Periodically flush the cache back to Minio while the collect runs,
  # so a timeout kill doesn't discard the run's cache progress -- see
  # fedora-43-full.sh's 2026-09-10 timeout incident for why this matters.
  ( while sleep 300; do
      mc mirror --overwrite --exclude "*.tmp" --exclude "*.lock" "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
    done ) &
  CACHE_SYNC_PID=$!
  trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true' EXIT
else
  echo "WARNING: Minio alias setup failed, proceeding without remote cache"
fi

set +e
pg-collect maven --endpoint "$FUSEKI_ENDPOINT" --cache-dir "${CACHE_DIR}" -o /tmp/maven.nt
COLLECT_EXIT=$?
set -e

if [ "$CACHE_AVAILABLE" = "true" ]; then
  echo "Saving cache to Minio..."
  mc mirror --overwrite --exclude "*.tmp" --exclude "*.lock" "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || echo "Cache save failed"
fi

[ "$COLLECT_EXIT" -eq 0 ] && /app/scripts/upload-nt.sh /tmp/maven.nt "$GRAPH_URI"
exit "$COLLECT_EXIT"
