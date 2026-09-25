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
| `firewall/nftables.conf` | no k8s equivalent (network policy would be the analogue) |
| `firewall/sync-trusted-source.sh` + `.service` + `.timer` | no k8s equivalent |
| `fail2ban/` (jail.local, filter.d/, action.d/, nft-shared-ban.sh) | no k8s equivalent -- native package install, not a quadlet unit; see "Traffic filtering and abuse detection" |
| `collectors/pg-collect@.container` + `collectors/scripts/*.sh` + `collectors/timers/*.timer` | `deploy/overlays/{dev,prod}/jobs/collect-*.yaml` -- see "Package collectors" |
| `pg-collect-scratch.volume` | the CronJobs' `tmp` emptyDir |
| `podman-image-prune.service` + `.timer` | no k8s equivalent (kubelet's own image GC is the analogue) |
| `failure/pg-unit-failed@.service` + `scripts/unit-failed.sh` + `enrichers/dropins/` | no k8s equivalent (a Job's failure is visible in the cluster's own status; a systemd oneshot's is not) -- see "Failure visibility and per-enricher timeouts" |

The scripts are bind-mounted into their containers read-only rather than
baked into the `qlever-rebuild` image, so this set works against the image
already built by `make build-qlever-rebuild` / CI with no rebuild required.

## Dedicated data disk

`qlever-data.volume`, `qlever-rebuild-scratch.volume`, and
`pg-collect-scratch.volume` bind-mount a dedicated disk at
`/var/lib/packagegraph` (via `Device=`/`Type=none`/`Options=bind`) rather
than using default Podman-managed storage under `/` -- a full rebuild's
scratch space alone can approach 80G (see the comment in
`qlever-rebuild-scratch.volume`), and collector scratch space has its own
history of filling a root disk (see "Incident: root disk full" in "Package
collectors" below), neither of which will fit on a typical root
filesystem. If your host has no such disk, delete those three lines from
each `.volume` file to fall back to normal Podman storage.

To provision the disk (adjust the device path for your host):

```bash
mkfs.xfs -L qlever-data /dev/sdb
UUID=$(blkid -s UUID -o value /dev/sdb)
echo "UUID=$UUID /var/lib/packagegraph xfs defaults 0 2" >> /etc/fstab
mkdir -p /var/lib/packagegraph
mount -a

mkdir -p /var/lib/packagegraph/qlever-data /var/lib/packagegraph/qlever-rebuild-scratch /var/lib/packagegraph/pg-collect-scratch

# SELinux (skip if not enforcing): label the tree for container access,
# persisted so it survives future relabels.
semanage fcontext -a -t container_file_t "/var/lib/packagegraph(/.*)?"
restorecon -Rv /var/lib/packagegraph
```

## Install (system-wide; requires root)

**To re-sync a host that already has this stack, use `host-sync.sh`** rather
than replaying the blocks below:

```bash
./deploy/quadlet/host-sync.sh          # as root, on the host
```

It applies the collector/enricher templates, scripts, timers and the
per-enricher timeout drop-ins, installs the failure notifier, removes the
retired repology files, reloads systemd, and prints a verification block.
It is idempotent and deliberately starts nothing -- a collector mid-run is
left to finish, and enabling timers stays an explicit separate step. Files
are renamed into place rather than written through, because these scripts
are bind-mounted into running containers (see "Atomic script deploy" in the
incident notes). `deploy/quadlet/tests/test_host_sync.py` covers it.

The blocks below remain the reference for what a *first* install needs,
including the one-time host bootstrap (disk, SELinux, firewall, certs) that
`host-sync.sh` does not touch.

```bash
install -d /etc/containers/systemd/scripts
install -m 644 deploy/quadlet/*.container deploy/quadlet/*.volume /etc/containers/systemd/
install -m 755 deploy/quadlet/scripts/*.sh /etc/containers/systemd/scripts/
install -m 600 deploy/quadlet/scripts/*.env /etc/containers/systemd/scripts/
install -m 644 deploy/quadlet/qlever-rebuild-index.timer deploy/quadlet/sparql-proxy-certbot-renew.timer /etc/systemd/system/
install -m 644 deploy/quadlet/podman-image-prune.service deploy/quadlet/podman-image-prune.timer /etc/systemd/system/
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
systemctl enable --now podman-image-prune.timer
```

`qlever.service` requires `qlever-index-load.service` (see its `Requires=`/
`After=`), so starting it also runs the loader first. If no index has ever
been built, the loader fails fast ("No index available in Minio") — run
`systemctl start qlever-rebuild-index.service` once to build and promote the
first one, then start `qlever.service`.

## Host dependencies

`qlever-refresh-if-changed.sh` runs on the host (not in a container) and
needs `podman`, `jq`, and `curl` installed there — everything else runs
inside the `ghcr.io/packagegraph/qlever-rebuild` image, which already
bundles `mc`, `jq`, and the `qlever-index` tool.

It decides whether to reload by comparing two pieces of state, never by
reading a status report:

| Marker | Written by | Means |
| --- | --- | --- |
| `qlever-index/latest` | `qlever-rebuild-index.sh`, last step of a promotion | promoted |
| `/data/index/.loaded` | `qlever-load-index.sh`, after its atomic swap | bytes are on disk |
| `/data/index/.serving` | `qlever-refresh-if-changed.sh`, after a readiness check | qlever answered with them |

The script is wired as `ExecStopPost=` on `qlever-rebuild-index.service`, not
`ExecStartPost=`: systemd runs `ExecStartPost=` only after a successful
`ExecStart=`, and the run that most needs a reload is one that promoted and
then failed. `TimeoutStopSec=` is raised to outlast the script's own readiness
poll, which `ExecStopPost=` is bounded by.

A reload happens whenever `latest` differs from `.serving`, and `.serving`
advances only after the reloaded server answers a query — so a failed loader,
a failed restart or a readiness timeout all leave it retryable. `.loaded` is
kept separate so a host reboot does not re-download the whole index. The one
visible consequence: the first rebuild after a reboot bounces qlever once even
if nothing changed, because a boot-time load writes `.loaded` and never
`.serving`.

Rootless deployment (`~/.config/containers/systemd/` + `systemctl --user`)
works the same way, except `qlever-refresh-if-changed.sh`'s
`systemctl restart` calls need `systemctl --user restart` instead — edit the
script if you deploy rootless.

## Image tags and auto-update

**Collector checkpoint rollout exception:** deploying the checkpoint-enabled
collector requires the [coordinated cutover](collectors/checkpoint-cutover.md).
Freeze before publishing the first such image, install matching host scripts,
and pin both collector templates to its immutable image digest with auto-update
disabled. Repository CI does not update host-mounted scripts. Subsequent
collector upgrades and rollbacks must update the image/script pair together.
The floating-tag history below still applies to the other units; it is not a
collector rollout procedure.

`.github/workflows/images.yml` builds every image for `linux/amd64` and
`linux/arm64` — each on a runner of its own architecture, then assembled
into a manifest list — and two callers decide what tag the result gets:

| Trigger | Tags published | Moves production? |
|---|---|---|
| Merge to `main` (`ci.yml`) | `devel-latest`, `main-<sha>` | no |
| `v*` tag (`release.yml`) | `<tag>`, `latest` | yes |

`devel-latest` floats; `main-<sha>` is immutable and exists to roll back
to. `latest` moves only when someone cuts a release, so a merge to `main`
can never reach production on its own.

**This host currently tracks `devel-latest`**, because no `v*` tag has ever
been cut and `latest` therefore points at a hand-assembled image. Once
`v0.1.0` exists, flip the `Image=` lines in `qlever-index-load.container`,
`qlever-rebuild-index.container`, `collectors/pg-collect@.container`,
`collectors/pg-collect-rhel@.container`, and `enrichers/pg-enrich@.container`
back to `:latest` so production stops following `main`.

`AutoUpdate=registry` belongs only on units whose tag is meant to move.
Units pinned to an exact upstream version — `qlever.container`,
`sparql-proxy.container`, `sparql-proxy-certbot-renew.container` — do not
carry it: on a pinned tag the line is a no-op that reads as protection.
`sparql-proxy-certbot-renew.container` is the one worth being deliberate
about, since it mounts the Let's Encrypt store read-write and runs
unattended; bump its certbot pin by hand.

Pull-request CI still builds amd64-only and pushes nothing, so the arm64
build cost is only paid once a change reaches `main`.

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

## Traffic filtering and abuse detection

`firewall/nftables.conf` is a baseline packet filter (default-deny INPUT,
allow 22/80/443, coarse new-connection rate limits) loaded by the host's
native `nftables.service` -- without it, nothing filters inbound traffic
at all (Podman's own nftables rules are pure NAT/isolation plumbing,
`policy accept` throughout). Install:

```bash
install -m 644 deploy/quadlet/firewall/nftables.conf /etc/sysconfig/nftables.conf
systemctl enable --now nftables.service
```

**fail2ban is a native package install, not a container.** It started as
`crazymax/fail2ban` (containerized) and was deliberately moved off that
image: any containerized process hits the same SELinux confinement
crossing a container-native image doesn't fix -- reading a pre-existing,
security-sensitive host file (`/var/log/secure`, labeled `var_log_t`
for rsyslog's own write access) under a foreign SELinux label requires
either relabeling the file (risks breaking rsyslog itself) or disabling
SELinux confinement for that container. Neither is a fair price when a
package install has neither problem: `fail2ban-selinux` ships the proper
policy, `fail2ban-systemd` provides real journald support, and both
install for free from EPEL.

```bash
dnf install -y https://dl.fedoraproject.org/pub/epel/epel-release-latest-10.noarch.rpm
dnf install -y fail2ban fail2ban-systemd fail2ban-selinux

install -d /etc/fail2ban/filter.d /etc/fail2ban/action.d
install -m 644 deploy/quadlet/fail2ban/jail.local /etc/fail2ban/jail.d/packagegraph.conf
install -m 644 deploy/quadlet/fail2ban/filter.d/sparql-proxy-abuse.conf /etc/fail2ban/filter.d/
install -m 644 deploy/quadlet/fail2ban/action.d/nftables-shared-set.conf /etc/fail2ban/action.d/
install -m 755 deploy/quadlet/fail2ban/nft-shared-ban.sh /etc/fail2ban/
restorecon -Rv /etc/fail2ban/

systemctl enable --now fail2ban.service
```

fail2ban watches `sshd` (journald, `_SYSTEMD_UNIT=sshd.service`) and
`sparql-proxy` (journald, `_SYSTEMD_UNIT=sparql-proxy.service` --
`sparql-proxy.container`'s stdout, captured there by Podman's default
log driver) and bans into the *same* `banned_ips`/`banned_ips6` nftables
sets `firewall/nftables.conf` defines, via a custom action
(`fail2ban/action.d/nftables-shared-set.conf` + `nft-shared-ban.sh`)
rather than fail2ban's own default chain -- so `nft list set inet filter
banned_ips` shows every ban regardless of what issued it, and a future
CrowdSec-based alternative (not yet built; would write to the same sets)
stays a straight swap rather than a parallel, conflicting system.

**`trusted_ips` (in `nftables.conf`) is not a ban whitelist.** It only
bypasses the connection-rate pre-filter, and it's checked *after*
`banned_ips` in the input chain -- an explicit ban still wins, and it has
no visibility into nginx's own application-layer rate limiting either.
The actual ban-prevention mechanism is fail2ban's `ignoreip`
(`jail.local`'s `[DEFAULT]` section) -- `firewall/sync-trusted-source.sh`
keeps *both* in sync from the same `TRUSTED_HOSTNAMES` (hostnames and/or
raw IPs) so a dynamic-DNS-tracked admin source doesn't go stale in
either place, using `fail2ban-client set <jail> addignoreip/delignoreip`
since `ignoreip` has no bulk-replace the way an nftables set flush does.
An `ignoreip`'d source can never be banned by *any* jail -- narrow it to
known admin sources, never a broad range.

Install `sync-trusted-source.sh` + `.service` + `.timer` the same way as
`qlever-refresh-if-changed.sh` (see the main Install section above), and
fill in real values in `scripts/trusted-ips.env` (quoting matters -- it's
`source`d as shell, so an unquoted multi-word value parses as a second
command).

## Package collectors

`collectors/` ports the 47 `collect-*` CronJobs to one templated quadlet unit
(`pg-collect@.container`) plus one script and one `.timer` per collector
(`collectors/scripts/<name>.sh`, `collectors/timers/pg-collect-<name>.timer`).
The template is instance-agnostic -- `%i` is the collector name, and it just
runs `/etc/containers/systemd/scripts/collectors/<name>.sh` bind-mounted
read-only into the same `ghcr.io/packagegraph/etl:latest` image the
Kubernetes jobs use. See `pg-collect@.container`'s header comment for why
`Network=host` is required.

**No writable Fuseki on this host.** The Kubernetes jobs either query
Fuseki for discovery (`npm`, `pypi`, `maven`, ...) or, for the `-full`
jobs, load directly into it after collecting. This host only runs QLever,
which is read-only (`sparql-proxy.container`'s comment: it unconditionally
rejects SPARQL Update). So every collector here does collect + upload to
Minio's `nt-output/` prefix only -- the same thing every job does anyway as
its Minio-archival step -- and drops the direct-Fuseki-load step entirely.
`qlever-rebuild-index.timer` already rebuilds and promotes the QLever index
from that same `nt-output/` prefix nightly, so uploaded data reaches QLever
on its next scheduled rebuild, not synchronously. Discovery-mode collectors
still need *some* SPARQL endpoint to query, so `pg-collect@.container` points
`FUSEKI_ENDPOINT` (env var name kept as-is so the scripts stay copy-paste
identical to the Kubernetes jobs) at `http://127.0.0.1:7001` -- the local
QLever instance -- with `SPARQL_BACKEND=qlever` and `QLEVER_ACCESS_TOKEN`
(from `scripts/qlever.env`, already installed for the rebuild job) so
`pg-collect`'s discovery queries (plain SPARQL 1.1 SELECT) work against it.

**13 of the 47 upstream CronJobs are dead stubs, not ported.**
`collect-{centos-stream-10,centos-stream-10-aarch64,centos-stream-9,centos-stream-9-aarch64,fedora-43,fedora-43-aarch64,fedora-44,fedora-rawhide,fedora-rawhide-aarch64,fedora-riscv64,opensuse-tw,opensuse-tw-aarch64,opensuse-tw-riscv64}.yaml`
set an `RPM_REPOS` env var but have no `command`/`args` that consumes it --
they have never produced output in Kubernetes either. Real RPM data for
centos-stream-9 and fedora-43 comes from the separate, fully self-contained
`-full` jobs, which *are* ported (as `centos-stream-9-full` and
`fedora-43-full`). `fedora-44-full` was added directly here 2026-09-10
(no Kubernetes equivalent -- Fedora 44 hadn't released when those jobs
were written; both `x86_64`/`aarch64` under `releases/44` confirmed live
before writing it), modeled on `fedora-43-full.sh`. Fedora rawhide/riscv64
and all three opensuse-tumbleweed arches still have no working collector
anywhere in this repo -- writing one is a separate task, not a
quadlet-porting one.
`openwrt-2410-full`'s Kubernetes version uses a git-clone `initContainer`;
the `etl` image has no `git` binary (see `etl/Containerfile`), so its script
fetches the same 4 feed repos as GitHub branch tarballs instead, the same
approach `gentoo`/`void` already use.

This leaves 35 working collectors (36 timers -- `debian-trixie-riscv64` and
`fedora-44-full` were added directly here, no Kubernetes equivalent exists
for either: riscv64 is now a release architecture in trixie's main
archive, and Fedora 44 hadn't released when the Kubernetes jobs were
written). Two collectors (both ported from Kubernetes) were decommissioned
2026-09-10: `debian-sid-riscv64` -- Debian sid isn't tracked at all, by
explicit decision, not a technical limitation; its repo URL had also gone
stale (riscv64 for sid was promoted from `debian-ports` to the main
archive, same as trixie, so the old `debian-ports` URL 404'd). And
`debian-trixie` (the thin, amd64-only variant) -- it shared its graph URI
(`.../graph/debian/trixie`) with `debian-trixie-full`, and since
`debian-trixie-full`'s Saturday run strictly subsumes it (amd64+arm64,
sources, build-deps, maintainers, Salsa vs. amd64 basic packages only),
`debian-trixie`'s later Monday run was silently overwriting the richer
Saturday data every week -- caught by an external SPARQL-endpoint audit
that noticed `debian/trixie` had normalized down to ~92 triples/package
(matching bookworm/bullseye) despite retaining its full package count. The
corpus-wide `MAX_LOSS_PCT=25` promotion gate in `qlever-rebuild-index.sh`
couldn't see this: total triples still grew that week from unrelated new
graphs, masking the 38% loss in this one. Worth remembering for any future
per-graph collision: this gate is corpus-wide only, not per-graph.
Each has its own `.timer`, deliberately spread by both hour and
day: each `OnCalendar=` is chosen to sample its upstream at roughly twice
its actual release cadence (a Nyquist-style floor -- undersampling a
rolling-release source at a once-a-week schedule misses most of what
changed between runs), not an arbitrary stagger. See each timer's `#
Cadence:` comment for the specific reasoning; broadly: rolling/high-volume
upstreams (Arch, Gentoo, Void, nixpkgs, OSV, npm, PyPI, crates.io,
Maven Central, NuGet, RubyGems) run daily; moderate-churn ones (Hex,
Hackage, Go modules, Homebrew, conda-forge, CentOS Stream) run three times
a week; stable/point-release or low-churn upstreams (Debian/Ubuntu stable
suites, CRAN, CPAN, Flathub, Chocolatey, snap, FreeBSD ports, Fedora
Bodhi-gated updates, OpenWrt stable) stay weekly. There's no
cross-collector ordering dependency, so hour-of-day is just kept spread out
to avoid every timer firing at once. OSV is one exception to the
one-timer-per-collector shape: its 12 ecosystems share a single graph and
run sequentially in one script/timer instead of 12, since a single host has
no need for the hourly staggering Kubernetes uses to avoid concurrent hits
to OSV's GCS bucket.

**`alma-9-full`, `alma-10-full`, `rocky-9-full`, `rocky-10-full`** were
added 2026-09-10, no Kubernetes equivalent. `derive_comparison.rs` /
`derive_releases.rs` already implement RHEL-rebuild drift analysis expecting
graph URIs `almalinux/9`, `almalinux/10`, `rocky/9`, `rocky/10` (paired
against `rhel/9`/`rhel/10`), and `rpm.rs`'s `distro_display_name()` already
maps `almalinux`/`rocky` to display names -- the feature existed but nothing
ever populated those four graphs. Modeled directly on
`centos-stream-10-full.sh` (same `rpm-full` subcommand, same BaseOS
x86_64+aarch64 URL pair, same periodic cache-sync loop baked in from
creation -- see the fedora-43-full incident below for why that loop
matters). Weekly cadence (stable RHEL rebuild, low churn), staggered at
04:00 UTC across Mon/Tue/Wed/Thu to avoid clustering with the existing
02:00-03:00 rpm-full jobs.

**`rhel-9-full`, `rhel-10-full`** were added 2026-09-11, filling in the
`rhel/9`/`rhel/10` graphs the alma/rocky drift analysis above already
expected as its comparison target. Authenticates via `rpm-full`'s TLS
client-cert flags (`--sslclientcert`/`--sslclientkey`/`--sslcacert`,
mirroring the pre-existing `Rpm` subcommand's own flags of the same
name): the scripts glob `/etc/pki/entitlement/[0-9]*.pem` for the cert
(its filename embeds a serial number that rotates on renewal) and point
`--sslcacert` at `/etc/rhsm/ca/redhat-uep.pem`. Weekly cadence, Fri/Sat
04:00 UTC (continuing the alma/rocky stagger). See
`docs/rhel-collection.md` for the TLS setup and
`docs/superpowers/specs/2026-09-10-rhel-collection-rhsa-correlation-design.md`
for how RHSA advisories get correlated against packages collected here.

These two use their own template, `pg-collect-rhel@.container`, instead of
the shared `pg-collect@.container` every other collector uses. First
production run (2026-09-11) failed immediately: the entitlement/CA
bind mounts hit SELinux denials inside the container (`/etc/pki/entitlement`
and `/etc/rhsm/ca` carry `cert_t`/`rhsmcertd_config_t`, not a type
container processes can read, and Quadlet's plain `Volume=` doesn't
auto-relabel). `pg-collect-rhel@.container` adds
`PodmanArgs=--security-opt label=disable` to work around this --
relabeling the real files with `:z`/`:Z` was ruled out since that
permanently changes their SELinux context and could break `rhsmcertd`'s
own access to them. Scoped to a separate template (rather than added to
the shared one) so this SELinux-confinement relaxation only applies to
these two collectors, not all 40+.

Initial timer/collector installation is shown below for reference. For a
checkpoint-enabled release, install the digest-pinned templates and matching
RPM wrappers through the [coordinated cutover](collectors/checkpoint-cutover.md)
instead; do not overwrite that installation with the floating templates below.

```bash
install -m 644 deploy/quadlet/collectors/pg-collect@.container deploy/quadlet/collectors/pg-collect-rhel@.container /etc/containers/systemd/
install -d /etc/containers/systemd/scripts/collectors
install -m 755 deploy/quadlet/collectors/scripts/*.sh /etc/containers/systemd/scripts/collectors/
install -m 644 deploy/quadlet/collectors/timers/*.timer /etc/systemd/system/

systemctl daemon-reload
systemctl enable --now $(cd /etc/systemd/system && ls pg-collect-*.timer)
```

Requires `scripts/minio.env` and `scripts/qlever.env` already installed
(see the main Install section above) -- both are shared with the QLever
units, no separate credentials needed. Run one collector immediately
without waiting for its timer with `systemctl start pg-collect@<name>.service`
(e.g. `pg-collect@npm.service`); watch it with `journalctl -u pg-collect@<name>.service -f`.

### Collector checkpoints (rpm-full)

`rpm-full` collectors checkpoint each item's derived triples under
`<cache_dir>/output/<generation>/`, so a run killed by `TimeoutStartSec`
resumes instead of restarting. A generation is created at run start, reused
by retries of an interrupted run, and retired by `pg-collect checkpoint
commit` — which the wrapper runs only after `upload-nt.sh` succeeds, so a
run that collected everything but failed to publish stays resumable.

Both long stages are checkpointed: Koji enrichment per NVR, and spec
collection per SRPM. Each prints its own hit/miss line, so
`journalctl -u pg-collect@<name>.service` shows where a resumed run picked
up.

Deploying a checkpoint-enabled collector is **not** a plain image update —
the wrappers are host-mounted while the image auto-updates, so the two can
drift apart. Use the coordinated cutover in
[`collectors/checkpoint-cutover.md`](collectors/checkpoint-cutover.md).

Checkpoints are deliberately **local-only** and excluded from every Minio
mirror with `--exclude 'output/*'`. The pipeline's Minio credentials can PUT
but not delete (verified 2026-09-12: `mc rm` returns `Access Denied`), so
mirrored checkpoints could never be pruned and would accumulate forever. The
scratch volume is a bind mount on the data disk and already survives
container restarts and host reboots, which is what the failure mode this
feature addresses actually needs. Losing the host costs one full
re-collection.

A generation is also not reused past `MAX_GENERATION_AGE_DAYS` (30). That is
not a retry budget — these collectors run weekly, so a legitimate resume
lands 7 days later — it is the backstop for a host running a
checkpoint-enabled image against wrappers that never call `checkpoint
commit`. Without it such a generation replays forever; with it the damage is
one stale cycle and a logged warning naming the cutover doc.

Bumping `SPEC_SCHEMA_VERSION` or `KOJI_SCHEMA_VERSION` invalidates that
stage's checkpoints. Bump one whenever that stage's emitted triples change,
including via shared serialization or ontology helpers — a stale fragment is
indistinguishable from a correct one.

`KOJI_RPC_CACHE_VERSION` is separate: it retires the Koji stage's cached RPC
responses when a parser change makes previously-stored ones untrustworthy. It
is also part of that stage's checkpoint identity, so bumping it invalidates
both caches at once — necessary, because the source cache sits behind the
checkpoint and would otherwise never be consulted.

Note that the Koji `FileCache`'s nominal 30-day TTL is not enforced for
Minio-backed entries (`cache.rs:295` does not check age), so bumping the
version — not expiry — is what actually retires a bad entry.

`PG_COLLECT_DIST_GIT_BASE` overrides the origin spec files are fetched from,
keeping each distro's dist-git path shape. Set it to point at an internal
dist-git mirror. It is not part of the checkpoint identity: it names where a
spec came from, not which spec, so switching mirrors does not invalidate
fragments.

### Incident: `fedora-43-full` timeout + a real cache bug (2026-09-10)

The first real production run of `pg-collect@fedora-43-full` hit its (then)
5h `TimeoutStartSec` at 99.6% complete (23,900/23,999 spec files) and was
killed by systemd. Because `upload-nt.sh` only runs after `pg-collect
rpm-full` exits successfully (no incremental upload), the entire 5-hour run
-- 540K+ triples -- was lost, not just the missing tail.

Root cause found live, not assumed: the run logged **21,306**
`Minio PUT failed: 403 Forbidden` warnings from its Koji-build cache
sync. Traced to `etl/pg-collect/src/cache.rs`'s `read_minio`/`write_minio`
(the per-key granular cache backing `enrich_koji.rs` and any other
`FileCache` consumer given a `MinioConfig` -- distinct from the
directory-level `mc mirror` cache warm/save wrapping most collector
scripts, which was never affected) authenticating with **HTTP Basic
Auth**. Scaleway Object Storage's S3 API requires **AWS Signature Version
4**; Basic Auth gets 403 on every call. Confirmed directly with `curl -u
$MINIO_ACCESS_KEY:$MINIO_SECRET_KEY -X PUT ...` against the real endpoint
before touching any code. Worse than just failing loudly: `read_minio`
treats *any* non-success response as a cache miss and returns `None`, so
reads failed silently too -- this cache had likely never worked, for any
collector/enricher that uses it, forcing a full re-fetch from the live API
every single run with zero cross-run benefit.

Fixed by implementing real SigV4 request signing in `cache.rs` (added the
`hmac` crate alongside the already-present `sha2`; region derived from the
Scaleway `s3.<region>.scw.cloud` hostname convention, since `MinioConfig`
has no separate region field). Verified live, not just by compiling: a
`--limit 5 --with-koji` `rpm-full` run against the rebuilt image produced
zero Minio warnings and left real objects under `cache/koji/*.json` in
Scaleway -- confirmed via `mc ls` before and after.

Also raised `TimeoutStartSec` 5h → 8h in `pg-collect@.container` (shared by
all 37 collectors) for headroom, since a fixed cache doesn't guarantee a
cold-cache first run fits in the old window, and the fully-lost-not-partial
failure mode makes a stingy timeout expensive. Both fixes are live on this
host (multi-arch `etl:latest` rebuilt and repushed the same day) and
`fedora-43-full` was re-triggered after deploying them.

**Follow-up the same day: it timed out again at 8h**, killed right at
`TimeoutStartSec=28800`, having logged packages up to the very last second
(53min actual CPU time over the 8h wall clock -- genuinely network-latency
bound, not hung). Total data loss again, for the same reason: `upload-nt.sh`
still only runs on full success. Raising the timeout further would only
have deferred the same failure.

The deeper bug this surfaced: **the directory-level `mc mirror` cache
save-back also only ran after full success** (`mc mirror ... "${CACHE_DIR}/"
"${MINIO_CACHE}/"` sat after `upload-nt.sh` in every script that uses
`CACHE_DIR`). So two consecutive timeouts never left the cache any warmer
than the first cold run -- every retry re-fetched from scratch, guaranteeing
it would run long enough to time out again. The SigV4 fix above made the
cache *work*; it didn't make a failed run's cache *persist*.

Fixed by backgrounding a periodic `mc mirror` cache save-back (every 5min,
via `( while sleep 300; do ... done ) & trap 'kill ...' EXIT`) in every
script that uses `CACHE_DIR`: `fedora-43-full.sh`, `fedora-44-full.sh`,
`debian-trixie-full.sh`, `openwrt-2410.sh`, `centos-stream-9-full.sh`,
`maven.sh`, `pypi.sh`. Now a timeout/kill loses at most 5 minutes of fresh
cache entries instead of the whole run, and each retry starts measurably
warmer than the last -- the real fix for "shouldn't caching help with
this?", versus just buying more wall-clock headroom. Deployed directly to
`/etc/containers/systemd/scripts/collectors/` (no image rebuild needed --
these are bind-mounted, not baked in) and `fedora-43-full` re-triggered.

**Building a multi-arch `etl` image manually.** *Superseded — CI does this
now; see "Image tags and auto-update" above. Kept for the record, and as
the fallback if Actions is unavailable.* At the time,
`.github/workflows/release.yml` only built single-arch (`ubuntu-latest`, no
`platforms:`/QEMU setup) and only triggered on `v*` tags -- it was not used
for this fix, since pushing a tag would have overwritten this host's
existing multi-arch manifest with an amd64-only one (this host is aarch64).
Instead, build each arch where it matches natively and assemble a manifest
by hand:
```bash
# amd64, on an x86_64 dev machine:
cd etl && podman build --platform linux/amd64 -t ghcr.io/packagegraph/etl:amd64-tmp -f Containerfile .
podman push ghcr.io/packagegraph/etl:amd64-tmp

# arm64, natively on this (aarch64) host -- transfer source, excluding target/:
tar czf - --exclude=target --exclude=pg-collect/target -C etl . | ssh root@host 'mkdir -p /tmp/build && tar xzf - -C /tmp/build'
ssh root@host 'cd /tmp/build && podman build --platform linux/arm64 -t ghcr.io/packagegraph/etl:arm64-tmp -f Containerfile . && podman push ghcr.io/packagegraph/etl:arm64-tmp'

# assemble and push the manifest list from the dev machine:
podman manifest create ghcr.io/packagegraph/etl:latest \
  docker://ghcr.io/packagegraph/etl:amd64-tmp docker://ghcr.io/packagegraph/etl:arm64-tmp
podman manifest push ghcr.io/packagegraph/etl:latest docker://ghcr.io/packagegraph/etl:latest
```
Then `podman pull ghcr.io/packagegraph/etl:latest` on the host (or wait for
`io.containers.autoupdate=registry`'s own poll). Giving `release.yml` real
multi-arch support would make this manual dance unnecessary going forward --
not done here, out of scope for a same-day bug fix. **Done since**, in
`.github/workflows/images.yml`, using native per-arch runners rather than
QEMU: the etl image compiles `pg-collect`, and emulated Rust made the QEMU
route far slower than building each arch on a runner that matches it.

**Don't `podman rmi -f` the host's `latest` tag to clear a "name already in
use" conflict when re-assembling the manifest.** Doing this on
2026-09-10 force-stopped *every currently-running container* on that image
in one shot -- `fedora-43-full`, plus the `koji`/`repology`/`security`
enrichers that happened to be mid-run -- because `-f` forcibly kills any
container still using the image before removing it. The safer sequence is
`podman manifest create ... --replace` (no full `rmi` needed), or at minimum
check `podman ps` for containers on that image first and let them finish or
be restarted deliberately.

### Incident: `nix` collector broke on a nixpkgs schema change (2026-09-10)

`pg-collect@nix` started failing with `Error: invalid type: map, expected a
string at line 1 column 20351413` fetching `packages.json.br` from the
`nixos-24.05` channel. Root-caused by downloading and decompressing the real
channel file and inspecting the JSON at that byte offset directly (not
guessed): nixpkgs now emits `meta.platforms` as a list of structured
`{cpu:{...}, kernel:{...}}` objects for at least some packages (e.g.
`fs-uae-3.1.66`), not the plain `"x86_64-linux"`-style strings the older
schema always used. `nix.rs`'s `NixMeta.platforms` field was typed
`Option<Vec<String>>`, so serde hard-failed the whole file the first time it
hit a structured entry.

Fixed by checking whether `platforms` was even used downstream first -- it
wasn't (deserialized but never read when emitting triples) -- so the field
was deleted from `NixMeta` entirely rather than widened to a permissive
type, removing the dependency on nixpkgs' `meta.platforms` shape altogether.
Added a regression test using the real structured-platform JSON shape that
broke production. This required a full image rebuild (unlike the
`CACHE_DIR` fix above, `nix.rs` is compiled into the `pg-collect` binary,
not a bind-mounted script) -- built and pushed via the same manual
multi-arch process. Verified live: `pg-collect@nix` completed successfully
post-deploy (`Result=success`, uploaded `nix-nixpkgs.nt.gz`).

### Incident: malformed cargo names + a search.maven.org outage (2026-09-10)

An external SPARQL-endpoint audit that caught the `debian-trixie` graph
collision (previous section) also surfaced two smaller, real bugs while
investigating the corpus's predicate shape.

**Malformed `cargo` upstream package names from Debian's `librust-*` packages.**
`collect_spec.rs`'s `detect_ecosystem_by_name` maps Debian's
`librust-<crate>-dev` binary packages to their upstream crates.io name by
stripping the `librust-`/`-dev` wrapper. It didn't account for Debian's
per-feature packaging convention -- each enabled-feature combination of a
crate ships as its own binary package, named
`librust-<crate>+<feature>-dev` (e.g. `librust-adler+compiler-builtins-dev`
is the Debian package for the `adler` crate built with the
`compiler-builtins` feature) -- so the detected "upstream package name"
included the `+feature` suffix verbatim. 187 of the corpus's 17,050
cargo-tagged names had this shape. Since `cargo.sh`'s collector
auto-discovers its seed list from exactly these cross-referenced names
(`crate::seed::discover_by_ecosystem`, matching any `pkg:upstreamEcosystem`
tag anywhere in the corpus -- see the npm/cargo discovery mechanism
discussion below), every one of these produced a guaranteed 404 against
the real crates.io API on every run. Fixed by splitting on the first `+`
(a feature name can itself contain `+`, e.g. "c++14", so only the first
occurrence is a valid split point) in both of `detect_ecosystem_by_name`'s
two `librust-` detection branches (name-prefix and homepage-domain
strategies -- both had the bug). Verified with new unit tests, including
the `c++14`-in-feature-name edge case.

**`search.maven.org` outage with no fallback.** Separately (not caused by
the crate-name bug), `search.maven.org` -- the Solr search API `maven.rs`
uses to resolve "latest version" for a `groupId:artifactId` -- went fully
unreachable from this host for several hours (confirmed via direct `curl`,
both IPv4 and IPv6, consistent 8-10s timeouts with zero bytes back) while
`repo1.maven.org` (the actual artifact CDN, used by every real Maven/
Gradle build) stayed healthy throughout. `search.maven.org` is meant for
human search, not programmatic resolution -- using it as the sole
resolution path was a single point of failure `maven.rs` didn't need.
Fixed by adding a fallback in `get_latest_version`: on a transport-level
failure (`FetchError::Transport` -- DNS/TLS/connection/timeout, not a
legitimate 404) against the search API, read `maven-metadata.xml` directly
from the repository instead (the same mechanism real build tools use),
preferring `<release>` over `<latest>`. Verified live against the real
outage: a `junit:junit` collection succeeded in under 2 seconds with zero
fetch errors while `search.maven.org` was still confirmed down moments
before and after the test.

Both fixes are live on this host (bundled into the same multi-arch
`etl:latest` rebuild described above); `cargo`'s next run will need a
fresh `debian-trixie-full`-sourced corpus (see previous section) indexed
before its discovery seed is fully clean, since the malformed names are
still baked into whatever `nt-output/debian-trixie*.nt.gz` was indexed
most recently as of the fix landing.

### Incident: root disk full during a concurrent `rhel-9-full`/`rhel-10-full` run (2026-09-11)

The first attempt at running `rhel-9-full` and `rhel-10-full` to full
completion (both manually kicked off together, after the SELinux/TLS-cache/
OOM fixes above) filled the root disk to 100%. Podman's own database broke
as a result (`unable to open database file` on nearly every `podman`
command, including `image prune` and even plain `images`), because both
collectors' `--rm` containers died mid-write and left orphaned overlay
`diff`/`merged` mounts podman could no longer account for.

Two compounding causes, both structural, not just this one run's bad luck:

1. **Collector scratch space lived on the root disk.** `/tmp` inside
   `pg-collect@.container`/`pg-collect-rhel@.container` was the container's
   own overlay writable layer -- backed by whatever disk hosts Podman's
   default storage, the same 47G root filesystem as the OS itself. Each
   RHEL collector's cache + pre-upload output alone reached ~14-15GB before
   failing; two running at once left no headroom at all.
2. **No automated image pruning.** Roughly 30 hours of CI-published
   `:devel-latest` digests (several merges to `main`) had accumulated as
   dangling (`<none>:<none>`) images with nothing ever reclaiming them,
   eating most of the disk's headroom before the concurrent run even
   started.

Recovery (manual, one-time): confirmed QLever/sparql-proxy were still
functionally healthy (a real SPARQL query, not just the healthcheck's
stale status) despite showing `unhealthy`; found and unmounted the orphaned
overlay `merged` mounts and removed their `diff` directories directly
(bypassing podman, which couldn't write); once podman's metadata writes
succeeded again, force-removed the zombie container records and ran
`podman image prune -f`, reclaiming ~23GB from ~20 dangling images.

**The actual fix is structural, not "don't run collectors concurrently"**
-- running any number of collectors at once should never be able to
exhaust the disk. Two changes: `pg-collect-scratch.volume` moves collector
scratch space onto the dedicated data disk (see "Dedicated data disk"
above) instead of the shared 47G root disk, and `podman-image-prune.timer`
prunes dangling images daily so CI publishes stop accumulating baseline
pressure between runs. Neither depends on the other: the scratch volume
makes any single run's disk usage bounded by the data disk's 396G+ instead
of root's headroom, and the prune timer keeps that headroom from silently
eroding even when no collector is running at all.

### Incident: `:Z` on a volume shared by 40+ collectors caused EACCES (2026-09-11)

Hours after `pg-collect-scratch.volume` deployed (previous section),
`pg-collect@hackage` died with `Error: Permission denied (os error 13)`
two seconds after `pg-collect@osv` started -- and `pg-collect@gomod`
(a ~2h run) died the same way, right as another collector started near
its tail end. Root cause: both container templates mounted the shared
volume with `:Z` (`Volume=pg-collect-scratch.volume:/tmp:Z`) --
Podman/SELinux's *private/exclusive* relabel flag, meant for a volume
used by exactly one container (correct for `qlever-rebuild-scratch.volume`,
which only ever backs `qlever-rebuild-index.service`). Every time a
second container mounts a `:Z` volume, SELinux reassigns its category to
that new container, silently revoking the first container's
already-open write access -- its next `write()` fails with EACCES.

Confirmed directly, not just inferred from timing: two throwaway
containers mounting the same volume with `:Z`, one writing in a loop,
the second started 2s later, reproduced the exact failure on demand --
the first container's write failed immediately once the second one
started; killing the second and only running one container at a time
never failed.

Fixed by changing both templates' mount to `:z` (lowercase) -- the
*shared* label, which every collector instance uses in common, so no
per-container reassignment/revocation happens as collectors start and
stop around each other. `pg-collect-scratch.volume`'s own comment
documents the distinction so it isn't miscopied again.

## Data pipeline: collectors → Minio → QLever, and how to extend it

The full path from a collector run to queryable data is two independently
scheduled stages, joined only by the published corpus in object storage.
The contract between them is `docs/GRAPH-PUBLICATION.md`; the short form:

1. Every `pg-collect@<name>.service` collects, then publishes through
   `upload-nt.sh`. The payload goes to a generation key that has never
   existed before -- `graphs/<slug>/generations/<gen>.nt.gz`, gzip-compressed
   because these are large mostly-text N-Triples, see that script's comment
   on why gzip over xz -- is read back and checked, and only then is
   `graphs/<slug>/manifest.json` replaced. That single small `PUT` is the
   commit: atomic in S3, so a reader sees the whole old manifest or the
   whole new one. A failure before it leaves an orphan generation nothing
   references, and the previous generation still authoritative.

   It also refreshes the old `nt-output/<slug>.nt.gz` + `.graph` pair,
   best-effort, purely for readers not yet upgraded. That pair used to be
   the commit protocol, and was not one: on every upload after a graph's
   first, the sidecar already existed, so the new payload became
   discoverable the moment its `PUT` landed -- unverified, with nothing
   left to gate it (#72).
2. `qlever-rebuild-index.timer` (nightly, `03:30` UTC) reads every
   manifest, fetches and **verifies** the committed generation against its
   recorded size and SHA-256, adds the legacy pairs for graphs that have no
   manifest yet, converts everything to one `.nq` corpus, builds a fresh
   QLever index, and promotes it to `latest` only if it passes three gates:
   at least 10 graphs present, no more than 25% triple-count loss vs. the
   last successful promotion, and no previously-present graph URI has
   disappeared. `qlever-refresh-if-changed.sh` then bounces
   `qlever.service` when the promoted index differs from the one confirmed
   serving.

   A graph is manifest-backed or legacy, never both: a legacy copy of a
   graph that has a manifest is reported and skipped. Two discoverable
   payloads for one graph URI is how ~10GB of duplicate quads got indexed
   in September 2026.

   The log names each manifest-backed graph as `complete`, `PARTIAL`, or
   `completeness unrecorded`, and counts the partial ones at the end. A
   partial graph is the availability trade working as intended -- an
   optional enrichment stage left some items for the next run rather than
   failing the collector -- but an index quietly built from a growing number
   of them is something an operator should see. `completeness unrecorded` is
   the honest reading for every graph published before that record existed;
   absence is never reported as completeness (#70).

**This second stage does not run itself -- it must be enabled.** Unlike
the Kubernetes CronJob it replaces (scheduled by the cluster the moment
the CronJob object exists), `qlever-rebuild-index.timer` is a plain
systemd timer that requires its own `systemctl enable --now` (see Install,
above) *in addition to* installing the file. Installing the unit file
without enabling it leaves collectors uploading indefinitely with nothing
ever promoting that data into QLever -- this happened on the first
deployment of this host: the timer file was present but never enabled, so
one week's worth of collector uploads sat in `nt-output/` unpromoted until
caught by manually checking `systemctl list-timers`. Treat "is
`qlever-rebuild-index.timer` in `systemctl list-timers`'s output" as a
standing post-install/post-migration check, not a one-time setup step to
trust and forget.

**Caching.** `qlever-rebuild-index.sh` downloads into
`qlever-rebuild-scratch.volume`, which is a real disk (see "Dedicated data
disk" above), not tmpfs -- and, as of the gzip change above, no longer
deletes each `.nt.gz`/`.graph` pair after converting it to `.nq`, and no
longer forces `mc mirror --overwrite`. This means a graph nobody
re-uploaded since the last rebuild costs nothing to "download" on the next
one: `mc mirror`'s own size/mtime comparison skips it. Only genuinely
changed or newly-uploaded graphs are re-fetched. (The pinned `mc` version
here has no content-hash comparison flag -- `mc mirror --help` only offers
`--checksum`, which tags uploads, not compares them -- so this is a
same-size-and-mtime skip, not a byte-for-byte one; a collector re-run
always produces a fresh upload with a fresh mtime, so this only matters for
a deliberately backdated object, which nothing in this pipeline does.)

**Enrichers are not ported here yet, but the same pattern applies
directly.** The Kubernetes `enrich-*` jobs (GitHub, NVD, EPSS, Repology,
Koji, taxonomy, npm-provenance, ...) run the same `pg-collect` binary's
`Enrich*` subcommands, and at least `EnrichGithub` -- almost certainly the
rest, by the same CLI shape -- takes `--endpoint` (to query what needs
enriching) and `-o`/`--output` (to write plain N-Triples) exactly like a
collector, with an *optional* `--load-graph` flag that switches it to
writing directly into Fuseki via SPARQL Update instead. Since this host has
no writable Fuseki, porting an enricher means the same recipe as a
collector (never passing `--load-graph`) run through its own quadlet
template with its own timer -- no changes to `qlever-rebuild-index.sh`
required; it already treats every graph in `nt-output/` uniformly
regardless of whether a collector or an enricher produced it. This has
since been done -- see "Enrichers" below.

## Enrichers

`enrichers/` mirrors `collectors/` exactly: one templated unit
(`pg-enrich@.container`), one script and one `.timer` per enricher
(`enrichers/scripts/<name>.sh`, `enrichers/timers/pg-enrich-<name>.timer`).
Same reasoning throughout: no writable Fuseki here, so every script drops
the `pg-collect drop && pg-collect load` round-trip its Kubernetes
`enrich-*.yaml` equivalent does and just enriches + `upload-nt.sh`s to
Minio, picked up by the same nightly `qlever-rebuild-index.timer`.

**Ported from existing Kubernetes jobs** (`advisory`, `koji`,
`npm-provenance`, `repology`): straightforward, all four already ran in
production against Fuseki. (`repology` has since been retired -- see
"Retired: repology".) `enrich-github` was *not* ported -- its
Kubernetes version uses `--load-graph` to accumulate incrementally inside
Fuseki itself across bounded weekly batches (rate-limited by GitHub's API),
then does a full `CONSTRUCT` export for Minio archival; without a writable
Fuseki it's unclear whether plain file mode re-emits the full accumulated
history or just each run's new batch, which matters a great deal (silently
shrinking the graph every week instead of growing it). Needs that answered,
plus a real `GITHUB_TOKEN`, before porting.

**Never scheduled anywhere before this host** (`epss`, `taxonomy`,
`security`, `forge-version`, `nvd`, plus `revdeps`/`blast-radius` -- see
below): these exist as CLI subcommands but had no Kubernetes job at all,
which is why their CQ families showed up 100% empty in an earlier audit.
Each was smoke-tested live against this host's QLever instance
(2026-09-10) before being trusted with a timer:

- `epss` -- **found and fixed a real bug** in `enrich_epss.rs`'s discovery
  query: `<SEC_PLACEHOLDER>cveId` produced `<https://...security#>cveId`
  (the closing `>` landed before `cveId` instead of after it), a malformed
  IRI that fails on any SPARQL 1.1 engine, not a QLever-specific issue.
  Fixed to `<SEC_PLACEHOLDERcveId>`. After the fix: 9,026 CVEs matched,
  45K triples.
- `taxonomy` -- worked immediately. 3.86M identities found, 1.8M triples.
- `nvd` (feed mode, not API mode -- API mode writes via `INSERT DATA` and
  needs a writable endpoint) -- worked immediately. 346,657 CVEs across 27
  feeds, 643K triples. `NVD_API_KEY` is optional (raises rate limits feed
  mode doesn't strictly need); not provisioned.
- `forge-version` -- worked, but only reaches 2 of 17 known forge
  instances without a `GITLAB_TOKEN` for self-hosted GitLab auth (14
  triples). Fine to run as-is; a token would substantially widen coverage.
- `security` -- worked, in the form it had then: a loop over all 11
  ecosystems `EnrichSecurity` supported, one OSV API request per package,
  into one shared graph (`enrichment/security`). That shape is gone. The
  loop needed ~63h at `api.osv.dev`'s 500ms pacing, four of its eleven
  arms (`deb`, `apk`, `rpm`, `fedora`) were not OSV ecosystem names and
  could only return HTTP 400 (#97), and five of the seven that did work
  were already covered daily by `collectors/scripts/osv.sh`. #99 rewrote
  it to read the Debian and Alpine archives that collector publishes and
  join them against the corpus, emitting only `sec:affectsPackage` for
  packages we actually hold -- no API, no cache, one graph, 103s measured.
- `revdeps` and `blast-radius` -- **unblocked and scheduled as of
  2026-09-10.** Both previously failed fast on `met:reverseDependencyCount`/
  `sec:blastRadius` "not declared in the ontology", because *the ontology's
  own `.ttl` files had never been loaded into this QLever instance as
  data* -- only collector and enricher output had. Fixed by running the
  pre-existing (never previously invoked against this host)
  `etl/scripts/upload-ontology.sh` -- converts all 36 `etl/ontology/*.ttl`
  files to N-Triples via `riot` and uploads as `nt-output/ontology.nt.gz` →
  graph `https://packagegraph.github.io/ontology`, same `upload-nt.sh`
  convention every collector/enricher already uses. One nightly rebuild
  later, both ontology-check queries resolved live (`owl:DatatypeProperty`).
  Note also: QLever's path-search `SERVICE` (confirmed working, see
  "Deliberate differences" below) may make `blast-radius`'s pre-materialized
  snapshot redundant -- worth deciding which approach to keep long-term, but
  both are live for now.
  **Cross-day scheduling gotcha, found live:** `enrich-blast-radius` reads
  `met:reverseDependencyCount` via a *query against QLever*, not directly
  from Minio -- so it only sees what `enrich-revdeps` materialized after
  the next `qlever-rebuild-index.timer` run (nightly, 03:30 UTC) has
  indexed it. Scheduling both enrichers back-to-back on the same day (the
  first attempt: Monday 13:30 / 14:00) produced a confirmed-empty 36-byte
  `blast-radius` output, because Monday's `revdeps` upload wasn't queryable
  until the *following* night's rebuild. Fixed by moving `blast-radius` to
  Tuesday 09:00 -- comfortably after the Tuesday 03:30 rebuild that picks up
  Monday's `revdeps` run. Any enricher that reads another enricher's output
  via a live QLever query (rather than reading Minio directly) needs this
  same one-rebuild-cycle gap, not just a same-day offset.

**A real bug this caught in the collector side too:** `osv.sh` loops every
OSV ecosystem it tracks into one shared graph (seventeen of them as of #97)
-- but its original form called `upload-nt.sh` once *per ecosystem* with
the *same* `GRAPH_URI`. Since `upload-nt.sh` derives the Minio object key
from the graph URI, every ecosystem's upload silently overwrote the
previous one, leaving only the last ecosystem's (SwiftURL's) data in
`nt-output/`. Caught and fixed before it ever ran for real (next scheduled
run was still days out). The fix, used by both `osv.sh` and `security.sh`:
accumulate every sub-run's output into one local file, call
`upload-nt.sh` exactly once at the end.

Install (identical shape to collectors):

```bash
install -m 644 deploy/quadlet/enrichers/pg-enrich@.container /etc/containers/systemd/
install -d /etc/containers/systemd/scripts/enrichers
install -m 755 deploy/quadlet/enrichers/scripts/*.sh /etc/containers/systemd/scripts/enrichers/
install -m 644 deploy/quadlet/enrichers/timers/*.timer /etc/systemd/system/

# Per-enricher timeouts (see "Failure visibility and per-enricher timeouts").
for d in deploy/quadlet/enrichers/dropins/*.service.d; do
  install -d "/etc/systemd/system/$(basename "$d")"
  install -m 644 "$d"/*.conf "/etc/systemd/system/$(basename "$d")/"
done

systemctl daemon-reload
systemctl enable --now pg-enrich-advisory.timer pg-enrich-koji.timer pg-enrich-npm-provenance.timer \
  pg-enrich-epss.timer pg-enrich-taxonomy.timer pg-enrich-security.timer \
  pg-enrich-forge-version.timer pg-enrich-nvd.timer pg-enrich-revdeps.timer pg-enrich-blast-radius.timer
```

### Failure visibility and per-enricher timeouts

Three enrichers had been failing since 2026-09-14 and one collector since
2026-09-12 with nothing reporting it (#59). They were not silent:
`systemctl --failed` knew, and so did the journal. Nobody was reading
either.

`failure/pg-unit-failed@.service` closes that by giving the listening a
fixed address. The collector and enricher templates name it in
`OnFailure=`, and it runs `scripts/unit-failed.sh`, which writes:

| path | contents |
|---|---|
| `/var/lib/packagegraph/failed-units/<unit>.txt` | the latest failure of that unit -- how it ended, when, and the last 100 journal lines. Overwritten each time: the question is "what is wrong now". |
| `/var/lib/packagegraph/failed-units/history.tsv` | one line per failure, appended forever, so a unit that fails every week looks different from one that failed once. |

It also logs to the journal under the `pg-unit-failed` tag, so
`journalctl -t pg-unit-failed` is a complete list even if the directory is
lost. Nothing leaves the host and no credentials are involved, so the
notifier cannot itself become a thing that breaks, leaks, or has to be
rotated.

```bash
install -m 644 deploy/quadlet/failure/pg-unit-failed@.service /etc/systemd/system/
install -m 755 deploy/quadlet/scripts/unit-failed.sh /etc/containers/systemd/scripts/
install -d -m 755 /var/lib/packagegraph/failed-units
systemctl daemon-reload
```

`enrichers/dropins/pg-enrich@<name>.service.d/timeout.conf` then gives each
enricher its own `TimeoutStartSec`. One number shared by ten enrichers
is what let `koji` and `security` be SIGKILL'd mid-run while `taxonomy`,
which finishes in about a minute, would have sat undetected for eight hours
if it hung. Every drop-in states whether its number is MEASURED, ESTIMATED
or UNMEASURED, and `tests/test_unit_failure_notifier.py` fails if a new
enricher arrives without one, or if a drop-in carries a number with no
stated basis.

Only `taxonomy` (1h, MEASURED at 72s) and `security` (1h, ESTIMATED from
measurement of its parts -- 108s to download and walk both OSV archives,
with the corpus-index query unmeasured) are currently below the shared 8h
ceiling. The other eight are honestly unmeasured and stay conservative: a
timeout below the real runtime is exactly how `koji` and `security` were
lost, so guessing downward is worse than leaving the ceiling in place
until a run measures it. `security`'s earlier 4h was sized for the OSV API
path that #99 removed, and was wrong even for that -- the real figure was
~63h.

### Retired: repology

`pg-enrich-repology` is **no longer scheduled**. Its timer, script and
timeout drop-in are removed; `enrich_repology.rs` and the
`pg-collect enrich-repology` subcommand remain, for the rework described
below.

It never published anything. `enrich_repology.rs` issues one
`GET /api/v1/project/{name}` per distinct package name at a 1s pacing
floor, and at its observed rate (~100 packages / 1.7 min against 314,416)
a complete pass needs roughly 90 hours. No `TimeoutStartSec` can cover
that, so every weekly run was SIGKILL'd -- and because the kill destroys
the container before `repology.sh` reaches its `upload-nt.sh` line, no run
ever reached the upload at all. Eight hours of host time per week, for
cache and nothing else.

The cost is also O(corpus): one request per package name, so it gets
*slower* as collection completes, never faster.

Removing the files from this repo does not uninstall anything. On a host
that already has it, the retirement needs:

```bash
systemctl disable --now pg-enrich-repology.timer
rm -f /etc/systemd/system/pg-enrich-repology.timer
rm -f /etc/containers/systemd/scripts/enrichers/repology.sh
rm -rf "/etc/systemd/system/pg-enrich@repology.service.d"
systemctl daemon-reload
```

The `enricher-cache/repology` prefix in the object store can go too; it
is the only thing the enricher ever produced.

**The rework, if it happens**, has to replace per-name lookups with bulk
enumeration -- Repology's paginated projects endpoint or its database
dumps -- so the cost tracks Repology's dataset rather than ours. That is
the same change `osv.sh` already embodies for vulnerabilities.

**One-time ontology bootstrap**, needed before `revdeps`/`blast-radius` (or
any future consumer of ontology-level declarations) will work -- not part
of the collector/enricher timer cadence, run once and re-run only if
`etl/ontology/*.ttl` changes:

```bash
podman run --rm --network host \
  --env-file /etc/containers/systemd/scripts/minio.env \
  --entrypoint /app/scripts/upload-ontology.sh \
  ghcr.io/packagegraph/etl:latest
systemctl start qlever-rebuild-index.service   # or wait for the nightly run
```

**SELinux note (RHEL/Fedora hosts):** unit files moved into
`/etc/systemd/system/` via `mv` from a `mktemp -d` staging dir (see
"Atomic script deploy" convention above) retain the source directory's
`user_tmp_t` SELinux context, which `systemd` silently refuses to load
("Unit ... does not exist" from `systemctl enable`, even though the file is
right there). Run `restorecon -v` on the moved file before
`daemon-reload`/`enable`.

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

## QLever capabilities beyond basic SPARQL SELECT

Confirmed live against this deployment's pinned `commit-1075455fae`
(2026-09-10), documented here since they change what future collector/
enricher/query work should reach for instead of custom-building:

- **SPARQL 1.1 Update and the Graph Store Protocol both work**, gated by
  the same admin access token `qlever.container` already starts with
  (`-a "$QLEVER_ACCESS_TOKEN"`). QLever calls it "experimental" but a
  scoped `INSERT DATA`/`DELETE WHERE` round-trip succeeded cleanly. This
  does **not** mean anything should start writing to QLever directly today:
  `qlever-rebuild-index.sh` atomically replaces the entire index every
  night from the Minio corpus, so any direct write would just be silently
  discarded on the next promotion unless a real design first reconciles
  the two. `pg-collect`'s own `sparql.rs` still hardcodes a refusal
  (`guard_write`) for any write op against the QLever backend, independent
  of what the server itself allows.
- **`qlever rebuild-index`** (a different thing from this repo's own
  `qlever-rebuild-index.sh`) rebuilds directly from QLever's own on-disk
  structures instead of re-parsing raw N-Quads -- documented as under a
  minute for 500M triples on a 16-core x86 host. It only helps if data is
  written incrementally via Update in the first place (see above); it does
  nothing for the Minio-corpus-based nightly rebuild this deployment
  actually uses.
- **Path search** (`SERVICE <https://qlever.cs.uni-freiburg.de/pathSearch/>`)
  answers "what's the path from A to B" / "what's reachable within N hops"
  queries that plain SPARQL property paths (`+`/`*`) can't -- those can only
  tell you *whether* two nodes are connected, not enumerate the path.
  Directly relevant to `enrich_blast_radius.rs`/`enrich_revdeps.rs`-style
  transitive-dependency questions; no index changes needed, pure query-time
  feature. See the "Enrichers" section above for the specific tension with
  `blast-radius`'s pre-materialized approach.
- **Text search** (`qlever add-text-index` + `SERVICE
  <https://qlever.cs.uni-freiburg.de/textSearch/>` or `ql:contains-word`/
  `ql:contains-entity`) combines full-text search over literals with
  structural SPARQL joins -- a strong fit for advisory/CVE description
  search and package-description search, but unlike path search this
  **does** require building a separate text index and more disk; not done.
