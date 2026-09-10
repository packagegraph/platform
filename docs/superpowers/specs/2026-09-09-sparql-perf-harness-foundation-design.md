# SPARQL Performance Harness — Foundation — Design

**Date:** 2026-09-09
**Branch:** `feat/qlever-quadlet`
**Status:** Design — awaiting review before implementation plan
**Scope:** Spec 1 of 2. This spec covers the corpus, run identity, serial profiler, and host telemetry. Load generation (k6) and the chaos/fault-injection program are **spec 2**, built on the contracts defined here.

## 1. Why this exists

`docs/QLEVER-HOST-OPERATIONS.md` describes a live, public, unauthenticated SPARQL endpoint — `https://packagegraph.di.riseproject.dev` — served by a single `qlever-server` process pinned to 4 CPUs and 6G of RAM on one aarch64 host. Three properties of that deployment make an ad-hoc measurement approach insufficient:

1. **The quadlet promotion path has no automatic rollback.** `deploy/quadlet/scripts/qlever-rebuild-index.sh` promotes a new index by writing a content hash to `qlever-index/latest`, and `qlever-refresh-if-changed.sh` restarts the server. On failure it does *not* revert the pointer — a deliberate divergence from the Kubernetes CronJob, which does (`STATUS=rollout-reverted`). The existing promotion gates are all *volumetric* (`MAX_LOSS_PCT=25`, graph-identity, a 1,000,000-triple floor). Nothing checks that queries still answer, still answer correctly, or still answer quickly. A rebuild that halves query performance or silently changes results promotes cleanly and stays promoted.

2. **The only existing measurement is not a baseline.** `spike/SPIKE-RESULTS.md` and `spike/cq-profile-results.json` were produced on `berstuk` (amd64) with `-j 8 -c 2G -e 1G`. Production is aarch64 with `-j 4 -c 1G -e 500M`. The numbers do not transfer, in either direction or magnitude.

3. **The public endpoint's failure modes are untested.** `rate=50r/s burst=100` per source IP, `proxy_read_timeout 60s` against QLever's own `-s 300s`, a 1G query cache with a 200-entry ceiling, and no resource limits on nginx itself. None of this has been exercised.

This spec builds the measurement substrate. It deliberately stops short of generating load, because the correctness and identity machinery must exist first — a throughput number measured against responses nobody verified is worse than no number, and a latency regression that can't be attributed to a specific index build is not actionable.

### Goals

- Make any performance or correctness claim about the endpoint reproducible: every run records what was measured, against what index, on what configuration.
- Provide a gate that can block or flag a bad index promotion on correctness *and* latency, not just triple count.
- Rank the 65 competency queries by real cost on the real hardware, with server-side attribution.
- Define the corpus and results contracts that spec 2's load and chaos layers consume, so the two halves cannot diverge.

### Non-goals

- **Concurrency of any kind.** The profiler is strictly serial, one request in flight. Everything about offered load, arrival rates, saturation, and queueing is spec 2.
- **Fault injection.** No `SIGKILL`, no disk fill, no induced outage. `ice-cold` (§6) restarts `qlever.service` through its normal systemd path, which is an operation the rebuild pipeline already performs routinely; that is the limit.
- **Config sweeps.** Varying `-j`/`-m`/`-c`/`-k` requires editing the host's `qlever.container` and is a spec 2 concern, though the run manifest (§7) captures the flags so sweeps can be attributed later.
- **Prometheus, Grafana, node_exporter, or any permanent host-side agent.** Telemetry is sampled over SSH for the duration of a run and stored with the run. Standing observability is a separate decision.
- **Replacing `etl/scripts/cq-validate.py`.** That script validates CQs against Fuseki for data-quality purposes and keeps its own lifecycle. §4.3 covers the corpus migration's effect on it.
- **A Python project.** No `pyproject.toml`, no `uv`, no dependency manager, no third-party packages. See §3.

## 2. What already exists and is reused

| Artifact | Reused as |
|---|---|
| `spike/profile-cqs.py` `canonicalize_bindings()` | The fingerprint canonicalization, unchanged in behavior (§5.2). It already includes `type`, `value`, `datatype`, and `xml:lang`, which is correct — `"1"^^xsd:integer` must not equal `"1"^^xsd:string`, and `"chat"@en` must not equal `"chat"@fr`. |
| `spike/cq-queries.json` | The 65-query corpus, migrated to per-file `.rq` plus a manifest (§4). |
| `spike/profile-cqs.py` `engine_ms` extraction | QLever reports `meta.query-time-ms` in its JSON results; kept as one of two server-side time sources (§8.2). |
| `.loaded` marker in the index dir | Exact index identity. `qlever-load-index.sh` writes the promoted `CONTENT_HASH` here. This is already a perfect run-manifest field and needs no new machinery (§7.2). |
| `etl/scripts/dataset-snapshot.sh --diff before.json after.json` | The idiom `pgperf compare` follows: two JSON files in, a diff and an exit code out. |
| QLever's `QueryEventLog` in `/data` | Per-query server-side event stream (§8.2). Its existence is why `qlever.container` pins `User=999`. |

## 3. Substrate and packaging

**Language: Python 3, standard library only.** This repo has no Python project — `pyproject.toml`, `requirements.txt`, `uv.lock`, pytest, duckdb, pyarrow, and parquet are all absent, and the two Python files that exist (`etl/scripts/cq-validate.py`, `spike/profile-cqs.py`) are stdlib-only by convention. That convention is kept. `urllib.request`, `json`, `subprocess`, `statistics`, and `hashlib` cover everything this spec needs; percentiles over at most a few thousand samples do not require a dataframe library.

**Results format: JSON.** Matching `dataset-snapshot.sh`, `last-run.json`, `test-data/manifest.json`, and `cq-profile-results.json`. Not parquet.

**Distribution: a podman container**, per the operating decision that the harness is a set of containers driven from a workstation.

