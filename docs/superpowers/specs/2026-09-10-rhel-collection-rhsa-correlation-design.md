# RHEL Collection + RHSA Package Correlation — Design

**Date:** 2026-09-10
**Status:** Implemented and live-tested 2026-09-11 (PR #24, `feat/rhel-collection-rhsa-correlation`). TLS collection against the real RHEL CDN and the RHSA resolution query are confirmed working against live production data (see the PR description for details); two bugs the unit tests missed (NVRA-vs-NVR parsing, `days_back_date()` year-rounding) were found and fixed via this live testing. Not yet merged, and `almalinux/9`/`rocky/9` correlation isn't yet confirmed against their own live promoted data (timing gap, not a defect — see PR description).
**Related:** [RHEL Rebuild Comparison Deriver](2026-09-01-rhel-rebuild-comparison-deriver-design.md) (already implemented as `rpmver.rs`/`rebuild_classify.rs`/`derive_comparison.rs`, expects `rhel/9`, `rhel/10` graphs that have never existed until this design); `docs/adr/0001-cq-data-contract.md` SD-6/SD-7 (the `sec:affectsPackage` / `sec:advisoryForPackage` contract this design implements the RPM-family side of)

## 1. Overview

Two structurally empty CQ areas were found to share upstream causes that are
now resolved enough to design against:

1. **RHEL was never collected.** The RHEL-rebuild drift deriver
   (`derive_comparison.rs`) has been waiting on `rhel/9`/`rhel/10` graphs
   since 2026-09-01. It was assumed RHEL requires a subscription the host
   doesn't have — that assumption was wrong: the host's RHEL entitlement
   (Simple Content Access) grants access to RHEL 9 and 10, both x86_64
   and aarch64 — `subscription-manager repos --list`'s local whitelist
   (scoped to the host's own installed product) undersells what the
   entitlement certificate itself actually grants at the CDN.
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

### 2.1 `rpm-full` TLS client-cert support (revised — reuse existing machinery)

**Correction from the original draft of this section:** `pg-collect` already
has a complete TLS-client-cert path — `RpmCollector::new_with_tls` /
`new_with_tls_and_repo_type` (`rpm.rs`), which takes `client_cert_path`,
`client_key_path`, **and a required `ca_cert_path`**, builds the identity via
`reqwest::Identity::from_pem` on the concatenated key+cert bytes, and adds
the CA via `.add_root_certificate()`. It is already wired to a CLI surface —
the plain `Rpm` subcommand accepts `--sslclientcert`/`--sslclientkey`/
`--sslcacert` and already has a full worked example for exactly this RHEL
scenario in `docs/rhel-collection.md` (Podman and Kubernetes, with the
correct CDN paths and a measured output scale of ~37M triples/graph).

The original draft invented a new, cert-only (no CA) mechanism under new
flag names (`--client-cert`/`--client-key`) — that doesn't match what the
TLS constructor requires (a CA is mandatory, not optional) and needlessly
duplicates working code under inconsistent naming. The actual gap is
narrower: **`RpmFull` (used by every other `-full.sh` collector, including
Alma/Rocky/CentOS Stream, for its cross-arch/noarch-dedup handling) has no
cert flags at all** — only the plain `Rpm` subcommand does, and it lacks
`RpmFull`'s multi-URL arch-combining behavior.

Fix: add the same three flags, under the same names, to `RpmFull`:

```
--sslclientcert <path>   # PEM certificate
--sslclientkey <path>    # PEM private key
--sslcacert <path>       # PEM CA certificate (required alongside the above two)
```

In `RpmFull`'s handler, mirror the existing `Rpm` subcommand's
`make_collector` closure exactly: when all three are present, construct
each per-URL `RpmCollector` via `new_with_tls_and_repo_type` instead of
`new_with_repo_type`. Absent, behavior is unchanged for every other
`-full.sh` collector — strictly additive, and consistent with the
already-documented, already-working `Rpm` path rather than a parallel
implementation.

### 2.2 New collector scripts

`deploy/quadlet/collectors/scripts/rhel-9-full.sh` and `rhel-10-full.sh`,
modeled directly on `centos-stream-10-full.sh`, using the flag names and CA
path from §2.1 and `docs/rhel-collection.md` (**not** the invented names/path
from the original draft):

```sh
#!/bin/sh
set -eu
GRAPH_URI="https://packagegraph.github.io/graph/rhel/9"

# Entitlement cert filename embeds a serial number that rotates on
# renewal -- glob for it rather than hardcode today's serial.
CLIENT_CERT=$(ls /etc/pki/entitlement/[0-9]*.pem | grep -v -- '-key.pem$' | head -1)
CLIENT_KEY="${CLIENT_CERT%.pem}-key.pem"
CA_CERT=/etc/rhsm/ca/redhat-uep.pem

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
  --sslclientcert "$CLIENT_CERT" --sslclientkey "$CLIENT_KEY" --sslcacert "$CA_CERT" \
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

`pg-collect@.container` needs `/etc/pki/entitlement/` (cert + key) and
`/etc/rhsm/ca/` (for `redhat-uep.pem`, the actual Red Hat CA — **not**
`redhat-entitlement-authority.pem`, which was this design's original,
incorrect guess; confirmed against `docs/rhel-collection.md` and
`etl/scripts/README.md`, both of which already document this exact path)
bind-mounted read-only into the container — they aren't part of the ETL
image and don't belong there (host-specific, renewal-managed by
`subscription-manager`, not something to bake into a shipped image).

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
external-API enricher in this codebase.

**Failure threshold, not silent per-CVE skip (revised).** The original
draft logged and skipped individual detail-fetch failures with no upper
bound. That's wrong: `advisory.sh` uploads a fresh `.nt` and replaces the
entire `graph/enrichment/advisory-rhsa` graph on any successful process
exit — if detail fetches degrade (rate limiting, a partial outage) but stay
above zero, the run still "succeeds" and publishes a graph missing
`advisoryForPackage` links that the *previous* run had, with no signal
anything regressed. Track failures against total CVEs attempted; if the
failure rate exceeds a fixed threshold (e.g. 5%), fail the whole run
(non-zero exit) instead of uploading a degraded graph — matching
`upload-nt.sh`'s upload-only-on-success contract, just moved one level up
to cover partial-correlation degradation the process exit code alone
wouldn't otherwise catch. A handful of scattered 404s/transient errors
well under the threshold still log-and-skip as before.

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

### 3.3 Resolution and emission (revised — match the actual RPM model)

**Correction from the original draft:** the original match key
("epoch:version-release") doesn't correspond to anything RPM collection
actually emits. `rpm.rs` builds `pkg:versionString` as
`"{version}-{release}.{arch}"` (arch included, epoch *not* included) on
the package's `Version` resource, and writes epoch as a **separate,
conditional** property — `pkg:epoch` (string) and `rpm:epoch` (integer) —
emitted only when the epoch is non-zero. A lookup keyed on an
"epoch:version-release" string, or one that requires an epoch triple to
exist for a zero-epoch RPM, matches nothing.

The corrected SPARQL shape, for a parsed (major-version, name, epoch,
version, release, arch) tuple, against each of the graphs matching that
major version — `rhel/{9,10}`, `almalinux/{9,10}`, `rocky/{9,10}`:

```sparql
SELECT ?pkg WHERE {
  GRAPH <...> {
    ?identity pkg:packageName "<name>" .
    ?pkg pkg:isVersionOf ?identity ;
         pkg:hasVersion ?ver .
    ?ver pkg:versionString "<version>-<release>.<arch>" .
    # Only constrain on epoch when Red Hat's NVRA reports one; a
    # zero-epoch RPM has no pkg:epoch triple at all, so requiring one
    # unconditionally would wrongly exclude every such package.
    OPTIONAL { ?ver pkg:epoch ?epoch }
    FILTER(<epoch is 0 and !BOUND(?epoch)> || ?epoch = "<epoch>")
  }
}
```

(Exact filter form to be finalized against the live ontology at
implementation time — the point fixed here is the match key:
`versionString` compared as a single `version-release.arch` string, with
epoch conditional, not required.)

This is an existence check, not a guess: **`sec:advisoryForPackage` is only
emitted for a URI a SPARQL query actually returned**, matching SD-7's
prohibition on synthetic or unresolvable targets. A NVRA that resolves to
zero matches across all six graphs is silently skipped — expected whenever
the current collected snapshot has already moved past (or hasn't yet
reached) that exact build, since `rpm-full` collectors capture the
*current* repo state, not historical NVR history.

Every graph that does resolve a match gets its own
`sec:advisoryForPackage` triple (a given NVR can legitimately exist in more
than one of the three families, and in both arches).

### 3.4 `AdvisoryEnricher` needs new CLI wiring (revised)

**Correction from the original draft:** it claimed `advisory.sh` "already
carried a now-functional `--endpoint`" argument — false. `advisory.sh`
passes no endpoint today, and `EnrichAdvisory`'s current CLI
(`advisory_type`, `output`, `days_back`, `cache_dir`) has no
endpoint/auth/backend fields at all; the `--endpoint` flag this session
removed earlier was dead on arrival (the CLI never accepted it) and stayed
removed. This design must add it, not "complete" something already there:

- New `EnrichAdvisory` args: `--endpoint`, plus the same
  `SparqlAuth`/`SparqlBackend` construction `enrich_security.rs` and
  `enrich_epss.rs` already use.
- `AdvisoryEnricher::new`/`with_graph` gains a `SparqlClient`, constructed
  the same way those two enrichers do.
- `advisory.sh` updated to pass `--endpoint "$FUSEKI_ENDPOINT"` (the
  variable name is legacy — see `deploy/quadlet/README.md`'s "Package
  collectors" section on why it's kept as-is — pointing at the local
  QLever instance, same as every other enricher on this host).

### 3.5 RHSA subject identity (new — was silently inherited, needs a decision)

Current RHSA emission (`enrich_advisory.rs`, `emit_rhsa_advisory`) creates
one `SecurityAdvisory` subject per **CVE** — `{DATA}advisory/rhsa/{cve_id}`
— not per RHSA. A single CVE can be addressed by multiple distinct RHSAs
(e.g. separate advisories for different RHEL minor releases or module
streams), and Red Hat's per-CVE detail response gives us the real RHSA
identifier per `affected_release` entry (`"advisory": "RHSA-2024:1234"`).
Attaching every resolved `advisoryForPackage` link to a CVE-keyed subject
conflates distinct advisories and loses that distinction — a package fixed
by RHSA-2024:1234 and another fixed by RHSA-2024:5678 for the same CVE
would incorrectly appear to share one advisory node.

Decision: migrate the RHSA subject to `{DATA}advisory/rhsa/{rhsa_id}`
(e.g. `advisory/rhsa/RHSA-2024-1234`, colon replaced since it's embedded in
an IRI path segment), one node per real advisory. Each still links to its
CVE(s) via `sec:addressesVulnerability` (an advisory can address more than
one CVE; keep this multi-valued). `sec:advisoryId` becomes the RHSA
identifier, not the CVE identifier. This is a breaking change to the
existing (CVE-keyed) `graph/enrichment/advisory-rhsa` output shape — the
whole graph is replaced wholesale by `advisory.sh` on every run regardless
(§3.1), so there's no migration path to design for beyond just changing
what gets emitted going forward.

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

- Per-CVE detail fetch failure → log, skip that CVE, count against the
  failure threshold (§3.1); abort the run (non-zero exit, no upload) if the
  threshold is exceeded rather than publishing a degraded graph.
- Unparseable `product_name` or `package` NVRA → log, skip that entry, continue
  (not counted toward the fetch-failure threshold — this is a data-shape
  skip, not a fetch failure).
- Zero SPARQL matches for a resolved NVRA → skip silently (expected/common).
- RHEL collector failures → identical to every other `rpm-full` collector:
  periodic cache-sync-on-timeout already in place, retried by its own timer.

## 6. Testing

- Unit tests: NVRA parsing (happy path + malformed epoch/arch), `product_name`
  major-version extraction (happy path + EUS/module/legacy strings that must
  skip cleanly).
- Unit test: the `versionString`-based match query construction (§3.3) —
  zero-epoch NVRA produces a filter that doesn't require a `pkg:epoch`
  triple; non-zero-epoch NVRA does.
- Unit test: the failure-threshold safeguard (§3.1) — a run under threshold
  completes and uploads; a run over threshold aborts without uploading.
- Unit test: RHSA subject identity (§3.5) — two different RHSAs addressing
  the same CVE produce two distinct `SecurityAdvisory` subjects, each
  correctly linked to the shared CVE via `addressesVulnerability`.
- Unit test: `RpmFull` selects `new_with_tls_and_repo_type` when
  `--sslclientcert`/`--sslclientkey`/`--sslcacert` are all given (mocked
  identity/CA), and `new_with_repo_type` (unmodified) when they're absent.
  Neither the existing `Rpm` subcommand nor `RpmCollector::new_with_tls`
  itself has any test coverage today (confirmed: no matches for
  `new_with_tls`/`sslclientcert` under `#[test]`) — this is new coverage,
  not a mirror of something already tested, and is worth adding for both
  the new `RpmFull` path and retroactively for the existing `Rpm` path
  while touching this code.
- Live verification post-deploy: confirm `rhel-9-full`/`rhel-10-full`
  produce real triple counts comparable to `almalinux-9-full`/etc.
  (`docs/rhel-collection.md` cites ~37M triples/graph as a reference
  point); spot-check a handful of resolved `advisoryForPackage` triples
  against known recent RHSAs, confirming the subject is RHSA-keyed, not
  CVE-keyed.

## 7. Open points

- Entitlement cert renewal: `subscription-manager` manages rotation
  automatically today; the glob-based lookup in §2.2 tolerates serial
  rotation but not a wholesale re-registration (new consumer identity) —
  out of scope to automate further here.
- CentOS Stream correlation (deferred per §1 non-goals) will need a
  different matching strategy entirely, likely package-name-only with an
  explicit "unreliable" marker, or dropped as infeasible — separate design.
