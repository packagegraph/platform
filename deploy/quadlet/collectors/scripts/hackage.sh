#!/bin/sh
# Collector: hackage
# Ported from deploy/overlays/dev/jobs/collect-hackage.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/hackage"

pg-collect hackage --endpoint "$FUSEKI_ENDPOINT" -o /tmp/hackage.nt
/app/scripts/upload-nt.sh /tmp/hackage.nt "$GRAPH_URI"
