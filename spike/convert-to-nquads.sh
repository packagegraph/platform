#!/bin/bash
# Convert N-Triples files to N-Quads by appending a graph URI to each triple.
# Usage: ./convert-to-nquads.sh <input.nt> <graph-uri> >> output.nq
#
# N-Triples format: <s> <p> <o> .
# N-Quads format:   <s> <p> <o> <graph> .

set -euo pipefail

INPUT="$1"
GRAPH_URI="$2"

if [ ! -f "$INPUT" ]; then
    echo "Error: $INPUT not found" >&2
    exit 1
fi

# Replace trailing " ." with " <graph-uri> ." on each line
sed "s| \\.$ | <${GRAPH_URI}> .|" "$INPUT"