```
perf/
├── Containerfile.perf          # slim base + python3 + openssh-client
├── README.md
├── corpus/
│   ├── manifest.json
│   ├── cq/CQ-*.rq              # 65 files, promoted from spike/cq-queries.json
│   ├── adversarial/*.rq        # authored here, consumed by spec 2
│   └── smoke/*.rq
├── pgperf/
│   ├── __main__.py             # CLI dispatch
│   ├── corpus.py               # load + validate manifest, resolve .rq files
│   ├── client.py               # SPARQL over HTTP; target modes; outcome classification
│   ├── fingerprint.py          # canonicalization + hashing
│   ├── profile.py              # cache levels, repetitions, percentiles
│   ├── telemetry.py            # SSH sampler + QueryEventLog harvest
│   ├── runmeta.py              # run manifest capture, client and server side
│   └── compare.py              # baseline diff + gate exit codes
└── baselines/
    └── <target>-<date>.json
```

`Containerfile.perf` follows the repo's container conventions: fully-qualified `docker.io/` base ref, pinned version, `--no-install-recommends` plus apt-list cleanup, non-root final `USER`. It installs `python3` and `openssh-client` and nothing else.

Invocation, wired into the root `Makefile` as `perf-build`, `perf-manifest`, `perf-profile`, `perf-compare`:

```
podman run --rm \
  -v ./perf:/perf:ro,Z \
  -v ./output/perf:/output:Z \
  -v $HOME/.ssh:/ssh:ro,Z \
  ghcr.io/packagegraph/pgperf:latest \
  profile --target public --corpus /perf/corpus/manifest.json --out /output
```

The SSH key is mounted read-only and used only for telemetry and cache control. `--target public` runs need no SSH at all beyond the server-side half of the run manifest; that half degrades to `"server": {"available": false, "reason": "..."}` rather than failing the run, so the profiler stays usable from a machine without host access.

## 4. The corpus

### 4.1 Layout

Queries move from string fields inside `spike/cq-queries.json` to individual `.rq` files under `perf/corpus/`, indexed by `perf/corpus/manifest.json`. The queries are 328–1141 characters each; as JSON string values they do not produce readable diffs, cannot be syntax-highlighted, and cannot be pasted into YASGUI or `curl` without unescaping. As files they diff cleanly, which matters because the gate's whole premise is attributing behavior change to a specific commit.

`query/rhel-rebuild-comparison.rq` establishes `.rq` as the repo's extension for standalone queries.

### 4.2 Manifest schema

