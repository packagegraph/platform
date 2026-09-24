#!/bin/sh
# Collector: flatpak
# Ported from deploy/overlays/dev/jobs/collect-flatpak.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-flatpak.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/flatpak"

pg-collect flatpak -o "$RUN_DIR/flatpak.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/flatpak.nt" "$GRAPH_URI"
