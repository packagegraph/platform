#!/bin/bash
# Build a full QLever index from all .nt files in Minio.
# Run on berstuk: bash ~/qlever-spike/build-full-index.sh
#
# Prerequisites:
#   - mc alias 'pgraph' configured for Minio
#   - graphs.json downloaded from Minio
#   - podman with adfreiburg/qlever:latest pulled
set -euo pipefail

SPIKE_DIR=~/qlever-spike
DATA_DIR="$SPIKE_DIR/data-full"
INDEX_DIR="$SPIKE_DIR/index-prod"
MINIO_PATH="pgraph/packagegraph/nt-output"

mkdir -p "$DATA_DIR" "$INDEX_DIR"

echo "=== Downloading graphs.json ==="
mc cat "$MINIO_PATH/graphs.json" > "$DATA_DIR/graphs.json"

echo ""
echo "=== Downloading .nt files from Minio ==="
TOTAL_FILES=$(python3 -c "import json; print(len(json.load(open('$DATA_DIR/graphs.json'))))")
echo "$TOTAL_FILES graphs to download"

IDX=0
python3 -c "
import json
for fname, graph_uri in json.load(open('$DATA_DIR/graphs.json')).items():
    print(f'{fname}\t{graph_uri}')
" | while IFS=$'\t' read -r filename graph_uri; do
    IDX=$((IDX + 1))
    if [ -f "$DATA_DIR/$filename" ]; then
        echo "  [$IDX/$TOTAL_FILES] $filename — already downloaded"
        continue
    fi
    echo "  [$IDX/$TOTAL_FILES] Downloading $filename..."
    mc cp "$MINIO_PATH/$filename" "$DATA_DIR/$filename"
done

echo ""
echo "=== Converting to N-Quads ==="
> "$DATA_DIR/packagegraph.nq"
python3 -c "
import json
for fname, graph_uri in json.load(open('$DATA_DIR/graphs.json')).items():
    print(f'{fname}\t{graph_uri}')
" | while IFS=$'\t' read -r filename graph_uri; do
    if [ ! -f "$DATA_DIR/$filename" ]; then
        echo "  SKIP: $filename not found"
        continue
    fi
    lines=$(wc -l < "$DATA_DIR/$filename")
    echo "  $filename ($lines triples) → <$graph_uri>"
    sed "s| \\.$ | <${graph_uri}> .|" "$DATA_DIR/$filename" >> "$DATA_DIR/packagegraph.nq"
done

total=$(wc -l < "$DATA_DIR/packagegraph.nq")
echo "  Total: $total quads"

echo ""
echo "=== Building QLever index ==="
cat > "$INDEX_DIR/packagegraph.settings.json" << 'SETTINGS'
{
  "ascii-prefixes-only": false,
  "num-triples-per-batch": 5000000,
  "prefixes-external": [],
  "languages-internal": [],
  "locale": {
    "language": "en",
    "country": "US",
    "ignore-punctuation": true
  }
}
SETTINGS

time podman run --rm --entrypoint /bin/bash --network=host \
    -u $(id -u):$(id -g) --userns=keep-id \
    -v "$SPIKE_DIR:/data:Z" -w /data \
    docker.io/adfreiburg/qlever -c \
    "/qlever/qlever-index -i /data/index-prod/packagegraph -s /data/index-prod/packagegraph.settings.json -F nq -f /data/data-full/packagegraph.nq -p true 2>&1"

echo ""
echo "=== Index size ==="
du -sh "$INDEX_DIR"

echo ""
echo "=== Starting server ==="
podman stop qlever-spike 2>/dev/null; podman rm qlever-spike 2>/dev/null
podman run -d --name qlever-spike --entrypoint /bin/bash --network=host \
    -u $(id -u):$(id -g) --userns=keep-id \
    -v "$SPIKE_DIR:/data:Z" -w /data \
    docker.io/adfreiburg/qlever -c \
    "/qlever/qlever-server -i /data/index-prod/packagegraph -j 8 -p 7001 -m 4G -c 2G -e 1G -k 200 -s 300s -a spike-token 2>&1 | tee /data/server-prod.log"

sleep 3
echo ""
echo "=== Quick verification ==="
curl -s -X POST "http://localhost:7001/" \
    --data-urlencode "query=SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }" \
    -H "Accept: text/tab-separated-values"

echo ""
curl -s -X POST "http://localhost:7001/" \
    --data-urlencode "query=SELECT ?g (COUNT(*) AS ?c) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ORDER BY DESC(?c)" \
    -H "Accept: text/tab-separated-values"
