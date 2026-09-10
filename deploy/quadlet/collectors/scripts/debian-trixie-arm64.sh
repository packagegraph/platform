#!/bin/sh
# Collector: debian-trixie-arm64
# Ported from deploy/overlays/dev/jobs/collect-debian-trixie-arm64.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/debian/trixie/arm64"

pg-collect debian --repo "http://deb.debian.org/debian" --dist trixie --component main --arch binary-arm64 -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
