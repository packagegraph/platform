#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["rdflib>=7"]
# ///
"""Compile the ontology's SHACL property constraints to SPARQL and count
violations exactly, corpus-wide, against a live endpoint.

Why not pyshacl: pyshacl needs the data in memory, so it needs a sample, and
sampling a graph database produces artifacts that swamp the real signal.
Subject-complete sampling leaves `sh:class` constraints failing 100% of the
time because the linked node's rdf:type is outside the sample; pulling those
types in makes the linked nodes themselves validation targets carrying only a
type triple, so they fail their own shapes' minCounts instead. Measured on a
25-subject sample: 6 pass / 7 fail without object types, 0 pass / 13 fail
with them. Neither number means anything.

Translating each constraint to a COUNT query instead is exact over all
~200M triples, has no sampling error, and is cheap on QLever.

Scoping decision that matters for correctness: a subject is matched inside
one named graph, but `sh:class` checks look for the object's rdf:type in ANY
graph. Cross-graph references are normal here -- a vulnerability in
graph/security/osv legitimately points at a package in graph/fedora/43 -- so
same-graph type resolution would report false violations.

Not translated (reported as skipped, never silently passed): sh:node,
sh:qualifiedValueShape, sh:or/and/not, sh:sparql, and property paths that are
not a single IRI.
"""

import argparse
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from rdflib import RDF, RDFS, Graph, Namespace, URIRef

SH = Namespace("http://www.w3.org/ns/shacl#")
DEFAULT_ENDPOINT = "https://packagegraph.di.riseproject.dev"


def q(endpoint: str, query: str, timeout: int, retries: int = 2) -> dict:
    url = f"{endpoint}/?query={urllib.parse.quote(query)}"
    req = urllib.request.Request(
        url,
        headers={"Accept": "application/sparql-results+json", "User-Agent": "pg-shape-check"},
    )
    for attempt in range(retries + 1):
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                d = json.loads(r.read().decode())
            # QLever reports query failures as HTTP 200 with an "exception"
            # field and no bindings. Left unchecked, scalar() reads that as
            # zero violations -- a clean bill of health for a query that never
            # ran. Raise instead, so it lands in the report as an error.
            if "exception" in d:
                raise RuntimeError(f"endpoint: {d['exception'][:200]}")
            return d
        except RuntimeError:
            raise
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError):
            if attempt == retries:
                raise
            # Back off rather than hammer: the endpoint sits behind an nginx
            # rate limit and a fail2ban jail that bans on repeated 4xx.
            time.sleep(2 * (attempt + 1))
    raise AssertionError("unreachable")


def scalar(endpoint: str, query: str, timeout: int, var: str = "n") -> int:
    d = q(endpoint, query, timeout)
    b = d["results"]["bindings"]
    return int(b[0][var]["value"]) if b and var in b[0] else 0


def extract_constraints(shapes_dir: Path) -> tuple[list[dict], list[dict]]:
    g = Graph()
    files = sorted(shapes_dir.rglob("*.shacl.ttl"))
    for f in files:
        g.parse(f)
    if not files:
        raise SystemExit(f"no *.shacl.ttl under {shapes_dir}")

    out, skipped = [], []
    for shape in g.subjects(RDF.type, SH.NodeShape):
        for target in g.objects(shape, SH.targetClass):
            for ps in g.objects(shape, SH.property):
                path = g.value(ps, SH.path)
                msg = str(g.value(ps, SH.message) or "")
                sev = str(g.value(ps, SH.severity) or SH.Violation).split("#")[-1]
                base = {
                    "target": str(target),
                    "message": msg,
                    "severity": sev,
                    "shape": str(shape),
                }
                if not isinstance(path, URIRef):
                    skipped.append(base | {"reason": "non-IRI property path"})
                    continue
                base["path"] = str(path)

                mn = g.value(ps, SH.minCount)
                mx = g.value(ps, SH.maxCount)
                cls = g.value(ps, SH["class"])
                dt = g.value(ps, SH.datatype)
                inlist = g.value(ps, SH["in"])

                if mn is not None:
                    out.append(base | {"kind": "minCount", "n": int(mn)})
                if mx is not None:
                    out.append(base | {"kind": "maxCount", "n": int(mx)})
                if cls is not None:
                    out.append(base | {"kind": "class", "class": str(cls)})
                if dt is not None:
                    out.append(base | {"kind": "datatype", "datatype": str(dt)})
                if inlist is not None:
                    items = list(g.items(inlist))
                    if items:
                        out.append(base | {"kind": "in", "values": [t.n3() for t in items]})
                    else:
                        skipped.append(base | {"reason": "empty sh:in list"})
                if all(x is None for x in (mn, mx, cls, dt, inlist)):
                    if any(g.triples((ps, p, None)) for p in (SH.node, SH.qualifiedValueShape, SH["or"])):
                        skipped.append(base | {"reason": "sh:node/qualifiedValueShape/or not translated"})
    return out, skipped


