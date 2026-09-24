#!/bin/sh
# Collector: osv
# Ported from deploy/overlays/dev/jobs/collect-osv.yaml.
#
# All 12 ecosystems share one graph, so they must accumulate into ONE local
# file before a SINGLE upload-nt.sh call at the end -- upload-nt.sh derives
# the Minio object key from the graph URI, so calling it once per ecosystem
# with the same GRAPH_URI would have each upload silently overwrite the
# previous one, leaving only the last ecosystem's data in nt-output/. Caught
# before this ever ran for real (next scheduled run was still days out).
set -eu

# Private per-invocation scratch. /tmp is one volume shared by every
# concurrently running collector -- see README.md, "Run directories".
find /tmp/ -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-osv.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT

GRAPH_URI="https://packagegraph.github.io/graph/security/osv"
COMBINED="$RUN_DIR/osv-combined.nt"
: > "$COMBINED"

for spec in "npm:npm" "PyPI:pypi" "crates.io:cratesio" "Go:go" "Maven:maven" "NuGet:nuget" "Packagist:packagist" "RubyGems:rubygems" "Hex:hex" "Pub:pub" "Hackage:hackage" "SwiftURL:swifturl"; do
  eco="${spec%%:*}"
  slug="${spec#*:}"
  echo "=== OSV: $eco ==="
  pg-collect osv --ecosystem "$eco" -o "$RUN_DIR/osv-${slug}.nt"
  cat "$RUN_DIR/osv-${slug}.nt" >> "$COMBINED"
  rm -f "$RUN_DIR/osv-${slug}.nt"
done

/app/scripts/upload-nt.sh "$COMBINED" "$GRAPH_URI"
