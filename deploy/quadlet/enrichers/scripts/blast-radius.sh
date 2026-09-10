#!/bin/sh
# Enricher: blast-radius
# No Kubernetes equivalent -- never scheduled anywhere before this. Note:
# QLever's path-search SERVICE (confirmed working live, 2026-09-10) can
# answer the same transitive-blast-radius question live at query time --
# this pre-materialized snapshot may turn out to be redundant. Ported
# anyway since it's cheap and the live-query alternative hasn't been
# validated against real blast-radius-shaped queries yet.
set -eu
mkdir -p /tmp/enrichment
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/blast-radius"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

pg-collect enrich-blast-radius \
  --endpoint "$FUSEKI_ENDPOINT" \
  -o /tmp/enrichment/blast_radius.nt

/app/scripts/upload-nt.sh /tmp/enrichment/blast_radius.nt "$GRAPH_URI"
