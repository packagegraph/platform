# Alma / Rocky / RHEL Three-Way Comparison — Research Spike

**Date:** 2026-09-01
**Branch:** `alma-rocky-rhel`
**Question:** How well can the platform differentiate AlmaLinux and Rocky Linux
from Red Hat Enterprise Linux, and how faithfully do those rebuilds track RHEL?

## TL;DR

- Before this spike the platform had **zero** support for Alma/Rocky and **no**
  cross-distro comparison capability. Both are now demonstrated end-to-end.
- **Coverage is near-identical.** For RHEL 9's 2,235 source packages, Alma ships
  2,176 and Rocky 2,187 of the same names; each adds only a handful of its own.
- **Non-modular builds track RHEL's exact NVR closely:** Alma 9 **96.5%**,
  Rocky 9 **86.3%**, Alma 10 **93.5%**, Rocky 10 **94.8%** of the rebuild's
  source builds are byte-identical to a real RHEL build.
- **Divergence is explainable, not random:** branding packages, vendor rebuild
  tags (`.rocky.0.N`, `.alma`, `.0.1`), the modular-package dist-tag convention
  (`.module_el9` vs `.module+el9`), and z-stream timing skew.
- **RHEL differentiation is real and visible:** RHEL-only packages are its
  value-add — `kpatch-patch-*` (kernel live patching), `insights-client`/
  `insights-core`, `command-line-assistant`, `go-toolset`, `kmod-redhat-*`.
