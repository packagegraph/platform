#!/bin/bash
set -euo pipefail

# Upload an N-Triples file to Minio and register it in the graph manifest.
# Also mints a pkg:DataSnapshot describing this upload, appended directly
# into the file before it's gzipped, so it lands in the same named graph
# as the data it describes.
#
# NOTE: this mutates the local .nt file in place by appending. A retried
# invocation against the same local file strips any DataSnapshot block it
# previously appended (matched by IRI prefix, see SNAPSHOT_PREFIX below)
# before appending a fresh one, so retries don't accumulate duplicates.
# Uploading the same local file to two different graphs is still the
# caller's responsibility to avoid -- osv.sh and
# enrichers/scripts/security.sh already concatenate all input before a
# single upload call.
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
# Publishes an immutable generation and then commits it by replacing that
# graph's manifest -- see docs/GRAPH-PUBLICATION.md for the full contract and
# for why the previous stable-key-plus-sidecar scheme was not one (#72):
#
#   graphs/debian-trixie/generations/<gen>.nt.gz   the payload, never replaced
#   graphs/debian-trixie/manifest.json             the commit point
#
# It also refreshes the legacy nt-output/debian-trixie.nt.gz pair, temporarily
# and best-effort, for readers that have not been upgraded yet. See the
# PG_UPLOAD_LEGACY_MIRROR block at the end.

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

# Reject anything that can't validly appear in an IRI reference, even though
# GRAPH_URI/SOURCE_URL are only ever embedded as string literals below (never
# as a bare <...> IRIREF) -- a value that isn't even a well-formed IRI has no
# business being called a graph or source URI regardless of literal escaping.
nt_check_iri_chars() {
  local label=$1 value=$2
  case "$value" in
    *[\<\>\"{}\|\^\`]*)
      echo "Error: $label contains a character forbidden in an IRI reference: $value" >&2
      exit 1
      ;;
  esac
  if printf '%s' "$value" | LC_ALL=C grep -q '[[:cntrl:]]'; then
    echo "Error: $label contains a control character: $value" >&2
    exit 1
  fi
}
nt_check_iri_chars "graph-uri" "$GRAPH_URI"
[ -n "$SOURCE_URL" ] && nt_check_iri_chars "source-url" "$SOURCE_URL"

# An upload REPLACES the whole named graph, so publishing a file with no data
# does not merely record nothing -- it erases that graph's contents on the next
# rebuild. Refuse instead, loudly, because every downstream signal is blind to
# this: the collector's exit code, the commit marker, and the rebuild's
# corpus-wide loss gate all treat an empty-but-present graph as healthy. cpan,
# hex and nuget published empty graphs, green, for as long as the journal
# retained (#58).
#
# Counted as data: any line that is not blank, not an N-Triples comment, and
# whose subject is neither a DataSnapshot nor a taxonomy node. Both exclusions
# are load-bearing rather than tidy:
#
#   snapshot/  This script appends its own DataSnapshot block below, and strips
#              a prior one on retry, so a retried run that collected nothing
#              still presents a file with lines in it. Every snapshot IRI is
#              excluded, not just this graph's, so the check does not depend on
#              the strip below having run -- which keeps it ahead of any
#              mutation of the caller's file.
#
#   distro/    Collectors emit a Distribution, its DistributionRelease and its
#   release/   Architecture nodes unconditionally, before they know whether any
#   arch/      package exists. That is the whole reason #58's zeroes were
#              invisible: hex and nuget each published exactly six triples
#              naming the distro and its release, and not one package. A floor
#              of "at least one triple" passes that file and fixes nothing.
#              These nodes describe the container, never the collected
#              content, so they cannot stand in for it. Enrichers emit none of
#              them, so excluding them cannot starve a small-but-real
#              enrichment graph (enrichment-forge-version legitimately
#              publishes ~14 triples, none of them taxonomy).
DATA_IRI_ROOT="https://packagegraph.github.io/d/"
MIN_DATA_TRIPLES="${PG_UPLOAD_MIN_TRIPLES:-1}"
case "$MIN_DATA_TRIPLES" in
    ''|*[!0-9]*)
        echo "Error: PG_UPLOAD_MIN_TRIPLES must be a non-negative integer, got: $MIN_DATA_TRIPLES" >&2
        exit 1
        ;;
esac

# grep exits 1 when it selects nothing and >1 on a real error. Both leave the
# capture empty, which floors the count at 0 and refuses: this fails closed.
DATA_TRIPLE_COUNT=$(grep -cv \
    -e '^[[:space:]]*$' \
    -e '^[[:space:]]*#' \
    -e "^<${DATA_IRI_ROOT}snapshot/collector/" \
    -e "^<${DATA_IRI_ROOT}distro/" \
    -e "^<${DATA_IRI_ROOT}release/" \
    -e "^<${DATA_IRI_ROOT}arch/" \
    "$LOCAL_FILE" || true)
