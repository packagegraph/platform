#!/bin/sh
# Collector: debian-trixie-arm64
# Ported from deploy/overlays/dev/jobs/collect-debian-trixie-arm64.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-debian-trixie-arm64.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/debian/trixie/arm64"

pg-collect debian --repo "http://deb.debian.org/debian" --dist trixie --component main --arch binary-arm64 -o "$RUN_DIR/packages.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/packages.nt" "$GRAPH_URI" "http://deb.debian.org/debian"
