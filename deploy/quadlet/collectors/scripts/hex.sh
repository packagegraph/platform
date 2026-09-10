#!/bin/sh
# Collector: hex
# Ported from deploy/overlays/dev/jobs/collect-hex.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/hex"

pg-collect hex --endpoint "$FUSEKI_ENDPOINT" -o /tmp/hex.nt
/app/scripts/upload-nt.sh /tmp/hex.nt "$GRAPH_URI"
