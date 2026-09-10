#!/bin/sh
# Enricher: npm-provenance
# Ported from deploy/overlays/dev/jobs/enrich-npm-provenance.yaml, minus the
# direct-to-Fuseki drop+load (no writable Fuseki here -- see
# pg-enrich@.container's header comment).
set -eu
mkdir -p /tmp/enrichment
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/npm-provenance"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

pg-collect enrich-npm-provenance \
  --endpoint "$FUSEKI_ENDPOINT" \
  -o /tmp/enrichment/npm_provenance.nt

/app/scripts/upload-nt.sh /tmp/enrichment/npm_provenance.nt "$GRAPH_URI"
