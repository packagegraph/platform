#!/bin/sh
# Collector: gentoo
# Ported from deploy/overlays/dev/jobs/collect-gentoo.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/gentoo"

export PYTHONUNBUFFERED=1
echo "=== Downloading Gentoo repo tarball ==="
curl -fsSL https://github.com/gentoo/gentoo/archive/refs/heads/master.tar.gz -o /tmp/gentoo.tar.gz
mkdir -p /tmp/gentoo-repo
tar -xzf /tmp/gentoo.tar.gz -C /tmp/gentoo-repo --strip-components=1
rm /tmp/gentoo.tar.gz

pg-collect gentoo --repo-path /tmp/gentoo-repo --output /tmp/gentoo.nt
/app/scripts/upload-nt.sh /tmp/gentoo.nt "$GRAPH_URI"
