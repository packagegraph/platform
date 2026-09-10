#!/bin/sh
# Collector: conda
# Ported from deploy/overlays/dev/jobs/collect-conda.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/conda-forge"

export PYTHONUNBUFFERED=1
pg-collect conda --channel-url https://conda.anaconda.org/conda-forge --subdir linux-64 --output /tmp/conda.nt
/app/scripts/upload-nt.sh /tmp/conda.nt "$GRAPH_URI"
