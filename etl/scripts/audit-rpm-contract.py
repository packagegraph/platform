#!/usr/bin/env python3
"""Check an RPM graph against the contract its source can support.

Parses repodata independently of the Rust collector and compares what the
source says to what the graph asserts. Comparing two outputs of the same
parser is not independence, so this reads `primary.xml` with the standard
library and never imports the collector.

Seven gates always run against the two files. Each can fail the run:

  fidelity   Every Provides token in the source is a capability in the graph,
             and every capability in the graph traces to a source token.
  edges      The count of `providesCapability` edges equals the source's
             Provides occurrences less the suppressed ones, exactly. Distinct
             tokens can agree while a per-provider edge is missing.
  types      The graph carries `rdf:type` for its packages and capabilities,
             and every typed capability has both an `rdfs:label` and a
             `capabilityName`. A corpus stripped of types satisfies every
             class-targeted shape over zero nodes, so a SHACL pass alone
             cannot establish this. Checked as set equality against the typed
             nodes, so a name on some untyped subject cannot make it up.
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

Three more run only when their input is supplied, and report `not_run` rather
than passing when it is not:

  budget     `total_triples <= max_triples` in `--manifest`, with both keys
             present. A manifest of a shape this audit does not understand
             fails rather than reading as unlimited.
  shacl      Conforms under `inference="none"` against `--ontology-root`.
  shacl_rdfs Conforms under the regime the ontology declares. Checked
             separately because the two are blind to different things:
             `none` leaves every superclass-targeted shape with no focus
             nodes, so `PackageShape` reaches no `pkg:BinaryPackage` at all.

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
        "capabilities_without_name": 0,
        "identity_types": 0,
        "entity_in_prose": 0,
        "undecoded_name_count": 0,
        "unparsed": 0,
        "unparsed_sample": [],
        "suppressed_token_count": 0,
        "suppressed_tokens_present": [],
    }
    # Sets, not counters: the gate is set equality against the typed nodes, so
    # a capabilityName sitting on some untyped subject cannot make the counts
    # coincide. ~46.5k URIs for BaseOS, ~1M corpus-wide -- well within reach.
    labelled = set()
    named = set()
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
                named.add(subject)
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
    state["capabilities_without_name"] = len(typed - named)
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


# Shape graphs the audit applies, and the axioms the declared regime needs.
# `*.examples.ttl` is excluded from both: it is the ontology's own instance
# data, and merging it in added 2 violations that belong to the examples rather
# than to any corpus. An audit that reports someone else's defects as yours is
# not usable.
SHAPE_FILES = ("core/core.shacl.ttl", "ecosystems/rpm/rpm.shacl.ttl")
AXIOM_FILES = ("core/core.ttl", "core/skos-schemes.ttl", "ecosystems/rpm/rpm.ttl")


def run_shacl(rdf_path, ontology_root, regime):
    """Validate under one entailment regime, or say plainly that it did not run.

    Two regimes, reported separately, because they fail differently.

    `none` is the honest reading of the bytes on disk. It is also blind in a
    second way beyond the empty-focus-node problem: `sh:targetClass pkg:Package`
    reaches no `pkg:BinaryPackage` node at all without subclass entailment, so
    `PackageShape` never runs against a single package in the corpus.

    `rdfs` is the regime the ontology declares ("all examples pass validation --
    pyshacl with RDFS inference"). It makes the superclass shapes apply. It does
    not rescue the missing-type case: measured on the type-erased fixture, RDFS
    reports conformance too, because nothing entails `pkg:Capability`
    membership from a capability's name or its provides edge. The type gate is
    what catches that, under either regime.
    """
    # Imported here, not at module scope: the audit's other nine gates must
    # keep working without pySHACL installed, reporting these two as not_run.
    try:
        import pyshacl
        import rdflib
    except ImportError as exc:
        return {
            "status": "not_run",
            "regime": regime,
            "reason": f"missing dependency: {exc.name}",
        }

    root = Path(ontology_root)
    missing = [f for f in SHAPE_FILES if not (root / f).is_file()]
    if missing:
        return {
            "status": "not_run",
            "regime": regime,
            "reason": f"no shapes at {', '.join(str(root / m) for m in missing)}",
        }

    data = rdflib.Graph()
    data.parse(rdf_path, format="nt")
    triples_read = len(data)

    if regime != "none":
        absent = [f for f in AXIOM_FILES if not (root / f).is_file()]
        if absent:
            return {
                "status": "not_run",
                "regime": regime,
                "reason": f"no axioms at {', '.join(str(root / a) for a in absent)}",
            }
        for name in AXIOM_FILES:
            data.parse(str(root / name), format="turtle")

    shapes = rdflib.Graph()
    for name in SHAPE_FILES:
        shapes.parse(str(root / name), format="turtle")

    conforms, _results_graph, text = pyshacl.validate(
        data, shacl_graph=shapes, inference=regime, abort_on_first=False
    )
    return {
        "status": "ran",
        "regime": regime,
        "conforms": bool(conforms),
        "violations": text.count("Constraint Violation"),
        "corpus_triples": triples_read,
        "shape_files": list(SHAPE_FILES),
        "axiom_files": list(AXIOM_FILES) if regime != "none" else [],
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
        and graph["capabilities_without_label"] == 0
        and graph["capabilities_without_name"] == 0,
        "detail": (
            f"{graph['package_types']} typed packages, "
            f"{graph['identity_types']} typed identities, "
            f"{graph['capability_types']} typed capabilities, "
            f"{graph['capabilities_without_label']} without a label, "
            f"{graph['capabilities_without_name']} without a capabilityName"
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
        # No inference first, then the declared regime, separately. A SHACL run
        # that did not happen is not a pass, and neither is one that passed
        # over zero focus nodes -- the types gate above is what makes either
        # conformance result mean anything.
        report["shacl"] = {}
        for regime, gate in (("none", "shacl"), ("rdfs", "shacl_rdfs")):
            result = run_shacl(rdf_path, ontology_root, regime)
            report["shacl"][regime] = result
            if result["status"] == "ran":
                report["gates"][gate] = {
                    "pass": result["conforms"],
                    "detail": (
                        f"{result['violations']} violations, inference={regime}, "
                        f"{result['corpus_triples']} corpus triples"
                    ),
                }
            else:
                report["gates"][gate] = {
                    "pass": None,
                    "detail": f"not run: {result['reason']}",
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
        # Both keys are non-Option fields on extract.rs's `Manifest`, so a
        # missing one means the manifest is not the one this audit understands.
        # Treating that as "within budget" would be a silent pass, which is the
        # failure mode this whole script exists to remove.
        missing = [k for k in ("total_triples", "max_triples") if k not in manifest]
        if missing:
            report["gates"]["budget"] = {
                "pass": False,
                "detail": f"manifest has no {', '.join(missing)}",
            }
        else:
            report["gates"]["budget"] = {
                "pass": manifest["total_triples"] <= manifest["max_triples"],
                "detail": (
                    f"{manifest['total_triples']} triples against "
                    f"{manifest['max_triples']}"
                ),
            }

    # --- per-kind accounting ---------------------------------------------
    # `declared` is the number the graph asserts for that kind. It is null,
    # not 0, for the three kinds with no term to assert: 0 would be a
    # measurement of an absence, and a consumer cannot tell the two apart.
    report["per_kind"] = {
        kind: {
            "source": section_counts[kind],
            "rejected_by_policy": suppressed[kind],
            "expected": section_counts[kind] - suppressed[kind],
            "declared": graph["provides_edges"] if kind == "provides" else None,
            "status": "compared" if kind == "provides" else "not_run",
            "reason": (
                None
                if kind == "provides"
                else "rpm:DependencyDeclaration is undecided (ontology#19)"
            ),
        }
        for kind in SECTIONS
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

    # What this run actually covered, in the report rather than only in prose.
    # `pass: true` on its own invites the reading that everything was checked.
    report["coverage"] = {
        "kinds_compared": [
            k for k, v in report["per_kind"].items() if v["status"] == "compared"
        ],
        "kinds_not_compared": [
            k for k, v in report["per_kind"].items() if v["status"] != "compared"
        ],
        "gates_run": sorted(
            name for name, g in report["gates"].items() if g["pass"] is not None
        ),
        "gates_not_run": sorted(
            name for name, g in report["gates"].items() if g["pass"] is None
        ),
        "shacl": {
            regime: result["status"]
            for regime, result in report.get("shacl", {}).items()
        }
        or "not_requested",
        "budget": "checked" if "budget" in report["gates"] else "no manifest given",
    }

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
