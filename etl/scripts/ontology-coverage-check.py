#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["rdflib>=7"]
# ///
"""Which ontology terms does the corpus never populate?

The conformance question is "is what we emit valid". This is the other half:
"do we emit anything at all for each declared term".

What this measures, and what it does not: it counts distinct types and
predicates in use against the terms the deployed ontology declares. It does
NOT measure competency-question satisfaction. A CQ returning zero rows can
mean absent data, but it can equally be a query defect, a graph or inference
scope mismatch, or a filter that excludes everything -- ontology issue #7 is a
published CQ that joins on an edge the collectors deliberately never emit, so
it returns nothing against entirely correct data. A term histogram cannot
distinguish those cases, and an earlier version of this docstring claimed it
could.

Term denominators come from the 37 modules sync-ontology.sh actually ships,
resolved from its EXPECTED_FILES allowlist rather than by globbing the
ontology checkout -- which swept negative SHACL fixtures and, in a working
checkout, pyshacl's bundled vocabularies under .venv/.

etl/pg-collect/src/extract.rs already computes something similar
(CoverageReport, with classes_missing / predicates_missing / shacl_missing)
but only as a byproduct of the manual ExtractTestCorpus subcommand, which also
pulls a whole test corpus. This does just the coverage part, in two endpoint
queries plus a local diff against the ontology's declarations.
"""

import argparse
import json
import re
import sys
import urllib.parse
import urllib.request
from pathlib import Path

from rdflib import OWL, RDF, RDFS, Graph, Namespace, URIRef

SH = Namespace("http://www.w3.org/ns/shacl#")
DEFAULT_ENDPOINT = "https://packagegraph.di.riseproject.dev"
PG_PREFIX = "https://purl.org/packagegraph/"

PROPERTY_TYPES = [OWL.ObjectProperty, OWL.DatatypeProperty, OWL.AnnotationProperty, RDF.Property]
CLASS_TYPES = [OWL.Class, RDFS.Class]


def deployed_modules(ontology_dir: Path) -> list[str]:
    """The module filenames sync-ontology.sh mirrors, parsed from its allowlist.

    Reading the list rather than restating it means this script and the
    deployment cannot disagree about what ships.
    """
    for cand in (
        Path(__file__).resolve().parent / "sync-ontology.sh",
        ontology_dir.parent / "platform" / "etl" / "scripts" / "sync-ontology.sh",
    ):
        if cand.is_file():
            m = re.search(r'EXPECTED_FILES="([^"]*)"', cand.read_text(), re.S)
            if m:
                mods = [ln.strip() for ln in m.group(1).splitlines() if ln.strip()]
                if mods:
                    return mods
    sys.exit("could not read the EXPECTED_FILES allowlist from sync-ontology.sh")


def resolve_module(ontology_dir: Path, filename: str) -> Path:
    """Map a flat mirror filename back to the restructured ontology layout."""
    stem = filename[: -len(".ttl")]
    for cand in (
        ontology_dir / "core" / filename,
        ontology_dir / "extensions" / stem / filename,
        ontology_dir / "ecosystems" / stem / filename,
    ):
        if cand.is_file():
            return cand
    sys.exit(f"deployed module {filename} not found under {ontology_dir}")


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

    # Declared terms: exactly the modules sync-ontology.sh mirrors into the ETL
    # image, resolved from its EXPECTED_FILES allowlist.
    #
    # This used to rglob("*.ttl"), which is not the deployed set. It swept 34
    # non-shipping files -- mostly negative SHACL fixtures written to violate
    # shapes -- plus, in a working checkout, the pyshacl assets under .venv/.
    # That is how the denominators reached 1213 classes / 1167 properties and
    # the headline became "6.5% of classes populated", a number that says more
    # about schema.org and SHACL's own vocabulary than about this corpus.
    onto = Graph()
    mods = [resolve_module(args.ontology_dir, m)
            for m in deployed_modules(args.ontology_dir)]
    for f in mods:
        onto.parse(f)
    declared_classes = {str(s) for t in CLASS_TYPES for s in onto.subjects(RDF.type, t)
                        if isinstance(s, URIRef)}
    declared_props = {str(s) for t in PROPERTY_TYPES for s in onto.subjects(RDF.type, t)
                      if isinstance(s, URIRef)}
    # Imported vocabularies (schema.org, SKOS, ...) are declared by the modules
    # but are not ours to populate, so they are reported separately rather than
    # dragging the denominator down.
    own_classes = {c for c in declared_classes if c.startswith(PG_PREFIX)}
    own_props = {p for p in declared_props if p.startswith(PG_PREFIX)}

    shapes = Graph()
    sfiles = sorted(args.ontology_dir.rglob("*.shacl.ttl"))
    for f in sfiles:
        shapes.parse(f)
    shape_targets = {str(o) for o in shapes.objects(None, SH.targetClass)}

    print(f"ontology: {len(mods)} deployed modules, {len(declared_classes)} classes, "
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

    own_classes_used = len(own_classes & used_classes.keys())
    own_props_used = len(own_props & used_props.keys())

    print()
    print("packagegraph-owned terms -- the figures to quote:")
    print(f"  classes:    {own_classes_used}/{len(own_classes)} populated "
          f"({pct(own_classes_used, len(own_classes))})")
    print(f"  properties: {own_props_used}/{len(own_props)} populated "
          f"({pct(own_props_used, len(own_props))})")
    print(f"  shapes:     {len(shape_targets) - len(missing_shapes)}/{len(shape_targets)} have instances "
          f"({pct(len(shape_targets) - len(missing_shapes), len(shape_targets))})")
    print("including imported vocabularies (schema.org, SKOS, ... -- not ours to populate):")
    print(f"  classes:    {len(declared_classes) - len(missing_classes)}/{len(declared_classes)} "
          f"({pct(len(declared_classes) - len(missing_classes), len(declared_classes))})")
    print(f"  properties: {len(declared_props) - len(missing_props)}/{len(declared_props)} "
          f"({pct(len(declared_props) - len(missing_props), len(declared_props))})")
    print(f"undeclared in ontology but emitted: {len(undeclared_classes)} classes, "
          f"{len(undeclared_props)} properties")
    print("\nnote: a zero-row competency question is not evidence of absent data -- it can "
          "also be a query defect or a scope mismatch. This script measures term usage only.")

    args.out.write_text(json.dumps({
        "deployed_modules": len(mods),
        "owned": {
            "classes_declared": len(own_classes),
            "classes_populated": own_classes_used,
            "properties_declared": len(own_props),
            "properties_populated": own_props_used,
        },
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
