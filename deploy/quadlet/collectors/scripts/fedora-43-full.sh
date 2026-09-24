#!/bin/sh
# Collector: fedora-43-full
# Ported from deploy/overlays/dev/jobs/collect-fedora-43-full.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-fedora-43-full.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/fedora/43"

CACHE_DIR=/tmp/cache/fedora-43-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/fedora-43-full"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

echo "Syncing cache from Minio..."
mc mirror --overwrite --exclude 'output/*' "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true
echo "Cache warmed: $(find "${CACHE_DIR}" -type f 2>/dev/null | wc -l) entries"

# Periodically flush the cache back to Minio while the (long-running,
# network-bound) collect runs, so a timeout kill doesn't discard hours
# of freshly-cached fetches -- previously the save-back only ran after
# full success, so the 2026-09-10 timeouts never left the next run any
# warmer than the last.
( while sleep 300; do
    mc mirror --overwrite --exclude 'output/*' "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
  done ) &
CACHE_SYNC_PID=$!
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true; rm -rf "$RUN_DIR"' EXIT

pg-collect rpm-full \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/43/Everything/x86_64/os/ \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/43/Everything/aarch64/os/ \
  --distro fedora --release 43 \
  --with-koji --koji-hub https://koji.fedoraproject.org/kojihub \
  --with-spec --with-maintainers \
  --cache-dir "${CACHE_DIR}" \
  -o "$RUN_DIR/fedora-43.nt"

/app/scripts/upload-nt.sh "$RUN_DIR/fedora-43.nt" "$GRAPH_URI"

# Only after a successful publication: retire this run's checkpoint
# generation so the next scheduled run starts fresh. `set -e` means a
# failed collect or upload never reaches this line, leaving the run
# resumable.
pg-collect checkpoint commit --cache-dir "${CACHE_DIR}"

echo "Syncing cache to Minio..."
mc mirror --overwrite --exclude 'output/*' "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"

