#!/bin/sh
# Collector: openwrt-2410
# Ported from deploy/overlays/dev/jobs/collect-openwrt-2410.yaml.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/openwrt/24.10/mips_24kc"

mkdir -p /tmp/collection /tmp/feeds
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
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true' EXIT

for feed in packages luci routing telephony; do
  echo "Fetching openwrt/${feed} @ openwrt-24.10..."
  curl -fsSL "https://github.com/openwrt/${feed}/archive/refs/heads/openwrt-24.10.tar.gz" -o "/tmp/${feed}.tar.gz"
  mkdir -p "/tmp/feeds/${feed}"
  tar -xzf "/tmp/${feed}.tar.gz" -C "/tmp/feeds/${feed}" --strip-components=1
  rm "/tmp/${feed}.tar.gz"
done

# Note: --with-attestation omitted -- OpenWrt does not publish GitHub
# attestations yet (verified 2026-04-27, see collect-openwrt-2410-full.yaml).
pg-collect openwrt-full \
  --feed /tmp/feeds/packages --feed /tmp/feeds/luci --feed /tmp/feeds/routing --feed /tmp/feeds/telephony \
  --distro openwrt --release 24.10 \
  --release-url https://downloads.openwrt.org/releases/24.10.0 \
  --arch mips_24kc \
  --with-upstream \
  --cache-dir "${CACHE_DIR}" \
  --minio-endpoint "${MINIO_ENDPOINT}" \
  --minio-bucket "${MINIO_BUCKET}" \
  --minio-access-key "${MINIO_ACCESS_KEY}" \
  --minio-secret-key "${MINIO_SECRET_KEY}" \
  -o /tmp/collection/openwrt-2410.nt

/app/scripts/upload-nt.sh /tmp/collection/openwrt-2410.nt "$GRAPH_URI"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"
