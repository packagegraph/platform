#!/bin/sh
# Collector: flatpak
# Ported from deploy/overlays/dev/jobs/collect-flatpak.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/flatpak"

pg-collect flatpak -o /tmp/flatpak.nt
/app/scripts/upload-nt.sh /tmp/flatpak.nt "$GRAPH_URI"
