#!/bin/sh
# Collector: homebrew
# Ported from deploy/overlays/dev/jobs/collect-homebrew.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/homebrew"

pg-collect homebrew -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
