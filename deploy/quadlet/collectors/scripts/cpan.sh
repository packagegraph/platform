#!/bin/sh
# Collector: cpan
# Ported from deploy/overlays/dev/jobs/collect-cpan.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/cpan"

pg-collect cpan --endpoint "$FUSEKI_ENDPOINT" -o /tmp/cpan.nt
/app/scripts/upload-nt.sh /tmp/cpan.nt "$GRAPH_URI"
