#!/usr/bin/env python3
"""Profile all CQ SPARQL queries against QLever (and optionally Fuseki)."""
import json
import os
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
CQ_FILE = SCRIPT_DIR / "cq-queries.json"
RESULTS_FILE = SCRIPT_DIR / "cq-profile-results.json"

def canonicalize_bindings(bindings: list[dict]) -> list[tuple]:
    """Convert SPARQL result bindings to sorted canonical tuples for comparison.

    Includes type, value, datatype, and xml:lang so that xsd:integer "1" != xsd:string "1"
    and "chat"@en != "chat"@fr.
    """
    rows = []
    for b in bindings:
        row = tuple(sorted(
            (k, v.get("type", ""), v.get("value", ""), v.get("datatype", ""), v.get("xml:lang", ""))
            for k, v in b.items()
        ))
        rows.append(row)
    rows.sort()
    return rows


QLEVER = os.environ.get("QLEVER", "http://localhost:7001")
QLEVER_TOKEN = os.environ.get("QLEVER_ACCESS_TOKEN", "")
FUSEKI = os.environ.get("FUSEKI", "")
TIMEOUT = int(os.environ.get("TIMEOUT", "310"))


def run_query(endpoint, query, timeout=TIMEOUT, access_token=""):
    params = {"query": query}
    if access_token:
        params["access-token"] = access_token
    data = urllib.parse.urlencode(params).encode()
    url = endpoint.rstrip("/")
    req = urllib.request.Request(
        url,
        data=data,
        headers={"Accept": "application/sparql-results+json"},
    )
    start = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = json.loads(resp.read())
        wall_ms = int((time.monotonic() - start) * 1000)
        if "exception" in body:
            return {"status": "ERROR", "error": body["exception"][:200], "wall_ms": wall_ms}
        bindings = body.get("results", {}).get("bindings", [])
        rows = len(bindings)
        engine_ms = body.get("meta", {}).get("query-time-ms", wall_ms)
        return {"status": "OK", "rows": rows, "bindings": bindings, "engine_ms": engine_ms, "wall_ms": wall_ms}
    except Exception as e:
        wall_ms = int((time.monotonic() - start) * 1000)
        err = str(e)[:200]
        if "timed out" in err.lower() or wall_ms > timeout * 900:
            return {"status": "TIMEOUT", "error": err, "wall_ms": wall_ms}
        return {"status": "ERROR", "error": err, "wall_ms": wall_ms}


