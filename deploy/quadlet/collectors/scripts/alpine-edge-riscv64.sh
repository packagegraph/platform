#!/bin/sh
# Collector: alpine-edge-riscv64
# Ported from deploy/overlays/dev/jobs/collect-alpine-edge-riscv64.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/alpine/edge/riscv64"

pg-collect alpine --mirror https://dl-cdn.alpinelinux.org/alpine --branch edge --repo main --repo community --arch riscv64 -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
