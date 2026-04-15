# Changelog

All notable changes to the PackageGraph platform are documented in this file.

## [Unreleased] - 2026-04-15

### Collectors

- **10 new collectors in `pg-collect`:** RubyGems, Maven, CPAN, CRAN, Hackage, NuGet, Hex, FreeBSD, Nix, Chocolatey
  - **RubyGems** — Seed-based collector for rubygems.org API. Fetches gem metadata, dependencies, platform info, SHA-256 checksums
  - **Maven** — Seed-based collector for Maven Central POM XML. Scoped to RPM-packaged artifacts. Emits groupId/artifactId for cross-distro linking
  - **CPAN** — Seed-based collector for MetaCPAN API. Fetches Perl distribution metadata, PAUSE author IDs, maturity levels
  - **CRAN** — Bulk collector parsing PACKAGES.gz (Debian control-file format). Full collection of ~20K R packages with dependency types
  - **Hackage** — Seed-based collector for Hackage with Cabal file parsing. Fetches Haskell package metadata and build-depends
  - **NuGet** — Seed-based collector for NuGet v3 API. Service index discovery + registration endpoint for .NET package metadata
  - **Hex** — Seed-based collector for Hex.pm API. Fetches Elixir/Erlang package metadata and requirements
  - **FreeBSD** — Bulk collector downloading packagesite.txz (tar.xz with NDJSON). Full collection of FreeBSD binary packages
  - **Nix** — Bulk collector downloading packages.json.br (Brotli-compressed). Full collection of Nix derivations with attribute paths
  - **Chocolatey** — Bulk collector using NuGet v2 OData API. Paginated collection of Chocolatey community packages via Atom feed XML

- **7 new seed commands in `packagegraph` CLI:** `seed-rubygems`, `seed-maven`, `seed-cpan`, `seed-hackage`, `seed-nuget`, `seed-hex`
  - All query Fuseki RPM/Debian/Gentoo graphs for matching package names/homepages
  - Extract upstream package identifiers via homepage patterns and `upstreamPackageName` property
  - Fallback to name-based extraction (strip prefixes: `rubygem-`, `maven-`, `perl-`, `ghc-`, `dotnet-`, `erlang-`)

- **10 new CronJob manifests** in `deploy/overlays/dev/jobs/`
  - Schedules staggered 06:00-19:30 UTC Monday weekly to avoid resource contention
  - Language collectors: seed command → pg-collect → load pipeline
  - Bulk collectors (CRAN, FreeBSD, Nix, Chocolatey): direct pg-collect → load

- **Namespace constants added:**
  - Rust `uris.rs`: `GEMS`, `MAVEN`, `CPAN`, `CRAN`, `HACKAGE`, `NUGET`, `HEX`, `FREEBSD`, `NIX`, `CHOCO`
  - Python `namespaces.py`: matching constants for URI parity

### Dependencies

- **xz2 = "0.1"** — Added for FreeBSD packagesite.txz extraction (tar.xz format)
- **brotli = "7"** — Added for Nix packages.json.br decompression

## [Unreleased] - 2026-04-14

### Infrastructure

