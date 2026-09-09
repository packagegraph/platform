# QLever stack — Podman Quadlet units

Runs the QLever SPARQL endpoint and its refresh jobs directly under systemd
via [Podman Quadlet](https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html),
as an alternative to the Kubernetes manifests in `deploy/base/qlever/` and
`deploy/overlays/{dev,prod}/jobs/rebuild-qlever-index.yaml`. Each unit here
maps to a Kubernetes equivalent — see the comment at the top of each file.

This set does **not** include Fuseki or Minio. Minio is assumed to already
be reachable at `MINIO_ENDPOINT` (either the existing Kubernetes-deployed
instance or a standalone one). It does include an optional public HTTPS
reverse proxy (`sparql-proxy.container` and friends) in front of QLever --
see "Public SPARQL reverse proxy" below.

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
| `sparql-proxy.container` + `sparql-proxy/nginx.conf` | `deploy/base/sparql-proxy/{deployment,configmap}.yaml`, adapted: proxies to QLever instead of Fuseki, no basic auth, tuned for throughput instead of a conservative rate limit |
| `sparql-proxy-certbot-renew.container` + `.timer` | no k8s equivalent (that deployment's TLS comes from cert-manager) |
| `sparql-proxy-certs.volume`, `sparql-proxy-webroot.volume` | no k8s equivalent |
| `scripts/sparql-proxy-reload-if-renewed.sh` | no k8s equivalent |

The scripts are bind-mounted into their containers read-only rather than
baked into the `qlever-rebuild` image, so this set works against the image
already built by `make build-qlever-rebuild` / CI with no rebuild required.

## Dedicated data disk

`qlever-data.volume` and `qlever-rebuild-scratch.volume` bind-mount a
dedicated disk at `/var/lib/packagegraph` (via `Device=`/`Type=none`/
`Options=bind`) rather than using default Podman-managed storage under `/`
-- a full rebuild's scratch space alone can approach 80G (see the comment in
`qlever-rebuild-scratch.volume`), which will not fit on a typical root
filesystem. If your host has no such disk, delete those three lines from
both `.volume` files to fall back to normal Podman storage.

To provision the disk (adjust the device path for your host):

```bash
mkfs.xfs -L qlever-data /dev/sdb
UUID=$(blkid -s UUID -o value /dev/sdb)
echo "UUID=$UUID /var/lib/packagegraph xfs defaults 0 2" >> /etc/fstab
mkdir -p /var/lib/packagegraph
mount -a

mkdir -p /var/lib/packagegraph/qlever-data /var/lib/packagegraph/qlever-rebuild-scratch

# SELinux (skip if not enforcing): label the tree for container access,
# persisted so it survives future relabels.
semanage fcontext -a -t container_file_t "/var/lib/packagegraph(/.*)?"
restorecon -Rv /var/lib/packagegraph
```

## Install (system-wide; requires root)

```bash
install -d /etc/containers/systemd/scripts
install -m 644 deploy/quadlet/*.container deploy/quadlet/*.volume /etc/containers/systemd/
install -m 755 deploy/quadlet/scripts/*.sh /etc/containers/systemd/scripts/
install -m 600 deploy/quadlet/scripts/*.env /etc/containers/systemd/scripts/
install -m 644 deploy/quadlet/qlever-rebuild-index.timer deploy/quadlet/sparql-proxy-certbot-renew.timer /etc/systemd/system/
install -d /etc/containers/systemd/sparql-proxy
install -m 644 deploy/quadlet/sparql-proxy/nginx.conf /etc/containers/systemd/sparql-proxy/

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

## Using Scaleway Object Storage as MINIO_ENDPOINT

`mc` (and the scripts here) always address objects as `alias/bucket/key` —
path-style. Scaleway's per-bucket "Bucket Endpoint"
(`https://<bucket>.s3.<region>.scw.cloud`, virtual-hosted-style, bucket
baked into the hostname) does not work as `MINIO_ENDPOINT` here: combined
with a path-style `bucket/key` reference, the bucket name ends up doubled
(`<bucket>.s3.<region>.scw.cloud/<bucket>/key`), and every operation fails
with a misleading `Object does not exist` / `Access Denied`. Use the
regional "API Endpoint" instead (`https://s3.<region>.scw.cloud`, no bucket
in the hostname) — `scw object bucket get <bucket>` prints both; take
`APIEndpoint`, not `BucketEndpoint`.

