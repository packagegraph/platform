#!/bin/sh
# Collector: nuget
# Ported from deploy/overlays/dev/jobs/collect-nuget.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-nuget.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/nuget"

pg-collect nuget --endpoint "$FUSEKI_ENDPOINT" -o "$RUN_DIR/nuget.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/nuget.nt" "$GRAPH_URI"
