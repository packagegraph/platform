# Operating the QLever host

Standalone QLever SPARQL endpoint for PackageGraph, running under Podman
Quadlet (systemd-managed containers) rather than Kubernetes. Unit
definitions live in `deploy/quadlet/` — this document covers running,
querying, and troubleshooting the actual deployed host; `deploy/quadlet/README.md`
covers installing the units on a *new* host.

> **Host address and project identifiers are deliberately not in this file.**
> This is a public repository, and the host accepts root SSH. Get the address
> from the team's password manager / internal wiki and export it before running
> any command below:
>
> ```bash
> export PGRAPH_HOST=<address>
> ```

## Host facts

| | |
|---|---|
| Address | `${PGRAPH_HOST}` (root SSH access; real value in the password manager) |
| OS | RHEL 10.2, `aarch64` |
| Podman | 5.8.2, cgroup v2 |
| Data disk | `/dev/sdb`, 512G, xfs, mounted at `/var/lib/packagegraph` |
| Quadlet units | `/etc/containers/systemd/*.container`, `*.volume` |
| Scripts/credentials | `/etc/containers/systemd/scripts/` |
| Rebuild timer | `/etc/systemd/system/qlever-rebuild-index.timer` |
| Public endpoint | `https://packagegraph.di.riseproject.dev` (nginx reverse proxy, real Let's Encrypt cert, no auth, read-only) |

## Architecture

```
qlever-rebuild-index.timer (03:30 daily, NOT enabled by default)
        │ triggers
        ▼
qlever-rebuild-index.service ──(reads/writes)──> Scaleway bucket rise-packagegraph
        │ ExecStartPost, only if a new index was promoted
        ▼
qlever-refresh-if-changed.sh (host script)
        │ systemctl restart
        ▼
qlever-index-load.service ──(pulls latest index)──> Scaleway bucket
        │ Requires=/After=
        ▼
qlever.service  (SPARQL endpoint, 127.0.0.1:7001)
```

All three container units share `/var/lib/packagegraph/qlever-data` (the
loaded index) and run as `User=999` — both must stay in sync or
`qlever-server` can't write its own log files next to the index (see
Troubleshooting).

Data source: Scaleway Object Storage, bucket `rise-packagegraph`, region
`fr-par`. Credentials and endpoint live in
`/etc/containers/systemd/scripts/minio.env`. The corpus (`nt-output/`) and
a pre-built index (`qlever-index/`) were mirrored here from the
Kubernetes deployment's in-cluster Minio on 2026-09-09 — see "Data
provenance" below.

## Querying the data

Since `sparql-proxy.service` went live, the simplest path is the public
endpoint directly — no SSH needed:

```bash
curl -s "https://packagegraph.di.riseproject.dev/?query=SELECT+%2A+WHERE+%7B+%3Fs+%3Fp+%3Fo+%7D+LIMIT+10"
```

It's read-only by construction (QLever itself rejects SPARQL Update
without a valid access-token — verified `HTTP 403` with none supplied —
and the proxy never has or forwards that token) and unauthenticated by
design; see `deploy/quadlet/README.md`'s "Public SPARQL reverse proxy"
section for the full rationale.

QLever's own port (`127.0.0.1:7001`) is still loopback-only underneath the
proxy — useful for bypassing the proxy/rate-limit entirely when debugging
on the host itself:

```bash
ssh root@$PGRAPH_HOST 'curl -s "http://localhost:7001/?query=$(python3 -c "import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1]))" "SELECT * WHERE { ?s ?p ?o } LIMIT 10")"'
```

Restricted API calls (e.g. index statistics, admin operations) need the
access token from `/etc/containers/systemd/scripts/qlever.env`
(`QLEVER_ACCESS_TOKEN=...`) as an `access-token` query parameter — the
public proxy explicitly rejects this parameter (`HTTP 403`), so these only
work against port 7001 directly.

For actual SPARQL query patterns over the PackageGraph ontology, see
`docs/QUERYING.md` — the data and predicates are identical to the
Kubernetes-deployed QLever/Fuseki instances, only the endpoint differs.

## Common operations

