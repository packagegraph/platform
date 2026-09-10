#!/bin/sh
# Collector: npm
# Ported from deploy/overlays/dev/jobs/collect-npm.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/npm"

pg-collect npm --endpoint "$FUSEKI_ENDPOINT" -o /tmp/npm.nt
/app/scripts/upload-nt.sh /tmp/npm.nt "$GRAPH_URI"