def violation_query(c: dict) -> str | None:
    """COUNT DISTINCT subjects of c['target'] that violate c, corpus-wide."""
    t, p = f"<{c['target']}>", f"<{c['path']}>"
    head = f"SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH ?g {{ ?s a {t} }}"
    k = c["kind"]
    if k == "minCount":
        if c["n"] == 1:
            return head + f" FILTER NOT EXISTS {{ GRAPH ?g2 {{ ?s {p} ?v }} }} }}"
        return (f"SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ SELECT ?s (COUNT(DISTINCT ?v) AS ?c) "
                f"WHERE {{ GRAPH ?g {{ ?s a {t} }} OPTIONAL {{ GRAPH ?g2 {{ ?s {p} ?v }} }} }} "
                f"GROUP BY ?s HAVING (?c < {c['n']}) }}")
    if k == "maxCount":
        return (f"SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ SELECT ?s (COUNT(DISTINCT ?v) AS ?c) "
                f"WHERE {{ GRAPH ?g {{ ?s a {t} }} GRAPH ?g2 {{ ?s {p} ?v }} }} "
                f"GROUP BY ?s HAVING (?c > {c['n']}) }}")
    if k == "class":
        # SHACL sh:class is satisfied by rdf:type/rdfs:subClassOf*, not by a
        # direct rdf:type. Testing direct type only reported 7,917,911/7,917,911
        # (100%) violations on Dependency.dependencyTarget against sh:class
        # pkg:PackageEntity, because PackageEntity is an abstract superclass
        # that nothing is typed as directly; the subClassOf*-aware form gives
        # 293,331 (3.7%). Written as two patterns rather than the inline
        # `a/rdfs:subClassOf*` path because the subject's and the hierarchy's
        # triples live in different named graphs.
        return (head + f" GRAPH ?g2 {{ ?s {p} ?v }} FILTER NOT EXISTS {{ "
                f"GRAPH ?g3 {{ ?v a ?vt }} "
                f"GRAPH ?g4 {{ ?vt <{RDFS.subClassOf}>* <{c['class']}> }} }} }}")
    if k == "datatype":
        return (head + f" GRAPH ?g2 {{ ?s {p} ?v }} "
                f"FILTER (!isLiteral(?v) || datatype(?v) != <{c['datatype']}>) }}")
    if k == "in":
        return (head + f" GRAPH ?g2 {{ ?s {p} ?v }} "
                f"FILTER (?v NOT IN ({', '.join(c['values'])})) }}")
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--endpoint", default=DEFAULT_ENDPOINT)
    ap.add_argument("--shapes-dir", type=Path, default=Path("/home/bharring/Projects/packagegraph/ontology"))
    ap.add_argument("--timeout", type=int, default=180)
    ap.add_argument("--delay", type=float, default=0.15, help="seconds between queries")
    ap.add_argument("--out", type=Path, default=Path("/tmp/shape-check.json"))
    args = ap.parse_args()

    constraints, skipped = extract_constraints(args.shapes_dir)
    print(f"constraints translated: {len(constraints)}   skipped: {len(skipped)}", flush=True)

    # Instance totals per target class, so a violation count becomes a rate.
    targets = sorted({c["target"] for c in constraints})
    print(f"target classes: {len(targets)}  -- counting instances", flush=True)
    totals: dict[str, int] = {}
    for t in targets:
        try:
            totals[t] = scalar(args.endpoint,
                               f"SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH ?g {{ ?s a <{t}> }} }}",
                               args.timeout)
        except Exception as e:
            print(f"  ! count failed {t}: {e}", flush=True)
            totals[t] = -1
        time.sleep(args.delay)

    present = [c for c in constraints if totals.get(c["target"], 0) > 0]
    absent = sorted({c["target"] for c in constraints if totals.get(c["target"], 0) == 0})
    print(f"classes with data: {len({c['target'] for c in present})}   "
          f"classes with zero instances: {len(absent)}")
    print(f"constraints to check: {len(present)}\n", flush=True)

    results = []
    for i, c in enumerate(present, 1):
        query = violation_query(c)
        if query is None:
            skipped.append(c | {"reason": f"kind {c['kind']} not translated"})
            continue
        rec = dict(c)
        rec["instances"] = totals[c["target"]]
        try:
            v = scalar(args.endpoint, query, args.timeout)
            rec["violating"] = v
            rec["rate"] = round(v / totals[c["target"]], 6) if totals[c["target"]] else None
        except Exception as e:
            rec["error"] = str(e)[:200]
        results.append(rec)
        if i % 25 == 0 or rec.get("violating", 0) > 0:
            tail = (f"{rec.get('violating', '?')}/{rec['instances']}"
                    if "error" not in rec else f"ERR {rec['error'][:40]}")
            print(f"  [{i}/{len(present)}] {c['target'].split('#')[-1]}.{c['path'].split('#')[-1]} "
                  f"{c['kind']} -> {tail}", flush=True)
        time.sleep(args.delay)

    args.out.write_text(json.dumps(
        {"results": results, "skipped": skipped, "totals": totals, "classes_absent": absent}, indent=2))
    bad = [r for r in results if r.get("violating", 0) > 0]
    err = [r for r in results if "error" in r]
    print(f"\nwrote {args.out}")
    print(f"checked={len(results)}  violating={len(bad)}  clean={len(results)-len(bad)-len(err)}  errors={len(err)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
