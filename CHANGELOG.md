# Changelog

All notable changes to the PackageGraph platform are documented in this file.

## [Unreleased] - 2026-05-12 (ontology v0.10.0)

### Collectors

- **Capability entities (CQ-PM-03)** — RPM and Debian collectors now emit `pkg:Capability` entities with `pkg:providesCapability` triples alongside existing provides emission. Stable URIs at `d/capability/{name}` enable "which packages provide libssl.so.3?" queries.
- **Spec file Source/Patch extraction** — Spec collector parses all `Source*:` and `Patch*:` entries (not just `Source0`). Emits `rpm:hasSpecSource` literals and `rpm:Patch` entities linked via `rpm:hasPatch`. Patch URIs include index to prevent basename collisions.
- **OpenWrt reclassification** — Dropped explicit `pkg:Package` typing from OpenWrt collector; `OpkgPackage` inherits `Package` via `subClassOf` (v0.9.0-pre). Added cross-feed deduplication via `HashSet<(name, version)>` — duplicates only emit `openwrt:feed` literal. Exposed `collect_with_writer()` and `OpenWrtPackageMeta` for downstream pipeline stages.
- **OpenWrt upstream collector** (new) — `collect_openwrt_upstream` creates `UpstreamProject` entities from Makefile source URLs, resolving sub-packages via parent map.
- **OpenWrt opkg index collector** (new) — `collect_opkg_index` collects binary package metadata from opkg `Packages.gz` feeds, linking binaries to source packages via identity map.
- **OpenWrt attestation enricher** (new, skeleton) — `enrich_openwrt_attestation` for GitHub SLSA attestations on OpenWrt binaries. Disabled until upstream adopts `actions/attest-build-provenance`.
- **`uris::encode` made public** — Exposed for use by collectors building percent-encoded URI path components (Capability URIs, Patch URIs).

### Collector Bug Fixes

- **fix(openwrt):** `identity_map`, `parsed_meta`, and `parent_map` now use `entry().or_insert()` for first-wins semantics, matching the `(name, version)` dedup key. Prevents silent metadata overwrite when the same package name appears at different versions across feeds.
- **fix(collect-spec):** Patch URI includes the `Patch*:` index (`d/patch/.../idx-name`) so two patches with the same basename from different URLs produce distinct RDF nodes.

### Deployment

- **Consolidated full collectors** — Replaced per-arch Fedora 43, Debian trixie, and CentOS Stream 9 jobs with `*-full` variants (multi-arch + enrichment). Superseded jobs commented in kustomization for traceability.
- **`collect-debian-trixie-full`** (new) — Weekly CronJob: multi-arch (amd64+arm64) Debian collection with Sources.gz, build-deps, maintainers, and salsa enrichment.
- **`collect-openwrt-2410-full`** (new) — Weekly CronJob: multi-feed (packages/luci/routing/telephony) OpenWrt collection with upstream enrichment. Uses initContainer to clone feed repos.
- **`rebuild-tdb2` v0.8.0-pre** (new) — One-shot Job for offline TDB2 rebuild from Minio N-Triples. Builds database with `tdb2.tdbloader`, uploads content-hashed snapshot, restarts Fuseki.

### Scripts

- **`dataset-snapshot.sh`** (new) — Captures triple counts and predicate distributions per graph from Fuseki as JSON. Supports `--diff` to compare two snapshots.
- **`v060-pipeline.sh`** (new) — Orchestrates the full v0.6.0 rebuild: waits for collectors → rebuild-tdb2 → ecosystem collectors → second rebuild.

### Documentation

- **CQ status legend** — Added Status Legend section to `docs/competency-questions.md` distinguishing ontology-complete, pipeline-complete, and query-complete readiness layers.
- **CQ-PM-03 predicate fix** — `pkg:provides` → `pkg:providesCapability` in competency question query to match collector emission.
- **CQ-SC-03 SLSA status** — Updated from BLOCKED to PARTIAL; npm-provenance enricher now emits L2 attestations.
- **16 spec plans** added (`docs/plans/2026-04-23` through `2026-05-12`): collector metadata enrichment, CQ validation harness, CVE metadata, GitHub enrichment strategies, consolidated RPM/Debian/OpenWrt collectors, SPDX SBOM ingestion, platform engineering hardening.
- **13 validation reports** added (`docs/reports/`, `docs/validation/`): advisory collector status, CQ validation runs, enrichment availability, GitHub enricher validation, live emitted shape contracts, NVD/RPM/vulnerability inventories.

