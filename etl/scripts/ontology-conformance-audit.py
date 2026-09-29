#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["rdflib>=7", "pyshacl>=0.26"]
# ///
"""Sample production data per (graph, class) and validate it against the
ontology's SHACL shapes.

This exists because ontology/scripts/production_shacl_validate.py (written
2026-04-20, never executed) cannot produce a valid result:

  1. It runs a CONSTRUCT with SPARQLWrapper's JSON return format and parses
     the result as JSON-LD. CONSTRUCT returns RDF, not SPARQL-JSON bindings,
     so the parse fails -- and a bare `except` turns that into an empty
     Graph, which the caller reports as "No data found" and scores as
     neither pass nor fail. The script then exits 0. A silent pass.
  2. Its `LIMIT n` bounds *triples*, not instances, with no ORDER BY. That
     slices an arbitrary subject in half, so every truncated subject
     manufactures "missing required property" violations that do not exist
     in the data.
  3. It loads 2 of the ontology's 36 shape files.
  4. It targets graph/security, which does not exist. The real graph is
     graph/security/osv.

Fixes here: request Turtle and parse it as Turtle; bound the sample by
SUBJECT via an inner SELECT so every sampled subject is complete; load every
*.shacl.ttl; use the graph URIs the endpoint actually reports.

Known limitation, deliberately not fixed: the sample is subject-complete but
only one hop deep. Shapes that constrain a linked node's type (sh:class,
sh:node) can report violations that are artifacts of the linked node's
rdf:type being outside the sample. --with-object-types pulls those types in;
it is slower. Violations are labelled accordingly.
"""

import argparse
import json
import re
import sys
import urllib.parse
import urllib.request
from collections import Counter
from pathlib import Path

from pyshacl import validate
from rdflib import Graph

DEFAULT_ENDPOINT = "https://packagegraph.di.riseproject.dev"
BASE = "https://packagegraph.github.io/graph"
PKG = "https://purl.org/packagegraph/ontology/core#"
SEC = "https://purl.org/packagegraph/ontology/security#"

# (graph, class, label). Chosen to cover the largest graphs, the classes the
# 2026-04-20 script meant to target, and the ecosystems whose triple counts
# look too small to be complete.
TEST_CASES = [
    (f"{BASE}/fedora/43", f"{PKG}Package", "Package"),
    (f"{BASE}/fedora/43", f"{PKG}BinaryPackage", "BinaryPackage"),
    (f"{BASE}/fedora/43", f"{PKG}Dependency", "Dependency"),
    (f"{BASE}/fedora/43", f"{PKG}SourcePackage", "SourcePackage"),
    (f"{BASE}/debian/trixie", f"{PKG}Package", "Package"),
    (f"{BASE}/debian/trixie", f"{PKG}BinaryPackage", "BinaryPackage"),
    (f"{BASE}/security/osv", f"{SEC}Vulnerability", "Vulnerability"),
    (f"{BASE}/security/osv", f"{SEC}SecurityAdvisory", "SecurityAdvisory"),
    (f"{BASE}/security/osv", f"{SEC}AffectedRange", "AffectedRange"),
    (f"{BASE}/cve/nvd", f"{SEC}Vulnerability", "Vulnerability"),
    (f"{BASE}/maven", f"{PKG}Package", "Package"),
    (f"{BASE}/pypi", f"{PKG}Package", "Package"),
    (f"{BASE}/npm", f"{PKG}Package", "Package"),
    (f"{BASE}/cargo", f"{PKG}Package", "Package"),
    (f"{BASE}/conda-forge", f"{PKG}Package", "Package"),
    (f"{BASE}/alpine/v3.20", f"{PKG}Package", "Package"),
    (f"{BASE}/nix/nixpkgs", f"{PKG}Package", "Package"),
]


