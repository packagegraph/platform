#!/bin/sh
# Collector: archarm-aarch64
# Ported from deploy/overlays/dev/jobs/collect-archarm-aarch64.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/archarm/aarch64"

pg-collect arch --mirror http://fl.us.mirror.archlinuxarm.org/aarch64 --repo core --repo extra -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI" "http://fl.us.mirror.archlinuxarm.org/aarch64"
