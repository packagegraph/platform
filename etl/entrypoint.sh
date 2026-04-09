#!/bin/bash
set -euo pipefail

# Default: run the full ETL pipeline
# 1. Collect from configured repos
# 2. Build TDB2
# 3. Upload to Minio

COLLECT_ARGS="${COLLECT_ARGS:-}"
OUTPUT_DIR="${OUTPUT_DIR:-/tmp/output}"
TDB2_DIR="${TDB2_DIR:-/tmp/tdb2}"
ONTOLOGY_DIR="${ONTOLOGY_DIR:-/app/ontology}"

mkdir -p "$OUTPUT_DIR"

echo "=== PackageGraph ETL Pipeline ==="

# Step 1: Collect package data
if [ -n "${REPO_URL:-}" ]; then
    echo "Collecting from ${REPO_URL}..."
    packagegraph collect "$REPO_URL" \
        --repo-type "${REPO_TYPE:-debian}" \
        --output-file "$OUTPUT_DIR/packages.ttl" \
        $COLLECT_ARGS
fi

# Step 2: Build TDB2 and upload to Minio
echo "Building TDB2 index..."
packagegraph build \
    --input-dir "$OUTPUT_DIR" \
    --ontology-dir "$ONTOLOGY_DIR" \
    --output-dir "$TDB2_DIR" \
    --jena-home "$JENA_HOME"

echo "=== ETL Pipeline Complete ==="
