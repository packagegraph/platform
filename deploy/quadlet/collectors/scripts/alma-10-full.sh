#!/bin/sh
# Collector: alma-10-full
# New 2026-09-10 -- no Kubernetes equivalent. Fills in the "almalinux/10"
# graph URI, which derive_comparison.rs's RHEL-rebuild drift analysis
# already expects to exist (paired against rhel/10) but nothing populated.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/almalinux/10"

mkdir -p /tmp/collection
CACHE_DIR=/tmp/cache/alma-10-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/alma-10-full"
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
  --url https://repo.almalinux.org/almalinux/10/BaseOS/x86_64/os/ \
  --url https://repo.almalinux.org/almalinux/10/BaseOS/aarch64/os/ \
  --distro almalinux --release 10 \
  --with-spec --with-maintainers \
  --cache-dir "${CACHE_DIR}" \
  -o /tmp/collection/alma-10.nt

/app/scripts/upload-nt.sh /tmp/collection/alma-10.nt "$GRAPH_URI"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"