### CI/CD

- **Rust tests in CI** — Added `test-rust` job to `ci.yml` running `cargo test` on `etl/pg-collect` (420 tests). Image builds gated on both Rust and Python tests passing.
- **Reusable workflow fix** — Added `workflow_call` trigger to `ci.yml` so `release.yml` can call it as a reusable workflow.
- **Image tag pinning** — Base deployment manifests pinned from `:latest` to `v0.10.0`. Makefile `TAG ?= latest` preserved for local dev builds.

### Housekeeping

- **Minio credentials** — Replaced hardcoded `minioadmin/minioadmin` in base secret with `CHANGE_ME` placeholders. Real credentials created out-of-band via `oc create secret`.
- **Dead Python fallback removed** — `entrypoint.sh` no longer falls back to `packagegraph collect` / `packagegraph build` (Python CLI removed from container in v0.6.0). TDB2 build path calls `tdb2.tdbloader` directly.
- **Hardcoded paths removed** — `cq-validate.py` reads `FUSEKI_ENDPOINT` and `ONTOLOGY_REPO` from environment (defaults: `localhost:3031`, `../ontology`). Absolute `/Users/...` paths replaced with repo-relative paths across 10 plan/report files.
- **README rewritten** — Reflects Rust `pg-collect` collector stack. Removed references to nonexistent `packagegraph query-*` CLI commands.
- **CQ validation** — Updated frozen commit hash to `7db2f99` (ontology snapshot).
- **`.gitignore`** — Added `test-data/collector/*.nt` (233MB) and `etl/snapshots/` to prevent large fixtures from entering git.
- **Test data** — Added `test-data/manifest.json` (collector output expectations) and `test-data/coverage-report.json` (predicate coverage tracking).

## [Unreleased] - 2026-04-25

### Collectors

- **Self-discovery mode** — 12 collectors gained `collect_discover()` methods that query Fuseki for upstream package names, eliminating external Python seed scripts. Fuseki-based: cargo, cpan, gomod, hackage, hex, maven, npm, nuget, pypi, rubygems. Native API discovery: flatpak (Flathub appstream), snap (Snap Store v1 search with pagination).
- **RPM inline advisory collection** — RPM collector now parses `updateinfo.xml` from repository metadata and emits `sec:SecurityAdvisory` triples linked to collected binary packages. Eliminates separate advisory enrichment pass for RPM-based distributions.
- **Bodhi advisory collector** (new) — `collect-bodhi` parses Fedora Bodhi RSS feeds, resolves NVR→binary package mappings via Fuseki, emits advisory triples with CVE linkages.
- **GLSA advisory collector** (new) — `collect-glsa` fetches Gentoo GLSA XML index, parses individual advisory files, resolves package atoms, emits advisory triples with affected version ranges.
- **NVD enricher** (new) — `enrich-nvd` queries NIST NVD API 2.0 for CVE details including CVSS scores, CWE classifications, and affected configurations.
- **Dependency type URI alignment** — 8 collectors (buildroot, cargo, gomod, maven, npm, openwrt, pypi, yocto) now emit `pkg:dependencyType` as SKOS concept URIs instead of string literals, matching ontology v0.6.0.

### Enrichers

- **GitHub enricher rewrite** — Replaced REST API v3 with GraphQL v4. Single query per repo fetches metadata, languages, license, topics, releases, contributors, and activity metrics. Incremental mode (`--max-repos`, `--load-graph`) for partial runs with direct GSP loading. Data quality reporting via `dq:DataQualityIssue` triples. Ranked repo discovery (unenriched first, by package coverage).

### Collector Bug Fixes