If provisioning a dedicated IAM application/API key for this host (as
opposed to reusing an existing one), two things are easy to get wrong and
both fail *silently permission-shaped* rather than obviously:
- The policy rule needs `ObjectStorageBucketsRead` in addition to
  `ObjectStorageObjectsRead`/`ObjectStorageObjectsWrite` — the Buckets/
  Objects split mirrors AWS's `s3:ListBucket` vs `s3:GetObject` distinction,
  and `mc ls`/`mc mirror`/`mc du` all need the former.
- The API key's `default_project_id` must match the policy's scoped
  project — Scaleway's S3 API resolves the acting project from the key
  itself, not purely from the IAM policy's `project-ids`. A key created
  without `default-project-id=<project>` explicitly set defaults to the
  organization's default project, which silently doesn't match a policy
  scoped to a different project, and every request comes back
  `Insufficient permissions` even though `scw iam policy get` shows the
  rule correctly attached.

## Public SPARQL reverse proxy (TLS, read-only)

`sparql-proxy.container` puts an nginx reverse proxy on 80/443 in front of
`qlever.container`'s loopback-only endpoint, with a real Let's Encrypt
certificate. Unlike `deploy/base/sparql-proxy`'s Fuseki proxy, this one has
**no authentication** -- it relies on QLever itself, not the proxy, for
the read-only guarantee: QLever unconditionally rejects SPARQL Update
without a valid `access-token` (verified: `HTTP 403` with none supplied),
and the proxy never has or forwards that token. The one nginx-level rule
(rejecting requests with an `access-token` query param) is defense in
depth on top of that, not the actual guarantee -- it doesn't inspect POST
bodies, so a client attempting Update via a POST body would still reach
QLever and still get rejected there instead of at the proxy.

Set `server_name`/`-d` in `sparql-proxy/nginx.conf` and the bootstrap
command below to your own domain; DNS must already point at the host
before requesting a certificate.

### One-time certificate bootstrap

Must happen **before** `sparql-proxy.service` first starts (needs port 80
free for the ACME HTTP-01 standalone challenge; all *renewals* afterward
use `--webroot` instead, served by nginx itself, so they don't need to
stop it):

```bash
systemctl start sparql-proxy-certs-volume.service sparql-proxy-webroot-volume.service
podman run --rm -p 80:80 \
  -v sparql-proxy-certs:/etc/letsencrypt \
  docker.io/certbot/certbot:latest \
  certonly --standalone --non-interactive --agree-tos \
  -m <your-email> \
  -d <your-domain>

# nginx-unprivileged (UID 101) can't read certbot's default 0700/0600
# permissions -- see the gotcha below. Fix once for the bootstrap cert;
# sparql-proxy-certbot-renew.container's --deploy-hook re-applies this on
# every future renewal automatically.
chmod 755 /var/lib/packagegraph/sparql-proxy-certs/live /var/lib/packagegraph/sparql-proxy-certs/archive
chmod 644 /var/lib/packagegraph/sparql-proxy-certs/archive/<your-domain>/privkey1.pem

systemctl start sparql-proxy.service
systemctl enable --now sparql-proxy-certbot-renew.timer
```

### Gotcha: nginx-unprivileged can't read Let's Encrypt's default permissions

Certbot creates `/etc/letsencrypt/{live,archive}` as `0700` root-only and
the private key as `0600`, on both initial issuance and every renewal --
deliberate isolation on a system where other users might exist. Since
`sparql-proxy.container` deliberately runs as `nginx-unprivileged` (a
fixed non-root UID, not the more common root-master/non-root-worker
pattern used by the regular `nginx` image), it cannot read those files
without loosening those permissions. Not a real weakening on this
single-purpose host: nothing but root and containers explicitly granted
this volume can reach these files regardless of the mode bits. This is
why the renewal container's `--deploy-hook` does two things, not one --
see the comment in `sparql-proxy-certbot-renew.container`.

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
