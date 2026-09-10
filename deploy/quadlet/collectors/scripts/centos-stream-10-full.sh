#!/bin/sh
# Collector: centos-stream-10-full
# New 2026-09-10 -- modeled on centos-stream-9-full.sh; both x86_64 and
# aarch64 confirmed live under 10-stream before writing this. Fills in
# the "centos-stream/10" graph URI, which previously held only 9 stray
# triples with no collector actually producing them.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/centos-stream/10"

mkdir -p /tmp/collection
CACHE_DIR=/tmp/cache/centos-stream-10-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/centos-stream-10-full"
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
  --url https://mirror.stream.centos.org/10-stream/BaseOS/x86_64/os/ \
  --url https://mirror.stream.centos.org/10-stream/BaseOS/aarch64/os/ \
  --distro centos-stream --release 10 \
  --with-spec --with-maintainers \
  --cache-dir "${CACHE_DIR}" \
  -o /tmp/collection/centos-stream-10.nt

/app/scripts/upload-nt.sh /tmp/collection/centos-stream-10.nt "$GRAPH_URI"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"
