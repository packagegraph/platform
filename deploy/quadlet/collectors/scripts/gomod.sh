#!/bin/sh
# Collector: gomod
# Ported from deploy/overlays/dev/jobs/collect-gomod.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/gomod"

pg-collect gomod --endpoint "$FUSEKI_ENDPOINT" -o /tmp/gomod.nt
/app/scripts/upload-nt.sh /tmp/gomod.nt "$GRAPH_URI"