def sparql(endpoint: str, query: str, accept: str, timeout: int) -> str:
    url = f"{endpoint}/?query={urllib.parse.quote(query)}"
    req = urllib.request.Request(url, headers={"Accept": accept, "User-Agent": "pg-ontology-audit"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode("utf-8", errors="replace")


def count_instances(endpoint: str, graph: str, cls: str, timeout: int) -> int:
    q = f"SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH <{graph}> {{ ?s a <{cls}> }} }}"
    d = json.loads(sparql(endpoint, q, "application/sparql-results+json", timeout))
    b = d["results"]["bindings"]
    return int(b[0]["n"]["value"]) if b else 0


def sample(endpoint: str, graph: str, cls: str, n: int, timeout: int, object_types: bool) -> Graph:
    """CONSTRUCT every triple of n complete subjects of `cls` in `graph`."""
    if object_types:
        q = f"""CONSTRUCT {{ ?s ?p ?o . ?o a ?ot }} WHERE {{
  {{ SELECT DISTINCT ?s WHERE {{ GRAPH <{graph}> {{ ?s a <{cls}> }} }} LIMIT {n} }}
  GRAPH <{graph}> {{ ?s ?p ?o }}
  OPTIONAL {{ GRAPH <{graph}> {{ ?o a ?ot }} }}
}}"""
    else:
        q = f"""CONSTRUCT {{ ?s ?p ?o }} WHERE {{
  {{ SELECT DISTINCT ?s WHERE {{ GRAPH <{graph}> {{ ?s a <{cls}> }} }} LIMIT {n} }}
  GRAPH <{graph}> {{ ?s ?p ?o }}
}}"""
    body = sparql(endpoint, q, "text/turtle", timeout)
    g = Graph()
    g.parse(data=body, format="turtle")
    return g


def load_shapes(ontology_dir: Path) -> tuple[Graph, int]:
    g = Graph()
    files = sorted(ontology_dir.rglob("*.shacl.ttl"))
    for f in files:
        g.parse(f, format="turtle")
    return g, len(files)


def violation_messages(results_text: str) -> Counter:
    """pyshacl's text report is per-result stanzas; pull the message lines."""
    out = Counter()
    for line in results_text.splitlines():
        line = line.strip()
        if line.startswith("Message:"):
            msg = line.split("Message:", 1)[1].strip()
            # Collapse instance-specific IRIs/literals so messages aggregate.
            msg = re.sub(r"<[^>]*>", "<IRI>", msg)
            msg = re.sub(r"Literal\([^)]*\)", "Literal(...)", msg)
            out[msg] += 1
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--endpoint", default=DEFAULT_ENDPOINT)
    ap.add_argument("--ontology-dir", default="../../../ontology", type=Path)
    ap.add_argument("--subjects", type=int, default=300, help="complete subjects per class")
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--with-object-types", action="store_true")
    ap.add_argument("--json-out", type=Path, default=Path("/tmp/conformance.json"))
    args = ap.parse_args()

    shapes, nfiles = load_shapes(args.ontology_dir)
    print(f"shapes: {len(shapes)} triples from {nfiles} *.shacl.ttl files", flush=True)
    if nfiles == 0:
        print(f"ERROR: no shape files under {args.ontology_dir}", file=sys.stderr)
        return 2
    print(f"endpoint: {args.endpoint}\nsubjects per class: {args.subjects}\n", flush=True)

    results = []
    for graph, cls, label in TEST_CASES:
        short = graph.replace(f"{BASE}/", "")
        rec = {"graph": graph, "graph_short": short, "class": cls, "label": label}
        try:
            total = count_instances(args.endpoint, graph, cls, args.timeout)
        except Exception as e:
            print(f"  {short:24} {label:18} COUNT FAILED: {e}", flush=True)
            rec |= {"error": f"count: {e}"}
            results.append(rec)
            continue
        rec["instances_in_graph"] = total
        if total == 0:
            print(f"  {short:24} {label:18} 0 instances -- NO DATA", flush=True)
            rec |= {"conforms": None, "reason": "no instances of this class in this graph"}
            results.append(rec)
            continue
        try:
            g = sample(args.endpoint, graph, cls, args.subjects, args.timeout, args.with_object_types)
        except Exception as e:
            print(f"  {short:24} {label:18} SAMPLE FAILED: {e}", flush=True)
            rec |= {"error": f"sample: {e}"}
            results.append(rec)
            continue
        subjects = len(set(g.subjects()))
        rec |= {"sample_triples": len(g), "sample_subjects": subjects}
        if len(g) == 0:
            print(f"  {short:24} {label:18} {total:>9,} inst, empty sample", flush=True)
            rec |= {"conforms": None, "reason": "CONSTRUCT returned no triples"}
            results.append(rec)
            continue
        conforms, _, text = validate(g, shacl_graph=shapes, inference="rdfs",
                                     serialize_report_graph=False, allow_warnings=False)
        v = violation_messages(text)
        rec |= {"conforms": bool(conforms), "violation_kinds": len(v),
                "violation_total": sum(v.values()), "violations": v.most_common(25)}
        flag = "PASS" if conforms else "FAIL"
        print(f"  {short:24} {label:18} {total:>9,} inst  {subjects:>4} sampled  "
              f"{len(g):>6} triples  {flag}"
              + ("" if conforms else f"  ({len(v)} kinds / {sum(v.values())} results)"), flush=True)
        results.append(rec)

    args.json_out.write_text(json.dumps(results, indent=2))
    print(f"\nwrote {args.json_out}")

    passed = sum(1 for r in results if r.get("conforms") is True)
    failed = sum(1 for r in results if r.get("conforms") is False)
    nodata = sum(1 for r in results if r.get("conforms") is None)
    errored = sum(1 for r in results if "error" in r)
    print(f"pass={passed} fail={failed} no-data={nodata} error={errored}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
