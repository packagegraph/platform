#!/bin/sh
# Collector: conda
# Ported from deploy/overlays/dev/jobs/collect-conda.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-conda.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/conda-forge"

export PYTHONUNBUFFERED=1
pg-collect conda --channel-url https://conda.anaconda.org/conda-forge --subdir linux-64 --output "$RUN_DIR/conda.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/conda.nt" "$GRAPH_URI" "https://conda.anaconda.org/conda-forge"
