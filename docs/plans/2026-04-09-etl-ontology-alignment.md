# ETL Ontology Alignment & Data Source Enrichment Plan

Created: 2026-04-09
Author: sovereign@local
Status: COMPLETE
Approved: Yes
Iterations: 1
Worktree: No
Type: Feature

## Summary

**Goal:** Re-align the ETL pipeline to emit triples using the canonical PackageGraph ontology namespace (`pkg:`, `sec:`, `vcs:`) instead of ad-hoc format-specific namespaces (`deb:`, `rpm:`), then add new data source collectors to enable SPARQL query patterns for software provenance analysis.

**Architecture:** Introduce a shared `GraphBuilder` abstraction that encapsulates ontology-aligned triple emission. Refactor existing collectors (Debian, RPM) to use it, then add new collectors (repology, GitHub, NVD, koji) that extend it. Move all collectors into a `collectors/` subpackage.

**Tech Stack:** Python 3.12, rdflib, requests, Click CLI. External APIs: repology.org REST, GitHub REST v3, NVD REST 2.0, koji XML-RPC.

## Scope

### In Scope

1. **ETL namespace alignment** — Replace `deb:`/`rpm:` ad-hoc triples with canonical `pkg:` ontology triples
2. **Maintainer identity resolution** — Parse `Maintainer: Name <email>` into `pkg:Maintainer` resources
3. **Multi-arch Debian collection** — `--arch` accepts multiple architectures
4. **Source→Binary package linking** — Parse `Source:` field into `pkg:SourcePackage` + `pkg:builtFromSource`
5. **Multi-release RPM collection** — Collect from multiple Fedora release repos
6. **repology.org cross-distro mapping** — Populate `pkg:equivalentInDistribution`
7. **GitHub/GitLab VCS metadata** — Populate `vcs:Repository`, `vcs:Commit`, `vcs:authoredBy`
8. **NVD/OSV security data** — Populate `sec:Vulnerability`, `sec:affectsVersion`, `sec:fixedInVersion`
9. **koji build metadata** — Populate `pkg:BuildActivity`, `pkg:wasBuiltBy`, `pkg:usedDependency`

### Out of Scope

- Fuseki configuration changes (existing config works with any namespace)
- Ontology schema modifications (core.ttl, security.ttl, vcs.ttl are sufficient)
- Web UI or query CLI (separate future plan)
- OWL reasoning configuration in Fuseki
- Jena Text / Lucene full-text search setup
- GitLab API support (GitHub only for now — GitLab can be added later with same pattern)

## Approach

**Chosen:** Graph builder abstraction

**Why:** Separates format-specific parsing from ontology-aligned triple emission. Eliminates duplication between Debian, RPM, and future collectors. Each collector becomes a thin parser that feeds structured data to GraphBuilder methods. The GraphBuilder is the single source of truth for how ontology triples are constructed. Cost: more upfront structural change than inline refactor.

**Alternatives considered:**
- *Inline refactor* — Change namespace strings in-place within each collector. Less upfront work but duplicates mapping logic across collectors. Rejected because 6+ collectors would each independently construct ontology triples, leading to inconsistency.
- *Declarative mapping (R2RML-style)* — Define YAML/JSON field-to-triple mappings. Most flexible but heavyweight abstraction. Rejected as overkill for this project's scale.

## Context for Implementer

> Write for an implementer who has never seen the codebase.

### Patterns to follow

- **Parallel processing pattern** (`etl/packagegraph/collector.py`): `BaseCollector.collect_parallel()` uses `ProcessPoolExecutor` with static chunk methods. Each worker creates a local `Graph()`, serializes to a temp `.ttl` file, main process merges. GraphBuilder must work within this pattern — instantiate with a local graph in each worker process.

- **CLI pattern** (`etl/packagegraph/cli.py`): Click command group with `collect` and `build` subcommands. New collectors get new CLI subcommands under the same group.

- **Profiling pattern** (`etl/packagegraph/profiler.py`): Wrap blocks with `with profiler.step("label"):` for timing. Use in all new collectors.

### Conventions

- **Naming:** Snake case for modules, PascalCase for classes. Collector classes named `<Format>Collector`.
- **Error handling:** Use `click.echo(..., err=True)` for warnings, `sys.exit(1)` for fatal errors, `raise RuntimeError(...)` for internal errors.
- **Dependencies:** External HTTP calls use `requests`. Mock with `unittest.mock.patch`.

### Key files

| File | Purpose |
|------|---------|
| `etl/packagegraph/collector.py` | `BaseCollector` ABC with parallel processing |
| `etl/packagegraph/debian_collector.py` | Debian Packages.gz collector (to be refactored) |
| `etl/packagegraph/rpm_collector.py` | RPM repomd.xml collector (to be refactored) |
| `etl/packagegraph/cli.py` | Click CLI — `collect` and `build` commands |
| `etl/packagegraph/minio.py` | Content-addressable Minio upload |
| `etl/packagegraph/tdb.py` | TDB2 index builder via tdb2.tdbloader |
| `etl/entrypoint.sh` | Container ETL pipeline (collect → build → upload) |
| `ontology/core.ttl` | Core ontology schema (classes + properties) |
| `ontology/security.ttl` | Security ontology (Vulnerability, SecurityAdvisory) |
| `ontology/vcs.ttl` | VCS ontology (Repository, Commit, Branch, etc.) |

### Gotchas

