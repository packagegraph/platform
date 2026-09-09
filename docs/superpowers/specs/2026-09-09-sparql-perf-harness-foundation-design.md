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
      "fingerprint": "sha256:…",
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
| `expect.min_rows` / `max_rows` | Cardinality bounds. A query dropping to 0 rows is the signature of a partial index promotion and must be caught even where the fingerprint is not gated. |
| `fingerprint` | `sha256` over the canonicalized bindings (§5.2). `null` when `deterministic` is false. |
| `deterministic` | Measured, not asserted. See §4.4. |
| `timeout_s` | Per-query client timeout. Defaults to 300 to match `qlever-server -s 300s`; `public` runs additionally see nginx's 60s ceiling, which is recorded as a distinct outcome rather than a timeout (§5.3). |
| `weight` | Relative frequency in a mixed load profile. **Unused by this spec**; present so spec 2's k6 workload consumes this same file rather than inventing a parallel corpus. |
| `enabled` | Allows retiring a query without deleting it and losing its history. |

The corpus at authoring time is heavy on inverse paths (42 of 65), `ORDER BY` (39), `DISTINCT` (24), `COUNT` (22), and `GROUP BY` (19), and light on property paths (3), named-graph scoping (3), and `UNION` (2). It contains no `SERVICE`, consistent with federation not being part of the service. The adversarial set (§4.5) exists to cover what the CQs do not.

### 4.3 Migration and the existing consumers

`spike/cq-queries.json` has two consumers: `spike/profile-cqs.py` and, in the same shape, `etl/scripts/cq-validate.py`. Neither is broken by this work:

- `spike/` is spike material that already served its purpose (`SPIKE-RESULTS.md`, recommendation GO, migration complete). `spike/profile-cqs.py` and `spike/cq-profile-results.json` are superseded by `perf/` and are **deleted** as part of this change; their behavior is subsumed by `pgperf profile`, and their amd64 results are actively misleading as a baseline (§1).
- `etl/scripts/cq-validate.py` targets Fuseki for data-quality validation on a different cadence and is **left untouched**. `spike/cq-queries.json` remains in place as its input. The `perf/corpus/cq/*.rq` files are generated from it once by `pgperf manifest --import spike/cq-queries.json`, after which `perf/corpus/` is the source of truth for performance work. The two corpora are permitted to drift; a divergence check is explicitly not built, because the queries serve different purposes and forcing them to stay identical would couple two independent lifecycles.

### 4.4 Determinism is measured

Gating on result fingerprints only works if the fingerprints are stable. 39 of the 65 CQs use `ORDER BY` and 28 use `LIMIT`. Where the sort key has ties, `ORDER BY … LIMIT n` returns a genuinely different *set* between runs — not merely a different order, which canonicalization would absorb. Gating those queries produces a flapping gate, and a flapping gate is an ignored gate.

`pgperf manifest` therefore executes each query three times in immediate succession and compares fingerprints:

- All three identical → `deterministic: true`, fingerprint recorded, gated on results.
- Any difference → `deterministic: false`, `fingerprint: null`, a `nondeterminism_note` recorded. The query is still timed and still cardinality-checked; it never gates on results.

Three runs is a heuristic, not a proof — a query with a rare tie may pass and later flap. `pgperf compare` therefore treats a first-ever fingerprint mismatch on a `deterministic: true` query as a finding to triage, and `pgperf manifest --recheck <id>` demotes it to non-deterministic if that is what triage concludes. The failure mode is a false positive requiring human judgment, which is the correct direction for a safety gate.

### 4.5 Adversarial set

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
| `public` | `https://packagegraph.di.riseproject.dev` | TLS 1.2/1.3, HTTP/2, gzip over `application/sparql-results+json`, `limit_req rate=50r/s burst=100 nodelay`, `proxy_read_timeout 60s`, `proxy_buffering on` with 8×16k buffers, upstream keepalive 32 |
| `direct` | `127.0.0.1:7001` via SSH local-forward to the host | `qlever-server` alone |

