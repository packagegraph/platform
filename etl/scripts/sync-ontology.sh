#!/usr/bin/env bash
# sync-ontology.sh — Sync ontology files from restructured ontology repo to flat mirror
#
# The ontology repo was restructured from flat *.ttl files to subdirectories:
#   core/core.ttl
#   extensions/*/module.ttl
#   ecosystems/*/module.ttl
#
# The platform mirror (etl/ontology/) must stay flat for the build pipeline
# (cli.py:252 globs ontology_dir.glob("*.ttl") non-recursively).
#
# This script collects only the ontology .ttl files (not .shacl.ttl or .examples.ttl)
# from the restructured layout into the flat mirror.
#
# Usage:
#   ./etl/scripts/sync-ontology.sh [source-ontology-dir]
#
# Arguments:
#   source-ontology-dir  Path to ontology repo root (default: ../../ontology)
#
# Output:
#   Populates etl/ontology/ with 36 ontology .ttl files (one per module)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLATFORM_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MIRROR_DIR="$PLATFORM_ROOT/etl/ontology"

# Source directory: default to sibling ontology repo, override with $1
ONTOLOGY_SOURCE="${1:-$PLATFORM_ROOT/../ontology}"

if [[ ! -d "$ONTOLOGY_SOURCE" ]]; then
    echo "ERROR: Ontology source not found: $ONTOLOGY_SOURCE"
    echo "Usage: $0 [ontology-repo-path]"
    exit 1
fi

echo "Syncing ontology files..."
echo "  Source: $ONTOLOGY_SOURCE"
echo "  Mirror: $MIRROR_DIR"

# Clean mirror directory
rm -rf "$MIRROR_DIR"
mkdir -p "$MIRROR_DIR"

# Collect ontology .ttl files (exclude .shacl.ttl and .examples.ttl)
count=0

# Core module (all .ttl files except .shacl.ttl and .examples.ttl)
for core_file in "$ONTOLOGY_SOURCE/core"/*.ttl; do
    if [[ -f "$core_file" ]]; then
        base=$(basename "$core_file")
        case "$base" in
            *.shacl.ttl|*.examples.ttl) continue ;;
        esac
        cp "$core_file" "$MIRROR_DIR/"
        count=$((count + 1))
    fi
done

# Extensions
for ext_dir in "$ONTOLOGY_SOURCE/extensions"/*; do
    if [[ -d "$ext_dir" ]]; then
        module=$(basename "$ext_dir")
        ttl_file="$ext_dir/$module.ttl"
        if [[ -f "$ttl_file" ]]; then
            cp "$ttl_file" "$MIRROR_DIR/"
            count=$((count + 1))
        fi
    fi
done

# Ecosystems
for eco_dir in "$ONTOLOGY_SOURCE/ecosystems"/*; do
    if [[ -d "$eco_dir" ]]; then
        module=$(basename "$eco_dir")
        ttl_file="$eco_dir/$module.ttl"
        if [[ -f "$ttl_file" ]]; then
            cp "$ttl_file" "$MIRROR_DIR/"
            count=$((count + 1))
        fi
    fi
done

echo "Synced $count ontology files to mirror."
echo ""
echo "Mirror contents:"
ls -1 "$MIRROR_DIR"/*.ttl 2>/dev/null | wc -l | xargs echo "  Total .ttl files:"
ls -1 "$MIRROR_DIR"/*.shacl.ttl 2>/dev/null | wc -l | xargs echo "  SHACL files (should be 0):"
ls -1 "$MIRROR_DIR"/*.examples.ttl 2>/dev/null | wc -l | xargs echo "  Example files (should be 0):"

# Verify no stale monolithic files
if [[ -f "$MIRROR_DIR/shacl.ttl" ]] || [[ -f "$MIRROR_DIR/examples.ttl" ]]; then
    echo "  WARNING: Stale monolithic files found and removed"
    rm -f "$MIRROR_DIR/shacl.ttl" "$MIRROR_DIR/examples.ttl"
fi

echo "Sync complete."