- **Parallel processing**: Static chunk methods can't access `self`. GraphBuilder must be instantiable without shared state. Each worker creates its own `GraphBuilder(Graph())`.
- **URI encoding**: Package names can contain `+` (e.g., `libstdc++-dev`). Must use `urllib.parse.quote()` with `safe=""` to encode all special characters in URI path segments.
- **Debian `Source:` field format**: Sometimes includes version: `Source: curl (8.4.0-2)`. Must parse both `Source: curl` and `Source: curl (8.4.0-2)`.
- **Debian `Maintainer:` field format**: `Name <email>` but can also be team addresses like `Debian OpenSSL Team <team+openssl@tracker.debian.org>`.
- **RPM `format_element`**: XML element stored directly in `packages_data` dict — must be parsed in the chunk worker, not passed to GraphBuilder.
- **Rate limiting**: GitHub API (5000 req/hr authenticated, 60 unauthenticated), NVD API (10 req/minute without key), repology (no published limit, be polite).

### Domain context

- **Binary vs Source packages**: In Debian, one source package (e.g., `curl`) produces multiple binary packages (e.g., `curl`, `libcurl4`, `libcurl4-openssl-dev`). The `Source:` field in binary package metadata links back to the source. This grouping is essential for architecture porting — you port source packages, not binary ones.
- **Cross-distro equivalence**: The same upstream project (e.g., OpenSSL) is packaged under different names in different distributions (e.g., `openssl` in Debian, `openssl-libs` in Fedora). repology.org tracks these equivalences.
- **Maintainer identity**: A person may maintain packages under different emails across distributions. `pkg:ContributorAccount` with `pkg:hasAccount` enables cross-platform identity linking.

### URI Scheme

Instance data URIs use a data namespace, separate from the ontology namespace:

```
# Ontology namespace (classes + properties)
PKG = "https://packagegraph.github.io/ontology/core#"
SEC = "https://packagegraph.github.io/ontology/security#"
VCS = "https://packagegraph.github.io/ontology/vcs#"

# Data namespace (instances)
DATA = "https://packagegraph.github.io/data/"

# Instance URI patterns:
data:package/debian/bookworm/amd64/curl/8.4.0-2  # BinaryPackage (arch required)
data:source/debian/bookworm/curl/8.4.0-2         # SourcePackage
data:package/fedora/41/curl/8.6.0-5.fc41.x86_64  # RPM BinaryPackage
data:maintainer/jane.doe@debian.org              # Maintainer (email is public)
data:arch/amd64                                  # Architecture
data:distro/debian                               # Distribution
data:release/debian/bookworm                     # DistributionRelease
data:cve/CVE-2024-1234                           # Vulnerability
data:repo/github.com/curl/curl                   # VCS Repository
data:advisory/RHSA-2024:1234                     # SecurityAdvisory
data:upstream/curl                                # UpstreamProject
data:build/fedora/41/curl/8.6.0-5.fc41           # BuildActivity
data:version/debian/bookworm/curl/8.4.0-2        # Version
```

## Assumptions

- The ontology schema (core.ttl, security.ttl, vcs.ttl) has all needed classes and properties — no schema changes required. Supported by: reading all three ontology files. Tasks 1-9 depend on this.
- Debian Packages.gz always contains `Maintainer:` field with `Name <email>` format. Supported by: Debian Policy §5.6.2. Task 2 depends on this.
- repology.org API is stable and returns JSON with package-per-repo structure. Supported by: public API docs. Task 6 depends on this.
- GitHub API v3 REST endpoints for repos/commits/contributors remain stable. Supported by: GitHub API stability commitment. Task 7 depends on this.
- NVD API 2.0 returns CVE data searchable by keyword (package name). Supported by: NIST documentation. Task 8 depends on this.
- koji XML-RPC API is publicly accessible for Fedora builds without authentication. Supported by: Fedora infrastructure policy. Task 9 depends on this.
- Existing TDB2 data in Minio can be discarded and re-ingested with new namespace. Supported by: ETL is re-runnable (~10 min). All tasks depend on this.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| repology.org rate limits or blocks scraping | Medium | Medium | Implement exponential backoff. Cache responses. Add `--repology-cache-dir` option. |
| GitHub API rate limit (60 unauthenticated) blocks enrichment | High | High | Require `GITHUB_TOKEN` env var for GitHub collector. Document in CLI help. Batch requests. |
| NVD API rate limit (10 req/min) makes full scan slow | High | Medium | Add `--nvd-api-key` option for higher rate limit. Process in batches with sleep. |
| koji API slow for large-scale build queries | Medium | Low | Limit to specific packages or releases via CLI options. Cache build metadata. |
| Parallel processing breaks with GraphBuilder | Low | High | GraphBuilder is stateless per-graph — each worker creates its own instance. Test parallel path in integration tests. |
| Package name URI encoding misses edge cases | Low | Medium | Use `urllib.parse.quote(name, safe="")` consistently. Add test for names with `+`, `@`, spaces. |

## Goal Verification

### Truths

1. SPARQL query `SELECT ?p WHERE { ?p a pkg:BinaryPackage }` returns results against Fuseki (ontology namespace in use)
2. SPARQL query traversing `pkg:directlyDependsOn+` returns transitive dependency chains
3. SPARQL `GROUP BY ?maintainer HAVING(COUNT(DISTINCT ?package) > N)` returns maintainer package counts
4. Source packages link to binary packages via `pkg:builtFromSource` / `pkg:producedBinary`
5. Multiple architectures appear in the graph (e.g., `data:arch/amd64`, `data:arch/arm64`)
6. Cross-distribution package equivalences exist via `pkg:equivalentInDistribution`
7. VCS repository and contributor data linked to packages via `pkg:hasUpstreamProject` → `vcs:hasUpstreamRepository`

### Artifacts

- `etl/packagegraph/namespaces.py` — Ontology + data namespace constants
- `etl/packagegraph/graph_builder.py` — Shared triple emission logic
- `etl/packagegraph/collectors/` — All collector modules
- `etl/tests/test_graph_builder.py` — GraphBuilder unit tests
- `etl/tests/test_collectors/` — Per-collector test modules

