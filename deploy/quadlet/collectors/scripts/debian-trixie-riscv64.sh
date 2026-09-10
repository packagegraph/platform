#!/bin/sh
# Collector: debian-trixie-riscv64
# riscv64 is a full release architecture in trixie's main archive (unlike
# sid, where it's still ports-only -- confirmed 2026-09-10:
# https://deb.debian.org/debian/dists/trixie/main/binary-riscv64/Packages.gz
# returns 200, https://deb.debian.org/debian-ports/dists/{trixie,sid}/main/binary-riscv64/Release
# both 404). No Kubernetes CronJob equivalent exists yet for this arch/dist
# combination -- added directly here.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/debian/trixie/riscv64"

pg-collect debian --repo "http://deb.debian.org/debian" --dist trixie --component main --arch binary-riscv64 -o /tmp/packages.nt
/app/scripts/upload-nt.sh /tmp/packages.nt "$GRAPH_URI"
