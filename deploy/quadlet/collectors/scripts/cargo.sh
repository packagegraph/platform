#!/bin/sh
# Collector: cargo
# Ported from deploy/overlays/dev/jobs/collect-cargo.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/cargo"

pg-collect cargo --endpoint "$FUSEKI_ENDPOINT" -o /tmp/cargo.nt
/app/scripts/upload-nt.sh /tmp/cargo.nt "$GRAPH_URI"