def main():
    cqs = json.loads(CQ_FILE.read_text())
    print(f"=== CQ Query Profiler ===")
    print(f"QLever:  {QLEVER}")
    if FUSEKI:
        print(f"Fuseki:  {FUSEKI}")
    print(f"Queries: {len(cqs)}")
    print(f"Date:    {time.strftime('%Y-%m-%dT%H:%M:%S')}")
    print()

    results = []
    counts = {"OK": 0, "ERROR": 0, "TIMEOUT": 0}

    for i, cq in enumerate(cqs):
        cq_id = cq["id"]
        title = cq["title"][:55]
        query = cq["query"]

        sys.stdout.write(f"  {cq_id:15s} {title:55s} ")
        sys.stdout.flush()

        ql = run_query(QLEVER, query, access_token=QLEVER_TOKEN)
        counts[ql["status"]] = counts.get(ql["status"], 0) + 1

        entry = {"id": cq_id, "title": cq["title"], "qlever": ql}

        if ql["status"] == "OK":
            sys.stdout.write(f"{'OK':6s} {ql['rows']:4d} rows {ql['engine_ms']:>6}ms")
        elif ql["status"] == "TIMEOUT":
            sys.stdout.write(f"{'TMOUT':6s} {ql['wall_ms']}ms")
        else:
            sys.stdout.write(f"{'ERR':6s} {ql.get('error', '?')[:50]}")

        if FUSEKI:
            fk = run_query(FUSEKI, query)
            entry["fuseki"] = fk
            if fk["status"] == "OK":
                sys.stdout.write(f"  | Fuseki: {fk['wall_ms']:>6}ms")
            else:
                sys.stdout.write(f"  | Fuseki: {fk['status']}")

        print()
        results.append(entry)

    results_out = []
    for r in results:
        entry = dict(r)
        entry["qlever"] = {k: v for k, v in r["qlever"].items() if k != "bindings"}
        if "fuseki" in r:
            entry["fuseki"] = {k: v for k, v in r["fuseki"].items() if k != "bindings"}
        results_out.append(entry)
    RESULTS_FILE.write_text(json.dumps(results_out, indent=2))

    print()
    print(f"--- Summary ---")
    print(f"OK:      {counts.get('OK', 0)}")
    print(f"ERROR:   {counts.get('ERROR', 0)}")
    print(f"TIMEOUT: {counts.get('TIMEOUT', 0)}")

    ok_results = [r for r in results if r["qlever"]["status"] == "OK"]
    if ok_results:
        times = [r["qlever"]["engine_ms"] for r in ok_results if r["qlever"]["engine_ms"] >= 0]
        if times:
            times.sort()
            print(f"\nEngine time (OK queries):")
            print(f"  min:    {min(times):>6}ms")
            print(f"  median: {times[len(times)//2]:>6}ms")
            print(f"  p95:    {times[int(len(times)*0.95)]:>6}ms")
            print(f"  max:    {max(times):>6}ms")
            print(f"  total:  {sum(times):>6}ms")

    with_rows = [r for r in ok_results if r["qlever"]["rows"] > 0]
    empty = [r for r in ok_results if r["qlever"]["rows"] == 0]
    print(f"\nWith results: {len(with_rows)}")
    print(f"Empty:        {len(empty)}")

    if FUSEKI:
        both_ok = [r for r in results if r["qlever"]["status"] == "OK" and r.get("fuseki", {}).get("status") == "OK"]
        both_empty = [r for r in both_ok if r["qlever"]["rows"] == 0 and r["fuseki"]["rows"] == 0]
        qlever_empty = [r for r in both_ok if r["qlever"]["rows"] == 0 and r["fuseki"]["rows"] > 0]
        fuseki_empty = [r for r in both_ok if r["fuseki"]["rows"] == 0 and r["qlever"]["rows"] > 0]
        count_diff = [r for r in both_ok if r["qlever"]["rows"] != r["fuseki"]["rows"] and r["qlever"]["rows"] > 0 and r["fuseki"]["rows"] > 0]

        binding_match = []
        binding_diff = []
        for r in both_ok:
            if r["qlever"]["rows"] == 0 and r["fuseki"]["rows"] == 0:
                continue
            ql_canon = canonicalize_bindings(r["qlever"].get("bindings", []))
            fk_canon = canonicalize_bindings(r["fuseki"].get("bindings", []))
            if ql_canon == fk_canon:
                binding_match.append(r)
            else:
                binding_diff.append(r)

        print(f"\n--- Comparison (QLever vs Fuseki) ---")
        print(f"Both empty (no data):  {len(both_empty)}")
        print(f"Bindings identical:    {len(binding_match)}")
        print(f"Bindings differ:       {len(binding_diff)}")
        print(f"QLever empty only:     {len(qlever_empty)}")
        print(f"Fuseki empty only:     {len(fuseki_empty)}")
        print(f"Count mismatch:        {len(count_diff)}")

        if qlever_empty:
            print(f"\n  QLever missing data:")
            for r in qlever_empty:
                print(f"    {r['id']}: {r['title']} (Fuseki={r['fuseki']['rows']})")
        if binding_diff:
            print(f"\n  Binding mismatches:")
            for r in binding_diff:
                print(f"    {r['id']}: {r['title']} — QLever={r['qlever']['rows']} Fuseki={r['fuseki']['rows']}")
        if count_diff:
            print(f"\n  Count mismatches:")
            for r in count_diff:
                print(f"    {r['id']}: {r['title']} — QLever={r['qlever']['rows']} Fuseki={r['fuseki']['rows']}")

        speedups = []
        for r in binding_match:
            ft = r["fuseki"]["wall_ms"]
            qt = r["qlever"]["wall_ms"]
            if ft > 0 and qt > 0:
                speedups.append(ft / qt)
        if speedups:
            speedups.sort()
            print(f"\n  Speedup (matching queries):")
            print(f"    median: {speedups[len(speedups)//2]:.0f}×")
            print(f"    max:    {max(speedups):.0f}×")

    print(f"\nResults written to {RESULTS_FILE}")

    # Exit non-zero on any discrepancy
    exit_code = 0
    if counts.get("ERROR", 0) > 0 or counts.get("TIMEOUT", 0) > 0:
        print(f"\nFAIL: {counts.get('ERROR', 0)} errors, {counts.get('TIMEOUT', 0)} timeouts")
        exit_code = 1
    if FUSEKI:
        fuseki_errors = [r for r in results if r.get("fuseki", {}).get("status") not in ("OK", None)]
        if fuseki_errors:
            print(f"\nFAIL: {len(fuseki_errors)} Fuseki queries failed")
            exit_code = max(exit_code, 2)
        if qlever_empty:
            print(f"\nFAIL: {len(qlever_empty)} queries empty on QLever but not Fuseki")
            exit_code = max(exit_code, 2)
        if fuseki_empty:
            print(f"\nFAIL: {len(fuseki_empty)} queries empty on Fuseki but not QLever")
            exit_code = max(exit_code, 2)
        if binding_diff:
            print(f"\nFAIL: {len(binding_diff)} queries have different result bindings")
            exit_code = max(exit_code, 2)
        if count_diff:
            print(f"\nFAIL: {len(count_diff)} queries have different row counts")
            exit_code = max(exit_code, 2)
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
