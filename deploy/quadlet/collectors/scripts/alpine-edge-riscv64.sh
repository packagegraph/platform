#!/bin/sh
# Collector: alpine-edge-riscv64
# Ported from deploy/overlays/dev/jobs/collect-alpine-edge-riscv64.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-alpine-edge-riscv64.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/alpine/edge/riscv64"

pg-collect alpine --mirror https://dl-cdn.alpinelinux.org/alpine --branch edge --repo main --repo community --arch riscv64 -o "$RUN_DIR/packages.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/packages.nt" "$GRAPH_URI" "https://dl-cdn.alpinelinux.org/alpine"
