# packagegraph/platform

Rust collectors and enrichers for building an RDF knowledge graph of Linux and language-ecosystem package metadata, plus Kustomize manifests for deploying Apache Jena Fuseki as a SPARQL endpoint.

## What It Does

`pg-collect` streams package repository metadata into [N-Triples](https://www.w3.org/TR/n-triples/), conforming to the [PackageGraph ontology](https://github.com/packagegraph/ontology). Fuseki stores the triples and serves SPARQL queries.

**Current dataset:** ~37.5M triples across 10 named graphs — Fedora 43, Fedora Rawhide, Debian trixie, openSUSE Tumbleweed, RHEL 9/10, CentOS Stream 9/10, Homebrew, Gentoo, plus security and enrichment graphs.

## Repository Relationship

- **[`packagegraph/ontology`](https://github.com/packagegraph/ontology)** — OWL 2 ontology (classes, properties, SHACL shapes)
- **`packagegraph/platform`** (this repo) — collectors, enrichers, deployment

## Directory Layout

```
platform/
  etl/
    pg-collect/       Rust crate — 28 collectors, 7 enrichers, CLI
    scripts/          Shell helpers (upload, snapshot, pipeline orchestration)
    Containerfile     Multi-stage build: Rust → runtime with Jena + mc
  fuseki/             Apache Jena Fuseki configuration
  deploy/             Kustomize manifests for MicroShift / OpenShift
    base/
    overlays/dev/
  docs/               Reports, validation results, architecture decisions
  Makefile            Build, push, and deploy targets
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
| `load` | Load N-Triples into a Fuseki named graph (GSP, chunked) |
| `drop` | Drop a named graph from Fuseki |
| `seed` | Query Fuseki for package names to feed into language collectors |
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

# Load into Fuseki
cargo run --release -- load \
  --endpoint http://localhost:3031/packagegraph \
  --graph https://packagegraph.github.io/graph/fedora/43 \
  fedora-43.nt

# Run tests (420 tests)
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

Enrichers query Fuseki for packages and call external APIs to add metadata:

```bash
# GitHub VCS metadata (requires GITHUB_TOKEN)
cargo run --release -- enrich-github \
  --endpoint http://localhost:3031/packagegraph \
  --github-token "$GITHUB_TOKEN" \
  -o github.nt

# OSV security vulnerabilities for Debian packages
cargo run --release -- enrich-security \
  --endpoint http://localhost:3031/packagegraph \
  --ecosystem debian \
  -o security-debian.nt

# Cross-distribution equivalences from Repology
cargo run --release -- enrich-repology \
  --endpoint http://localhost:3031/packagegraph \
  -o repology.nt
```

All enrichers support `--cache-dir` for file-based API response caching with TTL, and optional Minio S3 sync for persistence across container restarts.

## Deployment

### Building Images

```bash
make build              # Build both etl and fuseki images (podman)
make build TAG=v0.10.0  # Pin a version tag
make push               # Push to ghcr.io/packagegraph
```

Container builds target `linux/amd64` via the `berstuk` podman system connection (see CLAUDE.md).

### Deploying

```bash
make deploy-dev         # Apply dev overlay (requires oc logged in)
make port-forward       # Forward Fuseki to localhost:3031
```

### Architecture

```
┌─────────────────────────────────────────────────────┐
│  MicroShift / OpenShift  (namespace: packagegraph)  │
│                                                     │
│  CronJobs (weekly)     Fuseki (writer, replicas=1)  │
│  ┌──────────────┐      ┌──────────────────┐         │
│  │ pg-collect   │─────>│ TDB2 on PVC      │         │
│  │ collect +    │      │ SPARQL endpoint   │         │
│  │ enrich +     │      └──────────────────┘         │
│  │ load         │              │                    │
│  └──────────────┘        TDB2 snapshot              │
│         │                      │                    │
│  ┌──────────────┐      ┌──────────────────┐         │
│  │ Minio (S3)   │<─────│ fuseki-reader ×N │         │
│  │ .nt archives │      │ (read replicas)  │         │
│  │ + cache      │      └──────────────────┘         │
│  └──────────────┘                                   │
└─────────────────────────────────────────────────────┘
```

## Querying the Data

```bash
# Port-forward Fuseki
oc port-forward svc/fuseki 3031:3030 -n packagegraph

# Count triples
curl -H 'Accept: text/tab-separated-values' \
  'http://localhost:3031/packagegraph/sparql' \
  --data-urlencode 'query=SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }'

# Find packages by name
curl -H 'Accept: text/tab-separated-values' \
  'http://localhost:3031/packagegraph/sparql' \
  --data-urlencode 'query=
    PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
    SELECT ?name ?version WHERE {
      ?p pkg:packageName "openssl" ; pkg:hasVersion ?v .
      ?v pkg:versionString ?version .
    } LIMIT 20'
```

See [docs/QUERYING.md](docs/QUERYING.md) for YASGUI setup, Jupyter notebooks, SPARQL reference, namespace prefixes, administration, and troubleshooting.
