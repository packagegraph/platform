#!/bin/bash
set -euo pipefail

# Default: run the full ETL pipeline
# 1. Collect from configured repos
# 2. Build TDB2
# 3. Upload to Minio

COLLECT_ARGS="${COLLECT_ARGS:-}"
COLLECT_ARCHES="${COLLECT_ARCHES:-binary-amd64}"
OUTPUT_DIR="${OUTPUT_DIR:-/tmp/output}"
GRAPH_URI="${GRAPH_URI:-}"
FUSEKI_ENDPOINT="${FUSEKI_ENDPOINT:-}"
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
        [ -z "$repo_spec" ] && continue
        RPM_REPO_ARGS="$RPM_REPO_ARGS --rpm-repo \"$repo_spec\""
    done <<< "$RPM_REPOS"

    if command -v pg-collect >/dev/null 2>&1; then
        eval pg-collect rpm \
            --output "$OUTPUT_DIR/packages.nt" \
            $RPM_REPO_ARGS \
            $COLLECT_ARGS
    else
        echo "pg-collect not available, using Python collector..."
        eval packagegraph collect \
            --repo-type rpm \
            --output-file "$OUTPUT_DIR/packages.ttl" \
            $RPM_REPO_ARGS \
            $COLLECT_ARGS
    fi

elif [ -n "${REPO_URL:-}" ]; then
    echo "Collecting from ${REPO_URL}..."

    ARCH_ARGS=""
    for arch in $COLLECT_ARCHES; do
        ARCH_ARGS="$ARCH_ARGS --arch $arch"
    done

    if command -v pg-collect >/dev/null 2>&1; then
        eval pg-collect debian \
            --repo "$REPO_URL" \
            --output "$OUTPUT_DIR/packages.nt" \
            $ARCH_ARGS \
            $COLLECT_ARGS
    else
        echo "pg-collect not available, using Python collector..."
        packagegraph collect "$REPO_URL" \
            --repo-type debian \
            --output-file "$OUTPUT_DIR/packages.ttl" \
            $ARCH_ARGS \
            $COLLECT_ARGS
    fi
fi

# Step 2: Load into Fuseki or build TDB2
if [ -n "$FUSEKI_ENDPOINT" ] && [ -n "$GRAPH_URI" ]; then
    echo "Loading via GSP into graph <$GRAPH_URI>..."

    # Only DROP on explicit full reload (avoids TDB2 mmap corruption on large graphs)
    if [ "${COLLECTOR_FULL_RELOAD:-0}" = "1" ]; then
        echo "Full reload requested — dropping existing graph..."
        pg-collect drop --graph "$GRAPH_URI" --endpoint "$FUSEKI_ENDPOINT"
    fi

    # Load all .nt files (GSP POST appends to existing graph)
    for f in "$OUTPUT_DIR"/*.nt; do
        [ -f "$f" ] && pg-collect load "$f" --graph "$GRAPH_URI" --endpoint "$FUSEKI_ENDPOINT"
    done

    echo "SPARQL load complete."
else
    echo "Building TDB2 index..."
    packagegraph build \
        --input-dir "$OUTPUT_DIR" \
        --ontology-dir "$ONTOLOGY_DIR" \
        --output-dir "$TDB2_DIR" \
        --jena-home "$JENA_HOME"
fi

echo "=== ETL Pipeline Complete ==="
