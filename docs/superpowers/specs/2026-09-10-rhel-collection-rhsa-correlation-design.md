# RHEL Collection + RHSA Package Correlation — Design

**Date:** 2026-09-10
**Status:** Design — awaiting review before implementation plan
**Related:** [RHEL Rebuild Comparison Deriver](2026-09-01-rhel-rebuild-comparison-deriver-design.md) (already implemented as `rpmver.rs`/`rebuild_classify.rs`/`derive_comparison.rs`, expects `rhel/9`, `rhel/10` graphs that have never existed until this design); `docs/adr/0001-cq-data-contract.md` SD-6/SD-7 (the `sec:affectsPackage` / `sec:advisoryForPackage` contract this design implements the RPM-family side of)

## 1. Overview

Two structurally empty CQ areas were found to share upstream causes that are
now resolved enough to design against:

1. **RHEL was never collected.** The RHEL-rebuild drift deriver
   (`derive_comparison.rs`) has been waiting on `rhel/9`/`rhel/10` graphs
   since 2026-09-01. It was assumed RHEL requires a subscription the host
   doesn't have — that assumption was wrong. The host (packagegraph.di.riseproject.dev)
   has a valid, registered RHEL entitlement (Simple Content Access, cert
   valid through 2027-09-09). Empirically verified via direct `curl` against
   `cdn.redhat.com` with the entitlement cert: **RHEL 9 and 10, both x86_64
   and aarch64, all return real repodata** — `subscription-manager repos
   --list`'s local whitelist (scoped to the host's own installed product,
   RHEL 10 ARM64) undersells what the entitlement certificate itself
   actually grants at the CDN.
2. **`sec:advisoryForPackage` is never emitted for RPM-family packages.**
   `sec:affectsPackage` (OSV-based) is architecturally restricted to
   language ecosystems (`osv.rs`'s `ecosystem_mapping()` — npm, PyPI,
   crates.io, Go, Maven, NuGet, Packagist, RubyGems, Hex, Pub, Hackage,
   SwiftURL only; Debian and Alpine are explicitly skipped too, not just
   RPM). This is documented and anticipated in SD-6/SD-7: `advisoryForPackage`
   is listed as `"unsupported (needs NVRA resolution)"` for RPM. The
   existing `AdvisoryEnricher`'s RHSA path only calls Red Hat's bulk CVE
   list endpoint (CVE ID + severity + date) — it never fetches per-CVE
   affected-package data, so NVRA resolution was never attempted.

### Goals

- Collect RHEL 9 and RHEL 10 (BaseOS, x86_64 + aarch64) using the host's
  existing entitlement certificate for TLS client auth.
- Extend RHSA enrichment to fetch per-CVE affected-package detail, resolve
  it against real, currently-collected packages, and emit
  `sec:advisoryForPackage` pointing at the exact fixed-NVR package build.

### Non-goals (this design)

- RHEL 8 or any release other than 9/10.
- AppStream, CRB, or any repo beyond BaseOS (matches existing Alma/Rocky/
  CentOS Stream collector scope; trivial follow-on later).
- CentOS Stream in the correlation target set — CentOS Stream tracks
  *ahead* of RHEL (upstream, not a downstream rebuild), so exact-NVR
  matching against it is unreliable. Deferred; needs its own design.
- DSA (Debian) package correlation — `enrich_dsa` already documents the
  same SD-7 gap for its own, differently-shaped reason (source-package-only
  data, no version-aware resolution) and is untouched here.
- Resolving *vulnerable* (pre-fix) versions. `advisoryForPackage` targets
  only the exact fixed-NVR build Red Hat's data names — see §4.
- Any change to `sec:affectsPackage` or the OSV-based enrichment path.

## 2. RHEL collection

### 2.1 `rpm-full` client-cert support

`pg-collect`'s HTTP client has no TLS client-certificate support today —
every existing collector hits public, unauthenticated mirrors. Add two new
optional args to the `rpm-full` subcommand:

```
--client-cert <path>   # PEM certificate
--client-key <path>    # PEM private key
```

