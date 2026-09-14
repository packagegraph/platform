#!/bin/sh
# Collector: fedora-44-full
# New 2026-09-10 -- no Kubernetes equivalent (Fedora 44 hadn't released
# yet when the k8s jobs were written). Modeled directly on
# fedora-43-full.sh; both x86_64 and aarch64 confirmed live under
# releases/44 before writing this.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/fedora/44"

mkdir -p /tmp/collection
CACHE_DIR=/tmp/cache/fedora-44-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/fedora-44-full"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

echo "Syncing cache from Minio..."
mc mirror --overwrite --exclude 'output/*' "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true
echo "Cache warmed: $(find "${CACHE_DIR}" -type f 2>/dev/null | wc -l) entries"

# Periodically flush the cache back to Minio while the (long-running,
# network-bound) collect runs, so a timeout kill doesn't discard hours
# of freshly-cached fetches -- see fedora-43-full.sh's 2026-09-10
# timeout incident for why this matters.
( while sleep 300; do
    mc mirror --overwrite --exclude 'output/*' "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
  done ) &
CACHE_SYNC_PID=$!
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true' EXIT

pg-collect rpm-full \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/44/Everything/x86_64/os/ \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/44/Everything/aarch64/os/ \
  --distro fedora --release 44 \
  --with-koji --koji-hub https://koji.fedoraproject.org/kojihub \
  --with-spec --with-maintainers \
  --cache-dir "${CACHE_DIR}" \
  -o /tmp/collection/fedora-44.nt

/app/scripts/upload-nt.sh /tmp/collection/fedora-44.nt "$GRAPH_URI"

# Only after a successful publication: retire this run's checkpoint
# generation so the next scheduled run starts fresh. `set -e` means a
# failed collect or upload never reaches this line, leaving the run
# resumable.
pg-collect checkpoint commit --cache-dir "${CACHE_DIR}"

echo "Syncing cache to Minio..."
mc mirror --overwrite --exclude 'output/*' "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"