- **fix(enricher):** GitHub URL parser now handles dotted repo names (docopt.cpp, vue.js, socket.io) and strips URL fragments (#readme) before matching.
- **fix(chocolatey):** Handle HTTP 406 at Chocolatey OData pagination limit (~10K packages) as end-of-results. Fall back to `<title>` element when `d:Id` is missing from properties.
- **fix(gentoo):** Emit `pkg:partOfRelease` triple (was missing, making packages invisible to release-scoped queries).

### Deployment

- **Seedless collection jobs** — Ecosystem collector jobs (cargo, cpan, flatpak, gomod, hackage, hex, maven, npm, nuget, pypi, rubygems, snap) switched from Python seed scripts to `pg-collect --endpoint` discovery mode.
- **Multi-repo RPM collection** — Fedora 43/44 jobs updated to collect from both release and updates repositories.
- **GitHub enricher incremental** — `enrich-github` job updated with `--max-repos` and `--load-graph` for incremental enrichment.
- **Miscellaneous** — Arch ARM mirror URL updated. Flatpak job deadline increased to 1800s.

### Housekeeping

- **Ontology mirror untracked** — `etl/ontology/` removed from git index (was force-added despite being gitignored; copied at build time from ontology repo).
- **CQ validation** — Updated frozen commit hash to ontology v0.6.0. Added aggregate CQ handling for single-row summary queries.

### Plans

- `docs/plans/2026-04-23-ecosystem-advisory-collectors.md` — VERIFIED

## [Unreleased] - 2026-04-19

### Ontology: Semantic Depth & Data Quality (v0.6.0)

- **OWL property characteristics** — `directlyDependsOn` declared `owl:TransitiveProperty`, `equivalentInDistribution` declared `owl:SymmetricProperty`, `directlyConflictsWith` declared `owl:SymmetricProperty`, `partOfDistribution` declared `owl:TransitiveProperty`
- **Property hierarchy** — `rpmRequires`, `debDepends`, `pacmanDepends` declared `rdfs:subPropertyOf directlyDependsOn`. Same for conflict and provides properties. Enables cross-distro dependency queries via the generic property.
- **Cross-class object properties** — `sec:affectsPackage` (Vulnerability → PackageIdentity), `sec:CVE` class and `sec:cveEntity` property (shared CVE nodes across graphs), `sec:hasCWE` (link to CWE entity URIs), `core:contributesTo`/`core:hasContributor` (Maintainer ↔ Repository), `core:derivedFromDistribution` (Distribution lineage), `core:releaseContains` (inverse of `partOfRelease`)
- **Temporal properties** — `core:lastReleaseDate`, `core:lastCommitDate` on PackageIdentity
- **Data quality properties** — `core:repoType` (release/development/build/updates), `core:repoSourceURL` (collection source URL), `core:isPhantomPackage` (non-installable repo metadata entries), `core:supersededBy`/`core:supersedes` (subpackage reorganization), `core:excludedArchitecture` (architecturally impossible packages)
- **Dead property removal** — Removed `testedBy`, `wasPackagedBy`, `wasPublishedBy` (zero usage in production data)
- **SKOS concept schemes** — 5 schemes with 25 concepts:
  - `DependencyTypeScheme` (9 concepts: strong/weak hierarchy with requires, recommends, suggests, etc.)
  - `SeverityScheme` (4 concepts: critical → low with CVSS-aligned ordering)
  - `AdvisoryTypeScheme` (4 concepts: RHSA, DSA, GLSA, USA)
  - `ChecksumAlgorithmScheme` (4 concepts: sha256, sha512, sha1, md5)
  - `ArchExclusionScheme` (4 concepts: Intel HW, AMD GPU, x86 emulation, arch-specific compiler)
- **External alignments** — `references/alignments.ttl` with `owl:equivalentProperty`/`owl:equivalentClass` declarations for PROV-O, SPDX 3.0, Schema.org
- **Ontology mirror sync** — Updated `sync-ontology.sh` to handle multiple `core/*.ttl` files (35 files, up from 34)

### Collectors: Parameterization & Entity Emission

- **Collector parameterization** — All 28 collectors parameterized with `--distro`/`--release`/`--arch` CLI arguments for multi-distribution URI support. Enables collecting the same distro under different names (e.g., Ubuntu from Debian collector).
- **SPDX License entities** — All 27 collectors emit `core:hasLicense` object property with SPDX license URIs (e.g., `<https://spdx.org/licenses/MIT>`) alongside existing `licenseName` string literals. Each license entity typed as `core:License`. Backward-compatible.
- **Security bridge** — OSV collector emits `sec:affectsPackage` linking vulnerabilities directly to PackageIdentity URIs (eliminates 1.9M string-join queries). Emits `sec:hasCWE` with entity URIs from cwe.mitre.org. Emits `sec:cveEntity` with shared CVE entity URIs for cross-graph joins.
- **Shared CVE entities** — Advisory enricher updated to use `cve_entity_uri()` so RHSA/DSA advisories and OSV vulnerabilities share the same CVE node IRI.
- **Temporal properties** — GitHub enricher emits `core:lastCommitDate` from GitHub API `pushed_at` timestamp.
- **Maintainer→Repository links** — GitHub enricher SPARQL query extended to include maintainer URIs. Emits `core:contributesTo` and `core:hasContributor` bidirectional links.
- **Phantom package detection** — RPM collector parses `filelists.xml` alongside `primary.xml` to detect non-installable packages (e.g., TeX Live virtual subpackages). Packages without files marked `isPhantomPackage true`. Fedora 44: 9,366 of 76,354 packages detected as phantom.
- **Repo metadata emission** — RPM and Debian collectors emit `core:repoType` and `core:repoSourceURL` on DistributionRelease entities. Repo type inferred from URL pattern (koji→build, development→development, releases→release) with `--repo-type` CLI override.
- **`pg-collect seed` subcommand** — Queries Fuseki for distinct package names in a named graph, writes to text file (one per line). Replaces old Python `packagegraph seed-*` commands for NPM/PyPI collector seeding.
- **URI builders** — Added `spdx_license_uri()`, `cwe_uri()`, `cve_entity_uri()` to `uris.rs`

### Collector Bug Fixes

- **fix(alpine):** Use `MultiGzDecoder` for concatenated gzip archives. Alpine's signed APKINDEX.tar.gz uses multiple gzip members; `GzDecoder` only read the first (512-byte signature), missing all package data. Fix: 0 packages → 24,171 packages.
- **fix(freebsd):** Support zstd `packagesite.pkg` format. FreeBSD pkg repo changed from `packagesite.txz` (xz) to `packagesite.pkg` (zstd). Collector now tries `.pkg` first, falls back to `.txz`. Fix: HTTP 404 → 36,509 packages.
- **fix(advisory):** Fix RHSA pagination start page from `page=0` to `page=1`. Red Hat CVE API requires 1-based pages. Fix: 400 Bad Request → 10,482 advisories.

### Deployment: Job CLI Alignment

- **Python elimination** — All enricher CronJobs migrated from Python `packagegraph` CLI to Rust `pg-collect` CLI. ETL container no longer requires Python.
- **GitHub enricher consolidation** — 4 separate jobs (enrich-github-vcs, enrich-license, enrich-metrics, enrich-vcs-activity) replaced by single `enrich-github.yaml` using `pg-collect enrich-github` (which merges all 4 Python enrichers).
- **New enricher jobs** — `enrich-repology.yaml` (cross-distro equivalences), `enrich-github.yaml` (consolidated)
- **Enricher CLI migration** — `--fuseki-endpoint`→`--endpoint`, `--output-dir`→`-o`, `--type`→`--advisory-type`, `--distro-name`→`--distro` across advisory, npm-provenance, koji enrichers
- **NPM/PyPI seed migration** — Jobs updated to use `pg-collect seed --endpoint --graph` instead of `packagegraph seed-npm`/`seed-pypi`
- **Debian trixie dist fix** — `--dist stable` → `--dist trixie` in both trixie collector jobs
- **Fedora 44 beta** — New `collect-fedora-44.yaml` using `development/44/Everything/x86_64/os/`. Rawhide suspended from kustomization.
- **Fedora RISC-V** — Fixed `collect-fedora-riscv64.yaml` to use `riscv-koji.fedoraproject.org/repos/f44-build/latest/riscv64/` (65,863 packages)
- **Stale job cleanup** — Deleted enrich-github-vcs, enrich-license, enrich-metrics, enrich-vcs-activity, enrich-security-lang, enrich-security-system (6 files)
- **GitHub token secret** — Documented creation process in `deploy/base/secrets/github-token.yaml`, created on both clusters

### Scripts: Python-Free Pipeline

- **upload-nt.sh** — Replaced `python3` JSON manifest update with `jq`. Added atomic writes (temp file → mv), malformed JSON recovery, error handling.
- **rebuild-tdb2.yaml** — Replaced inline Python (graph iteration, manifest building) with shell/jq. Fixed `stat` command for Linux (`-c %s` first, `-f %z` fallback). Replaced `bc` with `awk` for size calculation.
- **sync-ontology.sh** — Updated to handle multiple `core/*.ttl` files (not just `core.ttl`)

### Container

- **jq added** — `jq` added to Containerfile `apt-get install` for JSON processing in upload-nt.sh and rebuild-tdb2
- **Python removed** — No Python in ETL container. All scripts and jobs use Rust `pg-collect` or shell/jq.
- **Fuseki init container** — Gracefully skips snapshot download when `tdb2/latest` is missing in Minio, instead of crash-looping. Enables fresh deployments without pre-existing snapshots.
- **Fuseki RAM doubled** — Container limit 6Gi→12Gi, JVM heap 2g→4g (8Gi for TDB2 page cache)
- **Rebuild-tdb2 RAM** — Container limit 4Gi→8Gi, JVM heap 3g→5g for 15+ graph builds

### Dataset

**62.7M triples** across 15 named graphs:

| Graph | Triples | Packages | New |
|-------|---------|----------|-----|
| conda-forge | 11.7M | 140K | |
| fedora/43 | 9.3M | 77K | |
| fedora/44 | 9.1M | 76K | New (beta) |
| opensuse/tumbleweed | 8.4M | 56K | |
| fedora/44/riscv64 | 7.8M | 66K | New (Koji build) |
| security/osv | 6.0M | — | +affectsPackage, CWE, CVE entities |
| debian/trixie | 5.6M | 69K | |
| centos-stream/9 | 1.6M | 5K | |
| alpine/v3.20 | 1.0M | 24K | Fixed (was 30K security-only) |
| freebsd/14 | 989K | 37K | Fixed (was 404) |
| ubuntu/noble | 462K | 6K | |
| homebrew | 405K | 16K | |
| advisory-dsa | 201K | — | Shared CVE entities |
| advisory-rhsa | 61K | — | Fixed pagination |
| ontology | 7K | — | +SKOS, OWL, alignments |

### Reports

- **Fedora 44 Beta Packaging Analysis** — `docs/reports/2026-04-18-fedora-44-beta-packaging-analysis.md`. Deprecated packages (97), license compliance (2,927 non-SPDX), dependency heavyweights, version-pinned interpreter deps, LLVM version accumulation, GConf2 retirement candidate, 5,304 packages dropped from F43.
- **Fedora RISC-V Coverage Triage** — `docs/reports/2026-04-18-fedora-riscv64-triage.md`. 86.3% coverage (65,863/76,354). Top gaps: GHC (1,339), Rust crates (1,081), TeX Live (675). Ecosystem-level analysis complementing community tracker.
- **RISC-V Top 25 Porting Priorities** — `docs/reports/2026-04-18-riscv64-triage-top25.md`. Validated against riscv-koji.fedoraproject.org. False positives identified and documented. Rust GNOME 0.21 chain as actionable target.

### Plans

- `docs/plans/2026-04-17-job-cli-alignment.md` — VERIFIED
- `docs/plans/2026-04-18-ontology-semantic-depth.md` — Complete (13 tasks across 3 phases)
- `docs/plans/2026-04-18-etl-data-quality.md` — VERIFIED

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
- **TDB2 snapshot archival** — The `rebuild-tdb2` CronJob produces consistent offline TDB2 snapshots from source N-Triples via `tdb2.tdbloader` and archives them to Minio. Fuseki restarts restore from snapshot via init container (~2 min restore vs ~40 min re-collect). The previous `snapshot-tdb2` live-tar approach was removed because raw tar of active TDB2 files is not transactionally safe.
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