When both are present, the collector's `reqwest::blocking::Client` is built
with `.identity(Identity::from_pem(&combined_pem_bytes))` (cert and key
concatenated in memory — `reqwest`'s `Identity::from_pem` accepts a single
buffer containing both) instead of the default client. Absent, behavior is
byte-for-byte unchanged for every other collector — this is strictly
additive.

### 2.2 New collector scripts

`deploy/quadlet/collectors/scripts/rhel-9-full.sh` and `rhel-10-full.sh`,
modeled directly on `centos-stream-10-full.sh`:

```sh
#!/bin/sh
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/rhel/9"

# Entitlement cert filename embeds a serial number that rotates on
# renewal -- glob for it rather than hardcode today's serial.
CLIENT_CERT=$(ls /etc/pki/entitlement/[0-9]*.pem | grep -v -- '-key.pem$' | head -1)
CLIENT_KEY="${CLIENT_CERT%.pem}-key.pem"

mkdir -p /tmp/collection
CACHE_DIR=/tmp/cache/rhel-9-full
MINIO_CACHE="pgraph/${MINIO_BUCKET}/collector-cache/rhel-9-full"
mc alias set pgraph "${MINIO_ENDPOINT}" "${MINIO_ACCESS_KEY}" "${MINIO_SECRET_KEY}" --api S3v4

echo "Syncing cache from Minio..."
mc mirror --overwrite "${MINIO_CACHE}/" "${CACHE_DIR}/" 2>/dev/null || true

( while sleep 300; do
    mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
  done ) &
CACHE_SYNC_PID=$!
trap 'kill "${CACHE_SYNC_PID}" 2>/dev/null || true' EXIT

pg-collect rpm-full \
  --url https://cdn.redhat.com/content/dist/rhel9/9/x86_64/baseos/os/ \
  --url https://cdn.redhat.com/content/dist/rhel9/9/aarch64/baseos/os/ \
  --distro rhel --release 9 \
  --client-cert "$CLIENT_CERT" --client-key "$CLIENT_KEY" \
  --with-spec --with-maintainers \
  --cache-dir "${CACHE_DIR}" \
  -o /tmp/collection/rhel-9.nt

/app/scripts/upload-nt.sh /tmp/collection/rhel-9.nt "$GRAPH_URI"

echo "Syncing cache to Minio..."
mc mirror --overwrite "${CACHE_DIR}/" "${MINIO_CACHE}/" 2>/dev/null || true
```

(`rhel-10-full.sh` identical with `9`→`10` throughout.) Two new timers,
weekly cadence matching Alma/Rocky, staggered to avoid clustering.

### 2.3 Entitlement cert bind-mount

