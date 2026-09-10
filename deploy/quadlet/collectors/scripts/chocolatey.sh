#!/bin/sh
# Collector: chocolatey
# Ported from deploy/overlays/dev/jobs/collect-chocolatey.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/chocolatey"

pg-collect chocolatey -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
