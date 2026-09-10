#!/bin/sh
# Collector: freebsd
# Ported from deploy/overlays/dev/jobs/collect-freebsd.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/freebsd/14"

pg-collect freebsd --release 14 --arch amd64 -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
