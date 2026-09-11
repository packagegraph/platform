#!/bin/sh
# Collector: ubuntu-noble-arm64
# Ported from deploy/overlays/dev/jobs/collect-ubuntu-noble-arm64.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/ubuntu/noble/arm64"

pg-collect debian --repo "http://ports.ubuntu.com/ubuntu-ports" --dist noble --component main --arch binary-arm64 -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI" "http://ports.ubuntu.com/ubuntu-ports"
