#!/usr/bin/env bash
# Load OSS Taxonomy SKOS data into Fuseki.
#
# Prerequisites:
#   - Fuseki accessible at $FUSEKI_URL (default: http://localhost:3031/packagegraph)
#   - taxonomy.ttl in etl/ontology/
#
# Usage:
#   ./import-taxonomy-concepts.sh [fuseki-url]
#
# The taxonomy is loaded into the named graph:
#   https://packagegraph.github.io/graph/ontology/taxonomy

set -euo pipefail

FUSEKI_URL="${1:-http://localhost:3031/packagegraph}"
GRAPH="https://packagegraph.github.io/graph/ontology/taxonomy"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TAXONOMY_FILE="$SCRIPT_DIR/../ontology/taxonomy.ttl"

if [[ ! -f "$TAXONOMY_FILE" ]]; then
    echo "Error: taxonomy.ttl not found at $TAXONOMY_FILE"
    echo "Run: cp ontology/extensions/taxonomy/taxonomy.ttl platform/etl/ontology/"
    exit 1
fi

echo "Loading taxonomy into $FUSEKI_URL graph <$GRAPH>"
echo "Source: $TAXONOMY_FILE"

# Drop existing taxonomy graph
curl -s -X DELETE "${FUSEKI_URL}/data?graph=${GRAPH}" || true

# Upload via GSP
curl -s -X PUT \
    -H "Content-Type: text/turtle" \
    --data-binary "@${TAXONOMY_FILE}" \
    "${FUSEKI_URL}/data?graph=${GRAPH}"

# Verify
COUNT=$(curl -s -H 'Accept: application/sparql-results+json' \
    "${FUSEKI_URL}/sparql" \
    --data-urlencode "query=SELECT (COUNT(*) AS ?c) WHERE { GRAPH <${GRAPH}> { ?s ?p ?o } }" \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['results']['bindings'][0]['c']['value'])" 2>/dev/null || echo "?")

echo "Loaded $COUNT triples into <$GRAPH>"