```bash
# Service status
ssh root@$PGRAPH_HOST 'systemctl status qlever.service qlever-index-load.service'

# Logs (follow)
ssh root@$PGRAPH_HOST 'journalctl -u qlever.service -f'

# Restart the server (e.g. after an index reload)
ssh root@$PGRAPH_HOST 'systemctl restart qlever.service'

# Manually trigger a full index rebuild from the published corpus
# (heavy: downloads ~28G, builds a new index, can take 30-60+ min)
ssh root@$PGRAPH_HOST 'systemctl start qlever-rebuild-index.service'
ssh root@$PGRAPH_HOST 'journalctl -u qlever-rebuild-index.service -f'

# Check whether the rebuild timer is enabled (it is NOT, by default)
ssh root@$PGRAPH_HOST 'systemctl is-enabled qlever-rebuild-index.timer'
ssh root@$PGRAPH_HOST 'systemctl enable --now qlever-rebuild-index.timer'

# Check what index is on disk vs. what is confirmed serving. They differ when
# a reload extracted new bytes but qlever never became ready -- the refresh
# will retry on its next run. See deploy/quadlet/README.md, "Host dependencies".
ssh root@$PGRAPH_HOST 'cat /var/lib/packagegraph/qlever-data/index/.loaded'
ssh root@$PGRAPH_HOST 'cat /var/lib/packagegraph/qlever-data/index/.serving'

# Reverse proxy status / logs
ssh root@$PGRAPH_HOST 'systemctl status sparql-proxy.service'
ssh root@$PGRAPH_HOST 'journalctl -u sparql-proxy.service -f'

# Check certificate expiry / next renewal check
ssh root@$PGRAPH_HOST 'systemctl list-timers sparql-proxy-certbot-renew.timer'
ssh root@$PGRAPH_HOST 'openssl x509 -enddate -noout -in /var/lib/packagegraph/sparql-proxy-certs/live/packagegraph.di.riseproject.dev/cert.pem'
```

## Data provenance

The corpus and index were copied from the Kubernetes deployment's Minio
(`minio.packagegraph.svc:9000`, bucket `packagegraph`) via `mc mirror`
through an `oc port-forward` tunnel, using kubeconfig
`~/.kube/config-2` (context reaching the real cluster — the default
`~/.kube/config` context for this cluster points at an unreachable
`localhost:6443` without a tunnel). This was a one-time bootstrap; going
forward, `qlever-rebuild-index.service` rebuilds directly from whatever
is in the `rise-packagegraph` Scaleway bucket's `nt-output/` prefix —
keeping that prefix current (e.g. via the ETL pipeline writing there
directly, or repeating the mirror) is what keeps future rebuilds fresh.

## IAM

Object Storage access uses a dedicated Scaleway IAM application
(`rise-packagegraph-qlever`) and API key, scoped to a dedicated
`packagegraph` project (project ID is **not** recorded here — see the
internal wiki) with `ObjectStorageBucketsRead` +
`ObjectStorageObjectsRead` + `ObjectStorageObjectsWrite` — deliberately
no delete permission. See `deploy/quadlet/README.md`'s "Using Scaleway
Object Storage as MINIO_ENDPOINT" section for two non-obvious gotchas hit
while setting this up (endpoint style, API key project scoping).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `qlever.service` logs only the "WELCOME TO THE QLEVER DOCKER IMAGE" banner then exits | `Exec=` appends to the image's ENTRYPOINT, doesn't replace it — the image's own entrypoint script requires `pwd == /data` and just prints help otherwise | Already fixed via `Entrypoint=/bin/sh` in `qlever.container` — if it recurs after an edit, check the entrypoint override survived |
| `ERROR: Assertion stream_.is_open() failed. QueryEventLog: failed to open output file` | UID mismatch between `qlever-index-load.container` (image default UID 1000) and `qlever.container` (image default UID 999) — qlever-server can't write its per-query log next to index files owned by a different UID | Already fixed via `User=999` pinned on both units. If it recurs, `chown -R 999:999 /var/lib/packagegraph/qlever-data` |
| `Access token for restricted API calls is "CHANGE_ME"` or `""` in the logs | `qlever.env` still has the placeholder, or a real value was generated in one shell and referenced in a separate one (shell variables don't persist across separate command invocations) | Generate and write in one shot, piping over stdin so the token never appears in either shell's history or in the remote process table: `openssl rand -hex 32 \| ssh root@$PGRAPH_HOST 'read -r t; printf "QLEVER_ACCESS_TOKEN=%s\n" "$t" > /etc/containers/systemd/scripts/qlever.env && chmod 600 /etc/containers/systemd/scripts/qlever.env'` |
| `mc: Access Denied` / `Insufficient permissions` against the Scaleway bucket, despite the IAM policy looking correct | Either the bucket-specific virtual-hosted endpoint was used instead of the regional path-style one, or the API key's `default_project_id` doesn't match the policy's scoped project | See `deploy/quadlet/README.md`'s Scaleway gotchas section |
| `qlever-index-load.service` fails with `No index available in Minio` | No index has ever been built/promoted to `qlever-index/latest` in the bucket | Run `systemctl start qlever-rebuild-index.service` once |
| `sparql-proxy.service` fails with `cannot load certificate ... Permission denied` | `nginx-unprivileged` runs as non-root (UID 101); certbot resets certs to `0700`/`0600` (root-only) on every issuance/renewal | Bootstrap: see `deploy/quadlet/README.md`'s cert bootstrap steps. Recurring: the renewal container's `--deploy-hook` already re-applies this automatically — only relevant if it was bypassed |