- **Fuseki TDB2 moved to PVC** — Migrated from `emptyDir` (100Gi limit on root partition) to a dedicated TopoLVM `PersistentVolumeClaim` (60Gi on `sdb`). Root partition usage dropped from 100% to ~58%.
- **Fuseki deployment strategy** — Changed from `RollingUpdate` to `Recreate` to prevent TDB2 lock conflicts during rollouts (`tdb.lock` held by old pod blocks new pod).
- **Fuseki memory** — Increased pod limit to 6Gi with JVM heap at `-Xmx2g`, leaving 4Gi for TDB2 memory-mapped file cache per [Jena guidance](https://github.com/apache/jena/discussions/2099).
- **Fuseki query timeout** — Added `arq:queryTimeout "30000,120000"` (30s soft / 120s hard) to protect against runaway YASGUI queries.
- **Fuseki GSP endpoint** — Enabled `fuseki:gsp-rw` at `/data` for Graph Store Protocol bulk uploads.
- **TDB2 snapshot archival** — New `snapshot-tdb2` CronJob archives TDB2 to Minio after collection. Future Fuseki restarts restore from snapshot via init container instead of re-running collectors (~2 min restore vs ~40 min re-collect).
- **Job TTL cleanup** — All CronJobs now set `ttlSecondsAfterFinished: 600` and `successfulJobsHistoryLimit: 1` / `failedJobsHistoryLimit: 1` to auto-clean completed pods and prevent disk exhaustion.
- **Removed base `etl-collect` job** — Redundant with distro-specific CronJobs. Removed from base kustomization along with `etl-single-distro.yaml` patch.
- **GitHub token secret** — Removed `secrets/github-token.yaml` from base kustomization to prevent `oc apply` overwriting the real token with the placeholder. Managed out-of-band via `oc create secret`.

### Performance: GSP Bulk Upload (pg-collect)

- **Graph Store Protocol loader** — Replaced batched `INSERT DATA` (1,000-1,800 triples/sec) with Fuseki's Graph Store Protocol (`POST /data?graph=<uri>`), achieving 8,000-13,000 triples/sec (8-10x improvement).
- **Chunked upload** — Large files (>50MB) are split into 50MB chunks at line boundaries and POSTed sequentially to avoid Fuseki JVM OOM on multi-GB N-Triples files.
- **Automatic fallback** — If GSP upload fails, automatically falls back to batched `INSERT DATA` with the original batch size.
- **Client timeout** — Increased from 300s to 600s for large file uploads.

### Enrichers

- **`enrich-github-vcs` CLI command** (new) — Fuseki-aware GitHub VCS enricher that queries packages with GitHub homepages, fetches repo metadata and recent commits from the GitHub API, and writes VCS triples as N-Triples for loading via `pg-collect load`.
- **`enrich-github-vcs` CronJob** (new) — Weekly job with Minio-backed cache persistence, scheduled before downstream enrichers.
- **Preflight auth check** — Validates GitHub token against `/rate_limit` endpoint before processing. Fails fast with clear error on 401 instead of making 73K failing API calls.
- **Error classification and backoff**:
  - 401 (auth failure) → abort immediately with clear message
  - 403 (rate limit) → wait for `X-RateLimit-Reset`, exponential backoff for secondary limits
  - 404 (repo deleted/private) → record as data quality issue, cache the 404
  - 5xx (server error) → retry 3x with exponential backoff
  - Timeout → retry 3x with exponential backoff
- **Adaptive rate pacing** — Uses `X-RateLimit-Remaining` and `X-RateLimit-Reset` headers to spread API calls evenly across the rate limit window, maximizing throughput within the 5,000 req/hr budget.
- **Deduplication** — `_query_packages()` deduplicates 73K package-homepage pairs down to unique repos, and pre-seeds `_processed_repos` from the existing enrichment graph to skip already-enriched repos on subsequent runs.
- **`pg-collect load` flag fix** — All enricher job YAMLs corrected from `--fuseki-endpoint`/`--graph-uri`/`--input-file` to the actual pg-collect CLI flags: positional `file`, `--graph`, `--endpoint`. Added `pg-collect drop` before load.

### Enricher Cache System

- **Minio-backed cache persistence** — API responses are cached locally during enrichment and synced to Minio (`enricher-cache/github/` prefix) via `mc mirror`. Cache survives pod restarts.
- **Periodic sync** — `CacheManager.sync_to_minio()` called every `ENRICHER_CACHE_SYNC_INTERVAL` items (default: 500) during processing, not just at exit. Ensures cache is preserved even on OOM or SIGTERM.
- **Cache warm on startup** — All enricher jobs download cache from Minio before processing via `mc mirror`. Downstream enrichers (license, metrics, vcs-activity) share the same `github` cache namespace.
- **`ENRICHER_CACHE_DISABLED` env var** — Set to `1` to skip all Minio cache sync (for debugging or fresh runs).
- **`ENRICHER_CACHE_SYNC_INTERVAL` env var** — Controls how often cache syncs during processing (default: `500`).
- **404 caching** — Dead repos (HTTP 404 on `/repos/{owner}/{repo}`) are cached with the same TTL as normal responses to avoid re-checking on every run.

### Data Quality System

- **`dq:` ontology** (new) — `ontology/dq.ttl` defines `dq:DataQualityIssue` class and properties for recording data quality issues as queryable triples in the graph.
- **`flag_quality_issue()` method** — Added to `NTriplesWriter` in `base.py`. Any enricher can record issues with type, field, raw value, and detection source.
- **Dead repo tracking** — Repos returning 404 are recorded with `vcs:repositoryStatus "not-found"` and `vcs:statusCheckedAt` timestamp, plus a `dq:DataQualityIssue` linked to the repo.
- **Malformed email detection** — Commit author emails with spaces, missing `@`, or backslashes are flagged as `dq:issueType "malformed-email"` instead of producing invalid URIs.
- **`validate-data` CronJob** (new) — Weekly validation job runs SPARQL checks for common data quality issues (None homepages, FTP URLs, empty versions, dead repos) and reports graph statistics.

### Ontology Updates

- **`ontology/vcs.ttl`** — Added 7 properties used by enrichers but previously missing:
  - `vcs:repositoryURL` — canonical repo URL
  - `vcs:repositoryDescription` — repo description
  - `vcs:repositoryStatus` — availability status (`not-found` for 404s)
  - `vcs:statusCheckedAt` — when status was last verified
  - `vcs:starCount` — stars (equivalent to `stargazerCount`)
  - `vcs:commitDate` — commit authored date (equivalent to `authorTimestamp`)
  - `vcs:releaseName` — human-readable release name

### Documentation

- **`docs/data-quality.md`** (new) — Complete guide to the data quality system: how it works, issue type taxonomy, SPARQL query cookbook, ontology reference, and environment variables.

### Environment Variables Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `ENRICHER_CACHE_DISABLED` | `0` | Set to `1` to skip Minio cache sync |
| `ENRICHER_CACHE_SYNC_INTERVAL` | `500` | Items between periodic cache syncs |
| `PYTHONUNBUFFERED` | `1` (in jobs) | Unbuffered stdout for real-time logs |
| `GITHUB_TOKEN` | (from secret) | GitHub API PAT (fine-grained: Metadata + Contents read-only on public repos) |
| `JAVA_OPTIONS` | `-Xmx2g` | JVM heap for Fuseki |
