#!/bin/sh
# Collector: rubygems
# Ported from deploy/overlays/dev/jobs/collect-rubygems.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/rubygems"

pg-collect rubygems --endpoint "$FUSEKI_ENDPOINT" -o /tmp/rubygems.nt
/app/scripts/upload-nt.sh /tmp/rubygems.nt "$GRAPH_URI"
