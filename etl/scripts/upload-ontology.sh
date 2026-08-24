#!/bin/bash
set -euo pipefail

# Convert ontology Turtle files to N-Triples and upload to Minio.
# Used by rebuild-tdb2 for QLever parity, and standalone for clean-install bootstrap.
#
# Usage: upload-ontology.sh
#
# Requires: MINIO_ENDPOINT, MINIO_ACCESS_KEY, MINIO_SECRET_KEY, MINIO_BUCKET env vars
#           mc alias "pgraph" must be configured
#           /app/ontology/*.ttl must exist (copied into ETL image)

ONTOLOGY_DIR="${ONTOLOGY_DIR:-/app/ontology}"
GRAPH_URI="https://packagegraph.github.io/ontology"

if [ ! -d "$ONTOLOGY_DIR" ] || [ -z "$(ls "$ONTOLOGY_DIR"/*.ttl 2>/dev/null)" ]; then
  echo "ERROR: no .ttl files found in $ONTOLOGY_DIR" >&2
  exit 1
fi

echo "=== Ontology Upload ==="
echo "Source: $ONTOLOGY_DIR"
echo "Graph:  $GRAPH_URI"

# Convert Turtle to N-Triples
/opt/jena/bin/riot --output=ntriples "$ONTOLOGY_DIR"/*.ttl > /tmp/ontology.nt
TRIPLE_COUNT=$(wc -l < /tmp/ontology.nt)
echo "Converted: $TRIPLE_COUNT triples"

if [ "$TRIPLE_COUNT" -eq 0 ]; then
  echo "ERROR: ontology produced 0 triples"
  exit 1
fi

# Upload using standard .nt + .graph sidecar pattern
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
"$SCRIPT_DIR/upload-nt.sh" /tmp/ontology.nt "$GRAPH_URI"
rm -f /tmp/ontology.nt
