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
GRAPH_URI="https://packagegraph.github.io/graph/security/osv"
COMBINED=/tmp/osv-combined.nt
: > "$COMBINED"

for spec in "npm:npm" "PyPI:pypi" "crates.io:cratesio" "Go:go" "Maven:maven" "NuGet:nuget" "Packagist:packagist" "RubyGems:rubygems" "Hex:hex" "Pub:pub" "Hackage:hackage" "SwiftURL:swifturl"; do
  eco="${spec%%:*}"
  slug="${spec#*:}"
  echo "=== OSV: $eco ==="
  pg-collect osv --ecosystem "$eco" -o "/tmp/osv-${slug}.nt"
  cat "/tmp/osv-${slug}.nt" >> "$COMBINED"
  rm -f "/tmp/osv-${slug}.nt"
done

/app/scripts/upload-nt.sh "$COMBINED" "$GRAPH_URI"
