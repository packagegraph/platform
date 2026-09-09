# QLever stack — Podman Quadlet units

Runs the QLever SPARQL endpoint and its refresh jobs directly under systemd
via [Podman Quadlet](https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html),
as an alternative to the Kubernetes manifests in `deploy/base/qlever/` and
`deploy/overlays/{dev,prod}/jobs/rebuild-qlever-index.yaml`. Each unit here
maps to a Kubernetes equivalent — see the comment at the top of each file.

This set does **not** include Fuseki, Minio, or sparql-proxy. Minio is
assumed to already be reachable at `MINIO_ENDPOINT` (either the existing
Kubernetes-deployed instance or a standalone one); this set only manages
QLever.

## What's here

| File | Kubernetes equivalent |
|---|---|
| `qlever.container` | `deploy/base/qlever/deployment.yaml` (main container) + `service.yaml` |
| `qlever-index-load.container` | `deploy/base/qlever/deployment.yaml` (`load-index` initContainer) |
| `qlever-rebuild-index.container` + `.timer` | `deploy/overlays/{dev,prod}/jobs/rebuild-qlever-index.yaml` |
| `qlever-data.volume` | `deploy/base/qlever/pvc.yaml` |
| `qlever-rebuild-scratch.volume` | the CronJob's `tmp` emptyDir |
| `scripts/qlever-load-index.sh` | the initContainer's inline script |
| `scripts/qlever-rebuild-index.sh` | the CronJob's inline script, minus the `kubectl rollout` steps |
| `scripts/qlever-refresh-if-changed.sh` | the CronJob's `kubectl rollout restart/status` steps, reimplemented as a host-side systemd `ExecStartPost` |

The scripts are bind-mounted into their containers read-only rather than
baked into the `qlever-rebuild` image, so this set works against the image
already built by `make build-qlever-rebuild` / CI with no rebuild required.

## Install (system-wide; requires root)

```bash
install -d /etc/containers/systemd/scripts
install -m 644 deploy/quadlet/*.container deploy/quadlet/*.volume /etc/containers/systemd/
install -m 755 deploy/quadlet/scripts/*.sh /etc/containers/systemd/scripts/
install -m 600 deploy/quadlet/scripts/*.env /etc/containers/systemd/scripts/
install -m 644 deploy/quadlet/qlever-rebuild-index.timer /etc/systemd/system/

# Real credentials -- do not leave the CHANGE_ME placeholders in place.
${EDITOR:-vi} /etc/containers/systemd/scripts/minio.env
printf 'QLEVER_ACCESS_TOKEN=%s\n' "$(openssl rand -hex 32)" \
  > /etc/containers/systemd/scripts/qlever.env
chown root:root /etc/containers/systemd/scripts/*.env
chmod 600 /etc/containers/systemd/scripts/*.env

systemctl daemon-reload
systemctl enable --now qlever.service          # pulls in qlever-index-load.service first
systemctl enable --now qlever-rebuild-index.timer
```

`qlever.service` requires `qlever-index-load.service` (see its `Requires=`/
`After=`), so starting it also runs the loader first. If no index has ever
been built, the loader fails fast ("No index available in Minio") — run
`systemctl start qlever-rebuild-index.service` once to build and promote the
first one, then start `qlever.service`.

## Host dependencies

`qlever-refresh-if-changed.sh` runs on the host (not in a container) and
needs `podman`, `jq`, and `curl` installed there — everything else runs
inside the `ghcr.io/packagegraph/qlever-rebuild:latest` image, which already
bundles `mc`, `jq`, and the `qlever-index` tool.

Rootless deployment (`~/.config/containers/systemd/` + `systemctl --user`)
works the same way, except `qlever-refresh-if-changed.sh`'s
`systemctl restart` calls need `systemctl --user restart` instead — edit the
script if you deploy rootless.

## Deliberate differences from the Kubernetes version

- **Loopback-only exposure.** `qlever.container` publishes `7001` to
  `127.0.0.1` only. The Kubernetes Service is ClusterIP — reachable
  externally only through the TLS+basic-auth `sparql-proxy`. Put an
  equivalent reverse proxy in front before publishing this any wider than
  loopback (see `deploy/base/sparql-proxy/configmap.yaml` for a model; not
  included in this set).
- **No automatic rollback on a failed reload.** The Kubernetes CronJob
  reverts the `latest` pointer in Minio and restarts the Deployment again if
  the post-rebuild rollout doesn't become ready within 5 minutes.
  `qlever-refresh-if-changed.sh` only logs and exits non-zero (visible via
  `systemctl status qlever-rebuild-index.service`) — see the comment above
  `PREV_HASH` in `qlever-rebuild-index.sh` for the manual revert command.
- **Health check is a port-listen check, not a SPARQL query.** The k8s
  readinessProbe issues a real SPARQL query via kubelet's own HTTP client
  (no in-container binary needed). Podman's `HealthCmd` execs inside the
  container, and curl/wget presence in the upstream
  `docker.io/adfreiburg/qlever` image is unverified, so `qlever.container`
  instead greps `/proc/net/tcp` for port 7001. Swap in a curl-based query
  check if you confirm the image has curl.
- **`EnvironmentFile=` instead of inline `Environment=` for credentials.**
  `MINIO_ENDPOINT`/`MINIO_BUCKET`/`MINIO_ACCESS_KEY`/`MINIO_SECRET_KEY` are
  consumed identically by two container units *and* the host-side refresh
  script; one shared file avoids three independently-maintained copies of
  the same secret material.
