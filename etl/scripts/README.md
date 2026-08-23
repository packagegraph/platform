# ETL Scripts

## sync-ontology.sh

Syncs ontology `.ttl` files from the restructured ontology repo into the flat mirror at `etl/ontology/`. The mirror must be flat because the `build` CLI command globs `ontology_dir.glob("*.ttl")` non-recursively, and the Containerfile does `COPY ontology/ /app/ontology/`.

```bash
# Default: expects ontology repo at ../../ontology (sibling directory)
bash etl/scripts/sync-ontology.sh

# Custom source path
bash etl/scripts/sync-ontology.sh /path/to/ontology
```

**Run before every container build.** The ontology repo has files in `core/`, `extensions/*/`, `ecosystems/*/` — this script collects only the module `.ttl` files (not `.shacl.ttl` or `.examples.ttl`) into the flat mirror.

Output: 37 `.ttl` files in `etl/ontology/`.

## upload-nt.sh

Uploads an N-Triples file to Minio and registers it in the graph manifest.

```bash
bash etl/scripts/upload-nt.sh /tmp/packages.nt "https://packagegraph.github.io/graph/debian/trixie"
```

## Collecting RHEL Data

RHEL CDN repos require subscription entitlement certificates. The collector supports `--sslclientcert`, `--sslclientkey`, and `--sslcacert` flags, but it's simpler to use a UBI container with certs mounted (the container inherits access to RHEL repos).

### Prerequisites

- Entitlement certs from a subscribed RHEL host (e.g., devstack):
  - `/etc/pki/entitlement/<id>.pem` (client cert)
  - `/etc/pki/entitlement/<id>-key.pem` (client key)
  - `/etc/rhsm/ca/redhat-uep.pem` (Red Hat CA)
- A host with podman and the ETL container image built

### RHEL 9 (direct repodata)

If you have a subscribed RHEL 9 host, grab the cached repodata and collect against it:

```bash
# On the RHEL 9 host, find the cache
BASEOS_CACHE=$(find /var/cache/dnf -maxdepth 1 -name "rhel-9*baseos*" -type d)
APPSTREAM_CACHE=$(find /var/cache/dnf -maxdepth 1 -name "rhel-9*appstream*" -type d)

# Copy repodata to the build host
ssh buildhost "mkdir -p /tmp/rhel9-baseos/repodata /tmp/rhel9-appstream/repodata"
tar -C $BASEOS_CACHE -cf - repodata | ssh buildhost "tar -C /tmp/rhel9-baseos -xf -"
tar -C $APPSTREAM_CACHE -cf - repodata | ssh buildhost "tar -C /tmp/rhel9-appstream -xf -"

# Serve and collect
ssh buildhost "cd /tmp && python3 -m http.server 9999 &"

podman run --rm --entrypoint pg-collect --network host \
  -v /path/to/output:/output \
  ghcr.io/packagegraph/etl:latest \
  rpm --repo http://localhost:9999/rhel9-baseos/ \
      --distro-name rhel --release-name 9 \
      --output /output/rhel9-baseos.nt

podman run --rm --entrypoint pg-collect --network host \
  -v /path/to/output:/output \
  ghcr.io/packagegraph/etl:latest \
  rpm --repo http://localhost:9999/rhel9-appstream/ \
      --distro-name rhel --release-name 9 \
      --output /output/rhel9-appstream.nt

cat /path/to/output/rhel9-baseos.nt /path/to/output/rhel9-appstream.nt > rhel9.nt
```

### RHEL 10 (via UBI 10 container)

RHEL 10 repodata isn't available on RHEL 9 hosts. Use a UBI 10 container with entitlement certs to download the repodata, then collect against it:

```bash
# Copy certs to the build host
scp rhel-host:/etc/pki/entitlement/<id>.pem buildhost:/tmp/rhel-certs/client.pem
scp rhel-host:/etc/pki/entitlement/<id>-key.pem buildhost:/tmp/rhel-certs/client-key.pem
scp rhel-host:/etc/rhsm/ca/redhat-uep.pem buildhost:/tmp/rhel-certs/redhat-uep.pem

# Step 1: Download RHEL 10 repodata inside a UBI 10 container
mkdir -p /tmp/rhel10-repo
podman run --rm --security-opt label=disable \
  -v /tmp/rhel-certs/client.pem:/etc/pki/entitlement/client.pem:ro \
  -v /tmp/rhel-certs/client-key.pem:/etc/pki/entitlement/client-key.pem:ro \
  -v /tmp/rhel-certs/redhat-uep.pem:/etc/rhsm/ca/redhat-uep.pem:ro \
  -v /tmp/rhel10-repo:/output \
  registry.access.redhat.com/ubi10/ubi-minimal:latest \
  sh -c '
    microdnf makecache
    for repo in baseos appstream; do
      DIR=$(find /var/cache -name "repomd.xml" -path "*rhel-10*${repo}*" -printf "%h\n" | head -1)
      mkdir -p /output/${repo}/repodata
      cp "$DIR"/* /output/${repo}/repodata/
    done
  '

# Step 2: Serve and collect
python3 -m http.server 9998 &

podman run --rm --entrypoint pg-collect --network host \
  -v /path/to/output:/output \
  ghcr.io/packagegraph/etl:latest \
  rpm --repo http://localhost:9998/rhel10-repo/baseos/ \
      --distro-name rhel --release-name 10 \
      --output /output/rhel10-baseos.nt

podman run --rm --entrypoint pg-collect --network host \
  -v /path/to/output:/output \
  ghcr.io/packagegraph/etl:latest \
  rpm --repo http://localhost:9998/rhel10-repo/appstream/ \
      --distro-name rhel --release-name 10 \
      --output /output/rhel10-appstream.nt

cat /path/to/output/rhel10-baseos.nt /path/to/output/rhel10-appstream.nt > rhel10.nt
```

### Key Notes

- **Do NOT use CentOS Stream repos as a substitute for RHEL.** They are different products with different packages.
- **Output files are large** (~9GB for RHEL 9, ~4GB for RHEL 10). Don't write to local disk on Mac — use the build host.
- **Always use `--entrypoint pg-collect`** when running the ETL container for collection. The default entrypoint runs the full ETL pipeline.
- **Use `--security-opt label=disable`** on Fedora/RHEL hosts to avoid SELinux relabeling errors with volume mounts.
