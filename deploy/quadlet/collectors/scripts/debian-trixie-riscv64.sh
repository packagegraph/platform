#!/bin/sh
# Collector: debian-trixie-riscv64
# riscv64 is a full release architecture in trixie's main archive (unlike
# sid, where it's still ports-only -- confirmed 2026-09-10:
# https://deb.debian.org/debian/dists/trixie/main/binary-riscv64/Packages.gz
# returns 200, https://deb.debian.org/debian-ports/dists/{trixie,sid}/main/binary-riscv64/Release
# both 404). No Kubernetes CronJob equivalent exists yet for this arch/dist
# combination -- added directly here.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-debian-trixie-riscv64.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/debian/trixie/riscv64"

pg-collect debian --repo "http://deb.debian.org/debian" --dist trixie --component main --arch binary-riscv64 -o "$RUN_DIR/packages.nt"
/app/scripts/upload-nt.sh "$RUN_DIR/packages.nt" "$GRAPH_URI" "http://deb.debian.org/debian"
