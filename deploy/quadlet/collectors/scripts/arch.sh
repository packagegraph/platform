#!/bin/sh
# Collector: arch
# Ported from deploy/overlays/dev/jobs/collect-arch.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/arch"

pg-collect arch --repo core --repo extra --repo multilib --include-aur -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
