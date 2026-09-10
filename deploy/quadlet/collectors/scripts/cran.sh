#!/bin/sh
# Collector: cran
# Ported from deploy/overlays/dev/jobs/collect-cran.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/cran"

pg-collect cran -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
