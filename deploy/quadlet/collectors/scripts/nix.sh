#!/bin/sh
# Collector: nix
# Ported from deploy/overlays/dev/jobs/collect-nix.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/nix/nixpkgs"

pg-collect nix -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
