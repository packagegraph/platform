#!/bin/sh
# Collector: void
# Ported from deploy/overlays/dev/jobs/collect-void.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-void.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/void"

export PYTHONUNBUFFERED=1
echo "Downloading Void packages repo..."
curl -fsSL https://github.com/void-linux/void-packages/archive/refs/heads/master.tar.gz -o "$RUN_DIR/void.tar.gz"
mkdir -p "$RUN_DIR/void-repo"
tar -xzf "$RUN_DIR/void.tar.gz" -C "$RUN_DIR/void-repo" --strip-components=1
rm "$RUN_DIR/void.tar.gz"

pg-collect void --repo-path "$RUN_DIR/void-repo" --output "$RUN_DIR/void.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/void.nt" "$GRAPH_URI" "https://github.com/void-linux/void-packages/archive/refs/heads/master.tar.gz"
