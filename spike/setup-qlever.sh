#!/bin/bash
# Set up QLever index for PackageGraph spike
# Run on berstuk: bash ~/qlever-spike/setup-qlever.sh
set -euo pipefail

SPIKE_DIR=~/qlever-spike
DATA_DIR="$SPIKE_DIR/data"
INDEX_DIR="$SPIKE_DIR/index"

cd "$SPIKE_DIR"

echo "=== Converting N-Triples to N-Quads ==="

# Graph URI mapping (matches PackageGraph convention)
declare -A GRAPHS
GRAPHS[fedora-43]="https://packagegraph.github.io/graph/fedora/43"
GRAPHS[debian-trixie]="https://packagegraph.github.io/graph/debian/trixie"
GRAPHS[centos-stream-9]="https://packagegraph.github.io/graph/centos-stream/9"

# Convert each .nt to .nq and concatenate
> "$DATA_DIR/packagegraph.nq"
for base in "${!GRAPHS[@]}"; do
    nt_file="$DATA_DIR/${base}.nt"
    graph_uri="${GRAPHS[$base]}"
    if [ -f "$nt_file" ]; then
        lines=$(wc -l < "$nt_file")
        echo "  $base: $lines triples → <$graph_uri>"
        sed "s| \\.\$| <${graph_uri}> .|" "$nt_file" >> "$DATA_DIR/packagegraph.nq"
    else
        echo "  SKIP: $nt_file not found"
    fi
done

total=$(wc -l < "$DATA_DIR/packagegraph.nq")
echo "  Total: $total quads in packagegraph.nq"

echo ""
echo "=== Writing QLever settings ==="

cat > "$INDEX_DIR/packagegraph.settings.json" << 'SETTINGS'
{
  "ascii-prefixes-only": false,
  "num-triples-per-batch": 1000000,
  "prefixes-external": [],
  "languages-internal": [],
  "locale": {
    "language": "en",
    "country": "US",
    "ignore-punctuation": true
  }
}
SETTINGS

echo "Settings written to $INDEX_DIR/packagegraph.settings.json"
echo ""
echo "=== Ready to build index ==="
echo "Next step: run the podman container to build the index"
echo ""
echo "  podman run --rm -u \$(id -u):\$(id -g) --userns=keep-id \\"
echo "    -v $SPIKE_DIR:/data -w /data \\"
echo "    docker.io/adfreiburg/qlever -c \\"
echo "    'IndexBuilderMain -i /data/index/packagegraph -s /data/index/packagegraph.settings.json -F nq -f /data/data/packagegraph.nq'"
