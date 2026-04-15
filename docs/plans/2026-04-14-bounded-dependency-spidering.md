# Bounded Dependency Spidering Implementation Plan

Created: 2026-04-14
Author: sovereign@local
Status: VERIFIED
Approved: Yes
Iterations: 0
Worktree: No
Type: Feature

## Summary

**Goal:** Implement bounded BFS dependency spidering for the Cargo, PyPI, and GoMod collectors so they follow runtime and build dependency edges to discover and collect transitive dependencies. Add Conda cross-ecosystem correlation via `pkg:upstreamPackageName`.

**Architecture:** Add BFS queue + visited set to each collector's `collect()` method. Seeds initialize the queue; dependencies discovered during collection are enqueued up to `--max-depth` / `--max-packages` bounds. Refactor each collector's inner loop to return extracted dependency names alongside triples. Conda gets a name-match heuristic for PyPI correlation.

**Tech Stack:** Rust (pg-collect), `VecDeque`/`HashSet` for BFS, existing API clients unchanged.

## Scope

### In Scope

- BFS spidering for PyPI, Cargo, GoMod collectors
- `--max-depth` (default 2) and `--max-packages` (default 5000) CLI flags
- Follow runtime + build dependencies; skip dev/test
- Conda cross-ecosystem correlation via `upstreamPackageName`/`upstreamEcosystem`
- Tests for spider logic, depth limiting, and dep extraction

### Out of Scope

- Spidering for NPM collector (similar pattern, separate task)
- Full conda-forge ↔ crates.io correlation (only PyPI correlation for now)
- Incremental spidering / resume across runs
- Parallelism / async in the spider loop

## Approach

**Chosen:** BFS queue in each collector's `collect()` method
**Why:** Minimal refactoring — the existing `collect_package`/`emit_triples` methods stay unchanged. The spider loop wraps around them, collecting dependency names from each package and enqueuing new ones.
**Alternatives considered:**
- Shared Spider trait: More elegant but requires refactoring all three collectors to a common interface. Unnecessary complexity for 3 files.
- External Python orchestration: No Rust changes but slower, fragile, and adds shell script complexity.

## Context for Implementer

> Write for an implementer who has never seen the codebase.

- **Patterns to follow:** Each collector has a `collect(&self, packages_file, output_path)` method that reads a seed file, iterates package names, and calls a per-package method. The spider replaces the `for name in seeds` loop with a BFS `while let Some(name) = queue.pop_front()` loop.
- **Conventions:** Collectors use `NTriplesWriter` for streaming output, `read_seed_file()` from `npm.rs` for seed loading, and per-collector `fetch_*_with_retry` for API calls with backoff.
- **Key files:**
  - `src/pypi.rs` — `PypiCollector::collect()`, `parse_requires_dist()` extracts dep names from `requires_dist` field
  - `src/cargo_collect.rs` — `CargoCollector::collect()`, deps come from `fetch_crate_with_retry()` returning `Vec<CrateDep>` with `.crate_id` and `.kind`
  - `src/gomod.rs` — `GoModCollector::collect()`, deps from `parse_go_mod()` returning `GoMod { requires: Vec<GoRequire> }` with `.module_path` and `.indirect`
  - `src/conda.rs` — `CondaCollector::emit_package_triples()`, `depends` field has `Vec<String>` of dependency specs
  - `src/main.rs` — CLI definitions with `#[arg]` attributes for each subcommand
- **Gotchas:**
  - PyPI `requires_dist` includes extras markers like `; extra == "test"` and `; python_version < "3.8"` — filter these in the dep extraction, not the spider
  - Cargo deps have `.kind` field: "normal" (runtime), "build", "dev" — only follow "normal" and "build"
  - Go `require` blocks have `// indirect` comments — the parser already sets `.indirect = true`. Follow both direct and indirect deps.
  - Conda `depends` format is `"python >=3.8"` or `"numpy >=1.20,<2.0"` — split on space to get package name
- **Domain context:** The dependency graph is the core value of the knowledge graph. Stub `PackageIdentity` URIs (name only, no metadata) are useless for queries like "what are the transitive dependencies of requests?" — spidering fills in the full metadata.

## Assumptions

- PyPI API returns `requires_dist` for all actively maintained packages — supported by the test run showing 352 triples from 9 packages — Tasks 1, 4 depend on this
- crates.io API returns deps at `/{name}/{version}/dependencies` — supported by `cargo_collect.rs:135` — Tasks 2, 4 depend on this
- Go module proxy returns `go.mod` at `/{path}/@v/{version}.mod` — supported by `gomod.rs:96` — Tasks 3, 4 depend on this
- Conda-forge packages with `python` in their depends list map 1:1 to PyPI packages by name — heuristic, may miss some — Task 5 depends on this

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Spider pulls in entire ecosystem (500K+ packages) | Low | High | Hard bounds: `--max-packages 5000` default, BFS terminates when reached |
| API rate limiting during deep spider | Medium | Medium | Existing retry/backoff in each collector; BFS breadth-first means popular packages hit first |
| Circular dependencies cause infinite loop | Low | High | `visited` HashSet prevents re-processing; BFS naturally handles cycles |
| Conda name-match produces false positives | Medium | Low | Only match packages with `python` in depends; false positives are low-impact (extra correlation links) |

