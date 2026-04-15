# OSV Bulk Security Collector Implementation Plan

Created: 2026-04-14
Author: sovereign@local
Status: PENDING
Approved: Yes
Iterations: 0
Worktree: No
Type: Feature

## Summary

**Goal:** Replace the per-package API security enricher with a Rust bulk collector (`pg-collect osv`) that downloads vulnerability ZIPs from the public OSV GCS bucket and matches against packages by name + version — eliminating API calls entirely.

**Architecture:** New `pg-collect osv` subcommand downloads `all.zip` per ecosystem via HTTPS, extracts JSON files, parses OSV records, matches against packages by name and version ranges (exact match from `versions[]` array, semver comparison from `ranges[]` for crates.io/PyPI), and emits N-Triples using the existing `sec:Vulnerability`/`sec:SecurityAdvisory` ontology.

**Tech Stack:** Rust, `reqwest` (download), `zip` crate (extraction), `semver` crate (version comparison), `serde_json` (OSV parsing).

## Scope

### In Scope

- `pg-collect osv` subcommand with `--ecosystems` flag
- Download and parse `all.zip` for: Debian, Alpine, PyPI, crates.io
- Version matching: exact match via `versions[]`, semver via `ranges[]` (type=SEMVER)
- OSV → security ontology triple mapping (Vulnerability + CVE + severity + affected packages)
- CronJob manifest for scheduled collection
- Tests for OSV parsing, version matching, and triple emission

### Out of Scope

- Go ecosystem (OSV Go data uses git commit ranges, not version ranges — needs separate handling)
- Linux kernel CVEs (different matching model)
- PEP 440 version comparison (rely on `versions[]` explicit list for PyPI)
- Replacing the Python enricher file — it stays for backward compatibility, the CronJob points to the new Rust collector

## Approach

**Chosen:** Single Rust subcommand with per-ecosystem ZIP download
**Why:** Eliminates all API calls. The ~200MB download per ecosystem processes in seconds vs hours of per-package API calls. Rust streaming keeps memory low.
**Alternatives considered:**
- Keep Python enricher, add bulk mode: Slower, still Python's memory overhead for large datasets
- Download individual JSON files via HTTPS listing: More HTTP requests, harder to parallelize than a single ZIP

## Context for Implementer

- **OSV JSON format:** Each vulnerability is a JSON file with `id`, `aliases` (CVE IDs), `summary`, `severity[].score` (CVSS vector), `affected[].package` (name + ecosystem), `affected[].versions[]` (explicit affected versions), `affected[].ranges[]` (version ranges with introduced/fixed events)
- **Version matching strategy:**
  - If `affected[].versions[]` is present: exact string match against our package versions (works for ALL ecosystems)
  - If `affected[].ranges[].type == "SEMVER"`: use `semver` crate for range comparison (crates.io, some PyPI)
  - If neither matches: emit the vulnerability linked to the package identity (name-only, no version specificity) with a flag indicating unverified version match
- **Security ontology:** `sec:Vulnerability` (cveId, cvssScore, severity, summary), `sec:SecurityAdvisory` (advisoryId), `sec:affectsVersion`, `sec:fixedInVersion`, `sec:addressesVulnerability`
- **Key files:**
  - `src/main.rs` — CLI subcommand definition
  - `ontology/security.ttl` — security ontology (classes + properties)
  - `src/uris.rs` — `cve_uri()`, `advisory_uri()` already defined
  - `deploy/overlays/dev/jobs/enrich-security-system.yaml` — current CronJob to update
- **Download URLs:** `https://osv-vulnerabilities.storage.googleapis.com/{ecosystem}/all.zip`
  - Ecosystems: `Debian`, `Alpine`, `PyPI`, `crates.io`
- **Gotchas:**
  - The ZIP contains thousands of individual JSON files (one per vulnerability)
  - Some OSV records have no `versions[]` and only `ranges[]` — need both paths
  - CVSS score must be parsed from the vector string (e.g., `CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H` → score 9.8)
  - Actually, OSV `severity[].score` IS the CVSS vector string, not the numeric score. The numeric score needs to be calculated or omitted.

