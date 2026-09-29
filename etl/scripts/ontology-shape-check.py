#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["rdflib>=7"]
# ///
"""Compile a SUBSET of the ontology's SHACL property constraints to SPARQL and
count violations corpus-wide against a live endpoint.

This is a partial SHACL implementation. It translates five constraint
components -- sh:minCount, sh:maxCount, sh:class, sh:datatype and sh:in --
over explicitly typed NodeShapes with sh:targetClass and a single-IRI
sh:path. Everything else is reported as skipped. A run's output is not a
conformance verdict and must not be quoted as one.

Why a SPARQL translation rather than pyshacl: pyshacl needs the data in
memory, so it needs a sample, and sampling a graph database produces artifacts
that swamp the real signal. Subject-complete sampling leaves `sh:class`
constraints failing 100% of the time because the linked node's rdf:type is
outside the sample; pulling those types in makes the linked nodes themselves
validation targets carrying only a type triple, so they fail their own shapes'
minCounts instead. Measured on a 25-subject sample: 6 pass / 7 fail without
object types, 0 pass / 13 fail with them. Neither number means anything.
For the components it does translate, a COUNT query is exact over all ~200M
triples and cheap on QLever.

Scoping decision that matters for correctness: a subject is matched inside
one named graph, but `sh:class` checks look for the object's rdf:type in ANY
graph. Cross-graph references are normal here -- a vulnerability in
graph/security/osv legitimately points at a package in graph/fedora/43 -- so
same-graph type resolution would report false violations.

## Why this file is explicit about what it does not do

An earlier version read only those five components and looked no further. A
property shape carrying a supported constraint AND an unsupported one emitted
the supported check and dropped the other with no record; a shape carrying
only an unsupported one produced no record at all. The run then reported
"skipped: 0", which read as full coverage. That is how sh:pattern came to be
never evaluated -- including on pkg:PURLShape, whose mandatory-version pattern
is violated by every versionless Maven purl in the corpus.

So the inventory is now exhaustive by construction: every sh:* predicate on a
property shape is classified as a constraint component, a known non-constraint
annotation, or an unrecognised term. Unsupported components produce one skip
record each, and an unrecognised sh:* term is itself reported rather than
ignored -- so a SHACL feature added to the shapes later cannot silently pass
through this checker.

Failures are likewise never read as clean: a target-count query that fails
marks its constraints unexecuted instead of dropping them, a result set with
no bindings raises instead of counting zero, and the process exits non-zero if
anything did not run.
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

# The five components this script translates to SPARQL.
SUPPORTED = {"minCount", "maxCount", "class", "datatype", "in"}

# Every other SHACL constraint component that can appear on a property shape
# (SHACL spec §4). Each one present in the shapes produces a skip record, so
# the count of unevaluated constraints is always visible.
UNSUPPORTED = {
    "nodeKind",
    "minExclusive", "minInclusive", "maxExclusive", "maxInclusive",
    "minLength", "maxLength", "pattern", "languageIn", "uniqueLang",
    "equals", "disjoint", "lessThan", "lessThanOrEquals",
    "not", "and", "or", "xone",
    "node", "property", "qualifiedValueShape",
    "qualifiedMinCount", "qualifiedMaxCount", "qualifiedValueShapesDisjoint",
    "closed", "ignoredProperties", "hasValue", "sparql",
}

# sh:* predicates that carry metadata rather than a constraint. Listing them
# explicitly is what lets an unrecognised sh:* term be reported instead of
# quietly assumed harmless.
NON_CONSTRAINT = {
    "path", "message", "severity", "name", "description", "order", "group",
    "defaultValue", "deactivated", "flags", "targetClass", "targetNode",
    "targetSubjectsOf", "targetObjectsOf", "nodeValidator", "propertyValidator",
}


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
    """Read a single COUNT result, or raise.

    A COUNT query always returns one row, so an empty result set or a missing
    binding means the query did not do what was asked -- not that the count is
    zero. Returning 0 here turned a broken query into a clean bill of health,
    which is the same defect this audit criticised production_shacl_validate.py
    for.
    """
    d = q(endpoint, query, timeout)
    b = d["results"]["bindings"]
    if not b or var not in b[0]:
        raise RuntimeError(f"no ?{var} binding in result (query returned no count)")
    return int(b[0][var]["value"])


def discovery_gaps(g: Graph) -> dict:
    """Count the shapes this script's target selection cannot reach.

    The component inventory below is exhaustive *within a discovered shape*,
    but discovery itself is partial: only explicitly typed NodeShapes carrying
    sh:targetClass, with immediate sh:property shapes. Reporting the inventory
    without this would swap one false-completeness claim for another.
    """
    nodeshapes = set(g.subjects(RDF.type, SH.NodeShape))
    reached = {s for s in nodeshapes if (s, SH.targetClass, None) in g}
    return {
        "nodeshapes_total": len(nodeshapes),
        "nodeshapes_reached_via_targetClass": len(reached),
        "nodeshapes_without_targetClass_unreached": len(nodeshapes - reached),
        "targetSubjectsOf_unreached": len(set(g.subjects(SH.targetSubjectsOf, None))),
        "targetObjectsOf_unreached": len(set(g.subjects(SH.targetObjectsOf, None))),
        "targetNode_unreached": len(set(g.subjects(SH.targetNode, None))),
        "sh_sparql_constraints_unreached": len(list(g.subjects(SH.sparql, None))),
        "standalone_propertyshapes_unreached": len(
            set(g.subjects(RDF.type, SH.PropertyShape)) - set(g.objects(None, SH.property))
        ),
    }


def extract_constraints(shapes_dir: Path) -> tuple[list[dict], list[dict], dict]:
    g = Graph()
    files = sorted(shapes_dir.rglob("*.shacl.ttl"))
    for f in files:
        g.parse(f)
    if not files:
        raise SystemExit(f"no *.shacl.ttl under {shapes_dir}")

    discovery = discovery_gaps(g)
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

                # Inventory every sh:* term on this property shape FIRST, so
                # nothing can be dropped by simply not being looked for.
                present = {
                    str(p).split("#")[-1]
                    for p in g.predicates(ps, None)
                    if str(p).startswith(str(SH))
                }
                components = present & (SUPPORTED | UNSUPPORTED)
                unrecognised = present - SUPPORTED - UNSUPPORTED - NON_CONSTRAINT

                if not isinstance(path, URIRef):
                    # One record per component, so the count of unevaluated
                    # constraints stays accurate even here.
                    for comp in sorted(components) or ["<none>"]:
                        skipped.append(base | {"component": comp,
                                               "reason": "non-IRI property path"})
                    continue
                base["path"] = str(path)

                for comp in sorted(unrecognised):
                    skipped.append(base | {
                        "component": comp,
                        "reason": "unrecognised sh: term -- not classified by this script",
                    })
                for comp in sorted(components & UNSUPPORTED):
                    skipped.append(base | {
                        "component": comp,
                        "reason": f"sh:{comp} not translated to SPARQL",
                    })

                if "minCount" in components:
                    out.append(base | {"kind": "minCount",
                                       "n": int(g.value(ps, SH.minCount))})
                if "maxCount" in components:
                    out.append(base | {"kind": "maxCount",
                                       "n": int(g.value(ps, SH.maxCount))})
                if "class" in components:
                    out.append(base | {"kind": "class",
                                       "class": str(g.value(ps, SH["class"]))})
                if "datatype" in components:
                    out.append(base | {"kind": "datatype",
                                       "datatype": str(g.value(ps, SH.datatype))})
                if "in" in components:
                    items = list(g.items(g.value(ps, SH["in"])))
                    if items:
                        out.append(base | {"kind": "in",
                                           "values": [t.n3() for t in items]})
                    else:
                        # An empty sh:in prohibits every value, so treating it
                        # as absent would pass data it forbids.
                        skipped.append(base | {"component": "in",
                                               "reason": "empty sh:in list"})
    return out, skipped, discovery


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

    constraints, skipped, discovery = extract_constraints(args.shapes_dir)
    by_comp: dict[str, int] = {}
    for s in skipped:
        by_comp[s.get("component", "?")] = by_comp.get(s.get("component", "?"), 0) + 1
    print(f"constraints translated: {len(constraints)}   "
          f"constraints NOT translated: {len(skipped)}", flush=True)
    if by_comp:
        detail = ", ".join(f"{k}={v}" for k, v in sorted(by_comp.items(), key=lambda x: -x[1]))
        print(f"  not translated by component: {detail}", flush=True)
    unreached = {k: v for k, v in discovery.items() if k.endswith("unreached") and v}
    if unreached:
        print("  shapes this script cannot reach at all: "
              + ", ".join(f"{k.replace('_unreached', '')}={v}" for k, v in sorted(unreached.items())),
              flush=True)

    # Instance totals per target class, so a violation count becomes a rate.
    targets = sorted({c["target"] for c in constraints})
    print(f"target classes: {len(targets)}  -- counting instances", flush=True)
    totals: dict[str, int] = {}
    count_errors: dict[str, str] = {}
    for t in targets:
        try:
            totals[t] = scalar(args.endpoint,
                               f"SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH ?g {{ ?s a <{t}> }} }}",
                               args.timeout)
        except Exception as e:
            # Do NOT fall through to a sentinel that the `> 0` filter below
            # silently drops. An unknown instance count is an unknown result,
            # and it has to survive into the report as one.
            print(f"  ! count failed {t}: {e}", flush=True)
            count_errors[t] = str(e)[:200]
        time.sleep(args.delay)

    present = [c for c in constraints if totals.get(c["target"], 0) > 0]
    absent = sorted({t for t in targets if totals.get(t) == 0})
    # Constraints whose target class could not be counted were never evaluated.
    unexecuted = [c | {"reason": f"target count failed: {count_errors[c['target']]}"}
                  for c in constraints if c["target"] in count_errors]
    print(f"classes with data: {len({c['target'] for c in present})}   "
          f"classes with zero instances: {len(absent)}   "
          f"classes whose count failed: {len(count_errors)}")
    print(f"constraints to check: {len(present)}   "
          f"constraints unexecuted: {len(unexecuted)}\n", flush=True)

    results = []
    for i, c in enumerate(present, 1):
        query = violation_query(c)
        if query is None:
            skipped.append(c | {"component": c["kind"],
                                "reason": f"kind {c['kind']} has no SPARQL translation"})
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

    bad = [r for r in results if r.get("violating", 0) > 0]
    err = [r for r in results if "error" in r]
    complete = not err and not unexecuted and not count_errors

    args.out.write_text(json.dumps({
        "complete": complete,
        "coverage": {
            "supported_components": sorted(SUPPORTED),
            "constraints_translated": len(constraints),
            "constraints_not_translated": len(skipped),
            "not_translated_by_component": by_comp,
            "shape_discovery": discovery,
        },
        "results": results,
        "skipped": skipped,
        "unexecuted": unexecuted,
        "count_errors": count_errors,
        "totals": totals,
        "classes_absent": absent,
    }, indent=2))

    print(f"\nwrote {args.out}")
    print(f"checked={len(results)}  violating={len(bad)}  "
          f"clean={len(results) - len(bad) - len(err)}  errors={len(err)}")
    print(f"not translated={len(skipped)}  unexecuted={len(unexecuted)}  "
          f"target counts failed={len(count_errors)}")
    if complete:
        print(f"verdict: every translated constraint ran. Scope: "
              f"{', '.join(sorted(SUPPORTED))} on sh:targetClass NodeShapes only; "
              f"{len(skipped)} constraints were not translated and "
              f"{sum(unreached.values())} shape targets were not reached. "
              f"This is not a full SHACL conformance verdict.")
        return 0
    # A partial run must not look like a clean one to a caller or to CI.
    print("verdict: INCOMPLETE -- some constraints did not run. "
          "Do not read the violation counts as a conformance result.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
