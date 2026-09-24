#!/bin/sh
# Collector: gentoo
# Ported from deploy/overlays/dev/jobs/collect-gentoo.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-gentoo.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/gentoo"

export PYTHONUNBUFFERED=1
echo "=== Downloading Gentoo repo tarball ==="
curl -fsSL https://github.com/gentoo/gentoo/archive/refs/heads/master.tar.gz -o "$RUN_DIR/gentoo.tar.gz"
mkdir -p "$RUN_DIR/gentoo-repo"
tar -xzf "$RUN_DIR/gentoo.tar.gz" -C "$RUN_DIR/gentoo-repo" --strip-components=1
rm "$RUN_DIR/gentoo.tar.gz"

pg-collect gentoo --repo-path "$RUN_DIR/gentoo-repo" --output "$RUN_DIR/gentoo.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/gentoo.nt" "$GRAPH_URI" "https://github.com/gentoo/gentoo/archive/refs/heads/master.tar.gz"