DATA_TRIPLE_COUNT="${DATA_TRIPLE_COUNT:-0}"

if [ "$DATA_TRIPLE_COUNT" -lt "$MIN_DATA_TRIPLES" ]; then
    if [ -n "${PG_UPLOAD_ALLOW_EMPTY:-}" ]; then
        echo "Warning: $LOCAL_FILE has $DATA_TRIPLE_COUNT data triples (minimum" \
             "$MIN_DATA_TRIPLES); publishing anyway because PG_UPLOAD_ALLOW_EMPTY is set" >&2
    else
        echo "Error: refusing to publish $GRAPH_URI -- $LOCAL_FILE has" \
             "$DATA_TRIPLE_COUNT data triples, below the minimum of $MIN_DATA_TRIPLES." >&2
        echo "  An upload replaces the entire named graph, so publishing this would" \
             "erase the existing contents of that graph." >&2
        echo "  If this collector is genuinely expected to produce nothing, set" \
             "PG_UPLOAD_ALLOW_EMPTY=1 to declare that explicitly." >&2
        exit 1
    fi
fi
echo "Data triples: $DATA_TRIPLE_COUNT (minimum $MIN_DATA_TRIPLES)"

# Escape a value for use inside an N-Triples STRING_LITERAL_QUOTE ("...").
# Backslash must be escaped first -- escaping it after the other characters
# would double-escape the backslashes those substitutions just introduced.
nt_escape() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\r'/\\r}
  s=${s//$'\t'/\\t}
  printf '%s' "$s"
}

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
SNAPSHOT_PREFIX="https://packagegraph.github.io/d/snapshot/collector/${GRAPH_SLUG}/"
SNAPSHOT_IRI="${SNAPSHOT_PREFIX}${SNAPSHOT_TIMESTAMP_COMPACT}"

# Idempotent retry: a previous invocation may have appended its own
# DataSnapshot block to this same $LOCAL_FILE and then failed before the
# upload completed. Strip any prior block for this graph slug (by its
# snapshot IRI prefix, not the timestamp suffix, so it catches every past
# attempt) before appending a fresh one, so a retry can't accumulate
# duplicate/conflicting DataSnapshot records.
if grep -qF "<${SNAPSHOT_PREFIX}" "$LOCAL_FILE"; then
  TMP_STRIPPED=$(mktemp)
  grep -vF "<${SNAPSHOT_PREFIX}" "$LOCAL_FILE" > "$TMP_STRIPPED"
  mv "$TMP_STRIPPED" "$LOCAL_FILE"
fi

GRAPH_URI_ESC=$(nt_escape "$GRAPH_URI")
NOW_ESC=$(nt_escape "$NOW")
{
  printf '<%s> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#DataSnapshot> .\n' "$SNAPSHOT_IRI"
  # snapshotGraph is owl:DatatypeProperty, range xsd:anyURI (core.ttl:823-829)
  # -- a typed literal, NOT an IRI reference. Getting this backwards makes
  # the join query in the design spec's §5 silently return zero rows (an
  # IRI and a literal never test equal in SPARQL, even with identical
  # string content).
  printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotGraph> "%s"^^<http://www.w3.org/2001/XMLSchema#anyURI> .\n' "$SNAPSHOT_IRI" "$GRAPH_URI_ESC"
  # DataSnapshotShape only requires rdfs:label (see derive_comparison.rs's
  # sibling DataSnapshot-minting code) -- without this, every DataSnapshot
  # minted here is shape-invalid relative to the rest of the corpus.
  printf '<%s> <http://www.w3.org/2000/01/rdf-schema#label> "Snapshot of graph %s uploaded %s" .\n' "$SNAPSHOT_IRI" "$GRAPH_URI_ESC" "$NOW_ESC"
  printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotTimestamp> "%s"^^<http://www.w3.org/2001/XMLSchema#dateTime> .\n' "$SNAPSHOT_IRI" "$NOW_ESC"
  if [ -n "$SOURCE_URL" ]; then
    printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotSource> "%s" .\n' "$SNAPSHOT_IRI" "$(nt_escape "$SOURCE_URL")"
  fi
} >> "$LOCAL_FILE"


# gzip, not xz: N-Triples text compresses well under either, but gzip
# decompresses several times faster, which matters more here than the last
# few percent of ratio -- qlever-rebuild-index.sh decompresses every graph
# on every nightly rebuild.
COMPRESSED_FILE="${LOCAL_FILE}.gz"
gzip -c "$LOCAL_FILE" > "$COMPRESSED_FILE"
trap 'rm -f "$COMPRESSED_FILE"' EXIT