## Progress Tracking

- [x] Task 1: Namespaces, GraphBuilder, and collectors subpackage scaffold
- [x] Task 2: Debian collector refactor with maintainer + source linking
- [x] Task 3: Multi-arch Debian collection
- [x] Task 4: RPM collector refactor
- [x] Task 5: Multi-release RPM collection
- [x] Task 6: repology.org cross-distro collector
- [x] Task 7: GitHub VCS metadata collector
- [x] Task 8: NVD/OSV security collector
- [x] Task 9: koji build metadata collector
- [x] Task 10: Integration test — SPARQL verification

**Total Tasks:** 10 | **Completed:** 10 | **Remaining:** 0

## Implementation Tasks

### Task 1: Namespaces, GraphBuilder, and Collectors Subpackage Scaffold

**Objective:** Create the foundation: namespace constants, GraphBuilder class with core methods, and restructure into `collectors/` subpackage. This is the foundation all subsequent tasks build on.

**Dependencies:** None

**Files:**

- Create: `etl/packagegraph/namespaces.py`
- Create: `etl/packagegraph/graph_builder.py`
- Create: `etl/packagegraph/collectors/__init__.py`
- Modify: `etl/packagegraph/__init__.py` (update exports if needed)
- Create: `etl/tests/test_graph_builder.py`

**Key Decisions / Notes:**

- `namespaces.py` defines ontology namespaces (`PKG`, `SEC`, `VCS`), data namespace (`DATA`), and external vocabularies (`FOAF`, `PROV`) as rdflib `Namespace` objects, plus URI builder helper functions.
- GraphBuilder core methods for Task 1: `add_distribution()`, `add_release()`, `add_package()`, `add_version()`, `add_dependency()`, `add_architecture()`, `add_maintainer()`, `add_source_package()`, `add_installed_file()`. Additional methods added by Tasks 6-9 as needed.
- `add_version()` creates a separate `pkg:Version` resource with `pkg:versionString`, `pkg:epoch`, `pkg:release`, `pkg:revision` data properties. Returns the version URI. `add_package()` calls `add_version()` and links via `pkg:hasVersion`. This is required because `sec:affectsVersion` and `sec:fixedInVersion` range on `pkg:Version`, not `pkg:Package`.
- GraphBuilder constructor takes a `Graph` instance — it adds triples to whatever graph is passed in. No shared state. Constructor binds all namespace prefixes (pkg:, sec:, vcs:, data:, foaf:, prov:) to the graph for readable Turtle serialization.
- URI builder functions: `package_uri(distro, release, arch, name, version)` (arch is required — disambiguates multi-arch binaries), `source_uri(distro, release, name, version)`, `version_uri(distro, release, name, version)`, `maintainer_uri(email)`, `arch_uri(name)`, `distro_uri(name)`, `release_uri(distro, codename)`, `upstream_uri(name)`.
- Each URI builder uses `urllib.parse.quote(component, safe="")` for path segments.
- `collectors/__init__.py` re-exports collector classes for backward compatibility during refactor.

**Definition of Done:**

