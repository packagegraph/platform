#!/bin/sh
# Collector: rocky-9-full
# New 2026-09-10 -- no Kubernetes equivalent. Fills in the "rocky/9"
# graph URI, which derive_comparison.rs's RHEL-rebuild drift analysis
# already expects to exist (paired against rhel/9) but nothing populated.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/rocky/9"

mkdir -p /tmp/collection
CACHE_DIR=/tmp/cache/rocky-9-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/rocky-9-full"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

echo "Syncing cache from Minio..."
mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true
echo "Cache warmed: $(find "${CACHE_DIR}" -type f 2>/dev/null | wc -l) entries"

# Periodically flush the cache back to Minio while the (long-running,
# network-bound) collect runs, so a timeout kill doesn't discard hours
# of freshly-cached fetches -- see fedora-43-full.sh's 2026-09-10
# timeout incident for why this matters.
( while sleep 300; do
    mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
  done ) &
CACHE_SYNC_PID=$!
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true' EXIT

pg-collect rpm-full \
  --url https://download.rockylinux.org/pub/rocky/9/BaseOS/x86_64/os/ \
  --url https://download.rockylinux.org/pub/rocky/9/BaseOS/aarch64/os/ \
  --distro rocky --release 9 \
  --with-spec --with-maintainers \
  --cache-dir "${CACHE_DIR}" \
  -o /tmp/collection/rocky-9.nt

/app/scripts/upload-nt.sh /tmp/collection/rocky-9.nt "$GRAPH_URI"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"
