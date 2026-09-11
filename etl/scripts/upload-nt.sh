#!/bin/bash
set -euo pipefail

# Upload an N-Triples file to Minio and register it in the graph manifest.
# Also mints a pkg:DataSnapshot describing this upload, appended directly
# into the file before it's gzipped, so it lands in the same named graph
# as the data it describes.
#
# NOTE: this mutates the local .nt file in place by appending. Running
# this script twice against the same .nt file (or uploading one file to
# two different graphs) accumulates multiple DataSnapshot triples in the
# same local file. No current caller does this -- osv.sh and
# enrichers/scripts/security.sh already concatenate all input before a
# single upload call -- but be aware of it if you add a new caller.
#
# Usage: upload-nt.sh <local-file.nt> <graph-uri> [source-url]
#
# source-url is optional: omit it for collectors with no single canonical
# upstream source (registry/API-based collectors, or multi-mirror
# collectors with several equally-canonical source URLs).
#
# Example:
#   upload-nt.sh /tmp/packages.nt "https://packagegraph.github.io/graph/debian/trixie" "http://deb.debian.org/debian"
#
# Uploads to: pgraph/${MINIO_BUCKET}/nt-output/debian-trixie.nt.gz
# Creates: pgraph/${MINIO_BUCKET}/nt-output/debian-trixie.nt.gz.graph (sidecar)

if [ $# -lt 2 ] || [ $# -gt 3 ]; then
    echo "Usage: upload-nt.sh <local-file.nt> <graph-uri> [source-url]" >&2
    exit 1
fi

LOCAL_FILE="$1"
GRAPH_URI="$2"
SOURCE_URL="${3:-}"

if [ ! -f "$LOCAL_FILE" ]; then
    echo "Error: file not found: $LOCAL_FILE" >&2
    exit 1
fi

# Derive Minio filename from graph URI
# https://packagegraph.github.io/graph/debian/trixie → debian-trixie
# https://packagegraph.github.io/graph/security/osv → security-osv
# https://packagegraph.github.io/ontology            → ontology
GRAPH_SLUG=$(echo "$GRAPH_URI" | sed 's|https://packagegraph.github.io/graph/||; s|https://packagegraph.github.io/||' | tr '/' '-')

# Append a DataSnapshot describing this graph, before gzip/upload, so it
# lands in the same named graph as the rest of this file's content --
# whichever named graph this .nt file's triples are loaded into is the
# same graph these triples describe.
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
SNAPSHOT_TIMESTAMP_COMPACT=$(echo "$NOW" | tr -d ':-')
SNAPSHOT_IRI="https://packagegraph.github.io/d/snapshot/collector/${GRAPH_SLUG}/${SNAPSHOT_TIMESTAMP_COMPACT}"
# NOTE: $GRAPH_URI / $SOURCE_URL are not escaped for N-Triples literal/IRI
# syntax below -- a `"`, `<`, `>`, or `\` in either would produce one
# malformed line and fail that graph's parse at index time. Acceptable
# today because both are hardcoded, trusted values passed by deploy
# scripts, not user input -- but worth flagging for anyone adding a new
# caller with a less-trusted source.
{
  printf '<%s> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#DataSnapshot> .\n' "$SNAPSHOT_IRI"
  # snapshotGraph is owl:DatatypeProperty, range xsd:anyURI (core.ttl:823-829)
  # -- a typed literal, NOT an IRI reference. Getting this backwards makes
  # the join query in the design spec's §5 silently return zero rows (an
  # IRI and a literal never test equal in SPARQL, even with identical
  # string content).
  printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotGraph> "%s"^^<http://www.w3.org/2001/XMLSchema#anyURI> .\n' "$SNAPSHOT_IRI" "$GRAPH_URI"
  # DataSnapshotShape only requires rdfs:label (see derive_comparison.rs's
  # sibling DataSnapshot-minting code) -- without this, every DataSnapshot
  # minted here is shape-invalid relative to the rest of the corpus.
  printf '<%s> <http://www.w3.org/2000/01/rdf-schema#label> "Snapshot of graph %s uploaded %s" .\n' "$SNAPSHOT_IRI" "$GRAPH_URI" "$NOW"
  printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotTimestamp> "%s"^^<http://www.w3.org/2001/XMLSchema#dateTime> .\n' "$SNAPSHOT_IRI" "$NOW"
  if [ -n "$SOURCE_URL" ]; then
    printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotSource> "%s" .\n' "$SNAPSHOT_IRI" "$SOURCE_URL"
  fi
} >> "$LOCAL_FILE"

# gzip, not xz: N-Triples text compresses well under either, but gzip
# decompresses several times faster, which matters more here than the last
# few percent of ratio -- qlever-rebuild-index.sh decompresses every graph
# on every nightly rebuild.
MINIO_FILENAME="${GRAPH_SLUG}.nt.gz"

echo "=== Uploading N-Triples to Minio ==="
echo "Local: $LOCAL_FILE"
echo "Graph: $GRAPH_URI"
echo "Minio: nt-output/$MINIO_FILENAME"

# Configure mc alias (idempotent)
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4 >/dev/null 2>&1

COMPRESSED_FILE="${LOCAL_FILE}.gz"
gzip -c "$LOCAL_FILE" > "$COMPRESSED_FILE"

# Upload .nt.gz file first — orphan .nt.gz without sidecar is safely excluded
# by rebuilds (they iterate .graph files). The .graph sidecar acts as a
# commit marker: only written after the .nt.gz upload succeeds.
mc cp "$COMPRESSED_FILE" "pgraph/${MINIO_BUCKET}/nt-output/${MINIO_FILENAME}"

# Create .graph sidecar (commit marker) — signals this .nt.gz is ready for rebuild
SIDECAR_PATH="pgraph/${MINIO_BUCKET}/nt-output/${MINIO_FILENAME}.graph"
echo -n "$GRAPH_URI" | mc pipe "$SIDECAR_PATH"

rm -f "$COMPRESSED_FILE"

echo "✓ Uploaded $MINIO_FILENAME + .graph sidecar"
