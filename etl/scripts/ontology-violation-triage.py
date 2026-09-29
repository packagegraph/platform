#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["rdflib>=7"]
# ///
"""Pull concrete examples for each violating constraint, so a fix can be
chosen instead of guessed.

ontology-shape-check.py answers "how many". This answers "what does the bad
data actually look like", which is what decides whether a violation is a
collector bug, a shape that is wrong, or a duplicate to dedupe:

  datatype  -> which datatypes ARE emitted, vs the one the shape wants
  class     -> which rdf:types the offending objects actually carry
  maxCount  -> are the extra values distinct (a real conflict) or the same
               value repeated across graphs (an artifact of union semantics)
  minCount  -> which graphs the subjects missing the property live in, so the
               responsible collector is identifiable
"""

import argparse
import json
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

from rdflib import RDF, RDFS, Graph, Namespace, URIRef

SH = Namespace("http://www.w3.org/ns/shacl#")
DEFAULT_ENDPOINT = "https://packagegraph.di.riseproject.dev"


def q(endpoint: str, query: str, timeout: int) -> list[dict]:
    url = f"{endpoint}/?query={urllib.parse.quote(query)}"
    req = urllib.request.Request(
        url, headers={"Accept": "application/sparql-results+json", "User-Agent": "pg-triage"}
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        d = json.loads(r.read().decode())
    if "exception" in d:
        raise RuntimeError(d["exception"][:200])
    return d["results"]["bindings"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--endpoint", default=DEFAULT_ENDPOINT)
    ap.add_argument("--shape-check", type=Path, default=Path("/tmp/shape-check.json"))
    ap.add_argument("--timeout", type=int, default=600)
    ap.add_argument("--delay", type=float, default=0.2)
    ap.add_argument("--out", type=Path, default=Path("/tmp/triage.json"))
    args = ap.parse_args()

    data = json.loads(args.shape_check.read_text())
    violating = [r for r in data["results"] if r.get("violating", 0) > 0]
    # Deduplicate the overlapping-shape double reports (finding 7).
    seen, uniq = set(), []
    for r in violating:
        k = (r["target"], r["path"], r["kind"])
        if k not in seen:
            seen.add(k)
            uniq.append(r)
    print(f"triaging {len(uniq)} distinct violating constraints "
          f"({len(violating) - len(uniq)} duplicate shape reports collapsed)\n", flush=True)

    out = []
    for r in uniq:
        t, p, k = f"<{r['target']}>", f"<{r['path']}>", r["kind"]
        label = f"{r['target'].split('#')[-1]}.{r['path'].split('#')[-1].split('/')[-1]} [{k}]"
        rec = dict(r)
        try:
            if k == "datatype":
                # BIND first: QLever rejects GROUP BY over an expression
                # (HTTP 400), so DATATYPE(?v) has to be bound to a variable.
                rows = q(args.endpoint,
                         f"SELECT ?dt (COUNT(*) AS ?n) WHERE {{ "
                         f"GRAPH ?g {{ ?s a {t} }} GRAPH ?g2 {{ ?s {p} ?v }} "
                         f"BIND(DATATYPE(?v) AS ?dt) }} "
                         f"GROUP BY ?dt ORDER BY DESC(?n) LIMIT 6", args.timeout)
                rec["actual_datatypes"] = [
                    (b.get("dt", {}).get("value", "<none/IRI>"), int(b["n"]["value"])) for b in rows]
                rec["expected_datatype"] = r.get("datatype")
                detail = ", ".join(f"{d.split('#')[-1]}={n:,}" for d, n in rec["actual_datatypes"])
                print(f"  {label}\n      expects {str(r.get('datatype','?')).split('#')[-1]};"
                      f" actual: {detail}", flush=True)

            elif k == "class":
                rows = q(args.endpoint,
                         f"SELECT ?vt (COUNT(DISTINCT ?v) AS ?n) WHERE {{ "
                         f"GRAPH ?g {{ ?s a {t} }} GRAPH ?g2 {{ ?s {p} ?v }} "
                         f"OPTIONAL {{ GRAPH ?g3 {{ ?v a ?vt }} }} "
                         f"FILTER NOT EXISTS {{ GRAPH ?g4 {{ ?v a ?ok }} "
                         f"GRAPH ?g5 {{ ?ok <{RDFS.subClassOf}>* <{r['class']}> }} }} }} "
                         f"GROUP BY ?vt ORDER BY DESC(?n) LIMIT 6", args.timeout)
                rec["actual_object_types"] = [
                    (b.get("vt", {}).get("value", "<UNTYPED>"), int(b["n"]["value"])) for b in rows]
                detail = ", ".join(f"{d.split('#')[-1]}={n:,}" for d, n in rec["actual_object_types"])
                print(f"  {label}\n      expects sh:class {r['class'].split('#')[-1]};"
                      f" actual: {detail}", flush=True)

            elif k == "maxCount":
                # Same-graph is the honest count; the union-graph number in
                # shape-check.json over-reports when one subject appears in
                # several graphs with a different value in each.
                within = q(args.endpoint,
                           f"SELECT (COUNT(*) AS ?n) WHERE {{ SELECT ?s ?g (COUNT(DISTINCT ?v) AS ?c) "
                           f"WHERE {{ GRAPH ?g {{ ?s a {t} . ?s {p} ?v }} }} "
                           f"GROUP BY ?s ?g HAVING (?c > {r['n']}) }}", args.timeout)
                rec["violating_within_graph"] = int(within[0]["n"]["value"]) if within else 0
                rec["violating_union_graph"] = r["violating"]
                print(f"  {label}\n      union-graph {r['violating']:,} ->"
                      f" within-graph {rec['violating_within_graph']:,}", flush=True)

            elif k == "minCount":
                rows = q(args.endpoint,
                         f"SELECT ?g (COUNT(DISTINCT ?s) AS ?n) WHERE {{ "
                         f"GRAPH ?g {{ ?s a {t} }} "
                         f"FILTER NOT EXISTS {{ GRAPH ?g2 {{ ?s {p} ?v }} }} }} "
                         f"GROUP BY ?g ORDER BY DESC(?n) LIMIT 6", args.timeout)
                rec["missing_by_graph"] = [
                    (b["g"]["value"].split("/graph/")[-1], int(b["n"]["value"])) for b in rows]
                detail = ", ".join(f"{g}={n:,}" for g, n in rec["missing_by_graph"])
                print(f"  {label}\n      missing in: {detail}", flush=True)

        except Exception as e:
            rec["triage_error"] = str(e)[:200]
            print(f"  {label}\n      TRIAGE FAILED: {str(e)[:110]}", flush=True)
        out.append(rec)
        time.sleep(args.delay)

    args.out.write_text(json.dumps(out, indent=2))
    print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
