#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["rdflib>=7"]
# ///
"""Generate etl/pg-collect/ontology-vocab.txt: every term the deployed ontology
declares, with the ROLE it is declared in.

Why a checked-in manifest rather than reading the ontology at test time:
etl/ontology/ is populated by sync-ontology.sh at image-build time and is not
in the repository, so `cargo test` in CI has no ontology to read. A manifest
is also reviewable -- bumping etl/ONTOLOGY_VERSION produces a diff a human can
read, which is the point at which a renamed or removed term should be noticed.

Two defects in the first version of this script, both found by independent
review of PR #25:

1. It globbed `**/*.ttl`, which swept 34 files that are NOT deployed --
   overwhelmingly negative SHACL fixtures (bad-confidence.ttl, two-fidelity.ttl,
   missing-snapshot.ttl ...). Harvesting "declared terms" from data written to
   violate shapes let the gate accept terms the published ontology does not
   have. The deployed set is the 37-module allowlist in sync-ontology.sh, which
   this script now parses rather than restating, so the two cannot drift.

2. It emitted bare URIs, so the gate could only ask "is this string declared?"
   -- not "is it declared as the kind of thing you are using it as?" That let
   rpm:RPMGroup pass: a declared owl:Class that two collectors write as a
   literal predicate. Roles are now recorded and checked.

Shapes and examples are excluded deliberately: a term appearing only in a
*.shacl.ttl is not declared by the ontology, it is merely constrained by it.
nix:attrPath and nix:NixPackage are in exactly that state.

Regenerate whenever etl/ONTOLOGY_VERSION changes:

    etl/scripts/gen-ontology-vocab.py --ontology-dir ../ontology
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

from rdflib import OWL, RDF, RDFS, Graph, URIRef

PG_PREFIX = "https://purl.org/packagegraph/"

CLASS_TYPES = {OWL.Class, RDFS.Class}
PROPERTY_TYPES = {
    OWL.ObjectProperty,
    OWL.DatatypeProperty,
    OWL.AnnotationProperty,
    OWL.FunctionalProperty,
    OWL.InverseFunctionalProperty,
    OWL.TransitiveProperty,
    OWL.SymmetricProperty,
    RDF.Property,
}
# The module IRIs themselves, not vocabulary.
NOT_VOCABULARY = {OWL.Ontology}


def deployed_modules(sync_script: Path) -> list[str]:
    """Parse the EXPECTED_FILES allowlist out of sync-ontology.sh.

    That script fails the build if the mirror does not match this list exactly,
    so it is the authoritative statement of what ships.
    """
    text = sync_script.read_text()
    m = re.search(r'EXPECTED_FILES="([^"]*)"', text, re.S)
    if not m:
        sys.exit(f"could not find the EXPECTED_FILES allowlist in {sync_script}")
    mods = [ln.strip() for ln in m.group(1).splitlines() if ln.strip()]
    if not mods:
        sys.exit(f"EXPECTED_FILES in {sync_script} is empty")
    return mods


def resolve(ontology_dir: Path, filename: str) -> Path:
    """Find a deployed module in the restructured ontology layout.

    sync-ontology.sh copies from core/, extensions/<m>/<m>.ttl and
    ecosystems/<m>/<m>.ttl into a flat mirror; this reverses that mapping.
    """
    stem = filename[: -len(".ttl")]
    for cand in (
        ontology_dir / "core" / filename,
        ontology_dir / "extensions" / stem / filename,
        ontology_dir / "ecosystems" / stem / filename,
    ):
        if cand.is_file():
            return cand
    sys.exit(
        f"deployed module {filename} is in the sync-ontology.sh allowlist but was "
        f"not found under {ontology_dir}/(core|extensions|ecosystems)"
    )


def roles_for(g: Graph) -> dict[str, set[str]]:
    """Map each packagegraph-namespaced declared term to its role(s).

    A term may legitimately hold more than one role (class/individual punning),
    so this is a set per term rather than a single label.
    """
    out: dict[str, set[str]] = {}
    subjects: dict[str, set] = {}
    for s, _, t in g.triples((None, RDF.type, None)):
        if isinstance(s, URIRef) and str(s).startswith(PG_PREFIX):
            subjects.setdefault(str(s), set()).add(t)

    for uri, types in subjects.items():
        if types <= NOT_VOCABULARY:
            continue
        roles = set()
        if types & CLASS_TYPES:
            roles.add("class")
        if types & PROPERTY_TYPES:
            roles.add("property")
        if OWL.NamedIndividual in types:
            roles.add("individual")
        # Typed by something that is neither a class nor a property declaration
        # (a pg class, skos:Concept, ...) means this URI names an individual --
        # e.g. `rpm:Applications a rpm:RPMGroup`.
        if not roles and (types - NOT_VOCABULARY):
            roles.add("individual")
        if roles:
            out[uri] = roles
    return out


def git_revision(ontology_dir: Path) -> str:
    try:
        r = subprocess.run(
            ["git", "-C", str(ontology_dir), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        )
        return r.stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return "unknown"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ontology-dir", type=Path, required=True)
    ap.add_argument(
        "--out",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "pg-collect" / "ontology-vocab.txt",
    )
    args = ap.parse_args()

    scripts_dir = Path(__file__).resolve().parent
    mods = deployed_modules(scripts_dir / "sync-ontology.sh")
    files = [resolve(args.ontology_dir, m) for m in mods]

    g = Graph()
    for f in files:
        g.parse(f)

    roles = roles_for(g)

    version_file = args.out.resolve().parent.parent / "ONTOLOGY_VERSION"
    version = "unknown"
    if version_file.exists():
        for line in version_file.read_text().splitlines():
            line = line.split("#", 1)[0].strip()
            if line:
                version = line
                break

    body = "\n".join(f"{uri}\t{','.join(sorted(roles[uri]))}" for uri in sorted(roles))
    args.out.write_text(
        f"# Generated by etl/scripts/gen-ontology-vocab.py -- do not edit by hand.\n"
        f"# Ontology {version} at {git_revision(args.ontology_dir)}\n"
        f"# {len(files)} deployed modules, per the EXPECTED_FILES allowlist in\n"
        f"# sync-ontology.sh. Shapes, examples and non-deployed fixtures are\n"
        f"# excluded: a term constrained only by a *.shacl.ttl is not declared,\n"
        f"# and a term appearing only in a negative test fixture is not either.\n"
        f"# Format: <uri>\\t<role>[,<role>...]  roles: class | property | individual\n"
        f"# Regenerate when etl/ONTOLOGY_VERSION changes.\n"
        f"{body}\n"
    )
    counts: dict[str, int] = {}
    for rs in roles.values():
        for r in rs:
            counts[r] = counts.get(r, 0) + 1
    summary = ", ".join(f"{v} {k}" for k, v in sorted(counts.items()))
    print(
        f"wrote {args.out} -- {len(roles)} declared terms "
        f"({summary}) from {len(files)} deployed modules ({version})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
