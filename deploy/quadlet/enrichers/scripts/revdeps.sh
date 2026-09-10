#!/bin/sh
# Enricher: revdeps
# No Kubernetes equivalent -- never scheduled anywhere before this.
set -eu
mkdir -p /tmp/enrichment
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/revdeps"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

pg-collect enrich-revdeps \
  --endpoint "$FUSEKI_ENDPOINT" \
  -o /tmp/enrichment/revdeps.nt

/app/scripts/upload-nt.sh /tmp/enrichment/revdeps.nt "$GRAPH_URI"
