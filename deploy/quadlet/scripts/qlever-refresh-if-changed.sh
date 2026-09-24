#!/bin/bash
# Runs on the HOST (systemd ExecStartPost for qlever-rebuild-index.service),
# NOT inside a container. Reloads qlever onto the promoted index whenever what
# is promoted differs from what has been confirmed serving.
#
# The decision is a comparison of two pieces of STATE, not a report:
#
#   promoted  = qlever-index/latest in object storage, written by
#               qlever-rebuild-index.sh as the last step of a promotion
#   serving   = /data/index/.serving, written by THIS script only after the
#               reloaded qlever has answered a query
#
# It used to branch on `.status == "success"` in last-run.json instead. That
# object is written best-effort from an EXIT trap with `|| true`, so a run
# could promote a new index and then fail, or be killed, before recording it.
# The next refresh then read a missing or stale status, concluded nothing had
# been promoted, and left the old index serving -- with no reload ever
# attempted, and nothing in the logs that looked like a failure. Run status is
# observability; it is not the transition authority (#73). It is still logged
# below, and still ignored when deciding.
#
# The two markers are deliberately separate:
#
#   .loaded   = written by qlever-load-index.sh -- "these bytes are on disk".
#               Keeps the loader's skip check cheap, so a host reboot does not
#               re-download a multi-gigabyte index.
#   .serving  = written here -- "qlever answered a query with them".
#
# A failed loader, a failed restart or a readiness timeout all leave .serving
# untouched, so the next run retries rather than believing a reload happened.
# The loader's atomic swap replaces the whole index directory, which discards
# .serving along with the old index: after new bytes land, nothing is confirmed
# until this script confirms it.
#
# Consequence worth knowing: the first rebuild after a host reboot bounces
# qlever once even if the index has not changed, because a boot-time load
# writes .loaded and never .serving. That is the deliberate cost of never
# recording "serving" without proof.
#
# No automatic rollback on a failed reload -- this only logs and exits
# non-zero (visible via `systemctl status qlever-rebuild-index.service` and
# journalctl). To manually revert, see the comment above `PREV_HASH` in
# qlever-rebuild-index.sh.
#
# Requires on the HOST: podman, jq, curl, systemctl.
set -euo pipefail

ENV_FILE="${QLEVER_ENV_FILE:-/etc/containers/systemd/scripts/minio.env}"
IMAGE="${QLEVER_REBUILD_IMAGE:-ghcr.io/packagegraph/qlever-rebuild:latest}"
# VolumeName= in qlever-data.volume. Mounted the same way qlever.container and
# qlever-index-load.container mount it, including :Z and the uid they share.
DATA_VOLUME="${QLEVER_DATA_VOLUME:-qlever-data}"
DATA_USER="${QLEVER_DATA_USER:-999}"
READY_URL="${QLEVER_READY_URL:-http://localhost:7001/?query=SELECT+%2A+WHERE+%7B+%3Fs+%3Fp+%3Fo+%7D+LIMIT+1}"
READY_ATTEMPTS="${QLEVER_READY_ATTEMPTS:-30}"
READY_INTERVAL="${QLEVER_READY_INTERVAL:-10}"

in_data_container() {
  podman run --rm --env-file "$ENV_FILE" \
    --user "$DATA_USER" \
    -v "${DATA_VOLUME}:/data:Z" \
    --entrypoint /bin/bash "$IMAGE" -c "$1"
}

# One container run for all three reads: the promotion, the confirmed-serving
# marker, and the run status we only log. Each is tolerant of absence --
# "missing" is a legitimate state here, not an error.
STATE=$(in_data_container '
  set -eu
  mc alias set pgraph "$MINIO_ENDPOINT" "$MINIO_ACCESS_KEY" "$MINIO_SECRET_KEY" --api S3v4 >/dev/null
  printf "promoted=%s\n" "$(mc cat "pgraph/${MINIO_BUCKET}/qlever-index/latest" 2>/dev/null || true)"
  printf "serving=%s\n"  "$(cat /data/index/.serving 2>/dev/null || true)"
  printf "status=%s\n"   "$(mc cat "pgraph/${MINIO_BUCKET}/qlever-index/last-run.json" 2>/dev/null \
                             | jq -r ".status // \"unknown\"" 2>/dev/null || true)"
')

PROMOTED=$(printf '%s\n' "$STATE" | sed -n 's/^promoted=//p' | head -1)
SERVING=$(printf '%s\n' "$STATE" | sed -n 's/^serving=//p' | head -1)
RUN_STATUS=$(printf '%s\n' "$STATE" | sed -n 's/^status=//p' | head -1)

echo "promoted index: ${PROMOTED:-<none>}"
echo "serving index:  ${SERVING:-<none>}"
# Logged for operators, deliberately not consulted for the decision.
echo "last run status (observability only): ${RUN_STATUS:-<unreadable>}"

if [ -z "$PROMOTED" ]; then
  echo "ERROR: no promoted index — qlever-index/latest is empty or unreadable" >&2
  exit 1
fi

if [ "$PROMOTED" = "$SERVING" ]; then
  echo "Already serving the promoted index — nothing to do"
  exit 0
fi

echo "Reloading qlever onto ${PROMOTED}"

if ! systemctl restart qlever-index-load.service; then
  echo "ERROR: index loader failed — not restarting qlever, marker left at ${SERVING:-<none>}" >&2
  exit 1
fi

if ! systemctl restart qlever.service; then
  echo "ERROR: qlever restart failed — marker left at ${SERVING:-<none>}" >&2
  exit 1
fi

# QLever mmaps its index at startup, so serving the new data requires the
# restart above to complete, not merely to be requested.
for _ in $(seq 1 "$READY_ATTEMPTS"); do
  if curl -fsS "$READY_URL" >/dev/null 2>&1; then
    # Record the identifier we decided to load, not a fresh read of `latest`:
    # a rebuild that promoted again while we were restarting must not be
    # credited to this reload.
    if ! in_data_container "printf '%s' '${PROMOTED}' > /data/index/.serving"; then
      echo "ERROR: qlever is ready on ${PROMOTED} but the serving marker could not be written" >&2
      echo "       the next run will reload again; this is safe but wasteful" >&2
      exit 1
    fi
    echo "qlever ready and serving ${PROMOTED}"
    exit 0
  fi
  sleep "$READY_INTERVAL"
done

echo "ERROR: qlever did not become ready after reloading ${PROMOTED}" >&2
echo "       serving marker left at ${SERVING:-<none>} so the next run retries" >&2
exit 1