`pg-collect@.container` needs `/etc/pki/entitlement/` and (for TLS chain
validation, if `pg-collect` doesn't already trust the system store) the CA
bundle at `/etc/rhsm/ca/redhat-entitlement-authority.pem` bind-mounted
read-only into the container — they aren't part of the ETL image and don't
belong there (host-specific, renewal-managed by `subscription-manager`,
not something to bake into a shipped image).

## 3. RHSA per-CVE correlation

### 3.1 Data source change

`enrich_rhsa()` (`enrich_advisory.rs`) currently only calls
`access.redhat.com/hydra/rest/securitydata/cve.json` (bulk list: CVE ID,
severity, `public_date` — no package data). Add a second fetch per CVE
already obtained from the bulk list:

```
GET https://access.redhat.com/hydra/rest/securitydata/cve/{cve_id}.json
```

Cached (existing `FileCache`, 1-week TTL — matches the bulk endpoint) and
rate-limited (existing `rate_limit()` helper) exactly like every other
external-API enricher in this codebase. A fetch failure for one CVE is
logged and skipped — it does not abort the run (matches `advisory.sh`'s
existing `ENRICH_OK` per-type failure isolation).

### 3.2 NVRA parsing and product mapping

Each detail response's `affected_release[]` entries look like:

```json
{"product_name": "Red Hat Enterprise Linux 9", "package": "kernel-0:5.14.0-427.13.1.el9_4.x86_64", "advisory": "RHSA-2024:1234"}
```

For each entry:

1. Extract the major version from `product_name` via `Red Hat Enterprise
   Linux (\d+)\b`. Anything that doesn't match a bare major version — EUS
   variants, module streams, legacy `(v. 6 for 64-bit)`-style strings — is
   skipped, not guessed at. Only `9` and `10` are handled (§1 non-goals).
2. Parse `package` (NVRA: `name-epoch:version-release.arch`) into its five
   components. Malformed entries (missing epoch, unparseable arch) are
   skipped and logged, not defaulted.

### 3.3 Resolution and emission

For a parsed (major-version, name, epoch, version, release) tuple, query
(via the existing `SparqlClient`, same construction as
`enrich_security.rs`) each of the graphs matching that major version —
`rhel/{9,10}`, `almalinux/{9,10}`, `rocky/{9,10}` — for a `Package` with
that exact name and epoch:version-release. This is an existence check, not
a guess: **`sec:advisoryForPackage` is only emitted for a URI a SPARQL
query actually returned**, matching SD-7's prohibition on synthetic or
unresolvable targets. A NVRA that resolves to zero matches across all six
graphs is silently skipped — expected whenever the current collected
snapshot has already moved past (or hasn't yet reached) that exact build,
since `rpm-full` collectors capture the *current* repo state, not
historical NVR history.

Every graph that does resolve a match gets its own
`sec:advisoryForPackage` triple (a given NVR can legitimately exist in more
than one of the three families, and in both arches).

### 3.4 `AdvisoryEnricher` now needs a SPARQL endpoint

This is the first time `AdvisoryEnricher` depends on a SPARQL endpoint —
`advisory.sh` already carried a now-functional `--endpoint "$FUSEKI_ENDPOINT"`
argument from before this session's fix removed it (the CLI never accepted
it at the time); this design is what finally gives that argument a real
job, following the identical `--endpoint`/`SparqlAuth`/`SparqlBackend`
construction `enrich_security.rs` and `enrich_epss.rs` already use.

## 4. Semantics: fixed-NVR only, not vulnerable-range

Red Hat's per-CVE detail tells us the exact NVR that **fixes** a CVE — not
the range of versions that were vulnerable before it. `advisoryForPackage`
therefore means "this concrete, currently-collected package build is what
this advisory's fix produced," not "this package is vulnerable." This is
deliberately simpler than OSV's `affectsPackage` range semantics, matches
the data Red Hat actually gives us with no invented inference, and needs no
RPM version-comparison logic (the RHEL-rebuild deriver's `rpmver.rs`
already exists for a different purpose — rebuild-fidelity comparison — and
is not a dependency of this design).

## 5. Error handling

- Per-CVE detail fetch failure → log, skip that CVE, continue.
- Unparseable `product_name` or `package` NVRA → log, skip that entry, continue.
- Zero SPARQL matches for a resolved NVRA → skip silently (expected/common).
- RHEL collector failures → identical to every other `rpm-full` collector:
  periodic cache-sync-on-timeout already in place, retried by its own timer.

## 6. Testing

- Unit tests: NVRA parsing (happy path + malformed epoch/arch), `product_name`
  major-version extraction (happy path + EUS/module/legacy strings that must
  skip cleanly).
- Unit test: `rpm-full` builds an authenticated `reqwest` client when
  `--client-cert`/`--client-key` are given (mocked identity), and an
  unmodified default client when they're absent.
- Live verification post-deploy: confirm `rhel-9-full`/`rhel-10-full`
  produce real triple counts comparable to `almalinux-9-full`/etc.; spot-
  check a handful of resolved `advisoryForPackage` triples against known
  recent RHSAs.

## 7. Open points

- Entitlement cert renewal: `subscription-manager` manages rotation
  automatically today; the glob-based lookup in §2.2 tolerates serial
  rotation but not a wholesale re-registration (new consumer identity) —
  out of scope to automate further here.
- CentOS Stream correlation (deferred per §1 non-goals) will need a
  different matching strategy entirely, likely package-name-only with an
  explicit "unreliable" marker, or dropped as infeasible — separate design.
