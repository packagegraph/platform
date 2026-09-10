#!/bin/sh
# Enricher: epss
# No Kubernetes equivalent -- never scheduled anywhere before this. Verified
# live 2026-09-10 against QLever after fixing a malformed-IRI bug in
# enrich_epss.rs's discovery query (fixed independently of this port).
set -eu
mkdir -p /tmp/enrichment
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/epss"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

pg-collect enrich-epss \
  --endpoint "$FUSEKI_ENDPOINT" \
  -o /tmp/enrichment/epss.nt

/app/scripts/upload-nt.sh /tmp/enrichment/epss.nt "$GRAPH_URI"
