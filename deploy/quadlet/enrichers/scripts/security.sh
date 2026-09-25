#!/bin/sh
# Enricher: security
#
# Links the advisories the OSV collector publishes to the packages this
# corpus actually holds. One graph, one upload, no loop.
#
# It used to loop eleven "ecosystems" through the OSV API, one request per
# package, and could not finish: ~63 hours of paced requests against an 8h
# timeout. Four of those eleven (deb, apk, rpm, fedora) are not OSV
# ecosystem names at all and were answered with HTTP 400 every time; five
# more duplicated archives the OSV COLLECTOR already downloads daily. What
# was left -- which of our packages an advisory touches -- is now the whole
# job, and it reads the same archives instead of the API (#59).
#
# No --cache-dir: there is no per-item fetch left to cache. No --ecosystem:
# the distros covered are the ones collected here, and OSV names them with
# their release, which is a per-graph fact rather than a per-run one.
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/enrichment/security"
mkdir -p /tmp/enrichment

ENRICH_OK=0
pg-collect enrich-security \
  --endpoint "$FUSEKI_ENDPOINT" \
  -o /tmp/enrichment/security.nt || ENRICH_OK=$?

if [ "$ENRICH_OK" -ne 0 ]; then
  echo "WARNING: enrich-security failed (exit $ENRICH_OK) — skipping upload"
elif [ -f /tmp/enrichment/security.nt ]; then
  /app/scripts/upload-nt.sh /tmp/enrichment/security.nt "$GRAPH_URI"
fi
