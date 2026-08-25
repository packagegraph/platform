#!/bin/bash
set -euo pipefail

DATA_DIR=~/qlever-spike/data-full
NQ_FILE="$DATA_DIR/packagegraph.nq"

echo "=== Reconverting .nt to .nq with graph URIs ==="
> "$NQ_FILE"

python3 -c "
import json
for fname, graph_uri in json.load(open('$DATA_DIR/graphs.json')).items():
    print(f'{fname}\t{graph_uri}')
" | while IFS=$'\t' read -r filename graph_uri; do
    nt_file="$DATA_DIR/$filename"
    if [ ! -f "$nt_file" ]; then
        echo "  SKIP: $filename"
        continue
    fi
    lines=$(wc -l < "$nt_file")
    echo "  $filename ($lines) → $graph_uri"
    sed "s| \.\$| <${graph_uri}> .|" "$nt_file" >> "$NQ_FILE"
done

total=$(wc -l < "$NQ_FILE")
echo ""
echo "Total: $total quads"
echo "Sample:"
head -1 "$NQ_FILE"
echo "..."
tail -1 "$NQ_FILE"