- [ ] All tests pass
- [ ] No diagnostics errors
- [ ] `GraphBuilder.add_package()` creates a `pkg:BinaryPackage` triple with `pkg:packageName`, `pkg:hasVersion`, `pkg:targetArchitecture`, `pkg:partOfDistribution`, `pkg:partOfRelease`
- [ ] `GraphBuilder.add_dependency()` creates `pkg:directlyDependsOn` link AND reified `pkg:Dependency` with `pkg:dependencyType`, plus optional `pkg:VersionConstraint` resource (with `pkg:versionConstraintOperator` + `pkg:versionConstraintValue` as separate data properties)
- [ ] `GraphBuilder.add_dependency()` accepts a `distro_property` parameter to emit both generic and distro-specific dependency properties (e.g., `pkg:debDepends` alongside `pkg:directlyDependsOn`)
- [ ] `GraphBuilder.add_maintainer()` creates `pkg:Maintainer` with `foaf:name` and `foaf:mbox`, linked via `pkg:maintainedBy`
- [ ] `GraphBuilder.add_source_package()` creates `pkg:SourcePackage` linked to binary via `pkg:builtFromSource`
- [ ] URI builder functions produce correct, encoded URIs
- [ ] GraphBuilder works with a fresh `Graph()` in isolation (no shared state)

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_graph_builder.py -q`

---

### Task 2: Debian Collector Refactor

**Objective:** Rewrite `debian_collector.py` to use GraphBuilder for all triple emission. Move to `collectors/debian.py`. Include maintainer parsing (item 2) and source→binary linking (item 4). Preserve parallel processing behavior.

**Dependencies:** Task 1

**Files:**

- Create: `etl/packagegraph/collectors/debian.py` (refactored from `debian_collector.py`)
- Modify: `etl/packagegraph/debian_collector.py` (make thin re-export wrapper for backward compat)
- Modify: `etl/packagegraph/cli.py` (update import path)
- Create: `etl/tests/test_collectors/__init__.py`
- Create: `etl/tests/test_collectors/test_debian.py`

**Key Decisions / Notes:**

- The existing `_process_package_chunk` static method creates a local `Graph()`. In the refactored version, it creates a local `GraphBuilder(Graph())` instead and calls `builder.add_package()`, `builder.add_dependency()`, `builder.add_maintainer()`, `builder.add_source_package()`.
- **Maintainer parsing**: Extract name and email from `Maintainer:` field using regex `r'^(.+?)\s*<(.+?)>$'`. Create `pkg:Maintainer` resource, link via `pkg:maintainedBy`. Handle edge cases: team addresses, missing email.
- **Source→Binary linking**: Parse `Source:` field. Format is either `sourcename` or `sourcename (version)`. Create `pkg:SourcePackage`, link binary via `pkg:builtFromSource`. If no `Source:` field, source name = binary package name.
- **Distribution metadata**: Use `GraphBuilder.add_distribution()` and `GraphBuilder.add_release()` instead of raw `deb:Distribution` / `deb:Suite` triples.
- **Dependency mapping**: Emit BOTH generic `pkg:` AND distro-specific `deb:` properties for each dependency. This enables both cross-distro queries ("all dependencies") and Debian-specific queries ("hard depends only"):
  - `Depends` / `Pre-Depends` → `pkg:directlyDependsOn` + `pkg:debDepends` + `pkg:dependencyType "runtime"`
  - `Recommends` → `pkg:directlyDependsOn` + `pkg:debRecommends` + `pkg:dependencyType "recommends"`
  - `Suggests` → `pkg:directlyDependsOn` + `pkg:debSuggests` + `pkg:dependencyType "suggests"`
  - `Conflicts` → `pkg:directlyConflictsWith` + `pkg:debConflicts` + `pkg:dependencyType "conflicts"`
  - `Replaces` → `pkg:directlyDependsOn` + `pkg:dependencyType "replaces"`
  - `Provides` → `pkg:directlyProvides` + `pkg:debProvides`
  - `Breaks` → `pkg:directlyConflictsWith` + `pkg:dependencyType "breaks"`
  - `Enhances` → `pkg:directlyDependsOn` + `pkg:debEnhances` + `pkg:dependencyType "enhances"`
- **Version constraints**: Use the ontology's `pkg:VersionConstraint` class. Parse Debian's `(>= 2.0)` into a `pkg:VersionConstraint` resource linked via `pkg:hasVersionConstraint` from the `pkg:Dependency`, with `pkg:versionConstraintOperator "≥"` and `pkg:versionConstraintValue "2.0"` as separate data properties. Similarly parse RPM version flags.
- **Additional package properties**: Map remaining Packages.gz fields to ontology datatype properties:
  - `Description` → `pkg:description`
  - `Homepage` → `pkg:homepage`
  - `Installed-Size` → `pkg:installSize` (convert KB to bytes)
  - `Size` → `pkg:packageSize`
  - `Section`, `Priority`, `Tag` → store as `pkg:` datatype properties (no ontology equivalent, but useful metadata)
  - `SHA256` → `pkg:checksum` + `pkg:checksumType "SHA256"`
- **Debian-specific properties**: Preserve alongside `pkg:` properties for distro-specific queries:
  - Suite → `deb:inSuite` (e.g., `"stable"`)
  - Component → `deb:inComponent` (e.g., `"main"`)
  - These are Debian-specific concepts with no `pkg:` equivalent.
- **Dual typing**: Emit BOTH `rdf:type pkg:BinaryPackage` AND `rdf:type deb:BinaryPackage` for Debian packages. Since OWL reasoning is out of scope, asserting both types is necessary for queries against either class. Requires adding `DEB = Namespace(...)` and `RPM = Namespace(...)` to `namespaces.py` (for the distribution-specific ontology extensions in `debian.ttl` and `rpm.ttl`).
- **Contents file processing**: Map file paths to `pkg:installsFile` → `pkg:InstalledFile` with `pkg:installedFilePath`. Use GraphBuilder method.
- **Caching**: Cache downloaded `Packages.gz` and `Contents-*.gz` files to `--cache-dir` (default: `~/.cache/packagegraph/debian/`). Key by `{mirror}/{dist}/{component}/{arch}`. Check the `Release` file's `Date` field — if it hasn't changed since the cached version, skip re-downloading. Use `If-Modified-Since` HTTP header for conditional requests.
- Preserve backward compatibility: `debian_collector.py` becomes `from packagegraph.collectors.debian import DebianCollector` re-export.

**Definition of Done:**

- [ ] All tests pass (new tests + existing tests still work via re-export)
- [ ] No diagnostics errors
- [ ] Collected graph contains both `pkg:BinaryPackage` AND `deb:BinaryPackage` type assertions for each package
- [ ] Every package has `pkg:maintainedBy` linking to a `pkg:Maintainer` resource
- [ ] Binary packages have `pkg:builtFromSource` links to `pkg:SourcePackage`
- [ ] Dependencies use `pkg:directlyDependsOn` + reified `pkg:Dependency` with type
- [ ] Parallel processing still works (test with `--parallel --chunk-size 100`)
- [ ] Old `from packagegraph.debian_collector import DebianCollector` import still works

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_collectors/test_debian.py -q`
- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest -q` (full suite)

---

### Task 3: Multi-Arch Debian Collection

**Objective:** Enable collecting multiple architectures in a single ETL run. Currently `--arch` takes a single value like `binary-amd64`.

**Dependencies:** Task 2

**Files:**

- Modify: `etl/packagegraph/cli.py` (change `--arch` to accept multiple values)
- Modify: `etl/packagegraph/collectors/debian.py` (loop over architectures)
- Modify: `etl/entrypoint.sh` (support `COLLECT_ARCHES` env var)
- Modify: `etl/tests/test_collectors/test_debian.py` (add multi-arch tests)

**Key Decisions / Notes:**

- Change `--arch` CLI option to `multiple=True` so it accepts `--arch binary-amd64 --arch binary-arm64 --arch binary-riscv64`.
- Default remains `binary-amd64` only (single arch) for backward compatibility.
- Internally, `DebianCollector` loops over each architecture, downloads its Packages.gz, and feeds everything into the same graph. The `pkg:targetArchitecture` property on each package distinguishes them.
- Architecture instances: `data:arch/amd64`, `data:arch/arm64`, `data:arch/riscv64` etc. (strip `binary-` prefix for the URI).
- `entrypoint.sh`: Add `COLLECT_ARCHES` env var, default `"binary-amd64"`. If set, pass multiple `--arch` flags.
- Contents file is per-arch too — process for each architecture.

**Definition of Done:**

- [ ] All tests pass
- [ ] `packagegraph collect <url> --arch binary-amd64 --arch binary-arm64` collects both architectures
- [ ] Graph contains packages with distinct `pkg:targetArchitecture` values for each arch
- [ ] Single `--arch` still works (backward compatible)
- [ ] `entrypoint.sh` passes multiple arches when `COLLECT_ARCHES` is set

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_collectors/test_debian.py -q`

