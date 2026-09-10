# SPARQL Perf Harness — Profiler (Plan A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `pgperf profile` — a serial SPARQL profiler that runs a labeled query corpus against the QLever endpoint at a chosen cache level and emits per-query latency, row counts, result fingerprints, and a precise outcome classification.

**Architecture:** A stdlib-only Python package `pgperf` under `perf/`, packaged as a podman container and driven from a workstation. Pure modules first (`stats`, `fingerprint`, `outcomes`), then the corpus contract, then the HTTP client, then the repetition/cache-level driver on top. Every module is unit-testable with no live endpoint; the only network code sits behind one seam (`client.py`) that tests substitute.

**Tech Stack:** Python 3.12, standard library only (`urllib.request`, `http.client`, `json`, `hashlib`, `statistics`, `subprocess`, `unittest`). Podman. GNU Make. No third-party packages, no dependency manager, no pytest.

**Spec:** `docs/superpowers/specs/2026-09-09-sparql-perf-harness-foundation-design.md`

**Scope:** Plan A of 2. This plan delivers the profiler (spec §3, §4, §5, §6, §8.1, §8.2). Run identity (§7), host telemetry (§8.3), event-log attribution (§8.2.1), and the `compare` gate (§9) are **Plan B**. Where this plan records a field only so Plan B can consume it, it says so.

## Global Constraints

Copied verbatim from the spec. Every task's requirements implicitly include these.

- **Standard library only.** No `pyproject.toml`, no `uv`, no dependency manager, no third-party packages (spec §3).
- **Results format: JSON** (spec §3). Not parquet.
- **Python 3.12.**
- **Test runner: stdlib `unittest`.** Invoked as `cd perf && python3 -m unittest discover -s tests -t .`
- **Container base must be fully-qualified and pinned**, with `--no-install-recommends` plus apt-list cleanup and a non-root final `USER` (spec §3, repo convention).
- **Never send `access-token` on a `public` target** (spec §5.4).
- **QLever server flags in production** are `-j 4 -m 4G -c 1G -e 500M -k 200 -s 300s`; container limits `Memory=6g`, `--cpus=4`. Default `timeout_s` is 300 to match `-s 300s`.
- **Public endpoint:** `https://packagegraph.di.riseproject.dev`, overridable via `PGPERF_PUBLIC_URL`. SSH host overridable via `PGPERF_SSH_HOST`, default `root@51.159.171.16`.
- **The profiler is strictly serial.** One request in flight, always. Concurrency is Plan B's successor spec, not this one.
- **`sync` must precede `drop_caches`** (spec §6). `drop_caches` frees only clean pages.
- **Cache clearing granularity is per individual query sample**, not per pass (spec §6): `for query: for repetition: clear; execute`.

## File Structure

| File | Responsibility |
|---|---|
| `perf/pgperf/__init__.py` | Package marker. Empty. |
| `perf/pgperf/stats.py` | Percentiles, mean, stdev, coefficient of variation. Pure functions over lists of floats. |
| `perf/pgperf/fingerprint.py` | Canonicalize SPARQL JSON bindings; hash them. Ported from `spike/profile-cqs.py`. |
| `perf/pgperf/outcomes.py` | The outcome taxonomy constants and HTTP-response classification. Shared by `client` and (Plan B) `compare`. |
| `perf/pgperf/corpus.py` | Load and validate `manifest.json`; resolve `.rq` files; select queries. |
| `perf/pgperf/manifestgen.py` | Import `spike/cq-queries.json` → `.rq` files + manifest; measure determinism. |
| `perf/pgperf/client.py` | One SPARQL request. Target modes, timing, response parsing, outcome + fingerprint. |
| `perf/pgperf/ssh.py` | One multiplexed SSH connection; run remote commands. Used here for cache control; reused by Plan B. |
| `perf/pgperf/profile.py` | Cache levels, the repetition loop, sample assembly. |
| `perf/pgperf/__main__.py` | CLI dispatch: `manifest`, `profile`. |
| `perf/Containerfile.perf` | Container image. |
| `perf/README.md` | How to run it. |
| `perf/tests/__init__.py` | Test package marker. Required by `unittest discover -t .`. |
| `perf/tests/test_*.py` | One test module per source module. |

Deferred to Plan B: `runmeta.py`, `telemetry.py`, `summary.py`, `compare.py`.

---

### Task 1: Scaffolding and `stats.py`

Percentiles are the foundation every later aggregate rests on, and the spec calls out that at N=3–5 they are order statistics that must not be over-read (§8.1). Pinning the definition in tests first prevents a silent change of percentile method later.