## Assumptions

- OSV `all.zip` files are publicly accessible without authentication — supported by GCS public bucket documentation — all tasks depend on this
- The `zip` crate can stream-read from an in-memory buffer (reqwest downloads to bytes, then zip processes) — Task 1 depends on this
- `semver` crate handles Rust/crates.io version comparison correctly — Task 3 depends on this
- OSV records for Debian use `versions[]` (explicit list) rather than ranges — Task 3 depends on this

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| ZIP download is large (~200MB per ecosystem) | Low | Low | Download one ecosystem at a time, process serially |
| Version comparison false negatives | Medium | Medium | Log unmatched packages for debugging; prefer `versions[]` explicit list when available |
| OSV schema changes | Low | Medium | Use `serde(default)` for optional fields; validate with test against real data |
| Memory usage for large ZIPs | Medium | Medium | Stream-extract files from ZIP, don't hold all JSON in memory |

## Goal Verification

### Truths

1. `pg-collect osv --ecosystems Debian,PyPI,crates.io,Alpine -o out.nt` produces vulnerability triples
2. Output contains `sec:Vulnerability` instances with `sec:cveId`, `sec:severity`, `sec:summary`
3. Vulnerabilities are linked to packages via `sec:affectsVersion` or `sec:affectsPackage`
4. Version matching correctly identifies affected versions (test with known CVE + known affected version)
5. The collector completes in under 10 minutes (vs hours for API-based)
6. CronJob manifest updated to use `pg-collect osv`

### Artifacts

- `src/osv.rs` — OSV collector implementation
- `src/main.rs` — CLI subcommand
- `tests/test_osv.rs` — unit tests
- `deploy/overlays/dev/jobs/enrich-security-system.yaml` — updated CronJob

## Progress Tracking

- [ ] Task 1: OSV data model and ZIP download
- [ ] Task 2: OSV JSON parsing and triple emission
- [ ] Task 3: Version matching (exact + semver)
- [ ] Task 4: CLI subcommand and CronJob manifest
- [ ] Task 5: Tests

**Total Tasks:** 5 | **Completed:** 0 | **Remaining:** 5

## Implementation Tasks

### Task 1: OSV Data Model and ZIP Download

**Objective:** Define the OSV serde data structures and implement ZIP download + extraction from HTTPS URLs.
**Dependencies:** None

**Files:**

- Create: `etl/pg-collect/src/osv.rs`
- Modify: `etl/pg-collect/src/lib.rs` (add module)
- Modify: `etl/pg-collect/Cargo.toml` (add `zip`, `semver` deps)

**Key Decisions / Notes:**

- Add to Cargo.toml: `zip = { version = "2.0", default-features = false, features = ["deflate"] }`, `semver = "1.0"`
- OSV data structures: `OsvEntry`, `OsvAffected`, `OsvPackage`, `OsvRange`, `OsvEvent`, `OsvSeverity`
- Download function: `reqwest::blocking::get(url).bytes()` → `zip::ZipArchive::new(Cursor::new(bytes))`
- Extract each file as a string, parse as `OsvEntry`

**Definition of Done:**

- [ ] OSV serde structs compile and parse real OSV JSON
- [ ] ZIP download and extraction works
- [ ] Module added to lib.rs

**Verify:**

- `cargo check`

---

### Task 2: OSV JSON Parsing and Triple Emission

**Objective:** Parse OSV records and emit security triples (Vulnerability, CVE, severity, affected packages).
**Dependencies:** Task 1

**Files:**

- Modify: `etl/pg-collect/src/osv.rs`

**Key Decisions / Notes:**

- For each OSV entry:
  - Create `sec:Vulnerability` with `sec:cveId` (from `aliases[]` starting with `CVE-`), `sec:summary`, `sec:severity`
  - Create `sec:SecurityAdvisory` with `sec:advisoryId` (the OSV `id`)
  - Link advisory → vulnerability via `sec:addressesVulnerability`
  - For each `affected[].package`: link to package identity URI via `sec:affectsPackage`