# Stage completeness, when the collector recorded any (#70). The sidecar is
# written by pg-collect beside the .nt it just produced; see
# src/stage_report.rs for what the counts mean.
#
# Absent is NOT the same as complete. Every graph published before this
# carried no such record, so a consumer has to read a missing quality block as
# "unknown" -- which is exactly why a present-but-unreadable one is refused
# rather than dropped. The collector wrote that file moments ago; if it cannot
# be parsed something is wrong, and publishing without it would quietly
# promote a knowingly partial graph into the indistinguishable "unknown" pile.
QUALITY_FILE="${LOCAL_FILE}.quality.json"
QUALITY_JSON="null"
if [ -f "$QUALITY_FILE" ]; then
  if ! QUALITY_JSON=$(jq -c '.' "$QUALITY_FILE" 2>/dev/null); then
    echo "Error: $QUALITY_FILE is not readable JSON -- refusing to publish <$GRAPH_URI>." >&2
    echo "  Publishing without it would record this graph's completeness as" >&2
    echo "  unknown, which is indistinguishable from a graph that was never" >&2
    echo "  measured at all." >&2
    exit 1
  fi
  QUALITY_COMPLETE=$(printf '%s' "$QUALITY_JSON" | jq -r '.complete | tostring')
  case "$QUALITY_COMPLETE" in
    true|false) ;;
    *)
      echo "Error: $QUALITY_FILE has no boolean .complete -- refusing to publish <$GRAPH_URI>." >&2
      exit 1
      ;;
  esac
  if [ "$QUALITY_COMPLETE" = "true" ]; then
    echo "Stage completeness: complete"
  else
    echo "Stage completeness: PARTIAL -- publishing anyway, recorded in the manifest"
    printf '%s' "$QUALITY_JSON" | jq -r '.stages[]? |
      "  \(.stage): \(.attempted) attempted, \(.completed) completed, \(.retryable) retryable, \(.failed) failed"'
  fi
fi

PAYLOAD_SHA=$(sha256sum "$COMPRESSED_FILE" | cut -d' ' -f1)
PAYLOAD_SIZE=$(stat -c '%s' "$COMPRESSED_FILE")
GENERATION="$(date -u +%Y%m%dT%H%M%SZ)-${PAYLOAD_SHA:0:12}"

MANIFEST_KEY="graphs/${GRAPH_SLUG}/manifest.json"
GENERATION_KEY="graphs/${GRAPH_SLUG}/generations/${GENERATION}.nt.gz"
MANIFEST_PATH="pgraph/${MINIO_BUCKET}/${MANIFEST_KEY}"
GENERATION_PATH="pgraph/${MINIO_BUCKET}/${GENERATION_KEY}"

echo "=== Publishing N-Triples to object storage ==="
echo "Local: $LOCAL_FILE"
echo "Graph: $GRAPH_URI"
echo "Generation: $GENERATION_KEY ($PAYLOAD_SIZE bytes)"

# Configure mc alias (idempotent)
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4 >/dev/null 2>&1

# The slug is a display name derived from the graph URI, and the derivation is
# not injective -- .../graph/a/b and .../graph/a-b both slug to "a-b". Two
# graphs sharing a slug would silently take turns overwriting each other's
# manifest, which is the same class of bug as the graph-URI collision that
# corpus-wide loss gates cannot see. Refuse instead.
EXISTING_MANIFEST=$(mc cat "$MANIFEST_PATH" 2>/dev/null || true)
if [ -n "$EXISTING_MANIFEST" ]; then
  EXISTING_GRAPH=$(printf '%s' "$EXISTING_MANIFEST" | jq -r '.graph // empty' 2>/dev/null || true)
  if [ -n "$EXISTING_GRAPH" ] && [ "$EXISTING_GRAPH" != "$GRAPH_URI" ]; then
    echo "Error: $MANIFEST_KEY already belongs to <$EXISTING_GRAPH>, refusing to publish <$GRAPH_URI> over it." >&2
    echo "  Two graph URIs are deriving the same slug '$GRAPH_SLUG'. One of them needs a different URI." >&2
    exit 1
  fi
fi

# Step 1: the payload, under a key that has never existed before. A generation
# key embeds a timestamp and the payload digest, so this upload cannot replace
# anything -- which is the whole point. See docs/GRAPH-PUBLICATION.md.
mc cp "$COMPRESSED_FILE" "$GENERATION_PATH"

# Step 2: verify what actually landed before anything points at it. Size is
# authoritative; the ETag is an MD5 only for a single-part upload (a multipart
# ETag ends in "-<partcount>" and is a digest of digests), so it is checked
# only when it is one. The reader re-verifies the SHA-256 on every rebuild,
# which is what makes the multipart case safe to wave through here.
STORED=$(mc ls --json "$GENERATION_PATH" 2>/dev/null | jq -s '.[0] // {}')
STORED_SIZE=$(printf '%s' "$STORED" | jq -r '.size // empty')
STORED_ETAG=$(printf '%s' "$STORED" | jq -r '.etag // empty')
if [ -z "$STORED_SIZE" ]; then
  echo "Error: uploaded generation $GENERATION_KEY is not readable back — not committing." >&2
  exit 1
