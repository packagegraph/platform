#!/bin/sh
# Collector: alpine-v320-aarch64
# Ported from deploy/overlays/dev/jobs/collect-alpine-v320-aarch64.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-alpine-v320-aarch64.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/alpine/v3.20/aarch64"

pg-collect alpine --mirror https://dl-cdn.alpinelinux.org/alpine --branch v3.20 --repo main --repo community --arch aarch64 -o "$RUN_DIR/packages.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/packages.nt" "$GRAPH_URI" "https://dl-cdn.alpinelinux.org/alpine"
