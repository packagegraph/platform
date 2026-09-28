#!/usr/bin/env python3
"""Check an RPM graph against the contract its source can support.

Parses repodata independently of the Rust collector and compares what the
source says to what the graph asserts. Comparing two outputs of the same
parser is not independence, so this reads `primary.xml` with the standard
library and never imports the collector.

Seven gates, each of which can fail the run:

  fidelity   Every Provides token in the source is a capability in the graph,
             and every capability in the graph traces to a source token.
  edges      The count of `providesCapability` edges equals the source's
             Provides occurrences less the suppressed ones, exactly. Distinct
             tokens can agree while a per-provider edge is missing.
  types      The graph carries `rdf:type` for its packages and capabilities.
             A corpus stripped of types satisfies every class-targeted shape
             over zero nodes, so a SHACL pass alone cannot establish this.
  encoding   No literal carries an undecoded XML entity. RPM's rich dependency
             syntax uses `<` and `>`, which XML escapes; reading the raw
             attribute bytes put `&gt;` into 9,919 identity names.
  prohibited No predicate the contract retired is still being emitted.
  parse      Every line in the graph matched one of the two N-Triples forms.
             A line the audit cannot read is a line it is not checking, so it
             fails rather than being skipped.
  policy     The tokens the collector suppresses by policy really are absent,
             and every token it does not suppress is present. The suppression
             list is mirrored from the Rust source, so the two can disagree --
             that disagreement is what this gate exists to surface.

What is *not* checked, and is reported as `not_run` rather than passing:
declaration occurrences, `pre`, and per-kind Requires/Conflicts/Obsoletes
fidelity. Those need the terms under discussion in ontology#19. Source counts
for those kinds are still reported, so the capacity question has numbers, but
the graph side reads `null` -- not zero, which would be a measurement.

Usage:
  audit-rpm-contract.py --primary primary.xml.gz --rdf graph.nt \\
      [--ontology-root ../ontology] [--manifest manifest.json] \\
      [--report report.json]

Exits nonzero when a gate fails.
"""

import argparse
import gzip
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path

PKG = "https://purl.org/packagegraph/ontology/core#"
RPM = "https://purl.org/packagegraph/ontology/rpm#"
RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
RDFS_LABEL = "http://www.w3.org/2000/01/rdf-schema#label"

COMMON_NS = "{http://linux.duke.edu/metadata/common}"
RPM_NS = "{http://linux.duke.edu/metadata/rpm}"

SECTIONS = ("provides", "requires", "conflicts", "obsoletes")

# Predicates the capability contract retired. `rpm:rpmProvides` duplicated
# core vocabulary; the provides path no longer mints package identities, so
# `directlyProvides` from a binary package is a leftover emission.
PROHIBITED_PREDICATES = (
    f"{RPM}rpmProvides",
    f"{PKG}directlyProvides",
)

# The types the RPM collector actually asserts for a package node. Listing
# `pkg:Package` here would make the types gate look satisfiable by something
# this route never writes.
PACKAGE_TYPES = (
    f"{PKG}BinaryPackage",
    f"{RPM}BinaryRPM",
    f"{PKG}SourcePackage",
    f"{RPM}SourceRPM",
)

# Predicates whose object is a name lifted straight out of the source. An
# entity reference in one of these is a decoding bug, not content. Prose
# predicates (`pkg:description`, `pkg:summary`) are deliberately out of scope:
# a description can legitimately contain the text `&gt;`, and failing a release
# on that would be a false positive. Hits outside this set are counted as
# `entity_in_prose` for a human to look at, and gate nothing.
NAME_PREDICATES = (
    f"{PKG}capabilityName",
    f"{PKG}identityName",
    f"{PKG}packageName",
    RDFS_LABEL,
)

