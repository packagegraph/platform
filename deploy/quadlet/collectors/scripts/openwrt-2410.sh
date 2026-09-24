#!/bin/sh
# Collector: openwrt-2410
# Ported from deploy/overlays/dev/jobs/collect-openwrt-2410.yaml.
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-openwrt-2410.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/openwrt/24.10/mips_24kc"

mkdir -p "$RUN_DIR/feeds"
CACHE_DIR=/tmp/cache/openwrt-attestation
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/openwrt-2410-full"
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
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true; rm -rf "$RUN_DIR"' EXIT

for feed in packages luci routing telephony; do
  echo "Fetching openwrt/${feed} @ openwrt-24.10..."
  curl -fsSL "https://github.com/openwrt/${feed}/archive/refs/heads/openwrt-24.10.tar.gz" -o "$RUN_DIR/${feed}.tar.gz"
  mkdir -p "$RUN_DIR/feeds/${feed}"
  tar -xzf "$RUN_DIR/${feed}.tar.gz" -C "$RUN_DIR/feeds/${feed}" --strip-components=1
  rm "$RUN_DIR/${feed}.tar.gz"
done

# Note: --with-attestation omitted -- OpenWrt does not publish GitHub
# attestations yet (verified 2026-04-27, see collect-openwrt-2410-full.yaml).
pg-collect openwrt-full \
  --feed "$RUN_DIR/feeds/packages" --feed "$RUN_DIR/feeds/luci" --feed "$RUN_DIR/feeds/routing" --feed "$RUN_DIR/feeds/telephony" \
  --distro openwrt --release 24.10 \
  --release-url https://downloads.openwrt.org/releases/24.10.0 \
  --arch mips_24kc \
  --with-upstream \
  --cache-dir "${CACHE_DIR}" \
  --minio-endpoint "${MINIO_ENDPOINT}" \
  --minio-bucket "${MINIO_BUCKET}" \
  --minio-access-key "${MINIO_ACCESS_KEY}" \
  --minio-secret-key "${MINIO_SECRET_KEY}" \
  -o "$RUN_DIR/openwrt-2410.nt"

/app/scripts/upload-nt.sh "$RUN_DIR/openwrt-2410.nt" "$GRAPH_URI"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"