fi
if [ "$STORED_SIZE" != "$PAYLOAD_SIZE" ]; then
  echo "Error: $GENERATION_KEY stored as $STORED_SIZE bytes, expected $PAYLOAD_SIZE — not committing." >&2
  exit 1
fi
case "$STORED_ETAG" in
  *-*|'')
    echo "Verified: $STORED_SIZE bytes (multipart or absent ETag; digest checked by the reader)"
    ;;
  *)
    PAYLOAD_MD5=$(md5sum "$COMPRESSED_FILE" | cut -d' ' -f1)
    if [ "$STORED_ETAG" != "$PAYLOAD_MD5" ]; then
      echo "Error: $GENERATION_KEY ETag $STORED_ETAG does not match uploaded MD5 $PAYLOAD_MD5 — not committing." >&2
      exit 1
    fi
    echo "Verified: $STORED_SIZE bytes, ETag matches"
    ;;
esac

# Step 3: the manifest. THIS is the commit -- one small object, one PUT, which
# S3 makes atomic, so a reader sees the whole old manifest or the whole new
# one and never a half-written pointer. Everything above this line is
# invisible to readers; everything below it is cleanup.
#
# Failing between steps 1 and 3 leaves an orphan generation that nothing
# references. Failing at step 1 leaves the previous manifest authoritative and
# its generation intact. Neither can make an unverified payload discoverable,
# which is exactly what the old stable-key-plus-sidecar scheme did on every
# upload after the first (#72).
MANIFEST_TMP="${COMPRESSED_FILE}.manifest.json"
jq -n \
  --arg graph "$GRAPH_URI" \
  --arg generation "$GENERATION" \
  --arg key "$GENERATION_KEY" \
  --arg sha256 "$PAYLOAD_SHA" \
  --arg committed_at "$NOW" \
  --arg source_url "$SOURCE_URL" \
  --argjson size_bytes "$PAYLOAD_SIZE" \
  --argjson data_triples "$DATA_TRIPLE_COUNT" \
  --argjson quality "$QUALITY_JSON" \
  '{
     schema: 1,
     graph: $graph,
     generation: $generation,
     key: $key,
     encoding: "gzip",
     size_bytes: $size_bytes,
     sha256: $sha256,
     data_triples: $data_triples,
     committed_at: $committed_at
   }
   + (if $source_url == "" then {} else {source_url: $source_url} end)
   + (if $quality == null then {} else {quality: $quality} end)' \
  > "$MANIFEST_TMP"
if ! mc pipe "$MANIFEST_PATH" < "$MANIFEST_TMP"; then
  rm -f "$MANIFEST_TMP"
  echo "Error: failed to commit $MANIFEST_KEY — $GENERATION_KEY is an orphan and will be ignored." >&2
  echo "  The previously committed generation remains authoritative. Retry this upload." >&2
  exit 1
fi
rm -f "$MANIFEST_TMP"
echo "✓ Committed <$GRAPH_URI> → $GENERATION_KEY"

# Step 4: the legacy pair, for readers that have not been upgraded yet.
#
# This is the unsafe stable-key write the manifest replaces, kept alive
# deliberately and temporarily: writers ship inside the collector image while
# the host readers are installed files under /etc/containers/systemd/scripts/,
# so the two do not move together, and a writer that stopped refreshing
# nt-output/ before its readers were synced would freeze those graphs at their
# last legacy upload -- silently, because a stale-but-present graph passes
# every gate. An upgraded reader ignores legacy copies of any graph that has a
# manifest, so this costs storage and nothing else.
#
# Remove this block, and PG_UPLOAD_LEGACY_MIRROR with it, once every reader in
# docs/GRAPH-PUBLICATION.md is deployed. Failures here are warnings: the graph
# is already committed above, and this is a courtesy to old readers.
if [ "${PG_UPLOAD_LEGACY_MIRROR:-1}" != "0" ]; then
  LEGACY_FILENAME="${GRAPH_SLUG}.nt.gz"
  LEGACY_PATH="pgraph/${MINIO_BUCKET}/nt-output/${LEGACY_FILENAME}"
  if mc cp "$COMPRESSED_FILE" "$LEGACY_PATH"; then
    if ! echo -n "$GRAPH_URI" | mc pipe "${LEGACY_PATH}.graph"; then
      echo "Warning: legacy sidecar ${LEGACY_FILENAME}.graph not refreshed (manifest is committed)" >&2
    fi
  else
    echo "Warning: legacy mirror of ${LEGACY_FILENAME} failed (manifest is committed)" >&2
  fi
fi
