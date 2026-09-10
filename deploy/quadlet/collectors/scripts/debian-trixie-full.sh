#!/bin/sh
# Collector: debian-trixie-full
# Ported from deploy/overlays/dev/jobs/collect-debian-trixie-full.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/debian/trixie"

mkdir -p /tmp/collection
CACHE_DIR=/tmp/cache/debian-trixie-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/debian-trixie-full"
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

pg-collect deb-full \
  --repo http://deb.debian.org/debian \
  --dist trixie --component main \
  --arch amd64 --arch arm64 \
  --distro debian \
  --with-sources --with-builddeps --with-maintainers --with-salsa \
  --cache-dir "${CACHE_DIR}" \
  -o /tmp/collection/debian-trixie.nt

/app/scripts/upload-nt.sh /tmp/collection/debian-trixie.nt "$GRAPH_URI"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"

