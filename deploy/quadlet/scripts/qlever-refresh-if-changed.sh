#!/bin/bash
# Runs on the HOST (systemd ExecStartPost for qlever-rebuild-index.service),
# NOT inside a container. After a rebuild run completes, check whether it
# actually promoted a new index (status == "success" in last-run.json) and,
# if so, reload qlever-index-load.service (pulls the new index onto local
# disk) then bounce qlever.service -- QLever mmaps its index at startup, so a
# restart is required to pick up new data, mirroring the `strategy: Recreate`
# behavior of the Kubernetes Deployment this replaces.
#
# A run that exits 0 without promoting (STATUS=unchanged, or a completeness
# gate rejected the candidate) must NOT bounce qlever.service for no reason.
#
# No automatic rollback on a failed reload -- unlike the Kubernetes CronJob's
# rollout-restart/rollout-status/revert loop, this only logs and exits
# non-zero (visible via `systemctl status qlever-rebuild-index.service` and
# journalctl). To manually revert, see the comment above `PREV_HASH` in
# qlever-rebuild-index.sh.
#
# Requires on the HOST: podman, jq, curl.
set -euo pipefail

ENV_FILE="/etc/containers/systemd/scripts/minio.env"
IMAGE="ghcr.io/packagegraph/qlever-rebuild:latest"

STATUS=$(podman run --rm --env-file "$ENV_FILE" --entrypoint /bin/bash "$IMAGE" -c '
  set -eu
  mc alias set pgraph "$MINIO_ENDPOINT" "$MINIO_ACCESS_KEY" "$MINIO_SECRET_KEY" --api S3v4 >/dev/null
  mc cat "pgraph/${MINIO_BUCKET}/qlever-index/last-run.json"
' | jq -r '.status // "unknown"')

echo "qlever-rebuild-index last run status: $STATUS"

if [ "$STATUS" != "success" ]; then
  echo "No new index promoted — leaving qlever running as-is"
  exit 0
fi

echo "New index promoted — reloading qlever"
systemctl restart qlever-index-load.service
systemctl restart qlever.service

# Wait for the readiness endpoint (mirrors the k8s rollout-status wait/timeout)
for _ in $(seq 1 30); do
  if curl -fsS "http://localhost:7001/?query=SELECT+%2A+WHERE+%7B+%3Fs+%3Fp+%3Fo+%7D+LIMIT+1" >/dev/null 2>&1; then
    echo "qlever ready with new index"
    exit 0
  fi
  sleep 10
done

echo "ERROR: qlever did not become ready within 5 minutes after reload" >&2
exit 1