- **Bug found & fixed along the way:** the RPM collector duplicated capability/
  identity definition triples ~223× (RHEL 9 BaseOS: 82.3M → 36.8M triples after
  the fix). Filed as [#15], fixed in `0c8fab6`.

## Method

Collected with `pg-collect rpm` (this branch) into six named graphs
`https://packagegraph.github.io/graph/{distro}/{release}`, BaseOS + AppStream,
x86_64:

| Source | Auth | Base URL |
|---|---|---|
| AlmaLinux 9/10 | none | `repo.almalinux.org/almalinux/{9,10}/…` |
| Rocky 9/10 | none | `dl.rockylinux.org/pub/rocky/{9,10}/…` |
| RHEL 9/10 | entitlement cert | `cdn.redhat.com/content/dist/rhel{9,10}/…` |

Each graph was filtered to comparison-relevant predicates (identity, version,
arch, source linkage, distribution) before loading into a local Fuseki; the
reusable queries are in [`query/rhel-rebuild-comparison.rq`](../../query/rhel-rebuild-comparison.rq).

The unit of comparison is the **source package (SRPM)** — the canonical build
unit a rebuild reproduces. Fidelity is exact `(packageName, versionString)`
matching, valid because Alma/Rocky emit byte-identical NVR strings to RHEL
(e.g. `openssl-3.5.5-6.el9_8`, `bash-5.1.8-9.el9`).

### Two properties of the data that shape the analysis

1. **RHEL CDN is cumulative; the rebuild mirrors are point-in-time.** RHEL's
   repo carries the full z-stream history (openssl 9: 3.0.1 → 3.5.5, ~30
   builds), while `repo.almalinux.org/9` and `dl.rockylinux.org/9` serve only
   current builds. Raw package *counts* are therefore incomparable (RHEL 9:
   8,504 name-version pairs vs ~2,660 for the rebuilds). All fidelity metrics
   run **rebuild → RHEL** ("does this rebuild's build exist anywhere in RHEL's
   history?"), which is robust to this.

2. **Modular packages cannot string-match by construction (release 9 only).**
   RHEL renders module builds as `…module+el9.X.0+NNNNN+hash`; Alma uses
   `…module_el9.X.0+…` (underscore) and both rebuilds regenerate the module
   build-IDs. So every modular package mismatches on the string regardless of
   content. We report fidelity **excluding modular packages** as the
   content-level number. RHEL 10 dropped modularity, so its numbers are already
   clean.

## Results

### Coverage (distinct source-package names)

| Release | RHEL | Alma (∩RHEL / only) | Rocky (∩RHEL / only) | 3-way common |
|---|---|---|---|---|
| 9  | 2,235 | 2,176 / 4  | 2,187 / 8 | 2,172 |
| 10 | 1,922 | 1,902 / 11 | 1,908 / 5 | 1,898 |

### Version fidelity (source builds identical to a real RHEL build)

| Release | Alma (non-modular) | Rocky (non-modular) | Alma (incl. modular) | Rocky (incl. modular) |
|---|---|---|---|---|
| 9  | **96.5%** (2410/2497) | **86.3%** (2156/2499) | 90.6% (2410/2661) | 80.7% (2156/2672) |
| 10 | **93.5%** (2111/2258) | **94.8%** (2123/2239) | — (no modules) | — (no modules) |

### What diverges, and why

**Rebuild-only names (added on top of RHEL):**
- Alma 9: `almalinux-release`, `almalinux-logos`, `almalinux-indexhtml`,
  `lorax-templates-almalinux` — pure branding.
- Rocky 9: branding equivalents **plus** `efs-utils`, `python-botocore`,
  `yggdrasil`, `yggdrasil-worker-package-manager` — cloud/AWS extras Rocky ships.

**Divergent versions (name in RHEL, this build isn't):** breakdown of Rocky 9's
516 divergent builds — 173 modular (`module+` build-IDs), 46 `.rocky` vendor
rebuilds (e.g. `anaconda …el9.rocky.0.6`, `cloud-init 24.4-8.el9.rocky.0.1`),
the rest `.0.N` re-tags (`basesystem 11-13.el9.0.1`, `bpftool …el9.0.1`) and
z-stream timing skew. Alma 9's 251 are 84 `.alma` vendor tags plus modular
convention differences — Alma preserves RHEL's exact NVR more often, hence its
higher release-9 fidelity.

**RHEL-only names (RHEL's differentiation):** `kpatch-patch-*` (kernel live
patching — a subscription feature; ~20+ of the 59), `insights-client`,
`insights-core`, `command-line-assistant`, `cockpit-leapp`, `go-toolset`,
`greenboot`, `compiler-rt`, `fips-provider-next`, `kmod-redhat-*`,
`ht-caladea-fonts`. These are genuine RHEL value-add, not collection artifacts.

## Caveats

- **Repo scope:** BaseOS + AppStream only. CRB/extras were not collected, so a
  few "RHEL-only" names may exist in a rebuild's CRB. x86_64 only.
- **Point-in-time:** collected 2026-09-01; z-stream timing skew between the RHEL
  snapshot and the rebuild mirrors accounts for some version mismatches.
- **Fidelity is a lower bound:** it measures string identity of the SRPM NVR,
  not upstream source content. A modular-aware / content-hash comparison would
  reclassify the modular "mismatches."

## Assessment & recommendations

The platform's generic RPM pipeline handled Alma/Rocky with only two one-line
allowlist additions, and the graph model expresses the three-way comparison
cleanly via named-graph SPARQL. Concretely:

1. **Promote Alma/Rocky to first-class** (done here: display names + ecosystem
   detection wired in `37d2b23`; add scheduled collection jobs under `deploy/`).
2. **Build a comparison deriver** that materializes `tracksUpstream` /
   `versionDelta` triples so fidelity is queryable without per-run SPARQL —
   with **module-aware normalization** (strip `.module[_+]el9…` build-IDs) and a
   `.dist`-tag/vendor-suffix normalizer so modular and re-tagged builds compare
   on content.
3. **Refresh `docs/rhel-collection.md`** — its ~2M-triple figure predates the
   duplication bug and is now ~37M/graph even after the [#15] fix.
4. **Follow-ups from [#15]:** redundant `directlyProvides`/`rpmProvides` pair;
   the remaining volume is dominated by real provides edges; the same
   per-occurrence duplication exists for source-package definitions.

[#15]: https://github.com/packagegraph/platform/issues/15