## Goal Verification

### Truths

1. Running `pg-collect pypi --packages-file seed.txt --max-depth 2 -o out.nt` produces triples for transitive dependencies not in the seed file
2. Running `pg-collect cargo --packages-file seed.txt --max-depth 2 -o out.nt` produces triples for transitive crate dependencies
3. Running `pg-collect gomod --packages-file seed.txt --max-depth 2 -o out.nt` produces triples for transitive Go module dependencies
4. `--max-packages` flag stops spidering when the limit is reached
5. `--max-depth 0` produces the same output as the current behavior (seed-only, no spidering)
6. Conda packages with Python dependencies emit `pkg:upstreamPackageName` enabling cross-ecosystem SPARQL joins

### Artifacts

- `src/pypi.rs` — modified `collect()` with BFS spider
- `src/cargo_collect.rs` — modified `collect()` with BFS spider
- `src/gomod.rs` — modified `collect()` with BFS spider
- `src/conda.rs` — modified `emit_package_triples()` with ecosystem heuristic
- `src/main.rs` — updated CLI args with `--max-depth` and `--max-packages`
- `tests/test_pypi_spider.rs` — spider-specific tests
- `tests/test_cargo_spider.rs` — spider-specific tests
- `tests/test_gomod_spider.rs` — spider-specific tests

## Progress Tracking

- [x] Task 1: PyPI BFS spider
- [x] Task 2: Cargo BFS spider
- [x] Task 3: GoMod BFS spider
- [x] Task 4: CLI flags and shared spider utilities
- [x] Task 5: Conda cross-ecosystem correlation
- [x] Task 6: Integration tests

**Total Tasks:** 6 | **Completed:** 6 | **Remaining:** 0

## Implementation Tasks

### Task 1: PyPI BFS Spider

**Objective:** Refactor `PypiCollector::collect()` to use BFS spidering instead of flat seed iteration. Extract dependency names from `parse_requires_dist()` results and enqueue them.
**Dependencies:** Task 4 (CLI flags — but can be done in parallel, hardcode defaults initially)
**Mapped Scenarios:** None (no UI)

**Files:**

- Modify: `etl/pg-collect/src/pypi.rs`
- Test: `etl/pg-collect/tests/test_pypi.rs` (new or extend existing)

**Key Decisions / Notes:**

- Refactor `collect()` to accept `max_depth: u32, max_packages: usize` params
- Split `parse_requires_dist()` to also return `Vec<String>` of dep names (currently only writes triples)
- Filter deps: skip entries with `; extra ==` markers (optional/test deps) unless they have no marker
- Filter by dep kind: PyPI doesn't distinguish build vs runtime in `requires_dist` — treat all non-extra deps as runtime

**Definition of Done:**

- [ ] `collect()` follows dependency edges up to `max_depth`
- [ ] Transitive deps appear in output triples
- [ ] `max_packages` terminates spider early
- [ ] `max_depth=0` produces seed-only output (backward compatible)
- [ ] Tests verify depth limiting and dep extraction

**Verify:**

- `cargo test --lib pypi`

---

### Task 2: Cargo BFS Spider

**Objective:** Refactor `CargoCollector::collect()` to use BFS spidering. Extract dependency crate names from the `Vec<CrateDep>` response and enqueue non-dev deps.
**Dependencies:** Task 4
**Mapped Scenarios:** None

**Files:**

- Modify: `etl/pg-collect/src/cargo_collect.rs`
- Test: `etl/pg-collect/tests/test_cargo.rs` (new or extend)

**Key Decisions / Notes:**

- `CrateDep` has `.crate_id` (name) and `.kind` ("normal", "build", "dev")
- Follow `kind == "normal"` and `kind == "build"` per user decision; skip `kind == "dev"`
- The `fetch_crate_with_retry()` already returns `(CratesResponse, Vec<CrateDep>)` — extract dep names from the `Vec<CrateDep>`

**Definition of Done:**

- [ ] `collect()` follows runtime + build dependency edges
- [ ] Dev deps are NOT followed
- [ ] `max_depth` and `max_packages` respected
- [ ] Tests verify spider with mock crate data

**Verify:**

- `cargo test --lib cargo_collect`

---

### Task 3: GoMod BFS Spider

**Objective:** Refactor `GoModCollector::collect()` to use BFS spidering. Extract module paths from `go.mod` requires and enqueue them.
**Dependencies:** Task 4
**Mapped Scenarios:** None

