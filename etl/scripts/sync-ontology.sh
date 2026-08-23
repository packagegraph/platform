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
#   Populates etl/ontology/ with 37 ontology .ttl files (one per module)

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
echo "  Total .ttl files: $(find "$MIRROR_DIR" -maxdepth 1 -name '*.ttl' | wc -l)"
echo "  SHACL files (should be 0): $(find "$MIRROR_DIR" -maxdepth 1 -name '*.shacl.ttl' | wc -l)"
echo "  Example files (should be 0): $(find "$MIRROR_DIR" -maxdepth 1 -name '*.examples.ttl' | wc -l)"

# Verify no stale monolithic files
if [[ -f "$MIRROR_DIR/shacl.ttl" ]] || [[ -f "$MIRROR_DIR/examples.ttl" ]]; then
    echo "  WARNING: Stale monolithic files found and removed"
    rm -f "$MIRROR_DIR/shacl.ttl" "$MIRROR_DIR/examples.ttl"
fi

# Verify exact allowlist (v0.12.0: 37 modules)
EXPECTED_FILES="apk.ttl
attestation.ttl
bitbake.ttl
bsdpkg.ttl
buildroot.ttl
cargo.ttl
chocolatey.ttl
conda.ttl
core.ttl
cpan.ttl
cran.ttl
deb.ttl
dq.ttl
flatpak.ttl
gomod.ttl
hackage.ttl
hex.ttl
homebrew.ttl
maven.ttl
metrics.ttl
nix.ttl
npm.ttl
nuget.ttl
opkg.ttl
pacman.ttl
portage.ttl
pypi.ttl
redhat.ttl
rpm.ttl
rubygems.ttl
security.ttl
skos-schemes.ttl
slsa.ttl
snap.ttl
taxonomy.ttl
vcs.ttl
xbps.ttl"

ACTUAL_FILES=$(find "$MIRROR_DIR" -maxdepth 1 -name '*.ttl' -exec basename {} \; | sort)

MISSING=$(comm -23 <(echo "$EXPECTED_FILES") <(echo "$ACTUAL_FILES"))
UNEXPECTED=$(comm -13 <(echo "$EXPECTED_FILES") <(echo "$ACTUAL_FILES"))

if [[ -n "$MISSING" ]]; then
    echo "ERROR: Missing expected ontology modules:"
    echo "$MISSING" | sed 's/^/  - /'
    exit 1
fi

if [[ -n "$UNEXPECTED" ]]; then
    echo "ERROR: Unexpected ontology modules (update allowlist in this script if intentional):"
    echo "$UNEXPECTED" | sed 's/^/  - /'
    exit 1
fi

echo "Sync complete."
