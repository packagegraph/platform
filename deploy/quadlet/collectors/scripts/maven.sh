#!/bin/sh
# Collector: maven
# Ported from deploy/overlays/dev/jobs/collect-maven.yaml.
set -eu
# The single general Maven ecosystem view. The curated roots below are only a
# starting point for traversal, not a separate corpus, so they publish here
# and nowhere else.
GRAPH_URI="https://packagegraph.github.io/graph/maven"

# Curated root coordinates, bind-mounted from the host at /seeds by
# pg-collect@.container. pg-collect resolves each to its newest release and
# walks that release's declared dependencies outward.
#
# The list is host-installed and deliberately not in this repository: it is a
# curated set drawn from a private source, and the set itself is the sensitive
# part. It is also a genuine runtime input -- changing which roots we explore
# should be a file sync, not an image rebuild plus a coordinated cutover.
# See deploy/quadlet/collectors/seeds/README.md for the format.
#
# This replaced `--endpoint "$FUSEKI_ENDPOINT"` (SPARQL auto-discovery).
# Discovery seeded from every package in the graph with
# `upstreamEcosystem = maven` -- which answers "which distro packages claim a
# Maven upstream", not "which Maven coordinates exist". 73.1% of that set was
# never published to Maven Central, so root resolution failed en masse and
# tripped pg-collect's 20% error-rate guard, producing zero data. See the
# seed file's header for the full history and the 2026-09-14 failure.
SEED_FILE=/seeds/maven-roots.txt

# Checked before any cache work so a host missing the seed install fails
# immediately and legibly, rather than after a multi-minute Minio cache warm.
if [ ! -s "${SEED_FILE}" ]; then
  echo "ERROR: seed file ${SEED_FILE} is missing or empty; refusing to run." >&2
  echo "Install the host seed list to /etc/containers/systemd/seeds/ (mode 644);" >&2
  echo "see deploy/quadlet/collectors/seeds/README.md." >&2
  exit 1
fi

CACHE_DIR=/tmp/cache/maven
mkdir -p "${CACHE_DIR}"
MINIO_CACHE="pgraph/${MINIO_BUCKET:-}/collector-cache/maven"

CACHE_AVAILABLE=false
if mc alias set pgraph "${MINIO_ENDPOINT:-}" "${MINIO_ACCESS_KEY:-}" "${MINIO_SECRET_KEY:-}" --api S3v4 2>/dev/null; then
  CACHE_AVAILABLE=true
  echo "Warming cache from Minio..."
  mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || echo "Cache warm failed, continuing"

  # Periodically flush the cache back to Minio while the collect runs,
  # so a timeout kill doesn't discard the run's cache progress -- see
  # fedora-43-full.sh's 2026-09-10 timeout incident for why this matters.
  ( while sleep 300; do
      mc mirror --overwrite --exclude "*.tmp" --exclude "*.lock" "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
    done ) &
  CACHE_SYNC_PID=$!
  trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true' EXIT
else
  echo "WARNING: Minio alias setup failed, proceeding without remote cache"
fi

set +e
pg-collect maven --packages-file "${SEED_FILE}" --cache-dir "${CACHE_DIR}" -o /tmp/maven.nt
COLLECT_EXIT=$?
set -e

if [ "$CACHE_AVAILABLE" = "true" ]; then
  echo "Saving cache to Minio..."
  mc mirror --overwrite --exclude "*.tmp" --exclude "*.lock" "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || echo "Cache save failed"
fi

[ "$COLLECT_EXIT" -eq 0 ] && /app/scripts/upload-nt.sh /tmp/maven.nt "$GRAPH_URI"
exit "$COLLECT_EXIT"
