#!/bin/bash
# Generate .graph sidecar files for existing .nt files in Minio.
# Run once to migrate from the old graphs.json approach to per-file sidecars.
#
# Usage: MINIO_ENDPOINT=http://localhost:9000 ./migrate-graph-sidecars.sh
set -euo pipefail

BUCKET="${MINIO_BUCKET:-packagegraph}"
GRAPH_BASE="https://packagegraph.github.io/graph"

# Explicit filename → graph URI mapping (from Fuseki graph inventory)
declare -A GRAPH_MAP=(
  ["alpine-edge-riscv64.nt"]="$GRAPH_BASE/alpine/edge/riscv64"
  ["alpine-v3.20.nt"]="$GRAPH_BASE/alpine/v3.20"
  ["alpine-v3.20-aarch64.nt"]="$GRAPH_BASE/alpine/v3.20/aarch64"
  ["arch.nt"]="$GRAPH_BASE/arch"
  ["archarm-aarch64.nt"]="$GRAPH_BASE/archarm/aarch64"
  ["centos-stream-9.nt"]="$GRAPH_BASE/centos-stream/9"
  ["centos-stream-10.nt"]="$GRAPH_BASE/centos-stream/10"
  ["chocolatey.nt"]="$GRAPH_BASE/chocolatey"
  ["conda-forge.nt"]="$GRAPH_BASE/conda-forge"
  ["cran.nt"]="$GRAPH_BASE/cran"
  ["debian-trixie.nt"]="$GRAPH_BASE/debian/trixie"
  ["debian-trixie-arm64.nt"]="$GRAPH_BASE/debian/trixie/arm64"
  ["enrichment-advisory-dsa.nt"]="$GRAPH_BASE/enrichment/advisory-dsa"
  ["enrichment-advisory-rhsa.nt"]="$GRAPH_BASE/enrichment/advisory-rhsa"
  ["advisory-dsa.nt"]="$GRAPH_BASE/enrichment/advisory-dsa"
  ["advisory-rhsa.nt"]="$GRAPH_BASE/enrichment/advisory-rhsa"
  ["fedora-42.nt"]="$GRAPH_BASE/fedora/42"
  ["fedora-42-aarch64.nt"]="$GRAPH_BASE/fedora/42/aarch64"
  ["fedora-43.nt"]="$GRAPH_BASE/fedora/43"
  ["fedora-44.nt"]="$GRAPH_BASE/fedora/44"
  ["fedora-44-aarch64.nt"]="$GRAPH_BASE/fedora/44/aarch64"
  ["fedora-44-riscv64.nt"]="$GRAPH_BASE/fedora/44/riscv64"
  ["fedora-rawhide.nt"]="$GRAPH_BASE/fedora/rawhide"
  ["flatpak.nt"]="$GRAPH_BASE/flatpak"
  ["freebsd-14.nt"]="$GRAPH_BASE/freebsd/14"
  ["hackage.nt"]="$GRAPH_BASE/hackage"
  ["hex.nt"]="$GRAPH_BASE/hex"
  ["homebrew.nt"]="$GRAPH_BASE/homebrew"
  ["maven.nt"]="$GRAPH_BASE/maven"
  ["npm.nt"]="$GRAPH_BASE/npm"
  ["nuget.nt"]="$GRAPH_BASE/nuget"
  ["opensuse-tumbleweed.nt"]="$GRAPH_BASE/opensuse/tumbleweed"
  ["pypi.nt"]="$GRAPH_BASE/pypi"
  ["rubygems.nt"]="$GRAPH_BASE/rubygems"
  ["security-osv.nt"]="$GRAPH_BASE/security/osv"
  ["snap.nt"]="$GRAPH_BASE/snap"
  ["ubuntu-noble.nt"]="$GRAPH_BASE/ubuntu/noble"
  ["ubuntu-noble-arm64.nt"]="$GRAPH_BASE/ubuntu/noble/arm64"
  ["ubuntu-noble-riscv64.nt"]="$GRAPH_BASE/ubuntu/noble/riscv64"
  ["void.nt"]="$GRAPH_BASE/void"
)

echo "=== Graph Sidecar Migration ==="
echo "Bucket: $BUCKET"
echo ""

# Configure mc alias (required for pgraph:// paths)
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

CREATED=0
SKIPPED=0
UNMAPPED=0

# Enumerate actual .nt files in Minio
echo "Enumerating .nt files in Minio..."
ACTUAL_FILES=$(mc ls "pgraph/${BUCKET}/nt-output/" | grep '\.nt$' | awk '{print $NF}' || true)
if [ -z "$ACTUAL_FILES" ]; then
  echo "ERROR: no .nt files found in nt-output/ — Minio unreachable or bucket empty"
  exit 1
fi

for filename in $ACTUAL_FILES; do
  nt_path="pgraph/${BUCKET}/nt-output/${filename}"
  graph_path="pgraph/${BUCKET}/nt-output/${filename}.graph"

  # Check if mapped
  if [ -z "${GRAPH_MAP[$filename]:-}" ]; then
    echo "  UNMAPPED $filename (add to GRAPH_MAP in this script)"
    UNMAPPED=$((UNMAPPED + 1))
    continue
  fi

  graph_uri="${GRAPH_MAP[$filename]}"

  # Check if .graph already exists
  if mc stat "$graph_path" >/dev/null 2>&1; then
    existing=$(mc cat "$graph_path" | tr -d '\n')
    if [ "$existing" = "$graph_uri" ]; then
      echo "  OK   $filename → $graph_uri (already exists)"
      SKIPPED=$((SKIPPED + 1))
      continue
    fi
  fi

  # Create sidecar
  echo -n "$graph_uri" | mc pipe "$graph_path"
  echo "  NEW  $filename → $graph_uri"
  CREATED=$((CREATED + 1))
done

echo ""
echo "Created: $CREATED  Skipped: $SKIPPED  Unmapped: $UNMAPPED"
if [ "$UNMAPPED" -gt 0 ]; then
  echo "ERROR: $UNMAPPED unmapped .nt files — add mappings to GRAPH_MAP in this script"
  exit 1
fi
