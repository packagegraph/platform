#!/bin/bash
set -euo pipefail

# Upload an N-Triples file to Minio and register it in the graph manifest.
#
# Usage: upload-nt.sh <local-file.nt> <graph-uri>
#
# Example:
#   upload-nt.sh /tmp/packages.nt "https://packagegraph.github.io/graph/debian/trixie"
#
# Uploads to: pgraph/${MINIO_BUCKET}/nt-output/debian-trixie.nt
# Updates: pgraph/${MINIO_BUCKET}/nt-output/graphs.json

if [ $# -ne 2 ]; then
    echo "Usage: upload-nt.sh <local-file.nt> <graph-uri>" >&2
    exit 1
fi

LOCAL_FILE="$1"
GRAPH_URI="$2"

if [ ! -f "$LOCAL_FILE" ]; then
    echo "Error: file not found: $LOCAL_FILE" >&2
    exit 1
fi

# Derive Minio filename from graph URI
# https://packagegraph.github.io/graph/debian/trixie → debian-trixie
# https://packagegraph.github.io/graph/security/osv → security-osv
GRAPH_SLUG=$(echo "$GRAPH_URI" | sed 's|https://packagegraph.github.io/graph/||' | tr '/' '-')
MINIO_FILENAME="${GRAPH_SLUG}.nt"

echo "=== Uploading N-Triples to Minio ==="
echo "Local: $LOCAL_FILE"
echo "Graph: $GRAPH_URI"
echo "Minio: nt-output/$MINIO_FILENAME"

# Configure mc alias (idempotent)
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4 >/dev/null 2>&1

# Upload .nt file
mc cp "$LOCAL_FILE" "pgraph/${MINIO_BUCKET}/nt-output/${MINIO_FILENAME}"

# Update graphs.json manifest
MANIFEST_PATH="pgraph/${MINIO_BUCKET}/nt-output/graphs.json"

# Download existing manifest (or create empty)
if ! mc cat "$MANIFEST_PATH" >/tmp/graphs.json 2>/dev/null; then
    echo "Creating new manifest..."
    echo '{}' > /tmp/graphs.json
else
    echo "Updating existing manifest..."
fi

# Validate existing manifest (handle malformed/empty JSON)
if ! jq empty /tmp/graphs.json 2>/dev/null; then
    echo "Warning: Existing manifest is invalid JSON, recreating..."
    echo '{}' > /tmp/graphs.json
fi

# Merge new entry using jq (atomic write to prevent corruption)
if ! jq --arg k "$MINIO_FILENAME" --arg v "$GRAPH_URI" \
     '. + {($k): $v}' /tmp/graphs.json > /tmp/graphs.json.tmp 2>/dev/null; then
    echo "Error: jq failed to update manifest" >&2
    rm -f /tmp/graphs.json /tmp/graphs.json.tmp
    exit 1
fi
mv /tmp/graphs.json.tmp /tmp/graphs.json

# Upload updated manifest
if ! mc cp /tmp/graphs.json "$MANIFEST_PATH"; then
    echo "Error: Failed to upload manifest to Minio" >&2
    rm -f /tmp/graphs.json
    exit 1
fi
rm /tmp/graphs.json

echo "✓ Uploaded $MINIO_FILENAME and updated manifest"