# Mirrored from RPM_INTERNAL_TOKEN_PREFIXES in etl/pg-collect/src/rpm.rs. These
# are ordinary capabilities with real providers -- glibc provides rtld(GNU_HASH)
# and 1,199 packages require it -- suppressed because they describe RPM's own
# contract. Mirroring rather than importing is deliberate: an audit that reads
# the collector's own list cannot detect the collector drifting from its stated
# policy. If these fall out of step, the policy gate fails and one of the two
# is wrong.
RPM_INTERNAL_TOKEN_PREFIXES = ("config(", "rpmlib(", "rtld(")


def is_rpm_internal_token(name):
    return name.startswith(RPM_INTERNAL_TOKEN_PREFIXES)


# An undecoded entity reference in a literal. `&` alone is legal in N-Triples
# literal text, so this looks for the shapes XML would have escaped.
UNDECODED_ENTITY = re.compile(r"&(?:gt|lt|amp|quot|apos|#\d+|#x[0-9A-Fa-f]+);")

# The subject may be a blank node. The RPM collector reifies every dependency
# as `_:dep_...` with `_:constraint_...` hanging off it -- 88,932 triples in
# AlmaLinux 9 BaseOS, whose literals come from the same `ver`/`rel` source
# attributes the decoding bug affected. An IRI-only subject pattern skipped all
# of them and still reported zero undecoded entities, which is the exact
# failure this audit exists to catch.
NT_SUBJECT = r"(?:<[^>]+>|_:[^\s]+)"
NT_LITERAL = re.compile(
    rf'^({NT_SUBJECT}) <([^>]+)> "(.*)"(?:\^\^<[^>]+>|@[\w-]+)? \.$'
)
NT_IRI = re.compile(rf"^({NT_SUBJECT}) <([^>]+)> (<[^>]+>|_:[^\s]+) \.$")


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def open_primary(path):
    """Open primary.xml whether or not it is gzipped."""
    with open(path, "rb") as probe:
        magic = probe.read(2)
    return gzip.open(path, "rb") if magic == b"\x1f\x8b" else open(path, "rb")


def uncompressed_sha256(path):
    digest = hashlib.sha256()
    with open_primary(path) as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def stream_source_tokens(primary_path, provides_out):
    """Stream primary.xml, writing Provides tokens and counting every section.

    Tokens go to a file rather than a set: BaseOS alone holds 3,236,779
    supported-section occurrences, and the comparison below is an external
    sorted merge so neither side has to fit in memory.
    """
    counts = {section: 0 for section in SECTIONS}
    suppressed = {section: 0 for section in SECTIONS}
    packages = 0

    with open_primary(primary_path) as handle:
        for event, elem in ET.iterparse(handle, events=("end",)):
            if elem.tag != f"{COMMON_NS}package":
                continue
            packages += 1
            fmt = elem.find(f"{COMMON_NS}format")
            if fmt is not None:
                for section in SECTIONS:
                    node = fmt.find(f"{RPM_NS}{section}")
                    if node is None:
                        continue
                    for entry in node.findall(f"{RPM_NS}entry"):
                        name = entry.get("name")
                        if not name:
                            continue
                        counts[section] += 1
                        if is_rpm_internal_token(name):
                            suppressed[section] += 1
                            continue
                        if section == "provides":
                            provides_out.write(name + "\n")
            elem.clear()

    return packages, counts, suppressed


