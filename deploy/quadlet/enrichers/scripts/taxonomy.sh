#!/bin/sh
# Enricher: taxonomy
# No Kubernetes equivalent -- never scheduled anywhere before this.
set -eu
mkdir -p /tmp/enrichment
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/taxonomy"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

pg-collect enrich-taxonomy \
  --endpoint "$FUSEKI_ENDPOINT" \
  -o /tmp/enrichment/taxonomy.nt

/app/scripts/upload-nt.sh /tmp/enrichment/taxonomy.nt "$GRAPH_URI"
