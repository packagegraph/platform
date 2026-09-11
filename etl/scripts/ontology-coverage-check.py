#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["rdflib>=7"]
# ///
"""Which ontology terms does the corpus never populate?

The conformance question is "is what we emit valid". This is the other half:
"do we emit anything at all for each declared term". Empty competency-question
results are a coverage failure, not a conformance failure -- the data is not
wrong, it is absent -- so this is the check that speaks to them.

etl/pg-collect/src/extract.rs already computes this (CoverageReport, with
classes_missing / predicates_missing / shacl_missing) but only as a byproduct
of the manual ExtractTestCorpus subcommand, which also pulls a whole test
corpus. This does just the coverage part, in two endpoint queries plus a
local diff against the ontology's declarations.
"""

import argparse
import json
import sys
import urllib.parse
import urllib.request
from pathlib import Path

from rdflib import OWL, RDF, RDFS, Graph, Namespace, URIRef

SH = Namespace("http://www.w3.org/ns/shacl#")
DEFAULT_ENDPOINT = "https://packagegraph.di.riseproject.dev"

PROPERTY_TYPES = [OWL.ObjectProperty, OWL.DatatypeProperty, OWL.AnnotationProperty, RDF.Property]
CLASS_TYPES = [OWL.Class, RDFS.Class]


def ask(endpoint: str, query: str, timeout: int) -> list[dict]:
    url = f"{endpoint}/?query={urllib.parse.quote(query)}"
    req = urllib.request.Request(
        url, headers={"Accept": "application/sparql-results+json", "User-Agent": "pg-coverage-check"}
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode())["results"]["bindings"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--endpoint", default=DEFAULT_ENDPOINT)
    ap.add_argument("--ontology-dir", type=Path,
                    default=Path("/home/bharring/Projects/packagegraph/ontology"))
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--out", type=Path, default=Path("/tmp/coverage-check.json"))
    args = ap.parse_args()

    # Declared terms: the ontology modules, excluding shapes and examples --
    # the same file set sync-ontology.sh mirrors into the ETL image.
    onto = Graph()
    mods = [f for f in sorted(args.ontology_dir.rglob("*.ttl"))
            if not f.name.endswith((".shacl.ttl", ".examples.ttl"))]
    for f in mods:
        onto.parse(f)
    declared_classes = {str(s) for t in CLASS_TYPES for s in onto.subjects(RDF.type, t)
                        if isinstance(s, URIRef)}
    declared_props = {str(s) for t in PROPERTY_TYPES for s in onto.subjects(RDF.type, t)
                      if isinstance(s, URIRef)}

    shapes = Graph()
    sfiles = sorted(args.ontology_dir.rglob("*.shacl.ttl"))
    for f in sfiles:
        shapes.parse(f)
    shape_targets = {str(o) for o in shapes.objects(None, SH.targetClass)}

    print(f"ontology: {len(mods)} modules, {len(declared_classes)} classes, "
          f"{len(declared_props)} properties; {len(sfiles)} shape files, "
          f"{len(shape_targets)} sh:targetClass", flush=True)

    print("querying used types...", flush=True)
    used_classes = {b["t"]["value"]: int(b["n"]["value"]) for b in ask(
        args.endpoint,
        "SELECT ?t (COUNT(DISTINCT ?s) AS ?n) WHERE { GRAPH ?g { ?s a ?t } } GROUP BY ?t",
        args.timeout)}
    print(f"  {len(used_classes)} distinct types in use", flush=True)

    print("querying used predicates...", flush=True)
    used_props = {b["p"]["value"]: int(b["n"]["value"]) for b in ask(
        args.endpoint,
        "SELECT ?p (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?p",
        args.timeout)}
    print(f"  {len(used_props)} distinct predicates in use", flush=True)

    missing_classes = sorted(declared_classes - used_classes.keys())
    missing_props = sorted(declared_props - used_props.keys())
    missing_shapes = sorted(shape_targets - used_classes.keys())
    # Terms in the data that the ontology never declares: the inverse risk,
    # and the one that silently breaks a consumer writing queries from docs.
    undeclared_props = sorted(
        p for p in used_props
        if p.startswith("https://purl.org/packagegraph/") and p not in declared_props)
    undeclared_classes = sorted(
        c for c in used_classes
        if c.startswith("https://purl.org/packagegraph/") and c not in declared_classes)

    def pct(a, b):
        return f"{100 * a / b:.1f}%" if b else "n/a"

    print()
    print(f"classes:    {len(declared_classes) - len(missing_classes)}/{len(declared_classes)} populated "
          f"({pct(len(declared_classes) - len(missing_classes), len(declared_classes))})")
    print(f"properties: {len(declared_props) - len(missing_props)}/{len(declared_props)} populated "
          f"({pct(len(declared_props) - len(missing_props), len(declared_props))})")
    print(f"shapes:     {len(shape_targets) - len(missing_shapes)}/{len(shape_targets)} have instances "
          f"({pct(len(shape_targets) - len(missing_shapes), len(shape_targets))})")
    print(f"undeclared in ontology but emitted: {len(undeclared_classes)} classes, "
          f"{len(undeclared_props)} properties")

    args.out.write_text(json.dumps({
        "declared_classes": len(declared_classes),
        "declared_properties": len(declared_props),
        "shape_targets": len(shape_targets),
        "missing_classes": missing_classes,
        "missing_properties": missing_props,
        "missing_shape_targets": missing_shapes,
        "undeclared_classes": undeclared_classes,
        "undeclared_properties": undeclared_props,
        "used_classes": used_classes,
        "used_properties": used_props,
    }, indent=2))
    print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
