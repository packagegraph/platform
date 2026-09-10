#!/bin/sh
# Collector: nuget
# Ported from deploy/overlays/dev/jobs/collect-nuget.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/nuget"

pg-collect nuget --endpoint "$FUSEKI_ENDPOINT" -o /tmp/nuget.nt
/app/scripts/upload-nt.sh /tmp/nuget.nt "$GRAPH_URI"
