#!/bin/bash
set -euo pipefail

# Default: run the full ETL pipeline
# 1. Collect from configured repos
# 2. Build TDB2
# 3. Upload to Minio

COLLECT_ARGS="${COLLECT_ARGS:-}"
COLLECT_ARCHES="${COLLECT_ARCHES:-binary-amd64}"
OUTPUT_DIR="${OUTPUT_DIR:-/tmp/output}"
TDB2_DIR="${TDB2_DIR:-/tmp/tdb2}"
ONTOLOGY_DIR="${ONTOLOGY_DIR:-/app/ontology}"

mkdir -p "$OUTPUT_DIR"

echo "=== PackageGraph ETL Pipeline ==="

# Step 1: Collect package data
if [ -n "${RPM_REPOS:-}" ]; then
    # Multi-release RPM collection
    echo "Collecting from multiple RPM repos..."

    RPM_REPO_ARGS=""
    while IFS= read -r repo_spec; do
        [ -z "$repo_spec" ] && continue  # Skip empty lines
        RPM_REPO_ARGS="$RPM_REPO_ARGS --rpm-repo \"$repo_spec\""
    done <<< "$RPM_REPOS"

    # Use pg-collect for RPM collection
    eval pg-collect rpm \
        --output "$OUTPUT_DIR/packages.nt" \
        $RPM_REPO_ARGS \
        $COLLECT_ARGS

elif [ -n "${REPO_URL:-}" ]; then
    echo "Collecting from ${REPO_URL}..."

    # Build arch arguments from COLLECT_ARCHES (space-separated list)
    ARCH_ARGS=""
    for arch in $COLLECT_ARCHES; do
        ARCH_ARGS="$ARCH_ARGS --arch $arch"
    done

    # Use pg-collect for Debian collection
    eval pg-collect debian \
        --repo "$REPO_URL" \
        --output "$OUTPUT_DIR/packages.nt" \
        $ARCH_ARGS \
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
