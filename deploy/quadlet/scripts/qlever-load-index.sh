#!/bin/bash
# Pull the latest built QLever index from Minio and atomically swap it into
# place. Equivalent of the `load-index` initContainer in
# deploy/base/qlever/deployment.yaml -- kept behaviorally identical so the
# quadlet and Kubernetes deployments share the same recovery/swap semantics.
#
# Requires: mc (Minio client), tar -- both present in the qlever-rebuild image.
# Env: MINIO_ENDPOINT, MINIO_ACCESS_KEY, MINIO_SECRET_KEY, MINIO_BUCKET.
# Bind-mounted into the qlever-index-load container at
# /usr/local/bin/qlever-load-index.sh (see qlever-index-load.container).
set -euo pipefail

INDEX_DIR=/data/index
BUCKET="${MINIO_BUCKET:-packagegraph}"
# .loaded means "these bytes are on disk", and is what makes a host reboot
# cheap: without it every boot would re-download a multi-gigabyte index.
# It deliberately does NOT mean qlever is serving them. That second fact
# lives in .serving, written by qlever-refresh-if-changed.sh only after the
# reloaded server has answered a query (#73). The atomic swap below replaces
# the whole index directory, so new bytes arrive with no .serving at all --
# which is correct: nothing is confirmed until the refresh confirms it.
MARKER="$INDEX_DIR/.loaded"

# Recover from interrupted swap: .prev exists but INDEX_DIR doesn't
if [ ! -d "$INDEX_DIR" ] && [ -d "${INDEX_DIR}.prev" ]; then
  echo "Recovering from interrupted swap — restoring .prev"
  mv "${INDEX_DIR}.prev" "$INDEX_DIR"
fi
# Clean up stale staging from interrupted extract
rm -rf "${INDEX_DIR}.staging"

mkdir -p "$INDEX_DIR"

mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

LATEST=$(mc cat "pgraph/${BUCKET}/qlever-index/latest" 2>/dev/null || echo "")
if [ -z "$LATEST" ]; then
  echo "ERROR: No index available in Minio"
  exit 1
fi

if [ -f "$MARKER" ]; then
  CURRENT=$(cat "$MARKER")
  if [ "$CURRENT" = "$LATEST" ]; then
    echo "Index $CURRENT already loaded, skipping"
    exit 0
  fi
  echo "Index changed: $CURRENT → $LATEST"
fi

echo "Downloading index $LATEST..."
mc cp "pgraph/${BUCKET}/qlever-index/${LATEST}/index.tar.gz" /tmp/index.tar.gz

STAGING="$INDEX_DIR.staging"
rm -rf "$STAGING"
mkdir -p "$STAGING"
if ! tar xzf /tmp/index.tar.gz -C "$STAGING"; then
  echo "ERROR: Failed to extract index — keeping current"
  rm -rf "$STAGING" /tmp/index.tar.gz
  exit 1
fi
rm -f /tmp/index.tar.gz

if [ ! -f "$STAGING/packagegraph.index.pos" ]; then
  echo "ERROR: Extracted index missing expected files — keeping current"
  rm -rf "$STAGING"
  exit 1
fi

# Atomic swap: rename old → .prev, move staging → active
if [ -d "$INDEX_DIR" ]; then
  mv "$INDEX_DIR" "${INDEX_DIR}.prev"
fi
if ! mv "$STAGING" "$INDEX_DIR"; then
  echo "ERROR: Failed to swap index — restoring previous"
  mv "${INDEX_DIR}.prev" "$INDEX_DIR" 2>/dev/null
  exit 1
fi
rm -rf "${INDEX_DIR}.prev"
echo "$LATEST" > "$INDEX_DIR/.loaded"
echo "Index $LATEST loaded ($(du -sh "$INDEX_DIR" | cut -f1))"
