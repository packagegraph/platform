#!/bin/bash
set -euo pipefail

# Upload an N-Triples file to Minio and register it in the graph manifest.
#
# Usage: upload-nt.sh <local-file.nt> <graph-uri>
#
# Example:
#   upload-nt.sh /tmp/packages.nt "https://packagegraph.github.io/graph/debian/trixie"
#
# Uploads to: pgraph/${MINIO_BUCKET}/nt-output/debian-trixie.nt.gz
# Creates: pgraph/${MINIO_BUCKET}/nt-output/debian-trixie.nt.gz.graph (sidecar)

if [ $# -ne 2 ]; then
    echo "Usage: upload-nt.sh <local-file.nt> <graph-uri>" >&2
    exit 1
fi

LOCAL_FILE="$1"
GRAPH_URI="$2"

if [ ! -f "$LOCAL_FILE" ]; then
    echo "Error: file not found: $LOCAL_FILE" >&2
    exit 1
fi

# Derive Minio filename from graph URI
# https://packagegraph.github.io/graph/debian/trixie → debian-trixie
# https://packagegraph.github.io/graph/security/osv → security-osv
# https://packagegraph.github.io/ontology            → ontology
GRAPH_SLUG=$(echo "$GRAPH_URI" | sed 's|https://packagegraph.github.io/graph/||; s|https://packagegraph.github.io/||' | tr '/' '-')
# gzip, not xz: N-Triples text compresses well under either, but gzip
# decompresses several times faster, which matters more here than the last
# few percent of ratio -- qlever-rebuild-index.sh decompresses every graph
# on every nightly rebuild.
MINIO_FILENAME="${GRAPH_SLUG}.nt.gz"

echo "=== Uploading N-Triples to Minio ==="
echo "Local: $LOCAL_FILE"
echo "Graph: $GRAPH_URI"
echo "Minio: nt-output/$MINIO_FILENAME"

# Configure mc alias (idempotent)
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4 >/dev/null 2>&1

COMPRESSED_FILE="${LOCAL_FILE}.gz"
gzip -c "$LOCAL_FILE" > "$COMPRESSED_FILE"

# Upload .nt.gz file first — orphan .nt.gz without sidecar is safely excluded
# by rebuilds (they iterate .graph files). The .graph sidecar acts as a
# commit marker: only written after the .nt.gz upload succeeds.
mc cp "$COMPRESSED_FILE" "pgraph/${MINIO_BUCKET}/nt-output/${MINIO_FILENAME}"

# Create .graph sidecar (commit marker) — signals this .nt.gz is ready for rebuild
SIDECAR_PATH="pgraph/${MINIO_BUCKET}/nt-output/${MINIO_FILENAME}.graph"
echo -n "$GRAPH_URI" | mc pipe "$SIDECAR_PATH"

rm -f "$COMPRESSED_FILE"

echo "✓ Uploaded $MINIO_FILENAME + .graph sidecar"