def stream_graph(rdf_path, capability_names_out):
    """Single pass over the N-Triples graph, collecting what the gates need."""
    state = {
        "triples": 0,
        "capability_types": 0,
        "package_types": 0,
        "capability_names": 0,
        "capability_labels": 0,
        "provides_edges": 0,
        "undecoded_literals": [],
        "prohibited": {p: 0 for p in PROHIBITED_PREDICATES},
        "capabilities_without_label": 0,
        "identity_types": 0,
        "entity_in_prose": 0,
        "undecoded_name_count": 0,
        "unparsed": 0,
        "unparsed_sample": [],
        "suppressed_token_count": 0,
        "suppressed_tokens_present": [],
    }
    labelled = set()
    typed = set()

    with open(rdf_path, "r", encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            state["triples"] += 1

            iri = NT_IRI.match(line)
            if iri:
                _subject, predicate, obj = iri.groups()
                obj = obj[1:-1] if obj.startswith("<") else obj
                if predicate in state["prohibited"]:
                    state["prohibited"][predicate] += 1
                elif predicate == RDF_TYPE:
                    if obj == f"{PKG}Capability":
                        state["capability_types"] += 1
                        typed.add(_subject)
                    elif obj in PACKAGE_TYPES:
                        state["package_types"] += 1
                    elif obj == f"{PKG}PackageIdentity":
                        state["identity_types"] += 1
                elif predicate == f"{PKG}providesCapability":
                    state["provides_edges"] += 1
                continue

            literal = NT_LITERAL.match(line)
            if not literal:
                # Neither form matched. Counted and gated rather than skipped:
                # a line the audit cannot read is a line it is not checking.
                state["unparsed"] += 1
                if len(state["unparsed_sample"]) < 5:
                    state["unparsed_sample"].append(line[:200])
                continue
            subject, predicate, value = literal.groups()
            if UNDECODED_ENTITY.search(value):
                if predicate in NAME_PREDICATES:
                    state["undecoded_name_count"] += 1
                    if len(state["undecoded_literals"]) < 20:
                        state["undecoded_literals"].append(
                            {"subject": subject, "predicate": predicate, "value": value}
                        )
                else:
                    state["entity_in_prose"] += 1
            if predicate == f"{PKG}capabilityName":
                state["capability_names"] += 1
                token = unescape_nt(value)
                if is_rpm_internal_token(token):
                    state["suppressed_token_count"] += 1
                    if len(state["suppressed_tokens_present"]) < 20:
                        state["suppressed_tokens_present"].append(token)
                    continue
                capability_names_out.write(token + "\n")
            elif predicate == RDFS_LABEL:
                labelled.add(subject)

    state["capability_labels"] = len(labelled & typed)
    state["capabilities_without_label"] = len(typed - labelled)
    return state


def unescape_nt(value):
    """Undo N-Triples literal escaping so tokens compare against the source."""
    return (
        value.replace("\\\\", "\x00")
        .replace('\\"', '"')
        .replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\t", "\t")
        .replace("\x00", "\\")
    )


def sorted_unique(path):
    """External sort, so neither token stream has to fit in memory."""
    out = path + ".sorted"
    env = dict(os.environ, LC_ALL="C")
    subprocess.run(["sort", "-u", "-o", out, path], check=True, env=env)
    return out


def compare_sorted(left_path, right_path, limit=20):
    """Merge-walk two sorted files; return counts and a bounded sample."""
    only_left, only_right, both = 0, 0, 0
    sample_left, sample_right = [], []

    with open(left_path, encoding="utf-8") as left, open(right_path, encoding="utf-8") as right:
        a, b = left.readline(), right.readline()
        while a or b:
            if a and (not b or a < b):
                only_left += 1
                if len(sample_left) < limit:
                    sample_left.append(a.rstrip("\n"))
                a = left.readline()
            elif b and (not a or b < a):
                only_right += 1
                if len(sample_right) < limit:
                    sample_right.append(b.rstrip("\n"))
                b = right.readline()
            else:
                both += 1
                a, b = left.readline(), right.readline()

    return {
        "in_both": both,
        "source_only": only_left,
        "graph_only": only_right,
        "source_only_sample": sample_left,
        "graph_only_sample": sample_right,
    }


def run_shacl(rdf_path, ontology_root):
    """Validate with no inference, or say plainly that it did not run."""
    try:
        import pyshacl  # noqa: F401
        import rdflib
    except ImportError as exc:
        return {"status": "not_run", "reason": f"missing dependency: {exc.name}"}

    shapes_path = Path(ontology_root) / "core" / "core.shacl.ttl"
    if not shapes_path.is_file():
        return {"status": "not_run", "reason": f"no shapes at {shapes_path}"}

    import pyshacl

    data = rdflib.Graph()
    data.parse(rdf_path, format="nt")
    shapes = rdflib.Graph()
    shapes.parse(str(shapes_path), format="turtle")

    conforms, _results_graph, text = pyshacl.validate(
        data, shacl_graph=shapes, inference="none", abort_on_first=False
    )
    violations = text.count("Constraint Violation")
    return {
        "status": "ran",
        "inference": "none",
        "conforms": bool(conforms),
        "violations": violations,
        "report_head": text[:2000],
    }


def audit(primary_path, rdf_path, ontology_root=None, manifest_path=None):
    report = {
        "source": {"path": str(primary_path)},
        "graph": {"path": str(rdf_path)},
        "gates": {},
        "deferred": {},
    }

    workdir = tempfile.mkdtemp(prefix="rpm-contract-audit.")
    source_tokens = os.path.join(workdir, "source-provides.txt")
    graph_tokens = os.path.join(workdir, "graph-capabilities.txt")

    report["source"]["sha256"] = sha256_file(primary_path)
    report["source"]["uncompressed_sha256"] = uncompressed_sha256(primary_path)
    report["graph"]["sha256"] = sha256_file(rdf_path)

    with open(source_tokens, "w", encoding="utf-8") as handle:
        packages, section_counts, suppressed = stream_source_tokens(primary_path, handle)
    report["source"]["packages"] = packages
    report["source"]["section_occurrences"] = section_counts
    report["source"]["suppressed_by_policy"] = suppressed
    report["source"]["supported_occurrences"] = sum(section_counts.values())

    with open(graph_tokens, "w", encoding="utf-8") as handle:
        graph = stream_graph(rdf_path, handle)
    skip = ("undecoded_literals", "suppressed_tokens_present", "unparsed_sample")
    report["graph"].update({k: v for k, v in graph.items() if k not in skip})

    comparison = compare_sorted(sorted_unique(source_tokens), sorted_unique(graph_tokens))
    report["fidelity"] = comparison

    # --- gates -----------------------------------------------------------
    report["gates"]["fidelity"] = {
        "pass": comparison["source_only"] == 0 and comparison["graph_only"] == 0,
        "detail": (
            f"{comparison['in_both']} tokens agree; "
            f"{comparison['source_only']} in source only, "
            f"{comparison['graph_only']} in graph only"
        ),
    }

    report["gates"]["parse"] = {
        "pass": graph["unparsed"] == 0,
        "detail": f"{graph['unparsed']} lines matched neither N-Triples form",
        "sample": graph["unparsed_sample"],
    }

    # Distinct tokens agreeing is not enough: a per-provider edge could be
    # lost while every token still has some provider. `providesCapability` is
    # documented as per (package, capability) and never deduplicated, so the
    # edge count must equal the source occurrences minus the suppressions
    # exactly. Measured on AlmaLinux 9 BaseOS: 3,210,726 - 433 = 3,210,293,
    # which is what the collector wrote.
    #
    # A source that lists the same entry twice inside one package would trip
    # this. That has not been observed, and if it happens it is worth a human
    # look rather than a silent tolerance.
    expected_edges = section_counts["provides"] - suppressed["provides"]
    report["gates"]["edges"] = {
        "pass": graph["provides_edges"] == expected_edges,
        "detail": (
            f"{graph['provides_edges']} providesCapability edges against "
            f"{section_counts['provides']} source occurrences less "
            f"{suppressed['provides']} suppressed = {expected_edges}"
        ),
    }

    # Nonzero expected targets: a graph with no typed nodes passes every
    # class-targeted shape, so "conforms" is meaningless without this.
    report["gates"]["types"] = {
        "pass": graph["capability_types"] > 0
        and graph["package_types"] > 0
        and graph["capabilities_without_label"] == 0,
        "detail": (
            f"{graph['package_types']} typed packages, "
            f"{graph['identity_types']} typed identities, "
            f"{graph['capability_types']} typed capabilities, "
            f"{graph['capabilities_without_label']} capabilities without a label"
        ),
    }

    report["gates"]["encoding"] = {
        "pass": graph["undecoded_name_count"] == 0,
        "detail": (
            f"{graph['undecoded_name_count']} name literals carry an XML entity "
            f"reference; {graph['entity_in_prose']} more outside the name predicates, "
            f"which do not gate"
        ),
        "sample": graph["undecoded_literals"],
    }

    emitted = {p: n for p, n in graph["prohibited"].items() if n}
    report["gates"]["prohibited"] = {
        "pass": not emitted,
        "detail": emitted or "no retired predicate is emitted",
    }

    # The suppression list here is a copy of the collector's, so this gate
    # catches the collector drifting from the policy it documents -- in either
    # direction. It says nothing about whether suppressing them is right; that
    # is recorded as a bounded policy choice, not a semantic claim.
    report["gates"]["policy"] = {
        "pass": graph["suppressed_token_count"] == 0,
        "detail": (
            f"{graph['suppressed_token_count']} capabilities carry a suppressed "
            f"prefix {RPM_INTERNAL_TOKEN_PREFIXES}; "
            f"{sum(suppressed.values())} such occurrences in the source"
        ),
        "sample": graph["suppressed_tokens_present"],
    }

    if ontology_root:
        shacl = run_shacl(rdf_path, ontology_root)
        report["shacl"] = shacl
        # A SHACL run that did not happen is not a pass. A run that passes
        # over zero focus nodes is not a pass either -- the types gate above
        # is what makes conformance mean something.
        if shacl["status"] == "ran":
            report["gates"]["shacl"] = {
                "pass": shacl["conforms"],
                "detail": f"{shacl['violations']} violations, inference=none",
            }
        else:
            report["gates"]["shacl"] = {
                "pass": None,
                "detail": f"not run: {shacl['reason']}",
            }

    if manifest_path:
        with open(manifest_path, encoding="utf-8") as handle:
            manifest = json.load(handle)
        over = manifest.get("graphs_over_budget", [])
        report["manifest"] = {
            "total_triples": manifest.get("total_triples"),
            "max_triples": manifest.get("max_triples"),
            "graphs_over_budget": len(over),
            "classes_over_budget": len(manifest.get("classes_over_budget", [])),
            "fan_out_cuts": manifest.get("selection", {}).get("fan_out_cuts"),
        }
        within = (
            manifest.get("max_triples") is None
            or manifest.get("total_triples", 0) <= manifest["max_triples"]
        )
        report["gates"]["budget"] = {
            "pass": within,
            "detail": (
                f"{manifest.get('total_triples')} triples against "
                f"{manifest.get('max_triples')}"
            ),
        }

    # --- deferred, reported rather than silently passed -------------------
    report["deferred"] = {
        "declarations": {
            "status": "not_run",
            "reason": "rpm:DependencyDeclaration is undecided (ontology#19)",
            "source_occurrences": {
                k: v for k, v in section_counts.items() if k != "provides"
            },
            "declared": None,
        },
        "declaration_pre": {
            "status": "not_run",
            "reason": "rpm:declarationPre is undecided (ontology#19)",
        },
    }

    failures = [name for name, gate in report["gates"].items() if gate["pass"] is False]
    report["failed_gates"] = failures
    report["pass"] = not failures
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--primary", required=True, help="primary.xml or primary.xml.gz")
    parser.add_argument("--rdf", required=True, help="N-Triples graph to audit")
    parser.add_argument("--ontology-root", help="ontology checkout, for SHACL shapes")
    parser.add_argument("--manifest", help="extraction manifest.json, for the budget gate")
    parser.add_argument("--report", help="write the JSON report here")
    args = parser.parse_args(argv)

    report = audit(args.primary, args.rdf, args.ontology_root, args.manifest)

    text = json.dumps(report, indent=2, sort_keys=True)
    if args.report:
        Path(args.report).write_text(text + "\n", encoding="utf-8")
    else:
        print(text)

    for name, gate in sorted(report["gates"].items()):
        mark = {True: "PASS", False: "FAIL", None: "not run"}[gate["pass"]]
        print(f"{mark:>8}  {name}: {gate['detail']}", file=sys.stderr)

    if report["failed_gates"]:
        print(
            f"\nFAILED: {', '.join(report['failed_gates'])}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
