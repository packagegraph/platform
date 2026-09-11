#!/bin/sh
# Collector: void
# Ported from deploy/overlays/dev/jobs/collect-void.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/void"

export PYTHONUNBUFFERED=1
echo "Downloading Void packages repo..."
curl -fsSL https://github.com/void-linux/void-packages/archive/refs/heads/master.tar.gz -o /tmp/void.tar.gz
mkdir -p /tmp/void-repo
tar -xzf /tmp/void.tar.gz -C /tmp/void-repo --strip-components=1
rm /tmp/void.tar.gz

pg-collect void --repo-path /tmp/void-repo --output /tmp/void.nt
/app/scripts/upload-nt.sh /tmp/void.nt "$GRAPH_URI" "https://github.com/void-linux/void-packages/archive/refs/heads/master.tar.gz"
