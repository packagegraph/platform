#!/bin/sh
# Enricher: koji
# Ported from deploy/overlays/dev/jobs/enrich-koji.yaml, minus the
# direct-to-Fuseki drop+load (no writable Fuseki here -- see
# pg-enrich@.container's header comment).
set -eu
mkdir -p /tmp/enrichment
CACHE_DIR=/tmp/cache/koji
MINIO_CACHE="pgraph/${MINIO_BUCKET}/enricher-cache/koji"
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/koji"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

echo "Syncing cache from Minio..."
mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true
echo "Cache warmed: $(find "${CACHE_DIR}" -name '*.json' 2>/dev/null | wc -l) entries"

# Periodically flush the cache back to Minio while the (long-running,
# network-bound) enrich runs, so a timeout kill doesn't discard hours of
# freshly-cached fetches -- see fedora-43-full.sh's 2026-09-10 timeout
# incident for why this matters.
( while sleep 300; do
    mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
  done ) &
CACHE_SYNC_PID=$!
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true' EXIT

ENRICH_OK=0
pg-collect enrich-koji \
  --endpoint "$FUSEKI_ENDPOINT" \
  -o /tmp/enrichment/koji.nt \
  --cache-dir "${CACHE_DIR}" \
  --koji-hub https://koji.fedoraproject.org/kojihub \
  --distro fedora || ENRICH_OK=$?

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

if [ "$ENRICH_OK" -ne 0 ]; then
  echo "WARNING: enrich-koji failed (exit $ENRICH_OK) — skipping upload"
elif [ -f /tmp/enrichment/koji.nt ]; then
  /app/scripts/upload-nt.sh /tmp/enrichment/koji.nt "$GRAPH_URI"
fi
