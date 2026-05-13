#!/bin/bash
set -euo pipefail

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
    echo "Collecting from multiple RPM repos..."

    RPM_REPO_ARGS=""
    while IFS= read -r repo_spec; do
        [ -z "$repo_spec" ] && continue
        RPM_REPO_ARGS="$RPM_REPO_ARGS --rpm-repo \"$repo_spec\""
    done <<< "$RPM_REPOS"

    eval pg-collect rpm \
        --output "$OUTPUT_DIR/packages.nt" \
        $RPM_REPO_ARGS \
        $COLLECT_ARGS

elif [ -n "${REPO_URL:-}" ]; then
    echo "Collecting from ${REPO_URL}..."

    ARCH_ARGS=""
    for arch in $COLLECT_ARCHES; do
        ARCH_ARGS="$ARCH_ARGS --arch $arch"
    done

    eval pg-collect debian \
        --repo "$REPO_URL" \
        --output "$OUTPUT_DIR/packages.nt" \
        $ARCH_ARGS \
        $COLLECT_ARGS
fi

# Step 2: Upload to Minio, load into Fuseki, or build TDB2
if [ -n "$GRAPH_URI" ] && [ -n "${MINIO_ACCESS_KEY:-}" ] && [ -z "${FUSEKI_ENDPOINT:-}" ]; then
    echo "Uploading to Minio for offline rebuild..."
    for f in "$OUTPUT_DIR"/*.nt; do
        [ -f "$f" ] && /app/scripts/upload-nt.sh "$f" "$GRAPH_URI"
    done
    echo "Minio upload complete."
elif [ -n "$FUSEKI_ENDPOINT" ] && [ -n "$GRAPH_URI" ]; then
    echo "Loading via GSP into graph <$GRAPH_URI>..."

    if [ "${COLLECTOR_FULL_RELOAD:-0}" = "1" ]; then
        echo "Full reload requested — dropping existing graph..."
        pg-collect drop --graph "$GRAPH_URI" --endpoint "$FUSEKI_ENDPOINT"
    fi

    for f in "$OUTPUT_DIR"/*.nt; do
        [ -f "$f" ] && pg-collect load "$f" --graph "$GRAPH_URI" --endpoint "$FUSEKI_ENDPOINT"
    done

    echo "SPARQL load complete."
else
    echo "Building TDB2 index..."
    JENA_HOME="${JENA_HOME:-/opt/jena}"
    "$JENA_HOME/bin/tdb2.tdbloader" \
        --loc "$TDB2_DIR" \
        "$OUTPUT_DIR"/*.nt
    echo "TDB2 build complete."
fi

echo "=== ETL Pipeline Complete ==="
