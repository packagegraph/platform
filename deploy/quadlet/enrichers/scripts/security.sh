#!/bin/sh
# Enricher: security
# No Kubernetes equivalent -- never scheduled anywhere before this. Loops
# all ecosystems EnrichSecurity supports into one shared graph.
#
# All ecosystems accumulate into ONE local file before a SINGLE
# upload-nt.sh call at the end -- see collectors/scripts/osv.sh's comment
# for why calling upload-nt.sh once per ecosystem with the same GRAPH_URI
# would silently overwrite all but the last ecosystem's data.
#
# ONE cache directory for all eleven ecosystems, not one each. What gets
# cached is an OSV vulnerability record keyed by its own ID, and that
# record says nothing about who asked for it: a CVE reached through `deb`
# is byte-identical to the same CVE reached through `rpm`. Sharing the
# namespace means each advisory is fetched once per run at most, and the
# next run starts warm (#59).
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/security"
COMBINED=/tmp/enrichment-security-combined.nt
CACHE_DIR=/tmp/cache/security
MINIO_CACHE="pgraph/${MINIO_BUCKET}/enricher-cache/security"
mkdir -p /tmp/enrichment
: > "$COMBINED"

mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

echo "Syncing cache from Minio..."
mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true
echo "Cache warmed: $(find "${CACHE_DIR}" -name '*.json' 2>/dev/null | wc -l) entries"

# Flush the cache back mid-run for the same reason repology.sh does: if
# the unit is killed on timeout, the advisories fetched so far are still
# worth something to the next run.
( while sleep 300; do
    mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
  done ) &
CACHE_SYNC_PID=$!
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true' EXIT

for ECO in deb apk rpm npm pypi cargo gomod maven debian alpine fedora; do
  echo "=== Security enrichment: $ECO ==="
  ENRICH_OK=0
  pg-collect enrich-security \
    --endpoint "$FUSEKI_ENDPOINT" \
    --ecosystem "$ECO" \
    -o "/tmp/enrichment/security-${ECO}.nt" \
    --cache-dir "${CACHE_DIR}" || ENRICH_OK=$?

  if [ "$ENRICH_OK" -ne 0 ]; then
    echo "WARNING: enrich-security $ECO failed (exit $ENRICH_OK) — skipping this ecosystem"
  elif [ -f "/tmp/enrichment/security-${ECO}.nt" ]; then
    cat "/tmp/enrichment/security-${ECO}.nt" >> "$COMBINED"
  fi
  rm -f "/tmp/enrichment/security-${ECO}.nt"
done

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true

/app/scripts/upload-nt.sh "$COMBINED" "$GRAPH_URI"
