#!/bin/sh
# Enricher: forge-version
# No Kubernetes equivalent -- never scheduled anywhere before this.
# GITLAB_TOKEN is optional (only needed for self-hosted GitLab instances
# requiring auth); omitted here since no such instance is in scope yet.
set -eu
mkdir -p /tmp/enrichment
CACHE_DIR=/tmp/cache/forge-version
MINIO_CACHE="pgraph/${MINIO_BUCKET}/enricher-cache/forge-version"
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/forge-version"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

echo "Syncing cache from Minio..."
mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true

pg-collect enrich-forge-version \
  --endpoint "$FUSEKI_ENDPOINT" \
  -o /tmp/enrichment/forge_version.nt \
  --cache-dir "${CACHE_DIR}"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

/app/scripts/upload-nt.sh /tmp/enrichment/forge_version.nt "$GRAPH_URI"
