#!/bin/sh
# Collector: npm
# Ported from deploy/overlays/dev/jobs/collect-npm.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-npm.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/npm"

pg-collect npm --endpoint "$FUSEKI_ENDPOINT" -o "$RUN_DIR/npm.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/npm.nt" "$GRAPH_URI"
