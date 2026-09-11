#!/bin/sh
# Collector: rhel-9-full
# New 2026-09-11 -- no Kubernetes equivalent. Fills in the "rhel/9" graph
# URI, which derive_comparison.rs's RHEL-rebuild drift analysis already
# expects to exist (as the comparison target for almalinux/9, rocky/9)
# but nothing populated it. See docs/rhel-collection.md for the
# underlying TLS-cert setup this script drives through pg-collect's
# --sslclientcert/--sslclientkey/--sslcacert flags (rpm-full).
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/rhel/9"

# Entitlement cert filename embeds a serial number that rotates on
# renewal -- glob for it rather than hardcode today's serial.
CLIENT_CERT=$(ls /etc/pki/entitlement/[0-9]*.pem | grep -v -- '-key.pem$' | head -1)
CLIENT_KEY="${CLIENT_CERT%.pem}-key.pem"
CA_CERT=/etc/rhsm/ca/redhat-uep.pem

mkdir -p /tmp/collection
CACHE_DIR=/tmp/cache/rhel-9-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/rhel-9-full"
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
  --url https://cdn.redhat.com/content/dist/rhel9/9/x86_64/baseos/os/ \
  --url https://cdn.redhat.com/content/dist/rhel9/9/aarch64/baseos/os/ \
  --distro rhel --release 9 \
  --sslclientcert "$CLIENT_CERT" --sslclientkey "$CLIENT_KEY" --sslcacert "$CA_CERT" \
  --with-spec --with-maintainers \
  --cache-dir "${CACHE_DIR}" \
  -o /tmp/collection/rhel-9.nt

/app/scripts/upload-nt.sh /tmp/collection/rhel-9.nt "$GRAPH_URI"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

echo "Collection complete"
