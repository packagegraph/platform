#!/bin/sh
# Collector: ubuntu-noble
# Ported from deploy/overlays/dev/jobs/collect-ubuntu-noble.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/ubuntu/noble"

pg-collect debian --repo "http://archive.ubuntu.com/ubuntu" --dist noble --component main --arch binary-amd64 -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI" "http://archive.ubuntu.com/ubuntu"
