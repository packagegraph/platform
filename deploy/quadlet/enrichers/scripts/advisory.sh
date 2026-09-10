#!/bin/sh
# Enricher: advisory (RHSA + DSA)
# Ported from deploy/overlays/dev/jobs/enrich-advisory.yaml, minus the
# direct-to-Fuseki drop+load (no writable Fuseki here -- see
# pg-enrich@.container's header comment).
set -eu
mkdir -p /tmp/enrichment
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

for TYPE in rhsa dsa; do
  echo "=== Advisory enrichment: $TYPE ==="
  CACHE_DIR="/tmp/cache/advisory_${TYPE}"
  MINIO_CACHE="pgraph/${MINIO_BUCKET}/enricher-cache/advisory/${TYPE}"

  echo "Syncing cache from Minio..."
  mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true
  echo "Cache warmed: $(find "${CACHE_DIR}" -name '*.json' 2>/dev/null | wc -l) entries"

  ENRICH_OK=0
  pg-collect enrich-advisory \
    --advisory-type "$TYPE" \
    -o "/tmp/enrichment/advisory_${TYPE}.nt" \
    --cache-dir "${CACHE_DIR}" \
    --days-back 365 || ENRICH_OK=$?

  echo "Syncing cache to Minio..."
  mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

  GRAPH_URI="https://packagegraph.github.io/graph/enrichment/advisory-${TYPE}"
  if [ "$ENRICH_OK" -ne 0 ]; then
    echo "WARNING: enrich-advisory $TYPE failed (exit $ENRICH_OK) — skipping upload"
  elif [ -f "/tmp/enrichment/advisory_${TYPE}.nt" ]; then
    /app/scripts/upload-nt.sh "/tmp/enrichment/advisory_${TYPE}.nt" "$GRAPH_URI"
  fi
done