```json
{
  "schema_version": 1,
  "queries": [
    {
      "id": "CQ-SEC-01",
      "title": "CVEs affecting a package",
      "file": "cq/CQ-SEC-01.rq",
      "class": "aggregate",
      "features": ["GROUP BY", "inverse-path", "FILTER NOT EXISTS"],
      "expect": { "min_rows": 1, "max_rows": 10000 },
      "max_row_drift_pct": 25,
      "fingerprint": { "index_hash": "a1b2c3d4e5f60718", "sha256": "…" },
      "deterministic": true,
      "timeout_s": 300,
      "weight": 3,
      "enabled": true
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `class` | One of `lookup`, `join`, `aggregate`, `path`, `graph`, `text`, `adversarial`. Drives per-class percentile reporting — a blended p95 across 65 queries hides the handful that are unusable. |
| `features` | SPARQL constructs present, derived mechanically at manifest-generation time. Diagnostic, not gating. |
| `expect.min_rows` / `max_rows` | Absolute cardinality bounds. A query that should never return zero returning zero is the signature of a partial index promotion, and must be caught even where results themselves cannot be compared (§4.5). |
| `max_row_drift_pct` | Relative cardinality tolerance across a promotion (§4.5). `null` opts a legitimately fast-moving query out. |
| `fingerprint` | `sha256` over the canonicalized bindings (§5.2), **paired with the `index_hash` it was captured against** — a fingerprint is evidence about one index, not about the data (§4.4). `null` when `deterministic` is false. |
| `deterministic` | Measured, not asserted. See §4.6. |
| `timeout_s` | Per-query client timeout. Defaults to 300 to match `qlever-server -s 300s`; `public` runs additionally see nginx's 60s ceiling, which is recorded as a distinct outcome rather than a timeout (§5.3). |
| `weight` | Relative frequency in a mixed load profile. **Unused by this spec**; present so spec 2's k6 workload consumes this same file rather than inventing a parallel corpus. |
| `enabled` | Allows retiring a query without deleting it and losing its history. |

The corpus at authoring time is heavy on inverse paths (42 of 65), `ORDER BY` (39), `DISTINCT` (24), `COUNT` (22), and `GROUP BY` (19), and light on property paths (3), named-graph scoping (3), and `UNION` (2). It contains no `SERVICE`, consistent with federation not being part of the service. The adversarial set (§4.7) exists to cover what the CQs do not.

### 4.3 Migration and the existing consumers

`spike/cq-queries.json` has two consumers: `spike/profile-cqs.py` and, in the same shape, `etl/scripts/cq-validate.py`. Neither is broken by this work:

- `spike/` is spike material that already served its purpose (`SPIKE-RESULTS.md`, recommendation GO, migration complete). `spike/profile-cqs.py` and `spike/cq-profile-results.json` are superseded by `perf/` and are **deleted** as part of this change; their behavior is subsumed by `pgperf profile`, and their amd64 results are actively misleading as a baseline (§1).
- `etl/scripts/cq-validate.py` targets Fuseki for data-quality validation on a different cadence and is **left untouched**. `spike/cq-queries.json` remains in place as its input. The `perf/corpus/cq/*.rq` files are generated from it once by `pgperf manifest --import spike/cq-queries.json`, after which `perf/corpus/` is the source of truth for performance work. The two corpora are permitted to drift; a divergence check is explicitly not built, because the queries serve different purposes and forcing them to stay identical would couple two independent lifecycles.

### 4.4 What a fingerprint can and cannot gate

A fingerprint is evidence about **one index**. It is not evidence about the *data*, and conflating the two produces a gate that fires on every successful rebuild.

`qlever-rebuild-index.sh` promotes if and only if the index content hash differs from `latest`; identical content sets `STATUS="unchanged"` and exits without promoting (lines 181–183). So a promotion, by construction, means the data changed — new packages, new CVEs, refreshed enrichment. Deterministic queries over changed data return different results, correctly. A gate comparing a post-promotion fingerprint against a pre-promotion baseline would therefore reject essentially every real promotion, which is worse than no gate: it trains the operator to ignore it.

Fingerprints are consequently scoped to the index they were captured against. Every fingerprint is stored with the `index_hash` it came from, and `pgperf compare` selects its mode from whether the two runs share one:

| Mode | Condition | Result gating |
|---|---|---|
| `same-index` | baseline and run share `index_hash` | Full. Fingerprint mismatch is exit 2. |
| `cross-index` | `index_hash` differs — a real promotion | Fingerprints are not compared. §4.6 applies instead. |

`same-index` is not a degenerate case; it is how you gate everything that is *not* a data change. A QLever version bump, a change to `-j`/`-m`/`-c`/`-k`, a kernel or filesystem change, a host migration, or latent non-determinism all reproduce against a fixed index, and against a fixed index a fingerprint mismatch is unambiguous evidence of a correctness regression. Any change to the serving stack should be validated this way *before* it meets a new index.

`cross-index` is the nightly-promotion case and is the one that needs §4.6.

### 4.5 Gating a data-changing promotion

Across a promotion, the question is not "are the results identical" but "does every query still answer, and does it answer with a plausible amount of data". Three checks, none of which require an authoritative reference dataset:

1. **Still answers.** The query returns a non-error result. A query that newly errors or times out is a regression regardless of what the data did (§9.3).
2. **Absolute bounds.** Row count within the manifest's `expect.min_rows`/`max_rows` (§4.2). These are wide, hand-set sanity rails — a query that should never return zero returning zero is caught here even when the exact set legitimately changed.
3. **Relative drift.** Row count change against the baseline within a per-query tolerance, `max_row_drift_pct`, defaulting to 25%. This mirrors the promotion pipeline's existing `MAX_LOSS_PCT=25` but per query rather than over the whole corpus — and that is the point. The global triple-count gate cannot see a rebuild where the total is healthy because one collector grew while another silently produced nothing; a per-query drift check over 65 queries spanning package management, licensing, security, provenance, and five ecosystems can. This is the single highest-value thing this gate adds over what already exists.

Queries whose result size legitimately moves fast (recent-CVE windows, "packages updated in the last 30 days") set a wider `max_row_drift_pct` or `null` to opt out. Setting it per query rather than globally is what keeps the tolerance tight where it can be tight.

Row *count* rather than row *content* is deliberate: content comparison across a data change requires knowing what the data should have become, which is the ETL pipeline's job and not something this harness can independently establish.

### 4.6 Determinism is measured

Independent of §4.4's scoping, a fingerprint is only usable if it is stable *within* a fixed index. 39 of the 65 CQs use `ORDER BY` and 28 use `LIMIT`. Where the sort key has ties, `ORDER BY … LIMIT n` returns a genuinely different *set* between executions — not merely a different order, which canonicalization would absorb. Gating those queries produces a flapping gate, and a flapping gate is an ignored gate.

`pgperf manifest` therefore executes each query three times in immediate succession against one index and compares fingerprints:

- All three identical → `deterministic: true`, fingerprint recorded with its `index_hash`, eligible for `same-index` result gating.
- Any difference → `deterministic: false`, `fingerprint: null`, a `nondeterminism_note` recorded. The query is still timed and still subject to §4.5's checks; it never gates on results.

Three executions is a heuristic, not a proof — a query with a rare tie may pass and later flap. In `same-index` mode `pgperf compare` treats a first-ever fingerprint mismatch as a finding to triage, and `pgperf manifest --recheck <id>` demotes it to non-deterministic if that is what triage concludes. The failure mode is a false positive requiring human judgment, which is the correct direction for a safety gate.

### 4.7 Adversarial set

Authored in this spec because the manifest schema and container must accommodate it, but **exercised by spec 2**. It covers what a public unauthenticated endpoint will eventually receive and what the CQ corpus does not:

- Unbounded property paths (`+`/`*`) over high-fan-out predicates such as `pkg:directlyDependsOn`.
- Cartesian and near-cartesian joins.
- `ORDER BY` over large unbounded result sets.
- Aggregates with `DISTINCT` over the full graph.
- `FILTER(CONTAINS(LCASE(…)))` scans — `SPIKE-RESULTS.md` measured 243ms for one on amd64 against a Lucene-backed ~5ms, and QLever has no text index configured here.
- Result sets large enough to pressure `-m 4G` and to exceed `-e 500M` (the single-entry cache ceiling), which changes cache behavior as well as memory.
- Queries engineered to land between nginx's 60s and QLever's 300s — the case where the client sees a 504, the connection is gone, and QLever keeps a thread of four busy for up to four more minutes.

Each carries `class: "adversarial"` and `deterministic: false`. Selection is by class, not by `enabled`: the profiler runs only `enabled` queries whose class is not `adversarial` unless `--include-adversarial` is passed. Keeping `enabled` orthogonal to class means retiring a query and excluding a class remain independent operations, and an adversarial query can be retired the same way any other is.

## 5. The client

### 5.1 Target modes

| Mode | Endpoint | Path exercised |
|---|---|---|
| `public` | `https://packagegraph.di.riseproject.dev` | TLS 1.2/1.3 over **HTTP/1.1** (see below), gzip over `application/sparql-results+json`, `limit_req rate=50r/s burst=100 nodelay`, `proxy_read_timeout 60s`, `proxy_buffering on` with 8×16k buffers, upstream keepalive 32 |
| `direct` | `127.0.0.1:7001` via SSH local-forward to the host | `qlever-server` alone |

Both are first-class and every run records which was used. The difference between them is itself a measurement: proxy and TLS overhead, gzip cost against response size, and the 60s cliff. Mixing them in one baseline is a category error, so `pgperf compare` refuses to compare runs with different `target` values.

**The profiler speaks HTTP/1.1, and production speaks HTTP/2.** `nginx.conf` sets `http2 on` for the 443 listener, but the stdlib decision in §3 fixes the client at HTTP/1.1 — `http.client.HTTPConnection._http_vsn_str` is `HTTP/1.1` and there is no HTTP/2 implementation in the standard library. This is accepted rather than fixed: adding an HTTP/2 client means adding a third-party dependency and abandoning the stdlib-only constraint, for a difference that is small and *constant* in the serial, one-request-in-flight regime this spec measures. HTTP/2's advantages — multiplexing, header compression across concurrent streams — are properties of concurrency, which this spec explicitly does not exercise.

Two consequences must be recorded rather than forgotten. `public` runs slightly overstate per-request overhead versus a real HTTP/2 client, so the `public`-minus-`direct` delta is an upper bound on proxy cost. And spec 2's k6 *does* negotiate HTTP/2, so its numbers are not directly comparable to this spec's `public` baselines. The protocol version is written into the run manifest for exactly this reason, and `compare` treats it as part of comparability.

`cache-cold` (§6) requires the QLever access token, which the proxy rejects by design (`if ($arg_access_token) { return 403; }`), so that cache level implies `direct`.

The endpoint host and the SSH host are configuration, defaulted from env (`PGPERF_PUBLIC_URL`, `PGPERF_SSH_HOST`) in the established `os.environ.get(…, default)` idiom, so the harness is not hardcoded to one deployment.

### 5.2 Fingerprints

`canonicalize_bindings()` is lifted from `spike/profile-cqs.py` unchanged in behavior: each binding row becomes a tuple of `(var, type, value, datatype, xml:lang)` sorted by variable, and the row list is sorted. The result is serialized deterministically and hashed with `sha256`.

Storing a hash rather than the bindings keeps run artifacts small and makes baselines committable. Full bindings are retained only in memory during a run, and only for the queries being fingerprinted.

### 5.3 Outcome taxonomy

`spike/profile-cqs.py` classifies into `OK`, `ERROR`, and `TIMEOUT`. That is insufficient the moment the proxy is in the path, and dangerously insufficient under the load spec 2 will apply, because rate-limited requests are fast and would flatter every percentile they appear in.

| Outcome | Detection |
|---|---|
| `OK` | HTTP 200, parseable SPARQL JSON, no `exception` key |
| `SPARQL_ERROR` | HTTP 200 with an `exception` key, or a 4xx carrying a QLever error body |
| `HTTP_ERROR` | Any other unexpected status from QLever itself |
| `PROXY_TIMEOUT` | 504 from nginx — QLever is still executing; the run notes the orphan |
| `RATE_LIMITED` | 503 **carrying a rate-limit provenance marker**. Without one, a 503 is `HTTP_ERROR` — see below. |
| `CLIENT_TIMEOUT` | Local socket timeout at `timeout_s` |
| `CONNECTION_ERROR` | TCP/TLS failure, connection reset, DNS failure |
| `BANNED` | Connections failing at the TCP layer on both 443 and 22 after earlier success — an nftables ban (§5.4) |
| `FINGERPRINT_MISMATCH` | 200 and well-formed, but the canonical hash differs from the manifest on a `deterministic: true` query |
| `CARDINALITY_VIOLATION` | 200 and well-formed, but row count falls outside `expect` |

`FINGERPRINT_MISMATCH` and `CARDINALITY_VIOLATION` are *correctness* outcomes layered on top of a successful request; a sample carries both a transport outcome and, where applicable, a correctness outcome, rather than collapsing them. Timing from a correctness-failing sample is retained but excluded from latency aggregates by default, since a query returning the wrong number of rows is not measuring the same work.

`BANNED` is a tenth outcome (§5.4): connections failing at the TCP layer on both 443 and 22 after earlier success is the signature of an nftables ban rather than an ordinary connection failure. It is separated from `CONNECTION_ERROR` so a banned run is discarded as a measurement rather than contributing a wall of fast failures to the latency distribution.

**A bare 503 is not attributable.** nginx returns 503 for `limit_req`, but also when every server in an upstream block is marked unavailable — and the current `sparql-proxy/nginx.conf` sets no `limit_req_status` and adds no header distinguishing the two. Both are plausible during exactly the saturation conditions where telling them apart matters most: one means "the proxy shed my load", the other means "QLever is down". Classifying every 503 as `RATE_LIMITED` would let a genuine backend outage be silently discarded as an enforcement artifact.

So the client classifies 503 as `RATE_LIMITED` only when a provenance marker is present, and as `HTTP_ERROR` otherwise, with the ambiguity recorded on the sample. Making the marker exist is a one-line proxy change, offered here as a recommendation rather than assumed as a dependency, since `nginx.conf` belongs to the team maintaining the enforcement layer:

- **Preferred:** keep `limit_req_status 503` and attach a marker — `limit_req_status 503; error_page 503 = @ratelimited; location @ratelimited { add_header X-PG-RateLimit 1 always; return 503; }`. Unambiguous, and stays 5xx.
- **Not recommended without discussion:** `limit_req_status 429`. Semantically the better code, but 429 matches `fail2ban/filter.d/sparql-proxy-abuse.conf`'s `failregex = ^<HOST> -.*"(GET|POST) \S+ HTTP/\d\.\d" 4\d\d`, so rate-limited clients would begin accruing toward a ban. That may even be desirable for real abusers, but it is a change in enforcement behavior and should be a deliberate decision, not a side effect of improving harness observability.

Until a marker exists, saturation findings involving 503s carry an explicit caveat rather than a rate-limit attribution.

### 5.4 The enforcement path as a measurement confounder

`deploy/quadlet/firewall/nftables.conf` and `deploy/quadlet/fail2ban/` add a packet filter and an abuse jail in front of the proxy. The harness models this **only** to keep its measurements honest, not to protect itself: operations holds root on the host and can rebuild the instance from Terraform, so a ban is an inconvenience for the team that owns it, not a risk this spec designs around.

What matters here is that three separate mechanisms can turn a request into a non-answer, and conflating them corrupts the results:

| Mechanism | Response | Layer |
|---|---|---|
| nginx `limit_req` (`rate=50r/s burst=100`) | 503 | application |
| nftables new-connection limits (200/s on 80/443) | packet drop | kernel, new connections only — `ct state established,related accept` precedes them, so a keepalive or HTTP/2 client is unaffected |
| fail2ban `[sparql-proxy-abuse]` (`maxretry=20`, `findtime=1m`, any 4xx) | all traffic dropped | kernel, via `banned_ips` |

A run that silently averages 503s, dropped SYNs, and real query latency into one percentile is reporting a number about the enforcement stack, not about QLever. So:

- **`RATE_LIMITED` and `BANNED` are distinct outcomes** (§5.3), never folded into `CONNECTION_ERROR` or into latency aggregates.
- **The run manifest records the enforcement config** — hashes of the live `nftables.conf` and `jail.local`, plus `fail2ban-client status` (§7.2). These thresholds now influence observed error rates as directly as `-j` or `-c` influence latency, so a change to them must be visible in a run diff.
- **The run manifest records ban state** before and after each pass (`nft list set inet filter banned_ips`). If the client IP appears, the run is marked `banned: true` and discarded as a measurement — the point is not to avoid the ban but to avoid publishing a p99 computed from dropped packets.
- **One multiplexed SSH connection** (`ControlMaster` / `ControlPersist`) serves telemetry and the `direct` tunnel. This is correct regardless of rate limits: `ice-cold` restarts the service before every repetition (§6), and reconnecting each time would add connection-setup cost to a measurement about query latency.

Adversarial queries (§4.7) default to `direct`, which bypasses nginx and the jail entirely — again for measurement cleanliness, since the point of those queries is to observe QLever's behavior under a hostile query, not nginx's behavior under a 4xx burst.

## 6. Cache levels

`-c 1G -e 500M -k 200` means cache state is the dominant source of variance for this corpus. Three levels, each recorded on the run, never silently mixed:

| Level | Action before the measured pass | Clears | Requires |
|---|---|---|---|
| `warm` | One warming pass over the corpus, discarded, before the measured pass begins | nothing | — |
| `cache-cold` | QLever's cache-clear admin endpoint, before **each individual query execution** | query cache | access token, therefore `direct` |
| `ice-cold` | `systemctl restart qlever.service`, then `sync && echo 3 > /proc/sys/vm/drop_caches` over SSH, before **each individual query execution** | query cache, process state, page cache | root SSH |

**Scheduling unit.** The clearing action for `cache-cold` and `ice-cold` runs before *every individual query sample* — that is, the loop is `for query: for repetition: clear; execute`, not `clear; for query: for repetition: execute`. Clearing once per pass would yield one cold sample and N−1 warm ones averaged together, which is the exact confusion these levels exist to prevent. `warm` is the opposite by design: the cache is primed once by the discarded pass and every subsequent sample benefits, because steady state is what it measures.

`sync` before `drop_caches` is not optional. `drop_caches` frees only *clean* pages; without a preceding `sync`, dirty pages stay resident and the "cold" condition is neither achieved nor reproducible between runs.

This makes `ice-cold` expensive — every sample pays a service restart plus a readiness wait — so its default repetition count is 3 rather than 5, and the profiler reports the pass's total wall time so the cost is visible up front.

`warm` explicitly *performs* the warming pass rather than discarding early samples, and the discarded pass is recorded in the run artifacts. Quietly dropping the first N samples is the standard way benchmarks become unreproducible.

`ice-cold` is the level that answers the operationally interesting question: what does the first user after an 03:30 promotion actually experience, when nothing is in the query cache and the index files are not in page cache. `qlever-refresh-if-changed.sh` already restarts the service and then polls readiness up to 30 times at 10-second intervals, so a restart is a routine operation on this host, not an intervention invented by the harness. `drop_caches` is additional, is host-wide, and is gated behind an explicit `--cache-level ice-cold` — it is never the default.

After each `ice-cold` restart the profiler waits for readiness using the same query `qlever-refresh-if-changed.sh` uses (`SELECT * WHERE { ?s ?p ?o } LIMIT 1`), and records each observed readiness delay. Time from restart to first successful query is itself worth tracking across rebuilds — it is what the promotion pipeline's 30×10s poll is budgeted against — so these are reported as their own distribution alongside query latency, not folded into it.

## 7. Run identity

Every run emits an immutable manifest. Its purpose is that a number is attributable: when a p95 moves, the manifest must say what else changed.

### 7.1 Client side

Harness git revision and dirty status; `sha256` of `corpus/manifest.json`; the full argv; target mode and resolved endpoint; cache level; repetition count; client kernel, CPU, and Python version; start and end timestamps.

### 7.2 Server side, over SSH

| Field | Source | Why |
|---|---|---|
| Index identity | `/var/lib/packagegraph/qlever-data/index/.loaded` | The promoted `CONTENT_HASH`. Attributes a regression to one specific rebuild. |
| Server flags | `podman inspect qlever` | Captures the literal `Exec=` line, so a change to `-j`/`-m`/`-c`/`-e`/`-k`/`-s` cannot go unrecorded. |
| Image reference | `podman inspect qlever` | Pinned to `docker.io/adfreiburg/qlever:commit-1075455fae` today; a QLever upgrade must be visible in the diff. |
| Container limits | `podman inspect qlever` | `Memory=6g`, `--cpus=4`. |
| Index size on disk | `du -sb` on the index dir | Feeds the index-growth check. |
| Proxy config hash | `sha256` of the host's live `nginx.conf` | The checked-in copy has already drifted from the deployed one (§11). Hashing what is actually running, not what is in git, is the only reliable option. |
| Host facts | `uname -a`, `lscpu`, `free -m`, `xfs_info` on the data disk | aarch64, RHEL 10.2, xfs on `/dev/sdb`. |
| Unit state | `systemctl show` for `qlever`, `qlever-index-load`, `sparql-proxy` | Detects a run started against a partially-restarted stack. |
| Concurrent activity | `systemctl is-active qlever-rebuild-index.service` | A rebuild is allotted `Memory=8g` and `--cpus=4` on a host whose server has 4. Any run overlapping one is contaminated and must be marked, not silently averaged in. |
| Ban state | `nft list set inet filter banned_ips` | §5.4. Checked before and after every pass; the client IP appearing aborts the run. |
| Enforcement config | `sha256` of the live `nftables.conf` and `jail.local`, plus `fail2ban-client status` | The 4xx thresholds and rate limits are now part of what determines observed latency and error rates, so a change to them must show up in a run diff exactly as a change to `-j` or `-c` would. |

The rebuild-overlap check runs both before and after the measured pass; a run that overlapped a rebuild is flagged `contaminated: true` and `pgperf compare` refuses to use it as a baseline.

## 8. Measurement

### 8.1 Timing

Each selected query runs `--repetitions N` times at the selected cache level, defaulting to 5 for `warm` and `cache-cold` and 3 for `ice-cold` (§6). Every individual sample is retained — raw samples, not only aggregates. Per query: min, median, p95, p99, max, mean, standard deviation, coefficient of variation, and outcome counts by category. At these sample sizes p95 and p99 are order statistics over very few points and are reported with their sample size so they are not over-read; the coefficient of variation is the more honest stability signal at that N and is what §9's gate uses to suppress noise. Baseline runs intended for gating should use a higher `--repetitions` than exploratory ones, and `compare` records both runs' N in its output so a comparison across differing sample sizes is visible rather than implicit.

Aggregates are reported per query, per class, and overall. Per-query is primary: the point of the exercise is finding the queries that are unusable, and any blended figure hides exactly those.

### 8.2 Client time versus server time

Three time sources per sample, all retained:

1. **Wall time**, `time.monotonic()` around the request: connection setup, TLS, queuing, planning, execution, serialization, gzip, and transfer.
2. **`meta.query-time-ms`** from QLever's own JSON response: execution as the engine accounts for it.
3. **`QueryEventLog`**, harvested from `/data` after the run: QLever's own per-query event stream. **Run-level evidence by default** — see §8.2.1 for why per-sample attribution is conditional.

The gap between (1) and (2) is where proxy, serialization, and transfer costs live, and separating them is what distinguishes "the engine got slower" from "the response got bigger" or "nginx started buffering to disk". Response size in bytes is recorded alongside, both compressed and uncompressed on `public` runs, because `gzip_comp_level 5` over large JSON is real CPU on a 4-core box.

#### 8.2.1 Joining the event log is conditional, not assumed

"Joined by query" is not a specification. With 5 repetitions of an identical query string, query text is not a key, and nothing in this spec has yet established that the log carries one. Promising per-sample attribution on an unverified schema would produce confidently mis-attributed server-side timings — worse than having none, because they would look authoritative.

Attribution is therefore staged, and the stage in force is recorded on the run:

| Stage | Condition | What is claimed |
|---|---|---|
| `per-sample` | The log exposes a stable per-query correlation key that the client can also observe or derive | Each sample joined to its event. Full attribution. |
| `ordinal` | No correlation key, but events are ordered and countable per query text | Events matched to samples by (query, execution ordinal) within the run window. Valid only for a serial profiler — which this is — and explicitly void for spec 2. |
| `run-level` | Neither holds, or the schema is unrecognized | Events retained as unjoined run evidence. No per-sample claim. |

The implementation plan must read the log's actual on-disk format in `docker.io/adfreiburg/qlever:commit-1075455fae` and record which stage applies before any code depends on it. `run-level` is the default until that verification happens, and the degradation path is unchanged: an unreadable or unrecognized log records `qlever_events: {"available": false, "reason": …}` and the run continues on sources (1) and (2).

Both `per-sample` and `ordinal` need a clock-skew policy, since the log is written on the host and the samples are timed on the workstation. At run start and again at run end the profiler records the offset between the host's clock and its own (`ssh <host> date +%s.%N` against a local reading, halving the round-trip), stores both offsets and the drift between them in the run manifest, and widens the join window by the observed drift. If drift exceeds the window, attribution downgrades to `run-level` for that run rather than producing joins that cannot be trusted.

### 8.3 Host telemetry

A 1 Hz sampler over a persistent SSH connection, running for the duration of the measured pass:

- `/proc/pressure/{cpu,io,memory}` — PSI. cgroup v2 is confirmed on this host, and `some avg10` is the most direct saturation signal available. It answers "is anything actually waiting" without the interpretation problems of `%util` on a device with a queue.
- `/proc/stat`, `/proc/meminfo` — utilization, iowait, page cache size, available memory.
- `/proc/diskstats` for `sdb` — read/write throughput and IO time against the index disk.
- `podman stats --no-stream --format json` — per-container CPU and RSS for `qlever` and `sparql-proxy`, so the 6G ceiling and the 4-CPU allocation are visible as they are approached.

Samples carry monotonic timestamps aligned to the run clock so telemetry can be joined to individual query samples. Sampling failure degrades the run to `telemetry: {"available": false, …}`; it never aborts a measurement.

## 9. Results and gating

### 9.1 Artifacts

Written to `output/perf/<run-id>/`, where `<run-id>` is `<target>-<cache-level>-<UTC timestamp>`. `output/` is already gitignored.

| File | Contents |
|---|---|
| `manifest.json` | §7, client and server |
| `samples.json` | Every individual sample: query id, repetition, wall ms, engine ms, rows, bytes, transport outcome, correctness outcome |
| `summary.json` | An `identity` block plus per-query, per-class, and overall aggregates. The comparison input, and self-contained (below). |
| `telemetry.json` | §8.3 time series |
| `qlever-events.jsonl` | Harvested `QueryEventLog`, when available |
| `summary.md` | Human-readable digest, in the shape of `docs/reports/` writeups |

`summary.json` embeds an `identity` block so it stands alone as a comparison operand. `manifest.json` remains the exhaustive record; `identity` is the subset `compare` needs, copied in at write time:

```json
{
  "identity": {
    "run_id": "public-warm-20260909T142211Z",
    "target": "public",
    "cache_level": "warm",
    "repetitions": 5,
    "corpus_hash": "sha256:…",
    "index_hash": "a1b2c3d4e5f60718",
    "index_size_bytes": 41203847612,
    "qlever_image": "docker.io/adfreiburg/qlever:commit-1075455fae",
    "qlever_exec": "/qlever/qlever-server -i … -j 4 -m 4G -c 1G -e 500M -k 200 -s 300s",
    "contaminated": false,
    "contamination_reasons": []
  },
  "queries": { }, "classes": { }, "overall": { }
}
```

Duplicating these fields is deliberate. A committed baseline is one file in git with no sibling run directory, so any field `compare` needs must live inside it — and pinning `index_hash`, `qlever_image`, and `qlever_exec` into the baseline is what lets a diff say *what changed* rather than only *that something did*.

### 9.2 Baselines

Committed under `perf/baselines/<target>-<date>.json` — the `summary.json` of a run designated as reference, one per target mode. Committing them makes "what did this look like before" answerable from a clone, which the `output/` directory does not provide. Because `identity` travels with the file, a baseline remains interpretable years later without the run that produced it.

### 9.3 `pgperf compare`

```
pgperf compare perf/baselines/public-2026-09-09.json output/perf/<run-id>/summary.json
```

Both operands are `summary.json` files and each is self-contained (§9.1): the `identity` block carries everything the comparison needs, so `compare` never has to reach into a sibling `manifest.json` or a run directory that a committed baseline does not have.

`compare` first establishes comparability, refusing to proceed if `target`, `cache_level`, or `corpus_hash` differ, or if either side is `contaminated`. It then selects `same-index` or `cross-index` mode by comparing `index_hash` (§4.4), and reports and gates on:

| Condition | Mode | Exit | Rationale |
|---|---|---|---|
| `FINGERPRINT_MISMATCH` on any `deterministic: true` query | `same-index` only | 2 | Correctness against a fixed index — unambiguous evidence of a serving-stack regression. Not evaluated across a promotion, where changed results are expected (§4.4). |
| `CARDINALITY_VIOLATION` — row count outside `expect` | both | 2 | Absolute sanity rails. |
| Row-count drift beyond `max_row_drift_pct` | `cross-index` | 2 | The per-query analogue of the pipeline's global `MAX_LOSS_PCT` (§4.5); catches one collector silently emptying while the corpus total stays healthy. |
| Query newly returns `SPARQL_ERROR`, `HTTP_ERROR`, `PROXY_TIMEOUT`, or `CLIENT_TIMEOUT` after previously succeeding | both | 2 | A query that stopped answering. `PROXY_TIMEOUT` is included deliberately: it means the query crossed nginx's 60s ceiling, which is a user-visible failure whatever QLever is still doing behind it. |
| Query newly returns `CONNECTION_ERROR` | both | 2 or contaminated | Routed by evidence, not assumption. If the run manifest's server-side unit state shows `qlever.service` restarted or changed `ActiveEnterTimestamp` mid-run, this is a crash and exits 2. Otherwise it is treated as client-side or network noise and marks the run contaminated. |
| Any `RATE_LIMITED` or `BANNED` sample | both | contaminated | Enforcement-layer artifacts, not properties of the server (§5.4). These never gate; they invalidate. |
| Per-query p95 regression beyond threshold (default 25%) | both | 1 | Suppressed when the baseline's coefficient of variation exceeds 0.3, so noisy queries do not generate standing false alarms. |
| Per-class p95 regression beyond threshold (default 15%) | both | 1 | Class aggregates are steadier than individual queries and catch broad regressions individual thresholds would miss. |
| Index size growth beyond threshold (default 20%) | both | 1 | Directly relevant on a 512G disk holding both the index and rebuild scratch. |

Exit 2 is correctness, exit 1 is performance, exit 0 is clean, and a contaminated verdict is exit 3 — distinct from all three, because "this run cannot be trusted" is not "this run passed" and must never be read as one. Separating them lets an operator wire correctness into a promotion gate immediately while treating latency as advisory until enough baselines exist to trust the thresholds.

The general rule behind the table: **every outcome that is not `OK` is a gate failure unless it is attributable to the enforcement layer or to the client.** New non-answer outcomes are enumerated explicitly rather than defaulted, so adding a tenth outcome to §5.3 forces a decision here rather than silently landing in the pass bucket.

### 9.4 Relationship to the promotion pipeline

This spec **does not** modify `qlever-rebuild-index.sh` or `qlever-refresh-if-changed.sh`. Wiring `pgperf compare` into promotion — as a post-restart gate that reverts the `latest` pointer on exit 2, closing the no-rollback gap in §1 — is a deliberate follow-on, taken only once the gate has demonstrated a low false-positive rate against real rebuilds. Shipping an unproven gate into an automated promotion path would trade a known failure mode for an unknown one.

Note also that `qlever-rebuild-index.timer` is **not currently enabled** on the host; rebuilds are manual. In the interim the gate is run manually after a rebuild, which is also the fastest way to accumulate the baseline history the thresholds need.

## 10. Testing

The harness is measurement code, so its own tests are about correctness of classification and aggregation, not about performance:

- `canonicalize_bindings` and hashing: order independence for permuted bindings, and inequality across differing `datatype`, `xml:lang`, and `type` — the distinctions §5.2 exists to preserve.
- Outcome classification: table-driven over synthetic responses covering every row of §5.3. Specifically: a 504 classifies as `PROXY_TIMEOUT`; a 503 **with** a rate-limit marker as `RATE_LIMITED`; a 503 **without** one as `HTTP_ERROR` with the ambiguity flagged. That last case is the regression test for the attribution bug in §5.3 — a bare 503 must never be silently absorbed as an enforcement artifact.
- **Gate mode selection** (§4.4): identical `index_hash` selects `same-index` and evaluates fingerprints; differing `index_hash` selects `cross-index` and does not. A fixture where results changed across a promotion must exit 0, since that is the false-positive this design exists to prevent.
- **Row-drift gating** (§4.5): drift inside `max_row_drift_pct` passes; outside it exits 2; `max_row_drift_pct: null` opts out entirely. Include a fixture where the corpus-wide triple count is healthy but one query collapses, since catching that is the gate's main value over the existing volumetric checks.
- Percentile and coefficient-of-variation computation against known inputs.
- Manifest validation: unknown `class`, missing `file`, `deterministic: true` with a null `fingerprint`, a `fingerprint` lacking its `index_hash`, and `min_rows > max_rows` are all rejected at load.
- `compare` gating: fixture pairs producing each of exit 0/1/2/3, including coefficient-of-variation suppression, the `CONNECTION_ERROR` routing that depends on server-side unit state, and refusal on mismatched `target`, `cache_level`, or `corpus_hash`.
- **Self-containment** (§9.1): `compare` must run against two `summary.json` files with no run directory and no `manifest.json` present — the committed-baseline case. A test that passes only because a sibling file happened to exist would hide exactly the defect this schema change fixes.
- Degradation paths: SSH unavailable, `QueryEventLog` absent or malformed, telemetry sampler failing mid-run, and clock drift exceeding the join window (§8.2.1). Each must produce a complete run with an `available: false` marker or a downgraded attribution stage, rather than a crash or, worse, a silently incomplete manifest.

Tests run against fixtures with no live endpoint. Whether these run in CI is deferred: `.github/workflows/ci.yml` has no Python job today, and adding one is a small but real new convention. The tests are written to be CI-ready regardless.

## 11. Incidental fixes

Two hygiene issues surfaced during design that this work depends on and should correct:

1. **`deploy/quadlet/sparql-proxy/nginx.conf` still says `sparql.example.com`** — both `server_name` directives (lines 40, 55) and both certificate paths (lines 57, 58) — while the deployed host serves `packagegraph.di.riseproject.dev`. Its own header comment compounds this by saying "both occurrences below" when there are four; anyone following that instruction literally produces an nginx that refuses to start, since the certificate paths would still point at a directory that does not exist. §7.2 hashes the live config rather than the checked-in one specifically because of this drift, but the drift itself should be fixed rather than designed around.
2. **`docs/QLEVER-HOST-OPERATIONS.md` is untracked.** It holds the host address, the architecture diagram, the IAM application and project identifiers, and the troubleshooting table — and will not survive a clean clone. It should be committed, with any credential material confirmed absent first.

## 12. Sequencing

1. Corpus migration and manifest generation, including determinism measurement (§4). Produces `perf/corpus/`.
2. Client, fingerprints, and outcome taxonomy (§5), with tests.
3. Serial profiler with cache levels and repetitions (§6, §8.1, §8.2).
4. Run manifest, client and server side (§7).
5. Telemetry sampler and `QueryEventLog` harvest (§8.3).
6. `compare` and gating (§9), plus the first committed baselines for both target modes.
7. Container and Makefile wiring (§3).

Steps 1–3 alone produce a usable per-query cost ranking on real hardware, which is the first thing missing today. Steps 4–6 make it a gate.

## 13. Risks and open points

- **`QueryEventLog` format is unverified.** Its existence is established by the `User=999` fix in `docs/QLEVER-HOST-OPERATIONS.md`, but its schema in `adfreiburg/qlever:commit-1075455fae` has not been read. Step 5 must confirm it before the join is relied on; §8.2's degradation path exists so this cannot block the rest.
- **QLever's cache-clear admin endpoint is assumed, not confirmed.** If the pinned build does not expose one, `cache-cold` collapses into `ice-cold` and the middle level is dropped. This does not affect §9's gate, which runs `warm`.
- **Determinism sampling can be wrong** (§4.6). Accepted, with an explicit triage path, because the alternative — gating nothing on results — forfeits the only correctness signal available.
- **`max_row_drift_pct` defaults are guesses** (§4.5). 25% is borrowed from the pipeline's `MAX_LOSS_PCT` and has no empirical basis per query. The first several promotions should be run with the gate advisory, its findings triaged, and the per-query tolerances tuned from observed drift before it blocks anything. A drift gate calibrated from one run's data is a gate calibrated from noise.
- **`cross-index` mode cannot detect a wrong-but-plausible result.** If a rebuild changes results *incorrectly* while keeping row counts within tolerance, nothing here catches it. Establishing that would require an authoritative reference for what the data should have become, which belongs to the ETL pipeline, not to a performance harness. The honest scope of this gate is "still answers, plausibly sized, no slower" — not "correct".
- **Server-side attribution may never reach `per-sample`** (§8.2.1). If the pinned image's `QueryEventLog` carries no correlation key, the profiler falls back to ordinal matching, which is sound only because this spec is strictly serial. Spec 2 gets no such fallback and will need a real key or no per-query server-side timing at all.
- **`public` measurements include internet path variance.** Runs from a workstation over the public internet carry RTT and jitter that have nothing to do with the server. This is why `direct` exists and why baselines are per-target; it also means `public` p99 should not be read as a server property.
- **The enforcement layer is new and still moving.** `nftables.conf`, `jail.local`, and the fail2ban units were untracked and being actively edited while this spec was written — `jail.local` changed from a journald backend to file polling against `/var/log/sparql-proxy/access.log` mid-design, and the team owns further changes. §5.4 rests on the jail's *thresholds* and on which layer produces which response code, both of which held across that change, but the implementation plan must re-read these files rather than trusting this spec's transcription. Since the run manifest hashes the live config (§7.2), drift shows up in run diffs rather than silently invalidating baselines.
- **Nothing here measures the endpoint under concurrency**, which is where the `rate=50r/s`, 60s-versus-300s, and 4-thread interactions actually bite. That is the entire point of spec 2, and no capacity claim should be made from this spec's output alone. Spec 2 inherits §5.4's distinction between enforcement responses and real latency as a correctness requirement for its own reporting — a saturation curve built partly on 503s and dropped SYNs describes the wrong system.