---

### Task 4: RPM Collector Refactor

**Objective:** Rewrite `rpm_collector.py` to use GraphBuilder. Move to `collectors/rpm.py`. Align RPM metadata with the same ontology namespace as Debian.

**Dependencies:** Task 1

**Files:**

- Create: `etl/packagegraph/collectors/rpm.py` (refactored from `rpm_collector.py`)
- Modify: `etl/packagegraph/rpm_collector.py` (make thin re-export wrapper)
- Modify: `etl/packagegraph/cli.py` (update import path)
- Create: `etl/tests/test_collectors/test_rpm.py`

**Key Decisions / Notes:**

- Same pattern as Task 2: static chunk method creates local `GraphBuilder(Graph())`.
- **RPM-specific mapping**:
  - `name` → `pkg:packageName`
  - `ver` + `rel` → `pkg:hasVersion` with `pkg:versionString`, `pkg:release`
  - `arch` → `pkg:targetArchitecture`
  - `rpm:requires` → `pkg:directlyDependsOn` + `pkg:rpmRequires` + `pkg:dependencyType "runtime"`
  - `rpm:provides` → `pkg:directlyProvides` + `pkg:rpmProvides`
  - `rpm:conflicts` → `pkg:directlyConflictsWith` + `pkg:rpmConflicts`
  - `summary` → `pkg:description` (first line)
  - `description` → `pkg:description`
  - File lists → `pkg:installsFile`
  - Changelog → use `pkg:wasPackagedBy` activity with timestamps
- **RPM `Packager:` field** (in `other` metadata): Parse same as Debian `Maintainer:` for `pkg:maintainedBy`.
- **RPM package URI**: `data:package/fedora/<release>/<name>/<ver>-<rel>.<arch>`
- **Caching**: Cache downloaded `repomd.xml`, `primary.xml.gz`, `filelists.xml.gz`, and `other.xml.gz` to `--cache-dir` (default: `~/.cache/packagegraph/rpm/`). Key by URL hash + `repomd.xml` revision timestamp. Only re-download when `repomd.xml` checksum changes.
- **RPM-specific properties**: Preserve alongside `pkg:` properties:
  - `rpm:epoch` — RPM epoch (integer, defaults to 0)
  - `rpm:sourceRPM` — the SRPM that produced this RPM
  - `rpm:RPMGroup` — RPM group classification
