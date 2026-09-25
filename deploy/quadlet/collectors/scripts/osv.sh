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

# Debian and Alpine are the two ecosystems the corpus collects packages for
# that OSV also publishes. They were fetched one advisory at a time by the
# security ENRICHER, at 500ms pacing against the API -- ~73,000 advisories,
# 13+ hours -- while the same data is 72 MB of ZIP on this job, which
# already runs daily. See #59 and the security enricher's header.
#
# OSV has no Fedora ecosystem at all, so the RPM corpus is not covered here
# and cannot be; AlmaLinux, Rocky Linux and Red Hat exist and are not yet
# collected -- tracked separately.
for spec in "npm:npm" "PyPI:pypi" "crates.io:cratesio" "Go:go" "Maven:maven" "NuGet:nuget" "Packagist:packagist" "RubyGems:rubygems" "Hex:hex" "Pub:pub" "Hackage:hackage" "SwiftURL:swifturl" "Debian:debian" "Alpine:alpine"; do
  eco="${spec%%:*}"
  slug="${spec#*:}"
  echo "=== OSV: $eco ==="
  pg-collect osv --ecosystem "$eco" -o "$RUN_DIR/osv-${slug}.nt"
  cat "$RUN_DIR/osv-${slug}.nt" >> "$COMBINED"
  rm -f "$RUN_DIR/osv-${slug}.nt"
done

/app/scripts/upload-nt.sh "$COMBINED" "$GRAPH_URI"