Both are first-class and every run records which was used. The difference between them is itself a measurement: proxy and TLS overhead, gzip cost against response size, and the 60s cliff. Mixing them in one baseline is a category error, so `pgperf compare` refuses to compare runs with different `target` values.

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
| `RATE_LIMITED` | 503 from `limit_req` |
| `CLIENT_TIMEOUT` | Local socket timeout at `timeout_s` |
| `CONNECTION_ERROR` | TCP/TLS failure, connection reset, DNS failure |
| `BANNED` | Connections failing at the TCP layer on both 443 and 22 after earlier success — an nftables ban (§5.4) |
| `FINGERPRINT_MISMATCH` | 200 and well-formed, but the canonical hash differs from the manifest on a `deterministic: true` query |
| `CARDINALITY_VIOLATION` | 200 and well-formed, but row count falls outside `expect` |

`FINGERPRINT_MISMATCH` and `CARDINALITY_VIOLATION` are *correctness* outcomes layered on top of a successful request; a sample carries both a transport outcome and, where applicable, a correctness outcome, rather than collapsing them. Timing from a correctness-failing sample is retained but excluded from latency aggregates by default, since a query returning the wrong number of rows is not measuring the same work.

`BANNED` is a tenth outcome, added because of §5.4: a connection that times out or is refused at the TCP layer *after* previous requests succeeded, on both 443 and 22, is the signature of an nftables ban rather than an ordinary connection failure. Distinguishing it from `CONNECTION_ERROR` matters because the remedy is completely different and time-critical.

### 5.4 The enforcement path, and not banning yourself

`deploy/quadlet/firewall/nftables.conf` and `deploy/quadlet/fail2ban/` add a packet-filter and abuse-detection layer in front of the proxy. The harness must model it, because the harness is the most likely thing to trip it.

The `input` chain's rule order is decisive:

```
ip saddr @banned_ips drop          # bans win
ct state established,related accept
ip saddr @trusted_ips accept       # exemption from rate limits only
tcp dport 22    ct state new limit rate 60/minute  burst 20
tcp dport {80,443} ct state new limit rate 200/second burst 400
```

Three consequences:

1. **`trusted_ips` does not protect against a ban.** The `banned_ips` drop precedes the trusted accept, and the config says so explicitly: "an explicit ban still wins". Adding the workstation to `TRUSTED_HOSTNAMES` exempts it from the coarse connection-rate pre-filter and nothing more.

2. **A ban costs SSH too.** `banned_ips` drops *all* traffic from the source, port 22 included. So the failure mode is not "the load test stops" — it is "the load test stops and you cannot get in to fix it for an hour", with no `direct` tunnel, no telemetry, and no way to unban short of out-of-band console access. This is the single most consequential operational hazard in the harness.

3. **The rate limits apply to new connections only.** `ct state established,related accept` precedes them, so a keepalive or HTTP/2-multiplexed client is unaffected by the 200/second ceiling; a client that disables connection reuse is not.

The abuse jail is `[sparql-proxy-abuse]`: `maxretry = 20`, `findtime = 1m`, `bantime = 1h`, with `failregex` matching **any 4xx** in the proxy's access log. Note what this does and does not catch:

- nginx's `limit_req` returns **503** by default, which is 5xx and therefore does *not* match. Rate-limiting alone will not ban the harness — a deliberate and correct separation.
- A **400 from QLever on a malformed query does** match. So does the proxy's own `if ($arg_access_token) { return 403; }` defense-in-depth rule. Twenty of either within one minute is a one-hour ban.

The adversarial corpus (§4.5) deliberately produces errors, and any harness bug that sends a token through the public endpoint produces 403s in a tight loop. Both are 20-requests-from-a-ban. The harness therefore:

- **Tracks its own 4xx rate client-side against the jail's own thresholds** and hard-stops the run at 15 4xx responses within any rolling 60-second window, well under `maxretry = 20`. This is a circuit breaker in the client, not a request to the operator to be careful.
- **Refuses to send `access-token` on a `public` target at all**, at the client layer, so the 403 rule is unreachable by construction rather than by discipline.
- **Runs adversarial queries against `direct` only** unless `--allow-adversarial-public` is passed, since `direct` bypasses both nginx and the jail that reads its log.
- **Uses one multiplexed SSH connection** (`ControlMaster` / `ControlPersist`) for telemetry and the `direct` tunnel. `ice-cold` restarts the service before every repetition (§6); reconnecting per restart would open hundreds of new connections against a `60/minute` limit. Multiplexing keeps it at one, which the `established` rule then exempts entirely.
- **Records ban state in the run manifest.** Before and after each pass, `nft list set inet filter banned_ips` over SSH; if the client IP appears, the run is marked `banned: true` and aborts rather than reporting a wall of connection errors as if they were latency.

Adding the workstation to `TRUSTED_HOSTNAMES` in `deploy/quadlet/scripts/trusted-ips.env` is still worthwhile — it removes the connection-rate pre-filter as a confounding variable — but it is explicitly **not** the mitigation for any of the above, and the spec should not be read as suggesting it is.

## 6. Cache levels

`-c 1G -e 500M -k 200` means cache state is the dominant source of variance for this corpus. Three levels, each recorded on the run, never silently mixed:

| Level | Action before the measured pass | Clears | Requires |
|---|---|---|---|
| `warm` | A full warming pass over the corpus, discarded | nothing | — |
| `cache-cold` | QLever's cache-clear admin endpoint, before *every* repetition | query cache | access token, therefore `direct` |
| `ice-cold` | `systemctl restart qlever.service` and `echo 3 > /proc/sys/vm/drop_caches` over SSH, before *every* repetition | query cache, process state, page cache | root SSH |

The granularity matters and differs by level. `warm` measures steady state, so the cache is primed once and every repetition benefits. `cache-cold` and `ice-cold` measure a first-execution cost, so the clearing action repeats before each repetition — clearing once and then running five repetitions would produce one cold sample and four warm ones averaged together, which is the exact confusion these levels exist to prevent. This makes `ice-cold` expensive: each repetition pays a service restart plus the readiness wait, so its default repetition count is 3 rather than 5, and the profiler reports the total wall time of the pass so the cost is visible.

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
3. **`QueryEventLog`**, harvested from `/data` after the run and joined by query: QLever's own per-query event stream.

The gap between (1) and (2) is where proxy, serialization, and transfer costs live, and separating them is what distinguishes "the engine got slower" from "the response got bigger" or "nginx started buffering to disk". Response size in bytes is recorded alongside, both compressed and uncompressed on `public` runs, because `gzip_comp_level 5` over large JSON is real CPU on a 4-core box.

The `QueryEventLog` join is best-effort: if the log is unreadable or the format is not what this spec assumes, the run records `qlever_events: {"available": false, "reason": …}` and continues on sources (1) and (2). The implementation plan must verify the log's actual on-disk format against the pinned image before relying on it.

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
| `summary.json` | Per-query, per-class, and overall aggregates. The comparison input. |
| `telemetry.json` | §8.3 time series |
| `qlever-events.jsonl` | Harvested `QueryEventLog`, when available |
| `summary.md` | Human-readable digest, in the shape of `docs/reports/` writeups |

### 9.2 Baselines

Committed under `perf/baselines/<target>-<date>.json` — the `summary.json` of a run designated as reference, one per target mode. Committing them makes "what did this look like before" answerable from a clone, which the `output/` directory does not provide.

### 9.3 `pgperf compare`

```
pgperf compare perf/baselines/public-2026-09-09.json output/perf/<run-id>/summary.json
```

Refuses to compare across different `target` or `cache_level` values, or against a `contaminated` run. Reports, and gates on:

| Condition | Exit | Rationale |
|---|---|---|
| `FINGERPRINT_MISMATCH` on any `deterministic: true` query | 2 | Correctness. The highest-severity signal available and the one the existing volumetric promotion gates cannot produce. |
| `CARDINALITY_VIOLATION` on any query | 2 | Catches partial index promotion even where results are not gated. |
| New `SPARQL_ERROR` or `CLIENT_TIMEOUT` on a query that previously succeeded | 2 | A query that stopped working. |
| Per-query p95 regression beyond threshold (default 25%) | 1 | Suppressed when the baseline's coefficient of variation exceeds 0.3, so noisy queries do not generate standing false alarms. |
| Per-class p95 regression beyond threshold (default 15%) | 1 | Class aggregates are steadier than individual queries and catch broad regressions individual thresholds would miss. |
| Index size growth beyond threshold (default 20%) | 1 | Directly relevant on a 512G disk holding both the index and rebuild scratch. |