- **Dual typing**: Emit BOTH `rdf:type pkg:BinaryPackage` AND `rdf:type rpm:BinaryRPM`. Same rationale as Task 2.
- **Version constraints**: Use `pkg:VersionConstraint` class. Parse RPM version flags (EQ, GE, GT, LE, LT) into `pkg:versionConstraintOperator` and `pkg:versionConstraintValue`.
- Distribution and release: Extract from repo URL or add CLI options `--distro-name` and `--release-name` for RPM repos (they don't have a standard Release file like Debian).

**Definition of Done:**

- [ ] All tests pass
- [ ] RPM packages emit both `pkg:BinaryPackage` AND `rpm:BinaryRPM` type assertions
- [ ] Dependencies use `pkg:directlyDependsOn`
- [ ] File lists use `pkg:installsFile`
- [ ] Old `from packagegraph.rpm_collector import RpmCollector` import still works

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_collectors/test_rpm.py -q`
- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest -q`

---

### Task 5: Multi-Release RPM Collection

**Objective:** Enable collecting from multiple Fedora release repos in one ETL run to support cross-release comparison.

**Dependencies:** Task 4

**Files:**

- Modify: `etl/packagegraph/cli.py` (add multi-repo support for RPM)
- Modify: `etl/packagegraph/collectors/rpm.py` (accept distro/release context)
- Modify: `etl/entrypoint.sh` (support `RPM_REPOS` env var)
- Modify: `etl/tests/test_collectors/test_rpm.py`

**Key Decisions / Notes:**

- For RPM, multiple repos = multiple URLs. Add `--rpm-repo` option (multiple=True) that takes `<name>:<release>:<url>` tuples. Example:
  ```
  packagegraph collect --repo-type rpm \
    --rpm-repo "fedora:41:https://dl.fedoraproject.org/pub/fedora/linux/releases/41/Everything/x86_64/os/" \
    --rpm-repo "fedora:42:https://dl.fedoraproject.org/pub/fedora/linux/releases/42/Everything/x86_64/os/"
  ```
- Each repo gets its own `pkg:DistributionRelease` and packages are linked via `pkg:partOfRelease`.
- `entrypoint.sh`: Add `RPM_REPOS` env var as newline-delimited `name:release:url` entries.

**Definition of Done:**

- [ ] All tests pass
- [ ] Collecting from two Fedora release URLs produces packages with distinct `pkg:partOfRelease` values
- [ ] `entrypoint.sh` supports `RPM_REPOS` env var

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_collectors/test_rpm.py -q`

---

### Task 6: repology.org Cross-Distribution Collector

**Objective:** Collect cross-distribution package equivalences from repology.org API to populate `pkg:equivalentInDistribution` links between packages already in the graph.

**Dependencies:** Task 2 AND Task 4 (needs BOTH Debian and RPM packages in graph for cross-distro linking to produce useful results)

**Files:**

- Create: `etl/packagegraph/collectors/repology.py`
- Modify: `etl/packagegraph/cli.py` (add `enrich-repology` subcommand)
- Modify: `etl/packagegraph/graph_builder.py` (add `add_cross_distro_mapping()` method)
- Create: `etl/tests/test_collectors/test_repology.py`

**Key Decisions / Notes:**

- **API**: `GET https://repology.org/api/v1/project/<name>` returns package info across distros. Response is a JSON array of package objects with `repo`, `visiblename`, `version`, `status` fields.
- **Workflow**: Read existing graph to get package names → query repology for each → create `pkg:equivalentInDistribution` links between matching packages.
- **CLI**: New `enrich-repology` command that takes `--input-file` (existing graph) and `--output-file`. Reads package names from graph, queries repology, adds equivalence triples.
- **Rate limiting**: Add 1-second delay between requests.
- **Caching**: Cache repology API responses to `--repology-cache-dir` (default: `~/.cache/packagegraph/repology/`). Key by project name. Cache TTL: 7 days (cross-distro mappings change infrequently). On subsequent runs, only re-fetch projects whose cache entry has expired.
- **Matching logic**: Map repology `repo` field to distribution names in our graph (e.g., `debian_12` → `debian/bookworm`, `fedora_41` → `fedora/41`). Only create links between packages that exist in our graph.
- Add `GraphBuilder.add_cross_distro_mapping(pkg1_uri, pkg2_uri)` which emits `pkg1 pkg:equivalentInDistribution pkg2` (and the reverse, since it's symmetric).

**Definition of Done:**

- [ ] All tests pass
- [ ] `packagegraph enrich-repology --input-file graph.ttl --output-file enriched.ttl` adds `pkg:equivalentInDistribution` triples
- [ ] Cached responses are reused on subsequent runs
- [ ] Rate limiting prevents API abuse (≤1 req/sec)
- [ ] Only links between packages that exist in the input graph are created

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_collectors/test_repology.py -q`

---

### Task 7: GitHub VCS Metadata Collector

**Objective:** Collect repository metadata, recent commits, and contributors from GitHub API for packages that have a GitHub homepage URL. Populate `vcs:Repository`, `vcs:Commit`, `vcs:authoredBy`, and link to packages.

**Dependencies:** Task 1 (GraphBuilder), Task 2 or 4 (needs packages with `pkg:homepage`)

**Files:**

- Create: `etl/packagegraph/collectors/github.py`
- Modify: `etl/packagegraph/cli.py` (add `enrich-github` subcommand)
- Modify: `etl/packagegraph/graph_builder.py` (add VCS methods)
- Modify: `etl/pyproject.toml` (no new deps needed — using `requests`)
- Create: `etl/tests/test_collectors/test_github.py`

**Key Decisions / Notes:**

- **API endpoints used**:
  - `GET /repos/{owner}/{repo}` — repo metadata (stars, forks, default branch, description)
  - `GET /repos/{owner}/{repo}/commits?per_page=100` — recent commits (author, date, message)
  - `GET /repos/{owner}/{repo}/contributors?per_page=100` — contributor list
  - `GET /repos/{owner}/{repo}/releases?per_page=30` — releases (tag, date, body)
- **Package→Repo discovery**: Read `pkg:homepage` values from existing graph. Parse GitHub URLs to extract `owner/repo`. Filter to `github.com` URLs only.
- **GraphBuilder VCS methods**:
  - `add_repository(url, vcs_type, default_branch, stars, forks, ...)` → `vcs:Repository`
  - `add_commit(repo_uri, hash, author_name, author_email, timestamp, message)` → `vcs:Commit` + `vcs:authoredBy`
  - `add_contributor_account(contributor_uri, platform, username, url)` → `pkg:ContributorAccount`
  - `link_package_to_repo(package_uri, repo_uri)` → `pkg:hasUpstreamProject` → `vcs:hasUpstreamRepository`
- **Authentication**: Require `GITHUB_TOKEN` env var. Fail with clear error if not set.
- **Rate limiting**: Respect `X-RateLimit-Remaining` header. Sleep when approaching limit.
- **CLI**: `packagegraph enrich-github --input-file graph.ttl --output-file enriched.ttl --github-token $GITHUB_TOKEN`
- **Contributor identity linking**: When a GitHub commit author email matches a `pkg:Maintainer` email in the graph, link them via `pkg:hasAccount`. This is the key bridge for supply chain queries.
- **Upstream project**: Create `pkg:UpstreamProject` resources using `data:upstream/{name}` URI. Extract project name from repo name. **Domain constraint**: `pkg:hasUpstreamProject` has domain `pkg:SourcePackage`, not `pkg:Package`. The link chain is: `pkg:SourcePackage` → `pkg:hasUpstreamProject` → `pkg:UpstreamProject` → `vcs:hasUpstreamRepository` → `vcs:Repository`. Since GitHub discovery starts from `pkg:homepage` on BinaryPackages, the collector must resolve back to the SourcePackage (via `pkg:builtFromSource` inverse query) before asserting `hasUpstreamProject`.
- **Caching**: Cache GitHub API responses to `--github-cache-dir` (default: `~/.cache/packagegraph/github/`). Key by `{owner}/{repo}/{endpoint}`. Respect `ETag`/`If-None-Match` headers for conditional requests — GitHub returns 304 Not Modified and doesn't count against rate limit. Cache TTL: 24 hours for repo metadata, 1 hour for commits.

**Definition of Done:**

- [ ] All tests pass
- [ ] `packagegraph enrich-github` creates `vcs:Repository` resources linked to packages
- [ ] Commits are stored with `vcs:authoredBy` linking to `pkg:Contributor` resources
- [ ] GitHub contributors with matching emails are linked to existing `pkg:Maintainer` resources
- [ ] Rate limiting respects GitHub API headers
- [ ] Works only for packages with GitHub homepage URLs (others are skipped)
- [ ] Upstream link chain goes SourcePackage → UpstreamProject → Repository (not BinaryPackage → Repository)
- [ ] Cached responses are reused within TTL (verify with `--github-cache-dir`)

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_collectors/test_github.py -q`

---

### Task 8: NVD/OSV Security Data Collector

**Objective:** Collect CVE data from NVD API and map to packages in the graph. Populate `sec:Vulnerability`, `sec:affectsVersion`, `sec:fixedInVersion`.

**Dependencies:** Task 1 (GraphBuilder), Task 2 or 4 (needs packages to map CVEs to)

**Files:**

- Create: `etl/packagegraph/collectors/security.py`
- Modify: `etl/packagegraph/cli.py` (add `enrich-security` subcommand)
- Modify: `etl/packagegraph/graph_builder.py` (add security methods)
- Create: `etl/tests/test_collectors/test_security.py`

**Key Decisions / Notes:**

- **Primary API**: NVD API 2.0 — `GET https://services.nvd.nist.gov/rest/json/cves/2.0?keywordSearch=<package_name>&resultsPerPage=100`
- **Fallback**: OSV API — `POST https://api.osv.dev/v1/query` with `{"package": {"name": "<name>", "ecosystem": "Debian"}}`. More accurate package matching but less comprehensive.
- **Strategy**: Use OSV as primary (better package matching), NVD as fallback (broader coverage).
- **GraphBuilder security methods**:
  - `add_vulnerability(cve_id, cvss_score, severity, summary, cwe_id, ...)` → `sec:Vulnerability`
  - `add_security_advisory(advisory_id, severity, date, type)` → `sec:SecurityAdvisory`
  - `link_vulnerability_to_version(vuln_uri, version_uri, fixed_version_uri)` → `sec:affectsVersion`, `sec:fixedInVersion`
- **Rate limiting**: NVD allows 10 req/min without key, 50 req/min with key. Add `--nvd-api-key` option. Sleep between batches.
- **CLI**: `packagegraph enrich-security --input-file graph.ttl --output-file enriched.ttl`
- **Matching**: Extract package names from graph, query OSV/NVD, match CVE affected versions to `pkg:Version` resources in graph. **v1 version matching is approximate**: exact string match on version strings, plus simple numeric comparison for version ranges. Proper Debian version comparison (epoch:upstream-revision) and RPM version comparison (epoch:version-release) are non-trivial and deferred to a follow-up. Document which CVEs were linked by exact match vs. skipped due to complex version ranges.
- **Caching**: Cache NVD/OSV API responses to `--security-cache-dir` (default: `~/.cache/packagegraph/security/`). Key by package name + ecosystem. Cache TTL: 24 hours (CVE data updates frequently). Use `If-Modified-Since` for NVD conditional requests.
- Only process a subset of packages per run (e.g., `--package-filter` or `--max-packages`) to manage API load.

**Definition of Done:**

- [ ] All tests pass
- [ ] `packagegraph enrich-security` creates `sec:Vulnerability` resources with CVSS scores
- [ ] Vulnerabilities are linked to affected package versions via `sec:affectsVersion`
- [ ] Fixed versions linked via `sec:fixedInVersion` when available
- [ ] Rate limiting prevents API abuse
- [ ] Handles packages with no CVEs gracefully (skip, no error)

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_collectors/test_security.py -q`

---

### Task 9: koji Build Metadata Collector

**Objective:** Collect Fedora/RHEL build metadata from koji API. Populate `pkg:BuildActivity`, `pkg:wasBuiltBy`, `pkg:usedDependency` for build-time dependency tracing.

**Dependencies:** Task 4 (needs RPM packages in graph), Task 5 (multi-release context)

**Files:**

- Create: `etl/packagegraph/collectors/koji.py`
- Modify: `etl/packagegraph/cli.py` (add `enrich-koji` subcommand)
- Modify: `etl/packagegraph/graph_builder.py` (add build activity methods)
- Create: `etl/tests/test_collectors/test_koji.py`

**Key Decisions / Notes:**

- **API**: koji uses XML-RPC. Fedora's hub is at `https://koji.fedoraproject.org/kojihub`. Use Python stdlib `xmlrpc.client` directly instead of the `koji` library — the four API calls needed are simple XML-RPC and this avoids a heavy dependency with platform compatibility concerns.
- **Key API calls**:
  - `getBuild(nvr)` — build info including owner, start/complete time, task_id
  - `getTaskInfo(task_id)` — build task details including architecture, method
  - `listBuildRPMs(build_id)` — RPMs produced by a build
  - `listRPMs(componentBuildrootID=...)` — buildroot deps (what was installed during build)
- **GraphBuilder build methods**:
  - `add_build_activity(package_uri, owner, start_time, end_time, build_system)` → `pkg:BuildActivity`
  - `link_build_dependency(build_uri, dep_package_uri)` → `pkg:usedDependency`
- **Matching**: For each RPM package in graph, construct NVR (name-version-release) and query koji.
- **Build dependencies**: The buildroot (packages installed during build) gives us `pkg:usedDependency`. This is how we trace "what GCC version was used to build X?".
- **CLI**: `packagegraph enrich-koji --input-file graph.ttl --output-file enriched.ttl --koji-hub https://koji.fedoraproject.org/kojihub`
- No new pyproject.toml dependency needed — `xmlrpc.client` is stdlib.
- Rate limiting: koji API has no published limits but we should be polite — add configurable delay.
- **Caching**: Cache koji API responses to `--koji-cache-dir` (default: `~/.cache/packagegraph/koji/`). Key by NVR. Cache TTL: 30 days (build metadata is immutable once a build completes). Buildroot dependencies are especially expensive to query — cache aggressively.

**Definition of Done:**

- [ ] All tests pass
- [ ] `packagegraph enrich-koji` creates `pkg:BuildActivity` resources linked to RPM packages
- [ ] Build dependencies are stored via `pkg:usedDependency`
- [ ] Build owner/timestamps are captured
- [ ] Can answer "which GCC version built package X?" via SPARQL after enrichment

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_collectors/test_koji.py -q`
- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest -q`

---

### Task 10: Integration Test — SPARQL Verification

**Objective:** Create an integration test that validates the full pipeline produces a graph that answers the key SPARQL query patterns from Goal Verification. Catches namespace, URI, and typing issues that unit tests miss.

**Dependencies:** Task 2, Task 4

**Files:**

- Create: `etl/tests/test_integration.py`
- Create: `etl/tests/fixtures/` (small fixture data files)

**Key Decisions / Notes:**

- **Fixture data**: Create small synthetic Packages.gz and primary.xml fixtures (5-10 packages each) with known dependency relationships, maintainers, and source packages.
- **Test approach**: Run DebianCollector and RpmCollector against fixture data, merge output graphs into a single in-memory rdflib Graph, then execute the key SPARQL queries from Goal Verification Truths.
- **Queries to verify**:
  1. `SELECT ?p WHERE { ?p a pkg:BinaryPackage }` returns expected count
  2. `SELECT ?p ?dep WHERE { ?p pkg:directlyDependsOn ?dep }` returns expected dependency links
  3. `SELECT ?m (COUNT(DISTINCT ?p) as ?count) WHERE { ?p pkg:maintainedBy ?m } GROUP BY ?m` returns maintainer counts
  4. `SELECT ?bin ?src WHERE { ?bin pkg:builtFromSource ?src }` returns source→binary links
  5. `SELECT ?p WHERE { ?p a deb:BinaryPackage }` (dual typing verification)
- **Pre-refactor snapshot**: Before Task 2 implementation begins, run current collector on fixture data and save output. After refactoring, verify all data relationships are preserved (modulo namespace changes).

**Definition of Done:**

- [ ] All tests pass
- [ ] Integration test executes 5+ SPARQL queries against collected graph
- [ ] Queries return expected results for fixture data
- [ ] Both Debian and RPM fixture data verified

**Verify:**

- `cd /Users/bharrington/Projects/packagegraph/platform/etl && uv run pytest tests/test_integration.py -q`

---

## Caching Strategy

All collectors cache upstream data to avoid hammering external APIs and to enable fast re-runs. Caching follows a consistent pattern:

| Collector | Cache Location | Cache Key | TTL | Conditional Request |
|-----------|---------------|-----------|-----|-------------------|
| Debian | `~/.cache/packagegraph/debian/` | `{mirror}/{dist}/{component}/{arch}` | Until `Release` file `Date` changes | `If-Modified-Since` on `Release` file |
| RPM | `~/.cache/packagegraph/rpm/` | `{repo_url_hash}/{repomd_checksum}` | Until `repomd.xml` checksum changes | Compare `repomd.xml` revision timestamp |
| repology | `~/.cache/packagegraph/repology/` | `{project_name}.json` | 7 days | None (no conditional API) |
| GitHub | `~/.cache/packagegraph/github/` | `{owner}/{repo}/{endpoint}.json` | 24h (repo), 1h (commits) | `ETag` / `If-None-Match` (304 = free) |
| NVD/OSV | `~/.cache/packagegraph/security/` | `{package}_{ecosystem}.json` | 24 hours | `If-Modified-Since` (NVD) |
| koji | `~/.cache/packagegraph/koji/` | `{nvr}.json` | 30 days (immutable builds) | None (builds are immutable) |

**Implementation pattern**: Each collector has a `_cached_fetch(url, cache_key, ttl)` helper that checks cache freshness, makes conditional requests where supported, and writes responses to disk. Global `--cache-dir` CLI option overrides all defaults. `--no-cache` flag bypasses caching entirely for debugging.

**Enrichment outputs additive graphs**: Enrichment commands (Tasks 6-9) produce new-triples-only output files that can be merged at TDB2 load time, avoiding re-serialization of the full base graph. The TDB2 loader already supports multiple input files. This also enables parallelizing enrichment steps (GitHub + NVD + koji can run concurrently on the same base graph).

## Open Questions

1. **Debian Sources index**: Should we also parse `Sources.gz` (the source package index) in addition to inferring source packages from the `Source:` field in binary packages? Sources.gz has build dependencies (`Build-Depends`) which would populate `pkg:buildDependsOn` without needing koji. Deferred — can add in a follow-up.

2. **Contributor identity resolution across distributions**: When the same person maintains packages in Debian (email A) and Fedora (email B), how do we link them? repology doesn't provide this. Manual curation or heuristic matching (same name) could work but is error-prone. Deferred — `pkg:hasAccount` + `pkg:ContributorAccount` model supports this when data is available.

3. **Incremental enrichment**: Should enrichment commands (Tasks 6-9) support incremental updates (only process new/changed packages)? Current design re-processes everything. Deferred — add `--since` flag in follow-up.

## Deferred Ideas

- **Debian Sources.gz collector** for build dependencies without koji
- **Arch Linux collector** for AUR/pacman packages
- **Alpine apk collector**
- **Query CLI** (`packagegraph query porting --target-arch riscv64`) — separate plan
- **Fuseki Jena Text configuration** for full-text search over package descriptions
- **OWL reasoning setup** in Fuseki for automatic cross-format equivalence inference
- **Graph visualization** frontend for dependency exploration
