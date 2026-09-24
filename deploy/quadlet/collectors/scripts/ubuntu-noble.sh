#!/bin/sh
# Collector: ubuntu-noble
# Ported from deploy/overlays/dev/jobs/collect-ubuntu-noble.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-ubuntu-noble.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/ubuntu/noble"

pg-collect debian --repo "http://archive.ubuntu.com/ubuntu" --dist noble --component main --arch binary-amd64 -o "$RUN_DIR/packages.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/packages.nt" "$GRAPH_URI" "http://archive.ubuntu.com/ubuntu"