Exit 2 is correctness, exit 1 is performance, exit 0 is clean. Separating them lets an operator wire correctness into a promotion gate immediately while treating latency as advisory until enough baselines exist to trust the thresholds.

### 9.4 Relationship to the promotion pipeline

This spec **does not** modify `qlever-rebuild-index.sh` or `qlever-refresh-if-changed.sh`. Wiring `pgperf compare` into promotion — as a post-restart gate that reverts the `latest` pointer on exit 2, closing the no-rollback gap in §1 — is a deliberate follow-on, taken only once the gate has demonstrated a low false-positive rate against real rebuilds. Shipping an unproven gate into an automated promotion path would trade a known failure mode for an unknown one.

Note also that `qlever-rebuild-index.timer` is **not currently enabled** on the host; rebuilds are manual. In the interim the gate is run manually after a rebuild, which is also the fastest way to accumulate the baseline history the thresholds need.

## 10. Testing

The harness is measurement code, so its own tests are about correctness of classification and aggregation, not about performance:

- `canonicalize_bindings` and hashing: order independence for permuted bindings, and inequality across differing `datatype`, `xml:lang`, and `type` — the distinctions §5.2 exists to preserve.
- Outcome classification: table-driven over synthetic responses covering every row of §5.3, in particular that a 503 from `limit_req` classifies as `RATE_LIMITED` and a 504 from nginx as `PROXY_TIMEOUT`, since collapsing either into a generic error is the specific failure this taxonomy prevents.
- Percentile and coefficient-of-variation computation against known inputs.
- Manifest validation: unknown `class`, missing `file`, `deterministic: true` with a null `fingerprint`, and `min_rows > max_rows` are all rejected at load.
- `compare` gating: fixture pairs producing each exit code, including the coefficient-of-variation suppression and the refusal to compare mismatched targets or a contaminated baseline.
- Degradation paths: SSH unavailable, `QueryEventLog` absent or malformed, telemetry sampler failing mid-run. Each must produce a complete run with an `available: false` marker rather than a crash or, worse, a silently incomplete manifest.

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
- **Determinism sampling can be wrong** (§4.4). Accepted, with an explicit triage path, because the alternative — gating nothing on results — forfeits the only correctness signal available.
- **`public` measurements include internet path variance.** Runs from a workstation over the public internet carry RTT and jitter that have nothing to do with the server. This is why `direct` exists and why baselines are per-target; it also means `public` p99 should not be read as a server property.
- **The client-side circuit breaker is a mitigation, not a guarantee** (§5.4). It counts the 4xx responses *it* causes; it cannot see 4xx from anything else sharing the workstation's egress IP, and behind NAT or a shared office address the jail's counter is fed by traffic the harness never sent. A run from a shared address can therefore be banned despite the breaker behaving correctly. Running from an address the harness controls exclusively is the only real answer, and the first `public` run should be treated as a probe with console access available.
- **The enforcement layer is new and still moving.** `nftables.conf`, `jail.local`, and the fail2ban units were untracked and being actively edited while this spec was written — `jail.local` changed from a journald backend to file polling against `/var/log/sparql-proxy/access.log` mid-design. §5.4's analysis rests on the jail's *thresholds* (`maxretry = 20`, `findtime = 1m`, any-4xx `failregex`), which held across that change, but the implementation plan must re-read these files rather than trusting this spec's transcription of them.
- **Nothing here measures the endpoint under concurrency**, which is where the `rate=50r/s`, 60s-versus-300s, 4-thread, and now fail2ban interactions actually bite. That is the entire point of spec 2, and no capacity claim should be made from this spec's output alone. Spec 2 inherits §5.4 as a hard constraint: a load generator that trips a one-hour ban on itself mid-ramp produces no data and locks the operator out of the host it was testing.