**Files:**

- Modify: `etl/pg-collect/src/gomod.rs`
- Test: `etl/pg-collect/tests/test_gomod.rs` (new or extend)

**Key Decisions / Notes:**

- `GoMod { requires: Vec<GoRequire> }` with `.module_path` — these are the dep names to enqueue
- Follow both direct and indirect requires (indirect still has metadata at the proxy)
- Go module paths need `encode_go_module_path()` for proxy API calls — already implemented

**Definition of Done:**

- [ ] `collect()` follows require edges from go.mod
- [ ] Both direct and indirect deps are spidered
- [ ] `max_depth` and `max_packages` respected
- [ ] Tests verify spider with mock go.mod data

**Verify:**

- `cargo test --lib gomod`

---

### Task 4: CLI Flags and Spider Utilities

**Objective:** Add `--max-depth` and `--max-packages` CLI arguments to the Pypi, Cargo, and Gomod subcommands in `main.rs`. Optionally extract shared BFS logic into a utility function.
**Dependencies:** None
**Mapped Scenarios:** None

**Files:**

- Modify: `etl/pg-collect/src/main.rs`

**Key Decisions / Notes:**

- Add to each subcommand:
  ```rust
  #[arg(long, default_value = "2")]
  max_depth: u32,
  #[arg(long, default_value = "5000")]
  max_packages: usize,
  ```
- Pass to each collector's `collect()` method
- Consider a shared `SpiderConfig { max_depth, max_packages }` struct if useful, but not required — 3 collectors with 2 params each is manageable

**Definition of Done:**

- [ ] `pg-collect pypi --max-depth 1 --max-packages 100` works
- [ ] `pg-collect cargo --max-depth 0` produces seed-only output
- [ ] Default values are depth=2, max=5000
- [ ] `--help` shows the new flags

**Verify:**

- `pg-collect pypi --help | grep max-depth`

---

### Task 5: Conda Cross-Ecosystem Correlation

**Objective:** Emit `pkg:upstreamPackageName` and `pkg:upstreamEcosystem` for conda-forge packages that map to PyPI packages, enabling SPARQL cross-ecosystem queries.
**Dependencies:** None
**Mapped Scenarios:** None

**Files:**

- Modify: `etl/pg-collect/src/conda.rs`
- Test: `etl/pg-collect/tests/test_conda.rs` (extend existing)

**Key Decisions / Notes:**

- Heuristic: if a conda package's `depends` list includes any entry starting with `python` (e.g., `python >=3.8`, `python`), emit the conda package name as `pkg:upstreamPackageName` with `pkg:upstreamEcosystem "pypi"`
- This enables: `?conda pkg:upstreamPackageName ?name . ?pypi pkg:upstreamPackageName ?name .` → joined by `?name`
- Also emit for `r-*` conda packages → `pkg:upstreamEcosystem "cran"`, and `rust-*` → `"cargo"` based on name prefix
- The `depends` field is `Option<Vec<String>>` in `CondaPackageEntry`

**Definition of Done:**

- [ ] Conda packages with Python depends emit `upstreamEcosystem "pypi"` + `upstreamPackageName`
- [ ] `r-*` packages emit `upstreamEcosystem "cran"`
- [ ] Test verifies ecosystem detection heuristic
- [ ] SPARQL query `?conda pkg:upstreamPackageName "numpy" . ?pypi pkg:upstreamPackageName "numpy" .` returns results when both graphs are loaded

**Verify:**

- `cargo test --lib conda`

---

### Task 6: Integration Tests

**Objective:** End-to-end tests that verify spidering produces transitive deps and cross-ecosystem correlation works.
**Dependencies:** Tasks 1-5
**Mapped Scenarios:** None

**Files:**

- Create: `etl/pg-collect/tests/test_spider_integration.rs`

**Key Decisions / Notes:**

- Use mockito to create mock API endpoints that return package metadata with known dependency chains: A → B → C
- Verify that collecting seed [A] with `max_depth=2` produces triples for A, B, and C
- Verify that `max_depth=1` produces triples for A and B but NOT C
- Verify that `max_packages=2` stops after 2 packages even with more deps
- Test each collector independently

**Definition of Done:**

- [ ] PyPI spider integration test with A → B → C chain passes
- [ ] Cargo spider integration test passes
- [ ] GoMod spider integration test passes
- [ ] Depth and package limit tests pass

**Verify:**

- `cargo test --test test_spider_integration`

## Open Questions

None — all design decisions resolved.

### Deferred Ideas

- **NPM spidering** — same pattern, separate task to keep this plan focused
- **Resume/checkpoint** — save spider state to resume interrupted runs
- **Parallel fetching** — async/tokio for concurrent API calls within the spider loop
- **Conda recipe parsing** — fetch actual recipe metadata for more accurate cross-ecosystem links (source.url field)
