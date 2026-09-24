#!/bin/sh
# Collector: archarm-aarch64
# Ported from deploy/overlays/dev/jobs/collect-archarm-aarch64.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-archarm-aarch64.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/archarm/aarch64"

pg-collect arch --mirror http://fl.us.mirror.archlinuxarm.org/aarch64 --repo core --repo extra -o "$RUN_DIR/packages.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/packages.nt" "$GRAPH_URI" "http://fl.us.mirror.archlinuxarm.org/aarch64"