- Map OSV ecosystem names to our distro names: `Debian` → `debian`, `Alpine` → `alpine`, `PyPI` → `pypi`, `crates.io` → `cargo`
- Use existing `cve_uri()` and `advisory_uri()` from `uris.rs`

**Definition of Done:**

- [ ] OSV entries produce valid N-Triples
- [ ] CVE IDs extracted from aliases
- [ ] Severity mapped from CVSS vector string
- [ ] Packages linked to vulnerabilities

**Verify:**

- `cargo test --lib osv`

---

### Task 3: Version Matching

**Objective:** Match OSV affected versions against package versions in the graph to determine which specific package versions are affected.
**Dependencies:** Task 2

**Files:**

- Modify: `etl/pg-collect/src/osv.rs`

**Key Decisions / Notes:**

- Two matching strategies:
  1. `versions[]` explicit list: exact string match → `sec:affectsVersion` link to specific version URI
  2. `ranges[]` with `type: "SEMVER"`: parse `introduced` and `fixed` events, use `semver` crate to check if a version is in the affected range
- For `ranges[]` with `type: "ECOSYSTEM"`: skip range comparison, fall back to `versions[]` or name-only link
- The collector doesn't query Fuseki — it emits ALL vulnerabilities and their affected package names. The graph consumer joins by package name.

**Definition of Done:**

- [ ] `versions[]` exact match identifies affected versions
- [ ] SEMVER range matching works for crates.io data
- [ ] Test with known CVE (e.g., CVE in openssl crate) verifies correct version matching

**Verify:**

- `cargo test --lib osv`

---

### Task 4: CLI Subcommand and CronJob

**Objective:** Add `pg-collect osv` CLI subcommand and update the CronJob manifest.
**Dependencies:** Tasks 1-3

**Files:**

- Modify: `etl/pg-collect/src/main.rs`
- Modify: `deploy/overlays/dev/jobs/enrich-security-system.yaml`

**Key Decisions / Notes:**

- CLI args: `--ecosystems` (comma-separated, default: `Debian,Alpine,PyPI,crates.io`), `-o` output
- CronJob: replace Python enricher command with `pg-collect osv --ecosystems Debian,Alpine,PyPI,crates.io -o /tmp/security.nt && pg-collect load /tmp/security.nt --graph ... --endpoint ...`
- Keep `activeDeadlineSeconds: 7200` (bulk download is fast but ZIP extraction + matching may take time for large ecosystems)

**Definition of Done:**

- [ ] `pg-collect osv --help` shows ecosystem flag
- [ ] CronJob manifest uses `pg-collect osv`
- [ ] `oc apply` succeeds

**Verify:**

- `pg-collect osv --help`

---

### Task 5: Tests

**Objective:** Unit tests for OSV parsing, version matching, and triple emission.
**Dependencies:** Tasks 1-3

**Files:**

- Create: `etl/pg-collect/tests/test_osv.rs`

**Key Decisions / Notes:**

- Test OSV JSON parsing with sample data (inline JSON, not live download)
- Test version matching: known CVE with known affected versions
- Test SEMVER range: `introduced: "0.10.0"`, `fixed: "0.10.64"`, verify `0.10.50` is affected but `0.10.64` is not
- Test triple emission: verify output contains `sec:Vulnerability`, `sec:cveId`, `sec:affectsPackage`

**Definition of Done:**

- [ ] OSV parsing test passes
- [ ] SEMVER range matching test passes
- [ ] Triple emission test passes
- [ ] All existing tests still pass

**Verify:**

- `cargo test`

## Deferred Ideas

- Go ecosystem (needs git commit range handling)
- Linux kernel CVEs
- PEP 440 version comparison for PyPI ranges
- Incremental OSV updates (only download new/modified entries since last run)
- CVSS numeric score calculation from vector string
