#!/bin/sh
# Enricher: nvd
# No Kubernetes equivalent -- never scheduled anywhere before this. Feed
# mode (bulk download, file-based -- the default), not API mode (per-CVE,
# writes via INSERT DATA and needs a writable endpoint we don't have here).
# NVD_API_KEY is optional (raises rate limits, feed mode works fine
# without one); omitted since we don't have one provisioned.
set -eu
mkdir -p /tmp/enrichment
CACHE_DIR=/tmp/cache/nvd
MINIO_CACHE="pgraph/${MINIO_BUCKET}/enricher-cache/nvd"
GRAPH_URI="https://packagegraph.github.io/graph/cve/nvd"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

echo "Syncing cache from Minio..."
mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true

pg-collect enrich-nvd \
  --endpoint "$FUSEKI_ENDPOINT" \
  --mode feed \
  -o /tmp/enrichment/nvd.nt \
  --cache-dir "${CACHE_DIR}"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

/app/scripts/upload-nt.sh /tmp/enrichment/nvd.nt "$GRAPH_URI"
