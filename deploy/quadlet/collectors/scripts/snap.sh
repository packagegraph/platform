#!/bin/sh
# Collector: snap
# Ported from deploy/overlays/dev/jobs/collect-snap.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/snap"

pg-collect snap -o /tmp/snap.nt
/app/scripts/upload-nt.sh /tmp/snap.nt "$GRAPH_URI"
