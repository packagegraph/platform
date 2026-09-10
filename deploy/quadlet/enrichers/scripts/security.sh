#!/bin/sh
# Enricher: security
# No Kubernetes equivalent -- never scheduled anywhere before this. Loops
# all ecosystems EnrichSecurity supports into one shared graph.
#
# All ecosystems accumulate into ONE local file before a SINGLE
# upload-nt.sh call at the end -- see collectors/scripts/osv.sh's comment
# for why calling upload-nt.sh once per ecosystem with the same GRAPH_URI
# would silently overwrite all but the last ecosystem's data.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/security"
COMBINED=/tmp/enrichment-security-combined.nt
mkdir -p /tmp/enrichment
: > "$COMBINED"

for ECO in deb apk rpm npm pypi cargo gomod maven debian alpine fedora; do
  echo "=== Security enrichment: $ECO ==="
  ENRICH_OK=0
  pg-collect enrich-security \
    --endpoint "$FUSEKI_ENDPOINT" \
    --ecosystem "$ECO" \
    -o "/tmp/enrichment/security-${ECO}.nt" || ENRICH_OK=$?

  if [ "$ENRICH_OK" -ne 0 ]; then
    echo "WARNING: enrich-security $ECO failed (exit $ENRICH_OK) — skipping this ecosystem"
  elif [ -f "/tmp/enrichment/security-${ECO}.nt" ]; then
    cat "/tmp/enrichment/security-${ECO}.nt" >> "$COMBINED"
  fi
  rm -f "/tmp/enrichment/security-${ECO}.nt"
done

/app/scripts/upload-nt.sh "$COMBINED" "$GRAPH_URI"
