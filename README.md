# packagegraph/platform

Rust collectors and enrichers for building an RDF knowledge graph of Linux and language-ecosystem package metadata. Deploys both Apache Jena Fuseki (read-write SPARQL) and QLever (high-performance read-only SPARQL) with Kustomize manifests for MicroShift/OpenShift.

## What It Does

`pg-collect` streams package repository metadata into [N-Triples](https://www.w3.org/TR/n-triples/) or [N-Quads](https://www.w3.org/TR/n-quads/), conforming to the [PackageGraph ontology](https://github.com/packagegraph/ontology). Data is loaded into Fuseki via Graph Store Protocol or into Minio for QLever batch indexing.

**Current dataset:** ~121M quads across 39 named graphs — Fedora 42-44, Debian trixie, openSUSE Tumbleweed, Alpine, Arch, Ubuntu, CentOS Stream, Homebrew, conda-forge, and 15 language ecosystem + security enrichment graphs. QLever serves queries with a median 15× speedup over Fuseki.

## Repository Relationship

- **[`packagegraph/ontology`](https://github.com/packagegraph/ontology)** — OWL 2 ontology (classes, properties, SHACL shapes)
- **`packagegraph/platform`** (this repo) — collectors, enrichers, deployment

## Directory Layout

```
platform/
  etl/
    pg-collect/                Rust crate — 28 collectors, 7 enrichers, CLI
    scripts/                   Shell helpers (upload, snapshot, migration)
    Containerfile              Multi-stage build: Rust → runtime with Jena + mc
    Containerfile.qlever-rebuild  QLever base + mc/kubectl/jq for index rebuilds
  fuseki/                      Apache Jena Fuseki configuration
  deploy/
    base/                      Shared resources (Fuseki, QLever, Minio, proxy)
    overlays/dev/              Dev overlay with CronJobs and RBAC
    archive/fuseki/            Archived Fuseki manifests for rollback
  spike/                       QLever evaluation results and benchmark scripts
  Makefile                     Build, push, and deploy targets
```

## pg-collect

A streaming collector that reads package repository metadata and emits N-Triples to stdout or file. No intermediate storage — each collector streams directly from the source format (repodata XML, Packages.gz, JSON APIs) into RDF triples.

### Collectors (28)

| Category | Collectors |
|----------|-----------|
| **RPM-based** | `rpm`, `rpm-full` (multi-arch + koji + spec) |
| **Debian-based** | `debian`, `deb-full` (multi-arch + sources + salsa) |
| **Other Linux** | `alpine`, `arch`, `gentoo`, `void`, `nix`, `opensuse` (via `rpm`) |
| **Embedded** | `openwrt`, `openwrt-full`, `yocto`, `buildroot` |
| **macOS/Windows** | `homebrew`, `chocolatey` |
| **Sandboxed** | `flatpak`, `snap` |
| **Language** | `npm`, `pypi`, `cargo`, `gomod`, `maven`, `rubygems`, `cpan`, `cran`, `hackage`, `nuget`, `hex`, `conda` |
| **Security** | `osv`, `collect-bodhi`, `collect-glsa` |

### Enrichers (7)

| Enricher | Source | What it adds |
|----------|--------|-------------|
| `enrich-github` | GitHub GraphQL API | VCS metadata, languages, license, topics, activity metrics |
| `enrich-security` | OSV API | Per-package vulnerability data |
| `enrich-advisory` | RHSA / DSA feeds | Vendor security advisories with CVE linkages |
| `enrich-nvd` | NIST NVD API 2.0 | CVSS scores, CWE classifications, CPE configurations |
| `enrich-koji` | Koji XML-RPC | RPM build metadata and SRPM provenance |
| `enrich-npm-provenance` | npm registry | SLSA build attestations |
| `enrich-repology` | Repology API | Cross-distribution package equivalences |

### Utilities

| Command | Purpose |
|---------|---------|
| `load` | Load N-Triples into Fuseki (GSP) or Minio (`--write-backend minio`) |
| `drop` | Drop a named graph from Fuseki or Minio |
| `seed` | Query SPARQL endpoint for package names to feed into language collectors |
| `rpm-full` | Consolidated multi-arch RPM collection with optional koji + spec enrichment |
| `deb-full` | Consolidated multi-arch Debian collection with sources + salsa enrichment |
| `openwrt-full` | Multi-feed OpenWrt collection with upstream + attestation enrichment |

### Quick Start

```bash
cd etl/pg-collect
cargo build --release

# Collect Fedora 43 x86_64
cargo run --release -- rpm \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/43/Everything/x86_64/os/ \
  --distro fedora --release 43 --arch x86_64 \
  -o fedora-43.nt

# Collect Debian trixie amd64
cargo run --release -- debian \
  --repo http://deb.debian.org/debian \
  --dist trixie --component main --arch amd64 \
  -o debian-trixie.nt

# Load into Fuseki (default)
cargo run --release -- load \
  --endpoint http://localhost:3031/packagegraph \
  --graph https://packagegraph.github.io/graph/fedora/43 \
  fedora-43.nt

# Or load into Minio for QLever indexing
cargo run --release -- \
  --write-backend minio \
  load fedora-43.nt \
  --graph https://packagegraph.github.io/graph/fedora/43 \
  --endpoint http://unused

# Output N-Quads directly (graph URI in every line)
cargo run --release -- --graph https://packagegraph.github.io/graph/fedora/43 \
  rpm --url ... -o fedora-43.nq

# Run tests (723 tests)
cargo test
```

### Consolidated Collectors

For production use, the `*-full` commands combine multi-arch collection with inline enrichment in a single pass:

```bash
# Fedora 43: collect x86_64 + aarch64, then enrich with koji build metadata and spec files
cargo run --release -- rpm-full \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/43/Everything/x86_64/os/ \
  --url https://dl.fedoraproject.org/pub/fedora/linux/releases/43/Everything/aarch64/os/ \
  --distro fedora --release 43 \
  --with-koji --with-spec \
  --cache-dir /tmp/cache \
  -o fedora-43-full.nt

# Debian trixie: collect amd64 + arm64 with Sources.gz and salsa build provenance
cargo run --release -- deb-full \
  --repo http://deb.debian.org/debian \
  --dist trixie --component main \
  --arch amd64 --arch arm64 \
  --with-sources --with-salsa \
  -o debian-trixie-full.nt
```

### Enrichment

Enrichers query the SPARQL endpoint for packages and call external APIs to add metadata. They support `--sparql-backend qlever` to query QLever instead of Fuseki, and `--sparql-username`/`--sparql-password` for Basic Auth:

```bash
# GitHub VCS metadata (requires GITHUB_TOKEN)
cargo run --release -- enrich-github \
  --endpoint http://localhost:3031/packagegraph \
  --github-token "$GITHUB_TOKEN" \
  -o github.nt

# Same enricher against QLever
cargo run --release -- \
  --sparql-backend qlever --qlever-access-token "$TOKEN" \
  enrich-github \
  --endpoint http://localhost:7001 \
  --github-token "$GITHUB_TOKEN" \
  -o github.nt
```

All enrichers support `--cache-dir` for file-based API response caching with TTL, and optional Minio S3 sync for persistence across container restarts.

## Deployment

### Building Images

```bash
make build                   # Build etl + fuseki images (podman on berstuk)
make build-qlever-rebuild    # Build QLever index rebuild image
make push                    # Push to ghcr.io/packagegraph
make push-qlever-rebuild     # Push qlever-rebuild image
```

Container builds target `linux/amd64` via the `berstuk` podman system connection (see CLAUDE.md).

### Deploying

```bash
make deploy-dev         # Apply dev overlay (requires oc logged in)
make port-forward       # Forward Fuseki to localhost:3031
```

### Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  MicroShift / OpenShift  (namespace: packagegraph)           │
│                                                              │
│  CronJobs (weekly)      Fuseki (read-write SPARQL)           │
│  ┌──────────────┐       ┌──────────────────┐                 │
│  │ pg-collect   │──GSP─>│ TDB2 on PVC      │                 │
│  │ collect +    │       │ /sparql, /update  │                 │
│  │ enrich +     │       └──────────────────┘                 │
│  │ load         │                                            │
│  └──────┬───────┘       QLever (read-only SPARQL, 15× faster)│
│         │               ┌──────────────────┐                 │
│         │  .nt + .graph  │ Pre-built index  │                 │
│         └──Minio────────>│ on PVC (2.3 GiB) │                 │
│                          │ port 7001        │                 │
│  ┌──────────────┐        └────────┬─────────┘                │
│  │ Minio (S3)   │                 │                          │
│  │ .nt files    │   rebuild-qlever-index CronJob (daily)     │
│  │ .graph       │   ┌──────────────────────────────┐         │
│  │ sidecars     │<──│ mc mirror → qlever-index →   │         │
│  │ qlever-index │   │ upload → promote → restart   │         │
│  │ archives     │   │ (completeness gate: ≥75% of  │         │
│  └──────────────┘   │  previous successful run)    │         │
│                     └──────────────────────────────┘         │
│  ┌──────────────┐                                            │
│  │ sparql-proxy │  nginx TLS + Basic Auth                    │
│  │ :8443/sparql │  (routes to Fuseki, switchable to QLever)  │
│  └──────────────┘                                            │
└──────────────────────────────────────────────────────────────┘
```

## Querying the Data

```bash
# Via Fuseki (read-write, slower)
oc port-forward svc/fuseki 3031:3030 -n packagegraph
curl -H 'Accept: text/tab-separated-values' \
  'http://localhost:3031/packagegraph/sparql' \
  --data-urlencode 'query=SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }'

# Via QLever (read-only, 15× faster)
oc port-forward svc/qlever 7001:7001 -n packagegraph
curl -H 'Accept: application/sparql-results+json' \
  'http://localhost:7001/' \
  -d 'query=SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }' \
  -d "access-token=$QLEVER_ACCESS_TOKEN"

# Via SPARQL proxy (TLS + Basic Auth, external access)
curl -k -u sparqluser:pass \
  'https://localhost:8443/packagegraph/sparql' \
  --data-urlencode 'query=SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }'
```

See [docs/QUERYING.md](docs/QUERYING.md) for SPARQL reference, namespace prefixes, and troubleshooting.
