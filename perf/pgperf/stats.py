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