**Files:**
- Create: `perf/pgperf/__init__.py`
- Create: `perf/pgperf/stats.py`
- Create: `perf/tests/__init__.py`
- Create: `perf/tests/test_stats.py`
- Modify: `Makefile` (add `perf-test`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `percentile(values: list[float], p: float) -> float | None` — nearest-rank, `None` for empty input.
  - `summarize(values: list[float]) -> dict` — keys `n`, `min`, `median`, `p95`, `p99`, `max`, `mean`, `stdev`, `cov`. `stdev` and `cov` are `None` when `n < 2`.

- [ ] **Step 1: Create the package and test markers**

```bash
mkdir -p perf/pgperf perf/tests
touch perf/pgperf/__init__.py perf/tests/__init__.py
```

- [ ] **Step 2: Write the failing test**

Create `perf/tests/test_stats.py`:

```python
import unittest

from pgperf import stats


class TestPercentile(unittest.TestCase):
    def test_empty_returns_none(self):
        self.assertIsNone(stats.percentile([], 95))

    def test_single_value(self):
        self.assertEqual(stats.percentile([42.0], 95), 42.0)

    def test_nearest_rank_definition(self):
        # Nearest-rank: idx = ceil(p/100 * n) - 1, clamped to [0, n-1].
        # n=10, p=95 -> ceil(9.5)-1 = 9 -> the max.
        values = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]
        self.assertEqual(stats.percentile(values, 95), 10.0)
        self.assertEqual(stats.percentile(values, 50), 5.0)
        self.assertEqual(stats.percentile(values, 100), 10.0)

    def test_p95_of_five_samples_is_the_max(self):
        # Documented consequence of N=5 (spec 8.1): p95 and p99 are the max.
        # This test exists so nobody "fixes" it into interpolation by accident.
        values = [10.0, 20.0, 30.0, 40.0, 500.0]
        self.assertEqual(stats.percentile(values, 95), 500.0)
        self.assertEqual(stats.percentile(values, 99), 500.0)

    def test_input_is_not_mutated(self):
        values = [3.0, 1.0, 2.0]
        stats.percentile(values, 50)
        self.assertEqual(values, [3.0, 1.0, 2.0])


class TestSummarize(unittest.TestCase):
    def test_empty(self):
        got = stats.summarize([])
        self.assertEqual(got["n"], 0)
        for key in ("min", "median", "p95", "p99", "max", "mean", "stdev", "cov"):
            self.assertIsNone(got[key], f"{key} should be None for empty input")

    def test_single_value_has_no_spread(self):
        got = stats.summarize([7.0])
        self.assertEqual(got["n"], 1)
        self.assertEqual(got["mean"], 7.0)
        self.assertIsNone(got["stdev"], "stdev undefined for n=1")
        self.assertIsNone(got["cov"], "cov undefined for n=1")

    def test_known_values(self):
        got = stats.summarize([2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0])
        self.assertEqual(got["n"], 8)
        self.assertEqual(got["min"], 2.0)
        self.assertEqual(got["max"], 9.0)
        self.assertEqual(got["mean"], 5.0)
        # statistics.stdev is the sample (n-1) standard deviation.
        self.assertAlmostEqual(got["stdev"], 2.1381, places=3)
        self.assertAlmostEqual(got["cov"], 0.4276, places=3)

    def test_cov_is_none_when_mean_is_zero(self):
        got = stats.summarize([0.0, 0.0, 0.0])
        self.assertEqual(got["mean"], 0.0)
        self.assertIsNone(got["cov"], "cov is undefined when mean is 0")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 3: Run the test to verify it fails**

```bash
cd perf && python3 -m unittest discover -s tests -t . -v
```

Expected: `ModuleNotFoundError: No module named 'pgperf.stats'`

- [ ] **Step 4: Write the minimal implementation**

Create `perf/pgperf/stats.py`:

```python
"""Percentiles and spread over small sample sets.

Deliberately uses the nearest-rank percentile, not interpolation. At the
sample sizes this harness runs (3-5 repetitions per query), interpolation
invents precision that is not there; nearest-rank at least always returns
a value that was actually measured.
"""

import math
import statistics

_KEYS = ("min", "median", "p95", "p99", "max", "mean", "stdev", "cov")


def percentile(values, p):
    """Nearest-rank percentile. Returns None for empty input."""
    if not values:
        return None
    ordered = sorted(values)
    idx = math.ceil(p / 100.0 * len(ordered)) - 1
    idx = max(0, min(idx, len(ordered) - 1))
    return ordered[idx]


def summarize(values):
    """Aggregate a list of samples.

    stdev and cov are None when n < 2 (undefined), and cov is also None
    when the mean is 0 (division undefined).
    """
    n = len(values)
    if n == 0:
        return dict({"n": 0}, **{k: None for k in _KEYS})

    ordered = sorted(values)
    mean = statistics.fmean(ordered)
    stdev = statistics.stdev(ordered) if n >= 2 else None
    cov = (stdev / mean) if (stdev is not None and mean) else None

    return {
        "n": n,
        "min": ordered[0],
        "median": statistics.median(ordered),
        "p95": percentile(ordered, 95),
        "p99": percentile(ordered, 99),
        "max": ordered[-1],
        "mean": mean,
        "stdev": stdev,
        "cov": cov,
    }
```

- [ ] **Step 5: Run the test to verify it passes**

```bash
cd perf && python3 -m unittest discover -s tests -t . -v
```

Expected: `OK` — 10 tests.

- [ ] **Step 6: Add the Make target**

Append to `Makefile`, and add `perf-test` to the `.PHONY` line at the top:

```makefile

# --- Performance harness (perf/) ---
# Stdlib-only Python; no venv, no dependency install. See perf/README.md.
perf-test:
	cd perf && python3 -m unittest discover -s tests -t . -v
```

- [ ] **Step 7: Verify the Make target**

```bash
make perf-test
```

Expected: `OK` — 10 tests.

- [ ] **Step 8: Commit**

```bash
git add perf/pgperf/__init__.py perf/pgperf/stats.py perf/tests/__init__.py perf/tests/test_stats.py Makefile
git commit -m "feat(perf): add stats module with nearest-rank percentiles"
```

---

### Task 2: `fingerprint.py`

`spike/profile-cqs.py` has the canonicalization logic but no tests for it, and the distinctions it preserves (datatype, language tag, term type) are exactly the ones a careless refactor would drop. Spec §5.2 requires them.

**Files:**
- Create: `perf/pgperf/fingerprint.py`
- Create: `perf/tests/test_fingerprint.py`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `canonicalize(bindings: list[dict]) -> list[tuple]` — order-independent canonical form.
  - `fingerprint(bindings: list[dict]) -> str` — `"sha256:<hex>"`.

- [ ] **Step 1: Write the failing test**

Create `perf/tests/test_fingerprint.py`:

```python
import unittest

from pgperf import fingerprint


def binding(**kwargs):
    """Build a SPARQL JSON binding row: {var: {"type":..,"value":..}}."""
    return {k: dict(v) for k, v in kwargs.items()}


LITERAL_ONE_INT = {"type": "literal", "value": "1", "datatype": "http://www.w3.org/2001/XMLSchema#integer"}
LITERAL_ONE_STR = {"type": "literal", "value": "1"}
CHAT_EN = {"type": "literal", "value": "chat", "xml:lang": "en"}
CHAT_FR = {"type": "literal", "value": "chat", "xml:lang": "fr"}
URI_A = {"type": "uri", "value": "http://example.org/a"}
LITERAL_A = {"type": "literal", "value": "http://example.org/a"}


class TestCanonicalize(unittest.TestCase):
    def test_row_order_does_not_matter(self):
        rows_a = [binding(s=URI_A), binding(s=LITERAL_ONE_INT)]
        rows_b = [binding(s=LITERAL_ONE_INT), binding(s=URI_A)]
        self.assertEqual(fingerprint.canonicalize(rows_a), fingerprint.canonicalize(rows_b))

    def test_variable_order_within_a_row_does_not_matter(self):
        rows_a = [{"a": dict(URI_A), "b": dict(CHAT_EN)}]
        rows_b = [{"b": dict(CHAT_EN), "a": dict(URI_A)}]
        self.assertEqual(fingerprint.canonicalize(rows_a), fingerprint.canonicalize(rows_b))

    def test_empty_bindings(self):
        self.assertEqual(fingerprint.canonicalize([]), [])


class TestDistinctionsPreserved(unittest.TestCase):
    """Spec 5.2: these must NOT collapse. Each is a real correctness signal."""

    def test_datatype_matters(self):
        self.assertNotEqual(
            fingerprint.fingerprint([binding(v=LITERAL_ONE_INT)]),
            fingerprint.fingerprint([binding(v=LITERAL_ONE_STR)]),
        )

    def test_language_tag_matters(self):
        self.assertNotEqual(
            fingerprint.fingerprint([binding(v=CHAT_EN)]),
            fingerprint.fingerprint([binding(v=CHAT_FR)]),
        )

    def test_term_type_matters(self):
        self.assertNotEqual(
            fingerprint.fingerprint([binding(v=URI_A)]),
            fingerprint.fingerprint([binding(v=LITERAL_A)]),
        )

    def test_variable_name_matters(self):
        self.assertNotEqual(
            fingerprint.fingerprint([binding(x=URI_A)]),
            fingerprint.fingerprint([binding(y=URI_A)]),
        )

    def test_cardinality_matters(self):
        self.assertNotEqual(
            fingerprint.fingerprint([binding(v=URI_A)]),
            fingerprint.fingerprint([binding(v=URI_A), binding(v=URI_A)]),
        )


class TestFingerprintFormat(unittest.TestCase):
    def test_prefixed_hex(self):
        fp = fingerprint.fingerprint([binding(v=URI_A)])
        self.assertTrue(fp.startswith("sha256:"), fp)
        self.assertEqual(len(fp), len("sha256:") + 64)

    def test_stable_across_calls(self):
        rows = [binding(v=URI_A), binding(v=CHAT_EN)]
        self.assertEqual(fingerprint.fingerprint(rows), fingerprint.fingerprint(list(reversed(rows))))

    def test_empty_is_stable_and_distinct(self):
        empty = fingerprint.fingerprint([])
        self.assertTrue(empty.startswith("sha256:"))
        self.assertNotEqual(empty, fingerprint.fingerprint([binding(v=URI_A)]))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
make perf-test
```

Expected: `ModuleNotFoundError: No module named 'pgperf.fingerprint'`

- [ ] **Step 3: Write the minimal implementation**

Create `perf/pgperf/fingerprint.py`:

```python
"""Canonicalize and hash SPARQL JSON result bindings.

Ported from spike/profile-cqs.py's canonicalize_bindings(), unchanged in
behavior. The four fields captured per term -- type, value, datatype, and
xml:lang -- are all load-bearing: "1"^^xsd:integer must not equal
"1"^^xsd:string, "chat"@en must not equal "chat"@fr, and a URI must not
equal a literal with the same characters.

A fingerprint is evidence about ONE index, not about the data. See spec
section 4.4 before using one to gate anything.
"""

import hashlib
import json


def canonicalize(bindings):
    """Convert result bindings to a sorted, order-independent canonical form."""
    rows = []
    for row in bindings:
        rows.append(
            tuple(
                sorted(
                    (
                        var,
                        term.get("type", ""),
                        term.get("value", ""),
                        term.get("datatype", ""),
                        term.get("xml:lang", ""),
                    )
                    for var, term in row.items()
                )
            )
        )
    rows.sort()
    return rows


def fingerprint(bindings):
    """Return "sha256:<hex>" over the canonicalized bindings."""
    canonical = canonicalize(bindings)
    payload = json.dumps(canonical, separators=(",", ":"), ensure_ascii=False, sort_keys=False)
    digest = hashlib.sha256(payload.encode("utf-8")).hexdigest()
    return f"sha256:{digest}"
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
make perf-test
```

Expected: `OK` — 21 tests.

- [ ] **Step 5: Commit**

```bash
git add perf/pgperf/fingerprint.py perf/tests/test_fingerprint.py
git commit -m "feat(perf): add result fingerprinting with tests for term distinctions"
```

---

### Task 3: `outcomes.py`

The taxonomy is where the existing spike script is wrong: it collapses everything into OK/ERROR/TIMEOUT (spec §5.3). The bare-503 rule is the specific defect to guard — nginx returns 503 both for `limit_req` and for an unavailable upstream, so attributing every 503 to rate limiting would silently discard a real backend outage.

**Files:**
- Create: `perf/pgperf/outcomes.py`
- Create: `perf/tests/test_outcomes.py`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - Transport constants: `OK`, `SPARQL_ERROR`, `HTTP_ERROR`, `PROXY_TIMEOUT`, `RATE_LIMITED`, `CLIENT_TIMEOUT`, `CONNECTION_ERROR`, `BANNED`.
  - Correctness constants: `FINGERPRINT_MISMATCH`, `CARDINALITY_VIOLATION`.
  - `RATE_LIMIT_MARKER = "X-PG-RateLimit"`
  - `classify_http(status: int, headers: dict, body: dict | None) -> tuple[str, str | None]` — returns `(outcome, note)`.
  - `check_cardinality(rows: int, expect: dict | None) -> str | None` — returns `CARDINALITY_VIOLATION` or `None`.
  - `ENVIRONMENTAL = frozenset({RATE_LIMITED, BANNED})` — outcomes that invalidate a run rather than failing it (spec §9.3). Plan B consumes this.

- [ ] **Step 1: Write the failing test**

Create `perf/tests/test_outcomes.py`:

```python
import unittest

from pgperf import outcomes

QLEVER_OK = {"head": {"vars": ["s"]}, "results": {"bindings": []}, "meta": {"query-time-ms": 7}}
QLEVER_EXCEPTION = {"exception": "Parse error at token 'SELCT'"}


class TestClassifyHttp(unittest.TestCase):
    def test_200_ok(self):
        got, note = outcomes.classify_http(200, {}, QLEVER_OK)
        self.assertEqual(got, outcomes.OK)
        self.assertIsNone(note)

    def test_200_with_exception_body_is_sparql_error(self):
        got, note = outcomes.classify_http(200, {}, QLEVER_EXCEPTION)
        self.assertEqual(got, outcomes.SPARQL_ERROR)
        self.assertIn("Parse error", note)

    def test_200_with_unparseable_body_is_http_error(self):
        got, note = outcomes.classify_http(200, {}, None)
        self.assertEqual(got, outcomes.HTTP_ERROR)
        self.assertIn("unparseable", note.lower())

    def test_504_is_proxy_timeout(self):
        got, note = outcomes.classify_http(504, {}, None)
        self.assertEqual(got, outcomes.PROXY_TIMEOUT)
        self.assertIn("60s", note)

    def test_400_with_qlever_error_body_is_sparql_error(self):
        got, note = outcomes.classify_http(400, {}, QLEVER_EXCEPTION)
        self.assertEqual(got, outcomes.SPARQL_ERROR)

    def test_400_without_error_body_is_http_error(self):
        got, _ = outcomes.classify_http(400, {}, None)
        self.assertEqual(got, outcomes.HTTP_ERROR)

    def test_403_is_http_error(self):
        got, _ = outcomes.classify_http(403, {}, None)
        self.assertEqual(got, outcomes.HTTP_ERROR)

    def test_502_is_http_error(self):
        got, _ = outcomes.classify_http(502, {}, None)
        self.assertEqual(got, outcomes.HTTP_ERROR)


class TestBare503IsNotAttributable(unittest.TestCase):
    """Spec 5.3: nginx returns 503 for limit_req AND for an unavailable
    upstream. Without a provenance marker the two are indistinguishable,
    and calling every 503 RATE_LIMITED would let a real QLever outage be
    discarded as an enforcement artifact."""

    def test_503_with_marker_is_rate_limited(self):
        got, note = outcomes.classify_http(503, {outcomes.RATE_LIMIT_MARKER: "1"}, None)
        self.assertEqual(got, outcomes.RATE_LIMITED)
        self.assertIsNone(note)

    def test_503_marker_lookup_is_case_insensitive(self):
        got, _ = outcomes.classify_http(503, {"x-pg-ratelimit": "1"}, None)
        self.assertEqual(got, outcomes.RATE_LIMITED)

    def test_503_without_marker_is_http_error_and_flags_ambiguity(self):
        got, note = outcomes.classify_http(503, {}, None)
        self.assertEqual(got, outcomes.HTTP_ERROR)
        self.assertIn("ambiguous", note.lower())
        self.assertIn("upstream", note.lower())


class TestCheckCardinality(unittest.TestCase):
    def test_none_expect_never_violates(self):
        self.assertIsNone(outcomes.check_cardinality(0, None))

    def test_within_bounds(self):
        self.assertIsNone(outcomes.check_cardinality(50, {"min_rows": 1, "max_rows": 100}))

    def test_at_bounds_is_inclusive(self):
        self.assertIsNone(outcomes.check_cardinality(1, {"min_rows": 1, "max_rows": 100}))
        self.assertIsNone(outcomes.check_cardinality(100, {"min_rows": 1, "max_rows": 100}))

    def test_below_min_violates(self):
        self.assertEqual(
            outcomes.check_cardinality(0, {"min_rows": 1, "max_rows": 100}),
            outcomes.CARDINALITY_VIOLATION,
        )

    def test_above_max_violates(self):
        self.assertEqual(
            outcomes.check_cardinality(101, {"min_rows": 1, "max_rows": 100}),
            outcomes.CARDINALITY_VIOLATION,
        )

    def test_partial_bounds(self):
        self.assertIsNone(outcomes.check_cardinality(9999, {"min_rows": 1}))
        self.assertEqual(
            outcomes.check_cardinality(0, {"min_rows": 1}), outcomes.CARDINALITY_VIOLATION
        )


class TestEnvironmental(unittest.TestCase):
    def test_membership(self):
        self.assertIn(outcomes.RATE_LIMITED, outcomes.ENVIRONMENTAL)
        self.assertIn(outcomes.BANNED, outcomes.ENVIRONMENTAL)

    def test_real_failures_are_not_environmental(self):
        for outcome in (
            outcomes.SPARQL_ERROR,
            outcomes.HTTP_ERROR,
            outcomes.PROXY_TIMEOUT,
            outcomes.CLIENT_TIMEOUT,
        ):
            self.assertNotIn(outcome, outcomes.ENVIRONMENTAL, outcome)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
make perf-test
```

Expected: `ModuleNotFoundError: No module named 'pgperf.outcomes'`

- [ ] **Step 3: Write the minimal implementation**

Create `perf/pgperf/outcomes.py`:

```python
"""The outcome taxonomy.

spike/profile-cqs.py classified into OK/ERROR/TIMEOUT. That is not enough
once a proxy is in the path: a rate-limited request is fast and would
flatter every percentile it appeared in, and a 504 from nginx means the
query crossed the proxy's 60s ceiling while QLever kept executing behind
it for up to 300s. Those are different facts and must stay different.
"""

# Transport outcomes -- what happened to the request.
OK = "OK"
SPARQL_ERROR = "SPARQL_ERROR"
HTTP_ERROR = "HTTP_ERROR"
PROXY_TIMEOUT = "PROXY_TIMEOUT"
RATE_LIMITED = "RATE_LIMITED"
CLIENT_TIMEOUT = "CLIENT_TIMEOUT"
CONNECTION_ERROR = "CONNECTION_ERROR"
BANNED = "BANNED"

# Correctness outcomes -- layered on top of a successful request.
FINGERPRINT_MISMATCH = "FINGERPRINT_MISMATCH"
CARDINALITY_VIOLATION = "CARDINALITY_VIOLATION"

# Outcomes attributable to the enforcement layer rather than to the server.
# These invalidate a run; they never fail a gate. See spec section 9.3.
ENVIRONMENTAL = frozenset({RATE_LIMITED, BANNED})

# Provenance header that makes a 503 attributable. The proxy does not set
# this today -- see spec section 5.3 for the recommended one-line change.
RATE_LIMIT_MARKER = "X-PG-RateLimit"

_AMBIGUOUS_503 = (
    "ambiguous 503: nginx returns this for limit_req and for an unavailable "
    "upstream, and no provenance marker was present"
)


def _has_marker(headers):
    if not headers:
        return False
    target = RATE_LIMIT_MARKER.lower()
    return any(key.lower() == target for key in headers)


def classify_http(status, headers, body):
    """Classify a completed HTTP response.

    body is the parsed JSON dict, or None if the response was not
    parseable JSON. Returns (outcome, note) where note is None on OK.
    """
    if status == 200:
        if body is None:
            return HTTP_ERROR, "HTTP 200 with unparseable body"
        if "exception" in body:
            return SPARQL_ERROR, str(body["exception"])[:200]
        return OK, None

    if status == 504:
        return PROXY_TIMEOUT, "504 from nginx; query exceeded proxy_read_timeout 60s"

    if status == 503:
        if _has_marker(headers):
            return RATE_LIMITED, None
        return HTTP_ERROR, _AMBIGUOUS_503

    if body is not None and "exception" in body:
        return SPARQL_ERROR, str(body["exception"])[:200]

    return HTTP_ERROR, f"HTTP {status}"


def check_cardinality(rows, expect):
    """Return CARDINALITY_VIOLATION if rows falls outside expect, else None.

    Bounds are inclusive. A missing bound is not checked.
    """
    if not expect:
        return None
    low = expect.get("min_rows")
    high = expect.get("max_rows")
    if low is not None and rows < low:
        return CARDINALITY_VIOLATION
    if high is not None and rows > high:
        return CARDINALITY_VIOLATION
    return None
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
make perf-test
```

Expected: `OK` — 42 tests.

- [ ] **Step 5: Commit**

```bash
git add perf/pgperf/outcomes.py perf/tests/test_outcomes.py
git commit -m "feat(perf): add outcome taxonomy; treat a bare 503 as unattributable"
```

---

### Task 4: `corpus.py`

The manifest is the contract Plan B and the successor spec's k6 workload both consume, so its validation is worth being strict about. Spec §4.2 defines the schema; §4.4 requires a fingerprint to travel with the `index_hash` it was captured against.

**Files:**
- Create: `perf/pgperf/corpus.py`
- Create: `perf/tests/test_corpus.py`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `CLASSES = frozenset({"lookup","join","aggregate","path","graph","text","adversarial"})`
  - `class CorpusError(Exception)`
  - `load(manifest_path: str) -> list[dict]` — validated query dicts, each with an added `sparql` key holding the query text read from `file`, and an added `path` key with the resolved absolute path. Raises `CorpusError` on any violation.
  - `select(queries: list[dict], include_adversarial: bool = False, ids: list[str] | None = None) -> list[dict]` — filters by `enabled` and class per spec §4.7.
  - `manifest_hash(manifest_path: str) -> str` — `"sha256:<hex>"` of the manifest bytes.

- [ ] **Step 1: Write the failing test**

Create `perf/tests/test_corpus.py`:

```python
import json
import os
import tempfile
import unittest

from pgperf import corpus


def write_corpus(tmpdir, queries, sparql_by_file=None):
    """Write a manifest plus its .rq files. Returns the manifest path."""
    sparql_by_file = sparql_by_file or {}
    for q in queries:
        rel = q.get("file")
        if not rel:
            continue
        path = os.path.join(tmpdir, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(sparql_by_file.get(rel, "SELECT * WHERE { ?s ?p ?o } LIMIT 1"))
    manifest = os.path.join(tmpdir, "manifest.json")
    with open(manifest, "w", encoding="utf-8") as fh:
        json.dump({"schema_version": 1, "queries": queries}, fh)
    return manifest


def valid_query(**overrides):
    q = {
        "id": "CQ-PM-01",
        "title": "Distribution Package Listing",
        "file": "cq/CQ-PM-01.rq",
        "class": "join",
        "features": ["inverse-path"],
        "expect": {"min_rows": 1, "max_rows": 10000},
        "max_row_drift_pct": 25,
        "fingerprint": {"index_hash": "a1b2c3d4e5f60718", "sha256": "sha256:" + "0" * 64},
        "deterministic": True,
        "timeout_s": 300,
        "weight": 3,
        "enabled": True,
    }
    q.update(overrides)
    return q


class TestLoad(unittest.TestCase):
    def test_loads_and_attaches_sparql_text(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(
                tmp, [valid_query()], {"cq/CQ-PM-01.rq": "SELECT ?x WHERE { ?x a ?t }"}
            )
            got = corpus.load(path)
            self.assertEqual(len(got), 1)
            self.assertEqual(got[0]["sparql"], "SELECT ?x WHERE { ?x a ?t }")
            self.assertTrue(os.path.isabs(got[0]["path"]))

    def test_rejects_unknown_class(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(tmp, [valid_query(**{"class": "wizardry"})])
            with self.assertRaises(corpus.CorpusError) as ctx:
                corpus.load(path)
            self.assertIn("wizardry", str(ctx.exception))

    def test_rejects_missing_rq_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            q = valid_query()
            path = write_corpus(tmp, [q])
            os.remove(os.path.join(tmp, q["file"]))
            with self.assertRaises(corpus.CorpusError) as ctx:
                corpus.load(path)
            self.assertIn("CQ-PM-01.rq", str(ctx.exception))

    def test_rejects_deterministic_true_with_null_fingerprint(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(tmp, [valid_query(deterministic=True, fingerprint=None)])
            with self.assertRaises(corpus.CorpusError) as ctx:
                corpus.load(path)
            self.assertIn("fingerprint", str(ctx.exception))

    def test_rejects_fingerprint_without_index_hash(self):
        # Spec 4.4: a fingerprint is evidence about one index. Without the
        # index_hash it was captured against, it cannot be safely compared.
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(
                tmp, [valid_query(fingerprint={"sha256": "sha256:" + "0" * 64})]
            )
            with self.assertRaises(corpus.CorpusError) as ctx:
                corpus.load(path)
            self.assertIn("index_hash", str(ctx.exception))

    def test_rejects_min_rows_greater_than_max_rows(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(tmp, [valid_query(expect={"min_rows": 500, "max_rows": 5})])
            with self.assertRaises(corpus.CorpusError) as ctx:
                corpus.load(path)
            self.assertIn("min_rows", str(ctx.exception))

    def test_rejects_duplicate_ids(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(tmp, [valid_query(), valid_query()])
            with self.assertRaises(corpus.CorpusError) as ctx:
                corpus.load(path)
            self.assertIn("duplicate", str(ctx.exception).lower())

    def test_rejects_unknown_schema_version(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(tmp, [valid_query()])
            with open(path, encoding="utf-8") as fh:
                doc = json.load(fh)
            doc["schema_version"] = 99
            with open(path, "w", encoding="utf-8") as fh:
                json.dump(doc, fh)
            with self.assertRaises(corpus.CorpusError):
                corpus.load(path)

    def test_deterministic_false_permits_null_fingerprint(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(tmp, [valid_query(deterministic=False, fingerprint=None)])
            self.assertEqual(len(corpus.load(path)), 1)

    def test_null_max_row_drift_pct_is_allowed(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(tmp, [valid_query(max_row_drift_pct=None)])
            self.assertEqual(len(corpus.load(path)), 1)


class TestSelect(unittest.TestCase):
    def setUp(self):
        self.queries = [
            valid_query(id="A", **{"class": "join"}),
            valid_query(id="B", **{"class": "join"}, enabled=False),
            valid_query(
                id="ADV", **{"class": "adversarial"}, deterministic=False, fingerprint=None
            ),
        ]

    def test_excludes_disabled(self):
        got = [q["id"] for q in corpus.select(self.queries)]
        self.assertNotIn("B", got)

    def test_excludes_adversarial_by_default(self):
        got = [q["id"] for q in corpus.select(self.queries)]
        self.assertEqual(got, ["A"])

    def test_includes_adversarial_when_asked(self):
        got = [q["id"] for q in corpus.select(self.queries, include_adversarial=True)]
        self.assertEqual(got, ["A", "ADV"])

    def test_enabled_is_orthogonal_to_class(self):
        # Spec 4.7: retiring a query and excluding a class are independent.
        queries = [valid_query(id="ADV2", **{"class": "adversarial"}, enabled=False,
                               deterministic=False, fingerprint=None)]
        self.assertEqual(corpus.select(queries, include_adversarial=True), [])

    def test_explicit_ids_override_class_filter(self):
        got = [q["id"] for q in corpus.select(self.queries, ids=["ADV"])]
        self.assertEqual(got, ["ADV"])

    def test_unknown_id_raises(self):
        with self.assertRaises(corpus.CorpusError):
            corpus.select(self.queries, ids=["NOPE"])


class TestManifestHash(unittest.TestCase):
    def test_stable_and_content_sensitive(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_corpus(tmp, [valid_query()])
            first = corpus.manifest_hash(path)
            self.assertEqual(first, corpus.manifest_hash(path))
            self.assertTrue(first.startswith("sha256:"))
            with open(path, "a", encoding="utf-8") as fh:
                fh.write(" ")
            self.assertNotEqual(first, corpus.manifest_hash(path))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
make perf-test
```

Expected: `ModuleNotFoundError: No module named 'pgperf.corpus'`

- [ ] **Step 3: Write the minimal implementation**

Create `perf/pgperf/corpus.py`:

```python
"""Load and validate the query corpus manifest.

The manifest is a contract with two consumers beyond this profiler: the
Plan B gate, and the successor spec's k6 workload (which is why `weight`
exists here and goes unused). Validation is strict on purpose -- a
malformed manifest that loads anyway becomes a silently wrong gate.
"""

import hashlib
import json
import os

SCHEMA_VERSION = 1

CLASSES = frozenset({"lookup", "join", "aggregate", "path", "graph", "text", "adversarial"})

DEFAULT_TIMEOUT_S = 300  # matches qlever-server -s 300s


class CorpusError(Exception):
    """The manifest or one of its queries is invalid."""


def _require(condition, message):
    if not condition:
        raise CorpusError(message)


def _validate(query, base_dir):
    qid = query.get("id")
    _require(qid, "query is missing 'id'")

    cls = query.get("class")
    _require(cls in CLASSES, f"{qid}: unknown class {cls!r}; expected one of {sorted(CLASSES)}")

    rel = query.get("file")
    _require(rel, f"{qid}: missing 'file'")
    path = os.path.normpath(os.path.join(base_dir, rel))
    _require(os.path.isfile(path), f"{qid}: query file not found: {rel}")

    expect = query.get("expect") or {}
    low, high = expect.get("min_rows"), expect.get("max_rows")
    if low is not None and high is not None:
        _require(low <= high, f"{qid}: min_rows {low} exceeds max_rows {high}")

    deterministic = query.get("deterministic")
    fp = query.get("fingerprint")
    if deterministic:
        _require(fp, f"{qid}: deterministic is true but fingerprint is null")
        _require(
            fp.get("index_hash"),
            f"{qid}: fingerprint is missing index_hash; a fingerprint is evidence "
            "about one index and cannot be compared without it (spec 4.4)",
        )
        _require(fp.get("sha256"), f"{qid}: fingerprint is missing sha256")

    with open(path, encoding="utf-8") as fh:
        sparql = fh.read()
    _require(sparql.strip(), f"{qid}: query file is empty: {rel}")

    resolved = dict(query)
    resolved["path"] = os.path.abspath(path)
    resolved["sparql"] = sparql
    resolved.setdefault("timeout_s", DEFAULT_TIMEOUT_S)
    resolved.setdefault("enabled", True)
    return resolved


def load(manifest_path):
    """Load, validate, and resolve every query in the manifest."""
    with open(manifest_path, encoding="utf-8") as fh:
        doc = json.load(fh)

    version = doc.get("schema_version")
    _require(
        version == SCHEMA_VERSION,
        f"unsupported schema_version {version!r}; this build understands {SCHEMA_VERSION}",
    )

    base_dir = os.path.dirname(os.path.abspath(manifest_path))
    queries = [_validate(q, base_dir) for q in doc.get("queries", [])]

    seen = set()
    for q in queries:
        _require(q["id"] not in seen, f"duplicate query id: {q['id']}")
        seen.add(q["id"])

    return queries


def select(queries, include_adversarial=False, ids=None):
    """Filter to the queries a run should execute.

    `enabled` and `class` are independent filters: an explicitly disabled
    adversarial query stays excluded even with include_adversarial set.
    """
    if ids is not None:
        by_id = {q["id"]: q for q in queries}
        unknown = [i for i in ids if i not in by_id]
        _require(not unknown, f"unknown query ids: {unknown}")
        return [by_id[i] for i in ids]

    chosen = []
    for q in queries:
        if not q.get("enabled", True):
            continue
        if q["class"] == "adversarial" and not include_adversarial:
            continue
        chosen.append(q)
    return chosen


def manifest_hash(manifest_path):
    """Return "sha256:<hex>" over the manifest file's bytes."""
    with open(manifest_path, "rb") as fh:
        return "sha256:" + hashlib.sha256(fh.read()).hexdigest()
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
make perf-test
```

Expected: `OK` — 60 tests.

- [ ] **Step 5: Commit**

```bash
git add perf/pgperf/corpus.py perf/tests/test_corpus.py
git commit -m "feat(perf): add corpus manifest loading and strict validation"
```

---

### Task 5: `client.py`

One SPARQL request, with the timing and classification the profiler needs. All network I/O lives behind `_open` so tests can substitute it — the rest of the harness never touches a socket in tests.

**Files:**
- Create: `perf/pgperf/client.py`
- Create: `perf/tests/test_client.py`

**Interfaces:**
- Consumes: `pgperf.outcomes`, `pgperf.fingerprint`.
- Produces:
  - `class Target` with `Target.public(url=None)` and `Target.direct(port=7001)` constructors; attributes `mode` (`"public"`/`"direct"`), `url`, `allows_token` (bool).
  - `class Client(target, timeout_s=300, access_token=None, opener=None)`
  - `Client.execute(sparql: str, timeout_s: int | None = None, expect: dict | None = None) -> dict` — a sample dict with keys `outcome`, `note`, `correctness`, `wall_ms`, `engine_ms`, `rows`, `bytes`, `fingerprint`, `http_status`, `http_version`.
  - `PUBLIC_URL_ENV = "PGPERF_PUBLIC_URL"`, `DEFAULT_PUBLIC_URL = "https://packagegraph.di.riseproject.dev"`

- [ ] **Step 1: Write the failing test**

Create `perf/tests/test_client.py`:

```python
import json
import unittest
import urllib.error

from pgperf import client, outcomes

BINDINGS = [{"s": {"type": "uri", "value": "http://example.org/a"}}]
OK_BODY = {"head": {"vars": ["s"]}, "results": {"bindings": BINDINGS}, "meta": {"query-time-ms": 12}}


class FakeResponse:
    def __init__(self, status=200, body=None, headers=None, raw=None):
        self.status = status
        self.headers = headers or {}
        self._raw = raw if raw is not None else json.dumps(body or OK_BODY).encode()

    def read(self):
        return self._raw

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def opener_returning(response):
    calls = []

    def _open(request, timeout):
        calls.append({"request": request, "timeout": timeout})
        return response

    _open.calls = calls
    return _open


def opener_raising(exc):
    def _open(request, timeout):
        raise exc

    return _open


class TestExecuteSuccess(unittest.TestCase):
    def test_ok_sample_shape(self):
        c = client.Client(client.Target.public(), opener=opener_returning(FakeResponse()))
        sample = c.execute("SELECT * WHERE { ?s ?p ?o }")
        self.assertEqual(sample["outcome"], outcomes.OK)
        self.assertIsNone(sample["correctness"])
        self.assertEqual(sample["rows"], 1)
        self.assertEqual(sample["engine_ms"], 12)
        self.assertIsInstance(sample["wall_ms"], float)
        self.assertGreater(sample["bytes"], 0)
        self.assertTrue(sample["fingerprint"].startswith("sha256:"))
        self.assertEqual(sample["http_status"], 200)

    def test_records_http_version_as_1_1(self):
        # Spec 5.1: urllib is HTTP/1.1 only. Recording it keeps the gap
        # versus production HTTP/2 visible instead of implied.
        c = client.Client(client.Target.public(), opener=opener_returning(FakeResponse()))
        self.assertEqual(c.execute("SELECT 1")["http_version"], "HTTP/1.1")

    def test_missing_meta_leaves_engine_ms_none(self):
        body = {"results": {"bindings": []}}
        c = client.Client(client.Target.public(), opener=opener_returning(FakeResponse(body=body)))
        self.assertIsNone(c.execute("SELECT 1")["engine_ms"])

    def test_cardinality_violation_is_recorded_alongside_ok(self):
        c = client.Client(client.Target.public(), opener=opener_returning(FakeResponse()))
        sample = c.execute("SELECT 1", expect={"min_rows": 5})
        self.assertEqual(sample["outcome"], outcomes.OK)
        self.assertEqual(sample["correctness"], outcomes.CARDINALITY_VIOLATION)


class TestExecuteFailures(unittest.TestCase):
    def test_http_error_is_classified_not_raised(self):
        err = urllib.error.HTTPError(
            "http://x", 504, "Gateway Timeout", {}, None
        )
        c = client.Client(client.Target.public(), opener=opener_raising(err))
        sample = c.execute("SELECT 1")
        self.assertEqual(sample["outcome"], outcomes.PROXY_TIMEOUT)
        self.assertEqual(sample["http_status"], 504)

    def test_socket_timeout_is_client_timeout(self):
        c = client.Client(client.Target.public(), opener=opener_raising(TimeoutError()))
        self.assertEqual(c.execute("SELECT 1")["outcome"], outcomes.CLIENT_TIMEOUT)

    def test_url_error_is_connection_error(self):
        c = client.Client(
            client.Target.public(), opener=opener_raising(urllib.error.URLError("unreachable"))
        )
        sample = c.execute("SELECT 1")
        self.assertEqual(sample["outcome"], outcomes.CONNECTION_ERROR)
        self.assertIsNone(sample["http_status"])

    def test_sparql_exception_body(self):
        resp = FakeResponse(body={"exception": "Parse error"})
        c = client.Client(client.Target.public(), opener=opener_returning(resp))
        sample = c.execute("SELCT 1")
        self.assertEqual(sample["outcome"], outcomes.SPARQL_ERROR)
        self.assertIsNone(sample["fingerprint"], "no fingerprint for a failed query")

    def test_non_json_body(self):
        resp = FakeResponse(raw=b"<html>oops</html>")
        c = client.Client(client.Target.public(), opener=opener_returning(resp))
        self.assertEqual(c.execute("SELECT 1")["outcome"], outcomes.HTTP_ERROR)


class TestTargets(unittest.TestCase):
    def test_public_target_defaults_and_forbids_token(self):
        t = client.Target.public()
        self.assertEqual(t.mode, "public")
        self.assertEqual(t.url, client.DEFAULT_PUBLIC_URL)
        self.assertFalse(t.allows_token)

    def test_public_url_override(self):
        self.assertEqual(client.Target.public("https://example.test").url, "https://example.test")

    def test_direct_target_allows_token(self):
        t = client.Target.direct(port=7001)
        self.assertEqual(t.mode, "direct")
        self.assertEqual(t.url, "http://127.0.0.1:7001")
        self.assertTrue(t.allows_token)

    def test_token_is_never_sent_on_public(self):
        # Spec 5.4: the proxy 403s access_token, and a 403 loop is
        # pointless noise. Make it unreachable by construction.
        op = opener_returning(FakeResponse())
        c = client.Client(client.Target.public(), access_token="secret", opener=op)
        c.execute("SELECT 1")
        body = op.calls[0]["request"].data.decode()
        self.assertNotIn("access-token", body)
        self.assertNotIn("secret", body)

    def test_token_is_sent_on_direct(self):
        op = opener_returning(FakeResponse())
        c = client.Client(client.Target.direct(), access_token="secret", opener=op)
        c.execute("SELECT 1")
        self.assertIn("access-token=secret", op.calls[0]["request"].data.decode())


class TestRequestShape(unittest.TestCase):
    def test_posts_urlencoded_query_with_json_accept(self):
        op = opener_returning(FakeResponse())
        client.Client(client.Target.public(), opener=op).execute("SELECT * WHERE { ?s ?p ?o }")
        req = op.calls[0]["request"]
        self.assertEqual(req.get_method(), "POST")
        self.assertIn("query=SELECT", req.data.decode().replace("+", " "))
        self.assertEqual(req.get_header("Accept"), "application/sparql-results+json")

    def test_per_query_timeout_overrides_client_default(self):
        op = opener_returning(FakeResponse())
        c = client.Client(client.Target.public(), timeout_s=300, opener=op)
        c.execute("SELECT 1", timeout_s=5)
        self.assertEqual(op.calls[0]["timeout"], 5)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
make perf-test
```

Expected: `ModuleNotFoundError: No module named 'pgperf.client'`

- [ ] **Step 3: Write the minimal implementation**

Create `perf/pgperf/client.py`:

```python
"""One SPARQL request over HTTP.

This module speaks HTTP/1.1, because urllib does and the spec keeps this
harness dependency-free (spec 3, 5.1). Production nginx has `http2 on`,
so a `public` run slightly overstates per-request overhead relative to a
real HTTP/2 client -- the public-minus-direct delta is an upper bound on
proxy cost. `http_version` is recorded on every sample so that gap stays
visible rather than implied.

All socket work goes through the injected `opener`, so nothing above this
module needs a network to be tested.
"""

import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request

from pgperf import fingerprint as fp
from pgperf import outcomes

PUBLIC_URL_ENV = "PGPERF_PUBLIC_URL"
DEFAULT_PUBLIC_URL = "https://packagegraph.di.riseproject.dev"

ACCEPT = "application/sparql-results+json"
HTTP_VERSION = "HTTP/1.1"


class Target:
    """Where requests go, and what may be sent there."""

    def __init__(self, mode, url, allows_token):
        self.mode = mode
        self.url = url
        self.allows_token = allows_token

    @classmethod
    def public(cls, url=None):
        resolved = url or os.environ.get(PUBLIC_URL_ENV) or DEFAULT_PUBLIC_URL
        # allows_token is False by construction: the proxy returns 403 for
        # access_token (defense in depth), so sending one is pure noise.
        return cls("public", resolved.rstrip("/"), allows_token=False)

    @classmethod
    def direct(cls, port=7001, host="127.0.0.1"):
        return cls("direct", f"http://{host}:{port}", allows_token=True)


def _default_opener(request, timeout):
    return urllib.request.urlopen(request, timeout=timeout)


class Client:
    def __init__(self, target, timeout_s=300, access_token=None, opener=None):
        self.target = target
        self.timeout_s = timeout_s
        self.access_token = access_token
        self._open = opener or _default_opener

    def _build_request(self, sparql):
        params = {"query": sparql}
        if self.access_token and self.target.allows_token:
            params["access-token"] = self.access_token
        data = urllib.parse.urlencode(params).encode()
        return urllib.request.Request(
            self.target.url,
            data=data,
            headers={"Accept": ACCEPT, "Content-Type": "application/x-www-form-urlencoded"},
            method="POST",
        )

    def execute(self, sparql, timeout_s=None, expect=None):
        """Execute one query and return a sample dict."""
        request = self._build_request(sparql)
        effective_timeout = self.timeout_s if timeout_s is None else timeout_s

        sample = {
            "outcome": None,
            "note": None,
            "correctness": None,
            "wall_ms": None,
            "engine_ms": None,
            "rows": None,
            "bytes": None,
            "fingerprint": None,
            "http_status": None,
            "http_version": HTTP_VERSION,
        }

        started = time.monotonic()
        status, headers, raw = None, {}, b""
        try:
            with self._open(request, effective_timeout) as response:
                status = getattr(response, "status", None)
                headers = dict(getattr(response, "headers", {}) or {})
                raw = response.read()
        except urllib.error.HTTPError as exc:
            status = exc.code
            headers = dict(exc.headers or {})
            try:
                raw = exc.read() or b""
            except Exception:  # noqa: BLE001 - a body-less HTTPError is normal
                raw = b""
        except TimeoutError:
            sample["wall_ms"] = (time.monotonic() - started) * 1000.0
            sample["outcome"] = outcomes.CLIENT_TIMEOUT
            sample["note"] = f"client timeout after {effective_timeout}s"
            return sample
        except urllib.error.URLError as exc:
            sample["wall_ms"] = (time.monotonic() - started) * 1000.0
            sample["outcome"] = outcomes.CONNECTION_ERROR
            sample["note"] = str(exc.reason)[:200]
            return sample

        sample["wall_ms"] = (time.monotonic() - started) * 1000.0
        sample["http_status"] = status
        sample["bytes"] = len(raw)

        try:
            body = json.loads(raw) if raw else None
        except (ValueError, UnicodeDecodeError):
            body = None

        outcome, note = outcomes.classify_http(status, headers, body)
        sample["outcome"] = outcome
        sample["note"] = note

        if outcome != outcomes.OK:
            return sample

        bindings = body.get("results", {}).get("bindings", [])
        sample["rows"] = len(bindings)
        sample["engine_ms"] = body.get("meta", {}).get("query-time-ms")
        sample["fingerprint"] = fp.fingerprint(bindings)
        sample["correctness"] = outcomes.check_cardinality(sample["rows"], expect)
        return sample
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
make perf-test
```

Expected: `OK` — 78 tests.

- [ ] **Step 5: Commit**

```bash
git add perf/pgperf/client.py perf/tests/test_client.py
git commit -m "feat(perf): add SPARQL client with target modes and sample capture"
```

---

### Task 6: `ssh.py`

One multiplexed connection, shared by cache control here and by Plan B's telemetry and run manifest. Multiplexing is correct on its own merits: `ice-cold` restarts the service before every sample, and paying TCP plus auth setup each time would add cost to a measurement about query latency (spec §5.4).

**Files:**
- Create: `perf/pgperf/ssh.py`
- Create: `perf/tests/test_ssh.py`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `SSH_HOST_ENV = "PGPERF_SSH_HOST"`, `DEFAULT_SSH_HOST = "root@51.159.171.16"`
  - `class SshError(Exception)`
  - `class Ssh(host=None, control_path=None, runner=None)` — context manager. `runner(argv, timeout)` returns `(returncode, stdout, stderr)`; defaults to `subprocess`.
  - `Ssh.run(command: str, timeout: int = 30, check: bool = True) -> str` — stdout, stripped. Raises `SshError` when `check` and returncode is non-zero.
  - `Ssh.argv(command: str) -> list[str]` — the exact argv, exposed for assertions.
  - `Ssh.available() -> bool` — a probe that never raises, so callers can degrade per spec §3.

- [ ] **Step 1: Write the failing test**

Create `perf/tests/test_ssh.py`:

```python
import unittest

from pgperf import ssh


class FakeRunner:
    def __init__(self, results=None):
        # results: list of (returncode, stdout, stderr), consumed in order.
        self.results = list(results or [(0, "ok\n", "")])
        self.calls = []

    def __call__(self, argv, timeout):
        self.calls.append({"argv": argv, "timeout": timeout})
        if len(self.results) > 1:
            return self.results.pop(0)
        return self.results[0]


class TestArgv(unittest.TestCase):
    def test_uses_multiplexing_flags(self):
        # One TCP connection for the whole run: ice-cold restarts the
        # service before every sample (spec 6), and reconnecting each time
        # would add setup cost to a latency measurement.
        conn = ssh.Ssh(host="root@example.test", control_path="/tmp/cp", runner=FakeRunner())
        argv = conn.argv("uptime")
        self.assertEqual(argv[0], "ssh")
        joined = " ".join(argv)
        self.assertIn("ControlMaster=auto", joined)
        self.assertIn("ControlPath=/tmp/cp", joined)
        self.assertIn("ControlPersist=", joined)
        self.assertIn("BatchMode=yes", joined)
        self.assertEqual(argv[-2:], ["root@example.test", "uptime"])

    def test_host_from_environment(self):
        conn = ssh.Ssh(host=None, runner=FakeRunner())
        self.assertIn(ssh.DEFAULT_SSH_HOST, conn.argv("true"))


class TestRun(unittest.TestCase):
    def test_returns_stripped_stdout(self):
        conn = ssh.Ssh(host="h", runner=FakeRunner([(0, "  hello  \n", "")]))
        self.assertEqual(conn.run("echo hello"), "hello")

    def test_raises_on_nonzero_when_checked(self):
        conn = ssh.Ssh(host="h", runner=FakeRunner([(255, "", "Permission denied")]))
        with self.assertRaises(ssh.SshError) as ctx:
            conn.run("true")
        self.assertIn("Permission denied", str(ctx.exception))

    def test_returns_stdout_on_nonzero_when_unchecked(self):
        conn = ssh.Ssh(host="h", runner=FakeRunner([(1, "partial", "warn")]))
        self.assertEqual(conn.run("true", check=False), "partial")

    def test_timeout_is_passed_through(self):
        runner = FakeRunner()
        ssh.Ssh(host="h", runner=runner).run("true", timeout=7)
        self.assertEqual(runner.calls[0]["timeout"], 7)


class TestAvailable(unittest.TestCase):
    def test_true_when_probe_succeeds(self):
        self.assertTrue(ssh.Ssh(host="h", runner=FakeRunner([(0, "ok", "")])).available())

    def test_false_and_never_raises_when_probe_fails(self):
        conn = ssh.Ssh(host="h", runner=FakeRunner([(255, "", "no route to host")]))
        self.assertFalse(conn.available())

    def test_false_when_runner_itself_explodes(self):
        def exploding(argv, timeout):
            raise OSError("ssh binary missing")

        self.assertFalse(ssh.Ssh(host="h", runner=exploding).available())


class TestContextManager(unittest.TestCase):
    def test_exit_tears_down_the_master(self):
        runner = FakeRunner()
        with ssh.Ssh(host="h", control_path="/tmp/cp", runner=runner) as conn:
            conn.run("true")
        joined = " ".join(runner.calls[-1]["argv"])
        self.assertIn("-O", joined)
        self.assertIn("exit", joined)

    def test_teardown_failure_is_swallowed(self):
        # Tearing down a connection that already died must not mask the
        # real work's result.
        runner = FakeRunner([(0, "", ""), (255, "", "no master")])
        with ssh.Ssh(host="h", runner=runner) as conn:
            conn.run("true")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
make perf-test
```

Expected: `ModuleNotFoundError: No module named 'pgperf.ssh'`

- [ ] **Step 3: Write the minimal implementation**

Create `perf/pgperf/ssh.py`:

```python
"""A single multiplexed SSH connection to the QLever host.

ControlMaster/ControlPersist means one TCP connection and one
authentication for an entire run, however many remote commands it issues.
That matters because ice-cold restarts qlever.service before every
individual sample (spec 6).

Every caller is expected to tolerate this being unavailable -- `available()`
never raises, and the profiler degrades rather than failing a run (spec 3).
"""

import os
import subprocess

SSH_HOST_ENV = "PGPERF_SSH_HOST"
DEFAULT_SSH_HOST = "root@51.159.171.16"

DEFAULT_CONTROL_PATH = "/tmp/pgperf-ssh-%C"
CONTROL_PERSIST = "120"


class SshError(Exception):
    """A remote command failed."""


def _subprocess_runner(argv, timeout):
    proc = subprocess.run(
        argv, capture_output=True, text=True, timeout=timeout, check=False
    )
    return proc.returncode, proc.stdout, proc.stderr


class Ssh:
    def __init__(self, host=None, control_path=None, runner=None):
        self.host = host or os.environ.get(SSH_HOST_ENV) or DEFAULT_SSH_HOST
        self.control_path = control_path or DEFAULT_CONTROL_PATH
        self._run_argv = runner or _subprocess_runner

    def argv(self, command):
        return [
            "ssh",
            "-o", "ControlMaster=auto",
            "-o", f"ControlPath={self.control_path}",
            "-o", f"ControlPersist={CONTROL_PERSIST}",
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=accept-new",
            self.host,
            command,
        ]

    def run(self, command, timeout=30, check=True):
        code, out, err = self._run_argv(self.argv(command), timeout)
        if check and code != 0:
            raise SshError(f"ssh {command!r} exited {code}: {err.strip()[:300]}")
        return out.strip()

    def available(self):
        """Probe the connection. Never raises."""
        try:
            self.run("true", timeout=15)
            return True
        except Exception:  # noqa: BLE001 - unavailability is a normal outcome
            return False

    def close(self):
        """Tear down the multiplexed master, ignoring failure."""
        try:
            self._run_argv(
                ["ssh", "-o", f"ControlPath={self.control_path}", "-O", "exit", self.host], 10
            )
        except Exception:  # noqa: BLE001 - teardown must not mask real results
            pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
make perf-test
```

Expected: `OK` — 90 tests.

- [ ] **Step 5: Commit**

```bash
git add perf/pgperf/ssh.py perf/tests/test_ssh.py
git commit -m "feat(perf): add multiplexed SSH helper with graceful unavailability"
```

---

### Task 7: `profile.py` — cache levels and the repetition loop

The two things to get right, both from spec §6: clearing happens before **every individual sample**, not once per pass, and `sync` precedes `drop_caches`. Both are easy to get subtly wrong and neither is visible in the output if you do.

**Files:**
- Create: `perf/pgperf/profile.py`
- Create: `perf/tests/test_profile.py`

**Interfaces:**
- Consumes: `pgperf.client`, `pgperf.ssh`, `pgperf.stats`, `pgperf.outcomes`.
- Produces:
  - `WARM, CACHE_COLD, ICE_COLD = "warm", "cache-cold", "ice-cold"`
  - `DEFAULT_REPETITIONS = {WARM: 5, CACHE_COLD: 5, ICE_COLD: 3}`
  - `class CacheController(level, conn=None, client=None, access_token=None)` with `.before_sample()`, `.warm_up(queries)`, `.readiness_delays: list[float]`
  - `run(queries, client, cache_level=WARM, repetitions=None, controller=None) -> dict` — `{"cache_level","repetitions","samples","queries","readiness_delays","warmup_samples"}` where `samples` is a flat list of per-sample dicts each carrying `query_id`, `repetition`, and the client's sample keys.
  - `aggregate(samples) -> dict` — per-query and per-class `stats.summarize` over `wall_ms`, plus outcome counts.

- [ ] **Step 1: Write the failing test**

Create `perf/tests/test_profile.py`:

```python
import unittest

from pgperf import outcomes, profile


class FakeClient:
    """Returns a scripted sample per call and records the queries seen."""

    def __init__(self, wall_ms_sequence=None):
        self.seen = []
        self.wall = list(wall_ms_sequence or [])
        self._n = 0

    def execute(self, sparql, timeout_s=None, expect=None):
        self.seen.append(sparql)
        wall = self.wall[self._n] if self._n < len(self.wall) else 10.0
        self._n += 1
        return {
            "outcome": outcomes.OK,
            "note": None,
            "correctness": None,
            "wall_ms": wall,
            "engine_ms": 3,
            "rows": 1,
            "bytes": 100,
            "fingerprint": "sha256:" + "0" * 64,
            "http_status": 200,
            "http_version": "HTTP/1.1",
        }


class RecordingController:
    def __init__(self):
        self.events = []
        self.readiness_delays = []

    def warm_up(self, queries):
        self.events.append("warm_up")

    def before_sample(self):
        self.events.append("clear")


def q(qid, cls="join", sparql=None):
    return {
        "id": qid,
        "class": cls,
        "sparql": sparql or f"SELECT * # {qid}",
        "timeout_s": 300,
        "expect": None,
    }


class TestRunLoop(unittest.TestCase):
    def test_emits_one_sample_per_query_per_repetition(self):
        queries = [q("A"), q("B")]
        result = profile.run(queries, FakeClient(), cache_level=profile.WARM, repetitions=3)
        self.assertEqual(len(result["samples"]), 6)
        self.assertEqual(
            [(s["query_id"], s["repetition"]) for s in result["samples"]],
            [("A", 1), ("A", 2), ("A", 3), ("B", 1), ("B", 2), ("B", 3)],
        )

    def test_repetition_default_depends_on_cache_level(self):
        for level, expected in profile.DEFAULT_REPETITIONS.items():
            got = profile.run([q("A")], FakeClient(), cache_level=level,
                              controller=RecordingController())
            self.assertEqual(got["repetitions"], expected, level)

    def test_records_cache_level(self):
        got = profile.run([q("A")], FakeClient(), cache_level=profile.WARM, repetitions=1)
        self.assertEqual(got["cache_level"], profile.WARM)

    def test_rejects_unknown_cache_level(self):
        with self.assertRaises(ValueError):
            profile.run([q("A")], FakeClient(), cache_level="tepid")


class TestClearingGranularity(unittest.TestCase):
    """Spec 6: the clearing action runs before EVERY individual sample.
    Clearing once per pass would yield one cold sample and N-1 warm ones
    averaged together -- the exact confusion these levels exist to prevent."""

    def test_cold_levels_clear_before_every_sample(self):
        ctrl = RecordingController()
        profile.run([q("A"), q("B")], FakeClient(), cache_level=profile.CACHE_COLD,
                    repetitions=2, controller=ctrl)
        self.assertEqual(ctrl.events, ["clear"] * 4)

    def test_warm_warms_once_and_never_clears(self):
        ctrl = RecordingController()
        profile.run([q("A"), q("B")], FakeClient(), cache_level=profile.WARM,
                    repetitions=2, controller=ctrl)
        self.assertEqual(ctrl.events, ["warm_up"])

    def test_warmup_samples_are_kept_not_discarded_silently(self):
        # Spec 6: the discarded pass is recorded in the run artifacts.
        # Quietly dropping samples is how benchmarks become irreproducible.
        result = profile.run([q("A")], FakeClient(), cache_level=profile.WARM, repetitions=1)
        self.assertEqual(len(result["warmup_samples"]), 1)
        self.assertEqual(len(result["samples"]), 1)


class TestAggregate(unittest.TestCase):
    def _samples(self):
        return [
            {"query_id": "A", "class": "join", "wall_ms": 10.0, "outcome": outcomes.OK,
             "correctness": None},
            {"query_id": "A", "class": "join", "wall_ms": 20.0, "outcome": outcomes.OK,
             "correctness": None},
            {"query_id": "B", "class": "aggregate", "wall_ms": 100.0,
             "outcome": outcomes.PROXY_TIMEOUT, "correctness": None},
        ]

    def test_per_query_and_per_class(self):
        got = profile.aggregate(self._samples())
        self.assertEqual(got["queries"]["A"]["wall_ms"]["n"], 2)
        self.assertEqual(got["queries"]["A"]["wall_ms"]["mean"], 15.0)
        self.assertEqual(got["classes"]["join"]["wall_ms"]["n"], 2)
        self.assertEqual(got["classes"]["aggregate"]["wall_ms"]["n"], 1)

    def test_outcome_counts(self):
        got = profile.aggregate(self._samples())
        self.assertEqual(got["queries"]["A"]["outcomes"], {outcomes.OK: 2})
        self.assertEqual(got["queries"]["B"]["outcomes"], {outcomes.PROXY_TIMEOUT: 1})

    def test_failed_samples_excluded_from_latency(self):
        # A query that did not answer is not measuring the same work.
        got = profile.aggregate(self._samples())
        self.assertEqual(got["queries"]["B"]["wall_ms"]["n"], 0)

    def test_correctness_failures_excluded_from_latency(self):
        samples = [
            {"query_id": "C", "class": "join", "wall_ms": 5.0, "outcome": outcomes.OK,
             "correctness": outcomes.CARDINALITY_VIOLATION},
        ]
        got = profile.aggregate(samples)
        self.assertEqual(got["queries"]["C"]["wall_ms"]["n"], 0)
        self.assertEqual(got["queries"]["C"]["outcomes"], {outcomes.OK: 1})


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
make perf-test
```

Expected: `ModuleNotFoundError: No module named 'pgperf.profile'`

- [ ] **Step 3: Write the minimal implementation**

Create `perf/pgperf/profile.py`:

```python
"""Cache levels and the serial repetition loop.

Two details here are load-bearing and easy to get silently wrong:

1. For the cold levels the clearing action runs before EVERY individual
   sample, not once per pass. Clearing once and then running N
   repetitions produces one cold sample and N-1 warm ones averaged
   together (spec 6).
2. `sync` precedes `drop_caches`. drop_caches frees only clean pages, so
   without a sync the "cold" condition is neither achieved nor
   reproducible (spec 6).
"""

import collections
import time

from pgperf import outcomes, stats

WARM = "warm"
CACHE_COLD = "cache-cold"
ICE_COLD = "ice-cold"

LEVELS = (WARM, CACHE_COLD, ICE_COLD)

# ice-cold pays a service restart plus a readiness wait per sample, so it
# defaults lower. The pass's total wall time is reported so the cost is
# visible up front.
DEFAULT_REPETITIONS = {WARM: 5, CACHE_COLD: 5, ICE_COLD: 3}

READINESS_QUERY = "SELECT * WHERE { ?s ?p ?o } LIMIT 1"
READINESS_ATTEMPTS = 30
READINESS_INTERVAL_S = 10


class CacheController:
    """Applies the selected cache level. Requires SSH for ice-cold."""

    def __init__(self, level, conn=None, client=None, access_token=None, sleep=time.sleep):
        if level not in LEVELS:
            raise ValueError(f"unknown cache level {level!r}; expected one of {list(LEVELS)}")
        self.level = level
        self.conn = conn
        self.client = client
        self.access_token = access_token
        self.readiness_delays = []
        self._sleep = sleep

    def warm_up(self, queries):
        """Prime the cache with one discarded pass. Only meaningful for warm."""
        samples = []
        if self.level != WARM or self.client is None:
            return samples
        for query in queries:
            samples.append(
                dict(
                    self.client.execute(
                        query["sparql"], timeout_s=query.get("timeout_s"), expect=None
                    ),
                    query_id=query["id"],
                    warmup=True,
                )
            )
        return samples

    def before_sample(self):
        if self.level == WARM:
            return
        if self.level == CACHE_COLD:
            self._clear_query_cache()
            return
        self._restart_and_drop_caches()

    def _clear_query_cache(self):
        # Requires the access token, which the proxy rejects -- so this
        # level implies a direct target (spec 6).
        self.client.execute("", timeout_s=30)

    def _restart_and_drop_caches(self):
        self.conn.run("systemctl restart qlever.service", timeout=180)
        # sync first: drop_caches frees only clean pages.
        self.conn.run("sync && echo 3 > /proc/sys/vm/drop_caches", timeout=120)
        self.readiness_delays.append(self._wait_for_readiness())

    def _wait_for_readiness(self):
        started = time.monotonic()
        for _ in range(READINESS_ATTEMPTS):
            sample = self.client.execute(READINESS_QUERY, timeout_s=30)
            if sample["outcome"] == outcomes.OK:
                return time.monotonic() - started
            self._sleep(READINESS_INTERVAL_S)
        raise RuntimeError(
            f"qlever did not become ready within "
            f"{READINESS_ATTEMPTS * READINESS_INTERVAL_S}s after restart"
        )


def run(queries, client, cache_level=WARM, repetitions=None, controller=None):
    """Execute the corpus serially at the given cache level."""
    if cache_level not in LEVELS:
        raise ValueError(f"unknown cache level {cache_level!r}; expected one of {list(LEVELS)}")

    reps = repetitions or DEFAULT_REPETITIONS[cache_level]
    ctrl = controller if controller is not None else CacheController(cache_level, client=client)

    started = time.monotonic()
    warmup_samples = ctrl.warm_up(queries)

    samples = []
    for query in queries:
        for repetition in range(1, reps + 1):
            ctrl.before_sample()
            sample = client.execute(
                query["sparql"],
                timeout_s=query.get("timeout_s"),
                expect=query.get("expect"),
            )
            sample["query_id"] = query["id"]
            sample["class"] = query.get("class")
            sample["repetition"] = repetition
            samples.append(sample)

    return {
        "cache_level": cache_level,
        "repetitions": reps,
        "samples": samples,
        "warmup_samples": warmup_samples,
        "readiness_delays": list(getattr(ctrl, "readiness_delays", [])),
        "pass_wall_s": time.monotonic() - started,
    }


def _is_measurable(sample):
    """Only a fully successful sample measures query latency."""
    return sample.get("outcome") == outcomes.OK and sample.get("correctness") is None


def _group(samples, key):
    buckets = collections.defaultdict(list)
    for sample in samples:
        buckets[sample.get(key)].append(sample)
    return buckets


def _summarize_bucket(bucket):
    latencies = [s["wall_ms"] for s in bucket if _is_measurable(s) and s.get("wall_ms") is not None]
    counts = collections.Counter(s.get("outcome") for s in bucket)
    correctness = collections.Counter(
        s["correctness"] for s in bucket if s.get("correctness")
    )
    return {
        "wall_ms": stats.summarize(latencies),
        "outcomes": dict(counts),
        "correctness": dict(correctness),
    }


def aggregate(samples):
    """Per-query and per-class aggregates.

    Per-query is primary: the point is finding the queries that are
    unusable, and any blended figure hides exactly those.
    """
    return {
        "queries": {qid: _summarize_bucket(b) for qid, b in _group(samples, "query_id").items()},
        "classes": {cls: _summarize_bucket(b) for cls, b in _group(samples, "class").items()},
        "overall": _summarize_bucket(samples),
    }
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
make perf-test
```

Expected: `OK` — 103 tests.

- [ ] **Step 5: Commit**

```bash
git add perf/pgperf/profile.py perf/tests/test_profile.py
git commit -m "feat(perf): add cache levels and serial repetition loop"
```

---

### Task 8: `manifestgen.py` — corpus migration and determinism measurement

Turns `spike/cq-queries.json` into 65 `.rq` files plus a manifest, and measures which queries are stable enough to gate on results (spec §4.6). The generation logic is tested with a fake client; the real run against the endpoint is a manual step at the end.

**Files:**
- Create: `perf/pgperf/manifestgen.py`
- Create: `perf/tests/test_manifestgen.py`

**Interfaces:**
- Consumes: `pgperf.client`, `pgperf.fingerprint`, `pgperf.corpus`.
- Produces:
  - `detect_features(sparql: str) -> list[str]`
  - `classify_query(sparql: str) -> str` — one of `corpus.CLASSES`.
  - `measure_determinism(sparql, client, attempts=3) -> tuple[bool, str | None, str | None]` — `(deterministic, fingerprint_sha, note)`.
  - `build(source_json, out_dir, client, index_hash) -> dict` — writes `.rq` files and returns the manifest document.

- [ ] **Step 1: Write the failing test**

Create `perf/tests/test_manifestgen.py`:

```python
import json
import os
import tempfile
import unittest

from pgperf import corpus, manifestgen, outcomes


class ScriptedClient:
    """Returns a scripted bindings list per call."""

    def __init__(self, bindings_sequence):
        self.sequence = list(bindings_sequence)
        self.calls = 0

    def execute(self, sparql, timeout_s=None, expect=None):
        bindings = self.sequence[min(self.calls, len(self.sequence) - 1)]
        self.calls += 1
        if bindings is None:
            return {"outcome": outcomes.SPARQL_ERROR, "note": "boom", "rows": None,
                    "fingerprint": None, "correctness": None}
        from pgperf import fingerprint as fp

        return {
            "outcome": outcomes.OK,
            "note": None,
            "rows": len(bindings),
            "fingerprint": fp.fingerprint(bindings),
            "correctness": None,
        }


URI_A = [{"s": {"type": "uri", "value": "http://example.org/a"}}]
URI_B = [{"s": {"type": "uri", "value": "http://example.org/b"}}]


class TestDetectFeatures(unittest.TestCase):
    def test_finds_constructs(self):
        sparql = "SELECT ?x WHERE { ?x ^p:q ?y OPTIONAL { ?y p:r ?z } } GROUP BY ?x ORDER BY ?x"
        got = manifestgen.detect_features(sparql)
        for feature in ("OPTIONAL", "GROUP BY", "ORDER BY", "inverse-path"):
            self.assertIn(feature, got)

    def test_detects_property_path(self):
        self.assertIn("property-path", manifestgen.detect_features("SELECT ?a { ?x p:dep+ ?a }"))

    def test_no_false_positives_on_a_bare_lookup(self):
        got = manifestgen.detect_features("SELECT ?o WHERE { <http://x> <http://y> ?o }")
        self.assertEqual(got, [])


class TestClassifyQuery(unittest.TestCase):
    def test_aggregate_wins_over_join(self):
        sparql = "SELECT ?t (COUNT(*) AS ?n) WHERE { ?s a ?t . ?s p:q ?r } GROUP BY ?t"
        self.assertEqual(manifestgen.classify_query(sparql), "aggregate")

    def test_path(self):
        self.assertEqual(manifestgen.classify_query("SELECT ?a { <x> p:dep+ ?a }"), "path")

    def test_graph(self):
        self.assertEqual(manifestgen.classify_query("SELECT ?g { GRAPH ?g { ?s ?p ?o } }"), "graph")

    def test_lookup_for_a_single_bound_pattern(self):
        self.assertEqual(
            manifestgen.classify_query("SELECT ?o WHERE { <http://x> <http://y> ?o }"), "lookup"
        )

    def test_every_class_is_a_valid_corpus_class(self):
        for sparql in (
            "SELECT ?o WHERE { <x> <y> ?o }",
            "SELECT ?t (COUNT(*) AS ?n) { ?s a ?t } GROUP BY ?t",
            "SELECT ?a { <x> p:dep+ ?a }",
            "SELECT ?g { GRAPH ?g { ?s ?p ?o } }",
            "SELECT ?x ?y { ?x p:a ?m . ?m p:b ?y }",
        ):
            self.assertIn(manifestgen.classify_query(sparql), corpus.CLASSES)


class TestMeasureDeterminism(unittest.TestCase):
    def test_stable_across_three_attempts(self):
        client = ScriptedClient([URI_A, URI_A, URI_A])
        deterministic, sha, note = manifestgen.measure_determinism("SELECT 1", client)
        self.assertTrue(deterministic)
        self.assertTrue(sha.startswith("sha256:"))
        self.assertIsNone(note)
        self.assertEqual(client.calls, 3)

    def test_unstable_is_marked_nondeterministic(self):
        # ORDER BY ... LIMIT n over a tied sort key returns a different
        # SET between runs. Gating those makes the gate flap (spec 4.6).
        client = ScriptedClient([URI_A, URI_B, URI_A])
        deterministic, sha, note = manifestgen.measure_determinism("SELECT 1", client)
        self.assertFalse(deterministic)
        self.assertIsNone(sha)
        self.assertIn("differed", note.lower())

    def test_failed_query_is_nondeterministic_with_a_note(self):
        client = ScriptedClient([None])
        deterministic, sha, note = manifestgen.measure_determinism("SELCT 1", client)
        self.assertFalse(deterministic)
        self.assertIsNone(sha)
        self.assertIn("SPARQL_ERROR", note)


class TestBuild(unittest.TestCase):
    def _source(self, tmp):
        path = os.path.join(tmp, "cq.json")
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(
                [
                    {"id": "CQ-A-01", "title": "Alpha", "query": "SELECT ?o WHERE { <x> <y> ?o }"},
                    {"id": "CQ-B-02", "title": "Beta",
                     "query": "SELECT ?t (COUNT(*) AS ?n) { ?s a ?t } GROUP BY ?t"},
                ],
                fh,
            )
        return path

    def test_writes_rq_files_and_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "corpus")
            doc = manifestgen.build(
                self._source(tmp), out, ScriptedClient([URI_A]), index_hash="deadbeefdeadbeef"
            )
            self.assertEqual(doc["schema_version"], corpus.SCHEMA_VERSION)
            self.assertEqual(len(doc["queries"]), 2)
            self.assertTrue(os.path.isfile(os.path.join(out, "cq", "CQ-A-01.rq")))
            with open(os.path.join(out, "cq", "CQ-A-01.rq"), encoding="utf-8") as fh:
                self.assertIn("SELECT ?o", fh.read())

    def test_fingerprint_carries_the_index_hash(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "corpus")
            doc = manifestgen.build(
                self._source(tmp), out, ScriptedClient([URI_A]), index_hash="deadbeefdeadbeef"
            )
            fp = doc["queries"][0]["fingerprint"]
            self.assertEqual(fp["index_hash"], "deadbeefdeadbeef")

    def test_output_passes_corpus_validation(self):
        # The generator and the loader must agree, or the manifest is
        # unusable the moment it is written.
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "corpus")
            doc = manifestgen.build(
                self._source(tmp), out, ScriptedClient([URI_A]), index_hash="deadbeefdeadbeef"
            )
            manifest_path = os.path.join(out, "manifest.json")
            with open(manifest_path, "w", encoding="utf-8") as fh:
                json.dump(doc, fh, indent=2)
            loaded = corpus.load(manifest_path)
            self.assertEqual(len(loaded), 2)

    def test_nondeterministic_query_gets_null_fingerprint(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "corpus")
            doc = manifestgen.build(
                self._source(tmp),
                out,
                ScriptedClient([URI_A, URI_B, URI_A]),
                index_hash="deadbeefdeadbeef",
            )
            first = doc["queries"][0]
            self.assertFalse(first["deterministic"])
            self.assertIsNone(first["fingerprint"])
            self.assertIn("nondeterminism_note", first)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
make perf-test
```

Expected: `ModuleNotFoundError: No module named 'pgperf.manifestgen'`

- [ ] **Step 3: Write the minimal implementation**

Create `perf/pgperf/manifestgen.py`:

```python
"""Generate the corpus from spike/cq-queries.json and measure determinism.

Determinism is measured, not asserted (spec 4.6). Each query runs three
times in immediate succession against one index; any variation in the
canonical fingerprint marks it non-deterministic, which excludes it from
result gating while keeping it timed and cardinality-checked.

Three executions is a heuristic, not a proof. A rare tie can pass here
and flap later -- see spec 4.6 for the triage path.
"""

import json
import os
import re

from pgperf import corpus, outcomes

ATTEMPTS = 3

_FEATURE_PATTERNS = [
    ("OPTIONAL", r"\bOPTIONAL\b"),
    ("UNION", r"\bUNION\b"),
    ("MINUS", r"\bMINUS\b"),
    ("EXISTS", r"\bEXISTS\b"),
    ("GROUP BY", r"\bGROUP\s+BY\b"),
    ("ORDER BY", r"\bORDER\s+BY\b"),
    ("HAVING", r"\bHAVING\b"),
    ("VALUES", r"\bVALUES\b"),
    ("BIND", r"\bBIND\s*\("),
    ("DISTINCT", r"\bDISTINCT\b"),
    ("COUNT", r"\bCOUNT\s*\("),
    ("SERVICE", r"\bSERVICE\b"),
    ("GRAPH", r"\bGRAPH\b"),
    ("REGEX", r"\bREGEX\s*\("),
    ("CONTAINS", r"\bCONTAINS\s*\("),
    ("LIMIT", r"\bLIMIT\b"),
    ("inverse-path", r"\^[A-Za-z_][\w.-]*:"),
    ("property-path", r"[A-Za-z_][\w.-]*:[A-Za-z_]\w*\s*[+*]"),
]


def detect_features(sparql):
    upper = sparql.upper()
    found = []
    for name, pattern in _FEATURE_PATTERNS:
        haystack = sparql if name in ("inverse-path", "property-path") else upper
        flags = 0 if name in ("inverse-path", "property-path") else re.IGNORECASE
        if re.search(pattern, haystack, flags):
            found.append(name)
    return found


def classify_query(sparql):
    """Assign an operational class. Most specific wins."""
    features = set(detect_features(sparql))
    if "SERVICE" in features:
        return "join"
    if "GRAPH" in features:
        return "graph"
    if "property-path" in features:
        return "path"
    if {"REGEX", "CONTAINS"} & features:
        return "text"
    if {"GROUP BY", "COUNT", "HAVING"} & features:
        return "aggregate"
    # A single triple pattern with a bound subject is a lookup; anything
    # with multiple patterns is a join.
    patterns = sparql.count(" .") + sparql.count(";")
    if patterns <= 1 and re.search(r"\{\s*<", sparql):
        return "lookup"
    return "join"


def measure_determinism(sparql, client, attempts=ATTEMPTS):
    """Run the query `attempts` times; report whether the fingerprint held."""
    seen = []
    for _ in range(attempts):
        sample = client.execute(sparql)
        if sample["outcome"] != outcomes.OK:
            return False, None, f"{sample['outcome']}: {sample.get('note')}"
        seen.append(sample["fingerprint"])

    if len(set(seen)) == 1:
        return True, seen[0], None
    return (
        False,
        None,
        f"fingerprints differed across {attempts} executions "
        f"({len(set(seen))} distinct); likely ORDER BY ... LIMIT over a tied sort key",
    )


def build(source_json, out_dir, client, index_hash, attempts=ATTEMPTS):
    """Write .rq files under out_dir/cq/ and return the manifest document."""
    with open(source_json, encoding="utf-8") as fh:
        source = json.load(fh)

    cq_dir = os.path.join(out_dir, "cq")
    os.makedirs(cq_dir, exist_ok=True)

    queries = []
    for entry in source:
        qid = entry["id"]
        sparql = entry["query"]

        rel = os.path.join("cq", f"{qid}.rq")
        with open(os.path.join(out_dir, rel), "w", encoding="utf-8") as fh:
            fh.write(sparql if sparql.endswith("\n") else sparql + "\n")

        deterministic, sha, note = measure_determinism(sparql, client, attempts)

        query = {
            "id": qid,
            "title": entry.get("title", ""),
            "file": rel.replace(os.sep, "/"),
            "class": classify_query(sparql),
            "features": detect_features(sparql),
            "expect": {"min_rows": None, "max_rows": None},
            "max_row_drift_pct": 25,
            "fingerprint": (
                {"index_hash": index_hash, "sha256": sha} if deterministic else None
            ),
            "deterministic": deterministic,
            "timeout_s": corpus.DEFAULT_TIMEOUT_S,
            "weight": 1,
            "enabled": True,
        }
        if note:
            query["nondeterminism_note"] = note
        queries.append(query)

    return {"schema_version": corpus.SCHEMA_VERSION, "index_hash": index_hash, "queries": queries}
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
make perf-test
```

Expected: `OK` — 119 tests.

- [ ] **Step 5: Sanity-check the classifier against the real corpus**

The classifier was validated against all 65 CQs while this plan was written, and produces: **42 `join`, 17 `aggregate`, 3 `path`, 3 `graph`** — no invalid classes. Confirm the same distribution after Step 10 generates the manifest:

```bash
python3 -c "
import json, collections
d = json.load(open('perf/corpus/manifest.json'))
print(collections.Counter(q['class'] for q in d['queries']).most_common())
"
```

**Note what is absent: zero `lookup` and zero `text` queries.** That is a real property of this corpus, not a classifier bug — every CQ is a multi-pattern analytical query, and none does a cheap single-pattern point lookup or a text scan. Two consequences worth carrying forward:

- Per-class p95 reporting will have only four populated classes, so the `lookup` and `text` rails have nothing to draw on.
- A weighted load profile built only from this corpus would contain no cheap queries at all, which is not what traffic against a public endpoint looks like. That gap belongs in the adversarial and representative sets, and it is worth raising when the load spec is written rather than discovering it mid-ramp.

- [ ] **Step 6: Commit**

```bash
git add perf/pgperf/manifestgen.py perf/tests/test_manifestgen.py
git commit -m "feat(perf): generate corpus from spike CQs and measure determinism"
```

---

### Task 9: CLI, container, and documentation

Wires the modules into `pgperf manifest` and `pgperf profile`, packages them, and documents the invocation. `expect` bounds come out of `manifestgen` as `null`, so the last step is a real run against the endpoint to populate them from observed data — the plan cannot guess them.

**Files:**
- Create: `perf/pgperf/__main__.py`
- Create: `perf/tests/test_cli.py`
- Create: `perf/Containerfile.perf`
- Create: `perf/README.md`
- Modify: `Makefile`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: every module above.
- Produces: `main(argv: list[str]) -> int` — process exit code.

- [ ] **Step 1: Write the failing test**

Create `perf/tests/test_cli.py`:

```python
import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

from pgperf import __main__ as cli


class TestArgParsing(unittest.TestCase):
    def test_no_subcommand_is_usage_error(self):
        buf = io.StringIO()
        with redirect_stderr(buf):
            self.assertEqual(cli.main([]), 2)

    def test_unknown_subcommand_is_usage_error(self):
        buf = io.StringIO()
        with redirect_stderr(buf):
            self.assertEqual(cli.main(["frobnicate"]), 2)

    def test_profile_rejects_cache_cold_on_public(self):
        # cache-cold needs the access token, which the proxy 403s, so the
        # combination is impossible rather than merely unwise (spec 6).
        buf = io.StringIO()
        with redirect_stderr(buf):
            code = cli.main(
                ["profile", "--target", "public", "--cache-level", "cache-cold",
                 "--corpus", "/nonexistent/manifest.json"]
            )
        self.assertEqual(code, 2)
        self.assertIn("cache-cold", buf.getvalue())
        self.assertIn("direct", buf.getvalue())

    def test_profile_rejects_unknown_cache_level(self):
        buf = io.StringIO()
        with redirect_stderr(buf):
            self.assertEqual(
                cli.main(["profile", "--cache-level", "tepid", "--corpus", "x"]), 2
            )

    def test_missing_corpus_reports_cleanly(self):
        buf = io.StringIO()
        with redirect_stderr(buf):
            code = cli.main(["profile", "--corpus", "/nonexistent/manifest.json"])
        self.assertEqual(code, 1)
        self.assertIn("manifest.json", buf.getvalue())


class TestRunIdAndOutput(unittest.TestCase):
    def test_run_id_encodes_target_and_cache_level(self):
        run_id = cli.build_run_id("public", "warm", now="20260909T142211Z")
        self.assertEqual(run_id, "public-warm-20260909T142211Z")

    def test_write_run_creates_expected_artifacts(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = {
                "cache_level": "warm",
                "repetitions": 5,
                "samples": [{"query_id": "A", "wall_ms": 1.0, "outcome": "OK",
                             "correctness": None, "class": "join"}],
                "warmup_samples": [],
                "readiness_delays": [],
                "pass_wall_s": 1.5,
            }
            out = cli.write_run(tmp, "public-warm-X", result, {"target": "public"})
            self.assertTrue(os.path.isdir(out))
            for name in ("samples.json", "summary.json", "run.json"):
                self.assertTrue(os.path.isfile(os.path.join(out, name)), name)
            with open(os.path.join(out, "summary.json"), encoding="utf-8") as fh:
                summary = json.load(fh)
            self.assertIn("queries", summary)
            self.assertIn("overall", summary)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
make perf-test
```

Expected: `ModuleNotFoundError: No module named 'pgperf.__main__'`

- [ ] **Step 3: Write the CLI**

Create `perf/pgperf/__main__.py`:

```python
"""pgperf CLI.

Subcommands:
  manifest  -- generate perf/corpus/ from spike/cq-queries.json
  profile   -- run the corpus serially and write a run directory

Run identity beyond target/cache-level/corpus (the server-side manifest,
host telemetry, and the compare gate) is Plan B. `run.json` here holds
only what this plan captures.
"""

import argparse
import datetime
import json
import os
import sys

from pgperf import client as client_mod
from pgperf import corpus, manifestgen, profile, ssh


def build_run_id(target, cache_level, now=None):
    stamp = now or datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return f"{target}-{cache_level}-{stamp}"


def write_run(out_root, run_id, result, meta):
    out_dir = os.path.join(out_root, run_id)
    os.makedirs(out_dir, exist_ok=True)

    with open(os.path.join(out_dir, "samples.json"), "w", encoding="utf-8") as fh:
        json.dump(
            {"samples": result["samples"], "warmup_samples": result["warmup_samples"]},
            fh,
            indent=2,
        )

    summary = profile.aggregate(result["samples"])
    summary["readiness_delays"] = result["readiness_delays"]
    summary["pass_wall_s"] = result["pass_wall_s"]
    with open(os.path.join(out_dir, "summary.json"), "w", encoding="utf-8") as fh:
        json.dump(summary, fh, indent=2)

    with open(os.path.join(out_dir, "run.json"), "w", encoding="utf-8") as fh:
        json.dump(
            dict(meta, run_id=run_id, cache_level=result["cache_level"],
                 repetitions=result["repetitions"]),
            fh,
            indent=2,
        )

    return out_dir


def _parser():
    parser = argparse.ArgumentParser(prog="pgperf")
    sub = parser.add_subparsers(dest="command")

    gen = sub.add_parser("manifest", help="generate the corpus from spike CQs")
    gen.add_argument("--import-from", default="spike/cq-queries.json")
    gen.add_argument("--out", default="perf/corpus")
    gen.add_argument("--index-hash", required=True,
                     help="the .loaded content hash this corpus was measured against")
    gen.add_argument("--attempts", type=int, default=manifestgen.ATTEMPTS)

    run = sub.add_parser("profile", help="profile the corpus")
    run.add_argument("--target", choices=("public", "direct"), default="public")
    run.add_argument("--cache-level", default=profile.WARM)
    run.add_argument("--corpus", default="perf/corpus/manifest.json")
    run.add_argument("--out", default="output/perf")
    run.add_argument("--repetitions", type=int, default=None)
    run.add_argument("--include-adversarial", action="store_true")
    run.add_argument("--ids", nargs="*", default=None)

    return parser


def _make_client(target_name, timeout_s=300):
    token = os.environ.get("QLEVER_ACCESS_TOKEN") or None
    if target_name == "public":
        return client_mod.Client(client_mod.Target.public(), timeout_s=timeout_s)
    return client_mod.Client(
        client_mod.Target.direct(), timeout_s=timeout_s, access_token=token
    )


def _cmd_manifest(args):
    cli_client = _make_client("direct")
    doc = manifestgen.build(
        args.import_from, args.out, cli_client, args.index_hash, attempts=args.attempts
    )
    os.makedirs(args.out, exist_ok=True)
    path = os.path.join(args.out, "manifest.json")
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(doc, fh, indent=2)
    nondet = [q["id"] for q in doc["queries"] if not q["deterministic"]]
    print(f"wrote {len(doc['queries'])} queries to {path}")
    print(f"non-deterministic ({len(nondet)}): {', '.join(nondet) or 'none'}")
    return 0


def _cmd_profile(args):
    if args.cache_level not in profile.LEVELS:
        print(
            f"unknown --cache-level {args.cache_level!r}; expected one of {list(profile.LEVELS)}",
            file=sys.stderr,
        )
        return 2
    if args.cache_level == profile.CACHE_COLD and args.target == "public":
        print(
            "cache-cold requires the QLever access token, which the public proxy "
            "rejects by design; use --target direct",
            file=sys.stderr,
        )
        return 2

    try:
        queries = corpus.select(
            corpus.load(args.corpus),
            include_adversarial=args.include_adversarial,
            ids=args.ids,
        )
    except (OSError, corpus.CorpusError) as exc:
        print(f"corpus error: {exc}", file=sys.stderr)
        return 1

    run_client = _make_client(args.target)
    conn = ssh.Ssh() if args.cache_level == profile.ICE_COLD else None
    controller = profile.CacheController(args.cache_level, conn=conn, client=run_client)

    result = profile.run(
        queries,
        run_client,
        cache_level=args.cache_level,
        repetitions=args.repetitions,
        controller=controller,
    )

    run_id = build_run_id(args.target, args.cache_level)
    meta = {
        "target": args.target,
        "endpoint": run_client.target.url,
        "corpus": os.path.abspath(args.corpus),
        "corpus_hash": corpus.manifest_hash(args.corpus),
        "query_count": len(queries),
        "http_version": client_mod.HTTP_VERSION,
    }
    out_dir = write_run(args.out, run_id, result, meta)
    if conn is not None:
        conn.close()

    print(f"wrote {out_dir}")
    print(f"{len(result['samples'])} samples in {result['pass_wall_s']:.1f}s")
    return 0


def main(argv=None):
    args = _parser().parse_args(argv if argv is not None else sys.argv[1:])
    if args.command == "manifest":
        return _cmd_manifest(args)
    if args.command == "profile":
        return _cmd_profile(args)
    print("a subcommand is required: manifest | profile", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
make perf-test
```

Expected: `OK` — 126 tests.

- [ ] **Step 5: Write the Containerfile**

Create `perf/Containerfile.perf`:

```dockerfile
# Performance harness for the PackageGraph QLever endpoint.
#
# Stdlib-only Python (see docs/superpowers/specs/2026-09-09-sparql-perf-harness-foundation-design.md
# section 3), so there is no dependency install step -- only python3 and an
# ssh client for cache control and (Plan B) host telemetry.
FROM docker.io/python:3.12-slim-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-client \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /perf
COPY pgperf /perf/pgperf

RUN useradd --create-home --uid 1000 pgperf
USER 1000

ENV PYTHONPATH=/perf
ENV PYTHONUNBUFFERED=1

ENTRYPOINT ["python3", "-m", "pgperf"]
CMD ["--help"]
```

- [ ] **Step 6: Add Make targets and ignore the output directory**

Append to the perf block in `Makefile`, adding these names to `.PHONY`:

```makefile
PGPERF_IMAGE = $(REGISTRY)/pgperf

perf-build:
	podman build -t $(PGPERF_IMAGE):$(TAG) -f perf/Containerfile.perf perf/

# Serial profile against the public endpoint. TARGET=direct requires an SSH
# tunnel to 127.0.0.1:7001; CACHE_LEVEL=ice-cold requires root SSH.
TARGET ?= public
CACHE_LEVEL ?= warm
perf-profile:
	podman run --rm \
	  -v ./perf:/perf:ro,Z \
	  -v ./output/perf:/output:Z \
	  -v $(HOME)/.ssh:/home/pgperf/.ssh:ro,Z \
	  --network host \
	  $(PGPERF_IMAGE):$(TAG) \
	  profile --target $(TARGET) --cache-level $(CACHE_LEVEL) \
	          --corpus /perf/corpus/manifest.json --out /output
```

Add to `.gitignore`:

```
output/perf/
```

- [ ] **Step 7: Verify the container builds and the CLI responds**

```bash
make perf-build
podman run --rm ghcr.io/packagegraph/pgperf:latest --help
```

Expected: the argparse usage block listing `manifest` and `profile`.

- [ ] **Step 8: Write the README**

Create `perf/README.md`:

```markdown
# SPARQL performance harness (profiler)

Serial profiler for the PackageGraph QLever endpoint. Design:
[`docs/superpowers/specs/2026-09-09-sparql-perf-harness-foundation-design.md`](../docs/superpowers/specs/2026-09-09-sparql-perf-harness-foundation-design.md).

Stdlib-only Python 3.12. No dependency install, no virtualenv, no pytest.

## Test

    make perf-test

## Generate the corpus

Needs an SSH tunnel to the server and the index's content hash, which pins
the fingerprints to the index they were measured against:

    ssh -N -L 7001:127.0.0.1:7001 root@51.159.171.16 &
    INDEX_HASH=$(ssh root@51.159.171.16 cat /var/lib/packagegraph/qlever-data/index/.loaded)
    python3 -m pgperf manifest --index-hash "$INDEX_HASH"

Writes `perf/corpus/cq/*.rq` and `perf/corpus/manifest.json`, reporting which
queries came out non-deterministic.

## Profile

    make perf-profile                              # public endpoint, warm
    make perf-profile TARGET=direct                # via the SSH tunnel
    make perf-profile TARGET=direct CACHE_LEVEL=ice-cold

Output lands in `output/perf/<target>-<cache-level>-<timestamp>/`
(`samples.json`, `summary.json`, `run.json`). It is gitignored.

## Cache levels

| Level | Clears | Needs |
|---|---|---|
| `warm` | nothing; one discarded warming pass first | — |
| `cache-cold` | query cache, before every sample | access token, so `direct` only |
| `ice-cold` | query cache, process, page cache, before every sample | root SSH |

`ice-cold` restarts `qlever.service` and runs `sync && drop_caches` before
**every individual sample**, so it is slow by construction. That is the point:
it measures what the first user after an index promotion actually experiences.

## Caveats

- **HTTP/1.1 only.** urllib does not speak HTTP/2, while production nginx has
  `http2 on`. The `public`-minus-`direct` delta is therefore an upper bound on
  proxy cost, and these numbers are not comparable to an HTTP/2 load generator.
- **A bare 503 is recorded as `HTTP_ERROR`, not `RATE_LIMITED`.** nginx returns
  503 both for `limit_req` and for an unavailable upstream, and the proxy sets
  no marker distinguishing them.
- **Fingerprints are evidence about one index**, not about the data. See spec
  §4.4 before using one to gate anything.
- **`expect` bounds start as `null`** and must be populated from observed runs.
- Concurrency, host telemetry, and the regression gate are Plan B.
```

- [ ] **Step 9: Commit**

```bash
git add perf/pgperf/__main__.py perf/tests/test_cli.py perf/Containerfile.perf perf/README.md Makefile .gitignore
git commit -m "feat(perf): add pgperf CLI, container image, and docs"
```

- [ ] **Step 10: Generate the real corpus against the live index**

This is the first step that needs the endpoint. Open the tunnel, read the index hash, generate:

```bash
ssh -f -N -L 7001:127.0.0.1:7001 root@51.159.171.16
INDEX_HASH=$(ssh root@51.159.171.16 cat /var/lib/packagegraph/qlever-data/index/.loaded)
echo "index: $INDEX_HASH"
cd perf && python3 -m pgperf manifest --import-from ../spike/cq-queries.json \
    --out corpus --index-hash "$INDEX_HASH"
```

Expected: `wrote 65 queries to corpus/manifest.json` plus a list of the non-deterministic ones. Read that list — spec §4.6 predicts the `ORDER BY … LIMIT` queries over tied sort keys will appear, and anything surprising there is worth investigating before it becomes a baseline.

- [ ] **Step 11: Take the first warm profile and populate `expect` bounds**

```bash
cd perf && python3 -m pgperf profile --target direct --cache-level warm \
    --corpus corpus/manifest.json --out ../output/perf
```

Then set each query's `expect.min_rows` and `expect.max_rows` in `perf/corpus/manifest.json` from the observed row counts in `summary.json`. These are **wide sanity rails, not tight assertions** — the guidance from spec §4.5 is that a query which should never return zero must fail when it returns zero. A reasonable default is `min_rows: 1` for any query that returned rows, and `max_rows` at roughly twice the observed count. Leave both `null` for queries that legitimately returned nothing.

Do not tighten these from a single run. Spec §13 is explicit that a tolerance calibrated from one run's data is calibrated from noise.

- [ ] **Step 12: Commit the corpus**

```bash
git add perf/corpus
git commit -m "feat(perf): add generated query corpus with measured determinism"
```

- [ ] **Step 13: Delete the superseded spike profiler**

Spec §4.3: `spike/profile-cqs.py` and `spike/cq-profile-results.json` are subsumed by `pgperf profile`, and their amd64 `-j 8 -c 2G` numbers are actively misleading as a baseline for the aarch64 `-j 4 -c 1G` host. `spike/cq-queries.json` **stays** — `etl/scripts/cq-validate.py` still consumes it.

```bash
git rm spike/profile-cqs.py spike/cq-profile-results.json
grep -rn "profile-cqs\|cq-profile-results" --include='*.md' --include='*.sh' --include='*.py' . || echo "no dangling references"
```

Fix any references the grep finds, then:

```bash
git commit -m "chore(perf): remove spike profiler superseded by pgperf"
```

---

## Plan A Self-Review

**Spec coverage.** §3 substrate/packaging → Tasks 1, 9. §4.1–4.2 corpus layout and schema → Tasks 4, 8. §4.3 migration → Task 9 step 13. §4.5 `max_row_drift_pct` field → Task 4 (validated) and Task 8 (emitted); the *gate* using it is Plan B. §4.6 determinism → Task 8. §4.7 adversarial selection → Task 4 (`select`); authoring the queries is Plan B, since only the load layer runs them. §5.1 target modes and the HTTP/1.1 limitation → Task 5. §5.2 fingerprints → Task 2. §5.3 taxonomy including the bare-503 rule → Task 3. §5.4 no token on public, multiplexed SSH → Tasks 5, 6. §6 cache levels, `sync`, per-sample granularity → Task 7. §8.1 percentiles and CoV → Tasks 1, 7. §8.2 wall vs engine time → Task 5.

**Deferred to Plan B, deliberately:** §7 run identity (server-side manifest), §8.2.1 `QueryEventLog` attribution staging, §8.3 host telemetry, §9 `summary.json` identity block and the `compare` gate, §10's compare-specific tests, §11 incidental fixes, §12 steps 4–6. `BANNED` and `FINGERPRINT_MISMATCH` are defined in Task 3 but not produced by this plan — `BANNED` needs the dual-port probe from §5.4 and `FINGERPRINT_MISMATCH` is a compare-time verdict per §4.4. Both constants exist now so Plan B does not redefine them.

**Placeholder scan.** No TBD/TODO. Every code step carries runnable code. Steps 10–12 require judgment against live data and say exactly what judgment, with the spec's own caution against over-tightening.

**Type consistency.** `Client.execute` returns the ten-key sample dict from Task 5; `profile.run` adds `query_id`, `class`, `repetition` and nothing else. `outcomes.check_cardinality` is called only from `client.execute`. `corpus.load` attaches `sparql` and `path`, which `profile.run` and `manifestgen.build` both read. `stats.summarize`'s key set is consumed by `profile._summarize_bucket` under the `wall_ms` key. `ssh.Ssh.run` is called from `profile.CacheController` only. `corpus.DEFAULT_TIMEOUT_S` and `corpus.SCHEMA_VERSION` are referenced by `manifestgen`; both are defined in Task 4, which precedes it.

**One gap found and left as a gap:** `CacheController._clear_query_cache` calls `self.client.execute("")` as a placeholder for QLever's cache-clear admin endpoint, whose existence and URL spec §13 lists as unconfirmed. Task 7's tests use a fake controller and never exercise it. The first person to run `--cache-level cache-cold` must verify the endpoint against the pinned image and fix that one method; if the endpoint does not exist, spec §13's stated fallback applies — drop `cache-cold` and keep `warm` and `ice-cold`. This is called out here rather than papered over with an invented URL.
