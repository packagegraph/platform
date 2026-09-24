#!/bin/sh
# Collector: rubygems
# Ported from deploy/overlays/dev/jobs/collect-rubygems.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-rubygems.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/rubygems"

pg-collect rubygems --endpoint "$FUSEKI_ENDPOINT" -o "$RUN_DIR/rubygems.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/rubygems.nt" "$GRAPH_URI"
