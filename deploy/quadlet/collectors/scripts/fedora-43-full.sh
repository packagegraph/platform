#!/bin/sh
# Collector: fedora-43-full
# Ported from deploy/overlays/dev/jobs/collect-fedora-43-full.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/fedora/43"

mkdir -p /tmp/collection
CACHE_DIR=/tmp/cache/fedora-43-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/fedora-43-full"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

echo "Syncing cache from Minio..."
mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true
echo "Cache warmed: $(find "${CACHE_DIR}" -type f 2>/dev/null | wc -l) entries"

# Periodically flush the cache back to Minio while the (long-running,
# network-bound) collect runs, so a timeout kill doesn't discard hours
# of freshly-cached fetches -- previously the save-back only ran after
# full success, so the 2026-09-10 timeouts never left the next run any
# warmer than the last.
( while sleep 300; do
    mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
  done ) &
CACHE_SYNC_PID=$!
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true' EXIT

pg-collect rpm-full \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/43/Everything/x86_64/os/ \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/43/Everything/aarch64/os/ \
  --distro fedora --release 43 \
  --with-koji --koji-hub https://koji.fedoraproject.org/kojihub \
  --with-spec --with-maintainers \
  --cache-dir "${CACHE_DIR}" \
  -o /tmp/collection/fedora-43.nt

/app/scripts/upload-nt.sh /tmp/collection/fedora-43.nt "$GRAPH_URI"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"

