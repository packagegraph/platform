#!/bin/sh
# Collector: alpine-v320-aarch64
# Ported from deploy/overlays/dev/jobs/collect-alpine-v320-aarch64.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/alpine/v3.20/aarch64"

pg-collect alpine --mirror https://dl-cdn.alpinelinux.org/alpine --branch v3.20 --repo main --repo community --arch aarch64 -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
