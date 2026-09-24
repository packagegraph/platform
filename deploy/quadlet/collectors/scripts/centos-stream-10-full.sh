#!/bin/sh
# Collector: centos-stream-10-full
# New 2026-09-10 -- modeled on centos-stream-9-full.sh; both x86_64 and
# aarch64 confirmed live under 10-stream before writing this. Fills in
# the "centos-stream/10" graph URI, which previously held only 9 stray
# triples with no collector actually producing them.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-centos-stream-10-full.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/centos-stream/10"

CACHE_DIR=/tmp/cache/centos-stream-10-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/centos-stream-10-full"
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
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true; rm -rf "$RUN_DIR"' EXIT

pg-collect rpm-full \
  --url https://mirror.stream.centos.org/10-stream/BaseOS/x86_64/os/ \
  --url https://mirror.stream.centos.org/10-stream/BaseOS/aarch64/os/ \
  --distro centos-stream --release 10 \
  --with-spec --with-maintainers \
  --cache-dir "${CACHE_DIR}" \
  -o "$RUN_DIR/centos-stream-10.nt"

/app/scripts/upload-nt.sh "$RUN_DIR/centos-stream-10.nt" "$GRAPH_URI"

# Only after a successful publication: retire this run's checkpoint
# generation so the next scheduled run starts fresh. `set -e` means a
# failed collect or upload never reaches this line, leaving the run
# resumable.
pg-collect checkpoint commit --cache-dir "${CACHE_DIR}"

echo "Syncing cache to Minio..."
mc mirror --overwrite --exclude 'output/*' "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"
