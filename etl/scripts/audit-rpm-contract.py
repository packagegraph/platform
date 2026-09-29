#!/usr/bin/env python3
"""Check an RPM graph against the contract its source can support.

Parses repodata independently of the Rust collector and compares what the
source says to what the graph asserts. Comparing two outputs of the same
parser is not independence, so this reads `primary.xml` with the standard
library and never imports the collector.

Two distinctions run through the whole file, because collapsing either one
produced a gate that could not fail:

**Relations are not serialized occurrences.** RDF is a set of triples. The
collector writes one `providesCapability` line per source occurrence without
deduplicating, and AlmaLinux 9 BaseOS declares 566 (package, token) pairs
twice -- so 3,210,293 edge lines carry 3,209,727 distinct relations. `edges`
checks the lines, `pairs` checks the relations, and the two catch different
things.

**Explicit types are not entailed membership.** `pkg:capabilityName` has
`rdfs:domain pkg:Capability` and `pkg:providesCapability` has
`rdfs:range pkg:Capability`, so RDFS *does* entail capability membership from
a graph with every type triple deleted. That makes the type triples a
serialization requirement, which no conformance result can speak to.

Nine gates run against the two files. Each can fail the run:

  pairs      Every (declaring package, capability token) relation the source
             states is in the graph, and the reverse. A capability credited
             to the wrong package passes a token-inventory check and fails
             here.
  edges      The count of `providesCapability` lines equals the source's
             non-suppressed Provides occurrences, exactly. Catches a lost
             duplicate occurrence, which `pairs` cannot see.
  coverage   Every capability token, binary package and package identity the
             source implies carries its `rdf:type` explicitly. Expected
             subject sets derived from the source, compared against asserted
             subjects before any inference, counting distinct nodes rather
             than type-assertion lines.
  types      Nonzero asserted subjects per gated class, and every asserted
             capability has both an `rdfs:label` and a `capabilityName`.
             Completeness among nodes that are already typed; `coverage` is
             what establishes that they all are.
  encoding   No name-bearing literal carries an undecoded XML entity. RPM's
             rich dependency syntax uses `<` and `>`, which XML escapes;
             reading the raw attribute bytes put `&gt;` into 9,919 identity
             names.
  prohibited No predicate the contract retired is still being emitted.
  parse      Every line in the graph matched one of the two N-Triples forms.
             A line the audit cannot read is a line it is not checking, so it
             fails rather than being skipped.
  policy     The tokens the collector suppresses by policy really are absent,
             and every token it does not suppress is present.
  keys       The mirrored derivations this audit depends on still match the
             collector: the suppression prefixes, the version-string format,
             and the manifest field names.

Three more run only when their input is supplied, and report `not_run` rather
than passing when it is not:

  budget     `total_triples <= max_triples` in `--manifest`, with both keys
             present. A manifest of a shape this audit does not understand
             fails rather than reading as unlimited.
  shacl      Conforms under `inference="none"`, reported beside the asserted
             focus count so conformance over zero nodes cannot be mistaken
             for a check.
  shacl_rdfs Conforms under the regime the ontology declares, reported beside
             the post-inference focus count. The two see different focus
             sets, in both directions: `none` leaves every superclass-targeted
             shape with no nodes, so `PackageShape` reaches no
             `pkg:BinaryPackage`; `rdfs` restores capability membership that
             was never written down.

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

# Classes whose asserted subjects the coverage gate holds to a source-derived
# expectation. Each needs a derivation that is exact on real data or the gate
# false-fails; these three were checked against AlmaLinux 9 BaseOS at 46,547
# capability, 2,996 binary-package and 3,787 identity nodes.
COVERED_CLASSES = (
    f"{PKG}Capability",
    f"{PKG}BinaryPackage",
    f"{PKG}PackageIdentity",
)

# Counted and reported, but not held to an expectation: a source RPM is shared
# across its subpackages (545 nodes for 2,996 packages) and the audit has no
# independent derivation of the version-node set.
REPORTED_CLASSES = (
    f"{RPM}BinaryRPM",
    f"{PKG}SourcePackage",
    f"{RPM}SourceRPM",
    f"{PKG}Version",
)

# Mirrored from RPM_INTERNAL_TOKEN_PREFIXES in etl/pg-collect/src/rpm.rs. These
# are ordinary capabilities with real providers -- glibc provides rtld(GNU_HASH)
# and 1,199 packages require it -- suppressed because they describe RPM's own
# contract. Mirroring rather than importing is deliberate: an audit that reads
# the collector's own list cannot detect the collector drifting from its stated
# policy. The `keys` gate is what notices if these fall out of step.
RPM_INTERNAL_TOKEN_PREFIXES = ("config(", "rpmlib(", "rtld(")


def is_rpm_internal_token(name):
    return name.startswith(RPM_INTERNAL_TOKEN_PREFIXES)


def version_string(ver, rel, arch):
    """Mirrored from rpm.rs:1061, `format!("{}-{}.{}", ver, rel, arch)`.

    The pairs gate needs a package key both sides can build independently,
    and the name alone is not one: BaseOS carries several versions of 475
    (name, arch) pairs, so a name-keyed comparison collapsed 3,209,727
    relations into 107,256.
    """
    return f"{ver}-{rel}.{arch}"


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

# Shape graphs the audit applies, and the axioms the declared regime needs.
# `*.examples.ttl` is excluded from both: it is the ontology's own instance
# data, and merging it in added 2 violations that belong to the examples rather
# than to any corpus. An audit that reports someone else's defects as yours is
# not usable.
SHAPE_FILES = ("core/core.shacl.ttl", "ecosystems/rpm/rpm.shacl.ttl")
AXIOM_FILES = ("core/core.ttl", "core/skos-schemes.ttl", "ecosystems/rpm/rpm.ttl")


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


class SourceFacts:
    """What the source says, derived without the collector.

    The sets here are bounded by distinct names -- 46,547 tokens and 3,787
    identities on BaseOS -- not by the 3.2M occurrences. The pair stream is
    the part that goes to disk.
    """

    def __init__(self):
        self.packages = 0
        self.occurrences = {section: 0 for section in SECTIONS}
        self.suppressed = {section: 0 for section in SECTIONS}
        self.expected_tokens = set()
        self.expected_package_keys = set()
        self.expected_identity_keys = set()
        self.expected_identity_names = set()
        self.provides_pairs = 0


def stream_source(primary_path, pair_out):
    """Stream primary.xml once, writing (package key, token) pairs to disk."""
    facts = SourceFacts()

    with open_primary(primary_path) as handle:
        for _event, elem in ET.iterparse(handle, events=("end",)):
            if elem.tag != f"{COMMON_NS}package":
                continue
            facts.packages += 1

            name_node = elem.find(f"{COMMON_NS}name")
            arch_node = elem.find(f"{COMMON_NS}arch")
            version_node = elem.find(f"{COMMON_NS}version")
            name = name_node.text if name_node is not None else None
            arch = arch_node.text if arch_node is not None else None

            key = None
            if name and arch and version_node is not None:
                key = "{}\t{}".format(
                    name,
                    version_string(
                        version_node.get("ver"), version_node.get("rel"), arch
                    ),
                )
                facts.expected_package_keys.add(key)
                facts.expected_identity_keys.add((arch, name))
                facts.expected_identity_names.add(name)

            fmt = elem.find(f"{COMMON_NS}format")
            if fmt is not None:
                for section in SECTIONS:
                    node = fmt.find(f"{RPM_NS}{section}")
                    if node is None:
                        continue
                    for entry in node.findall(f"{RPM_NS}entry"):
                        token = entry.get("name")
                        if not token:
                            continue
                        facts.occurrences[section] += 1
                        if is_rpm_internal_token(token):
                            facts.suppressed[section] += 1
                            continue
                        if section == "provides":
                            facts.expected_tokens.add(token)
                            if key is not None:
                                pair_out.write(f"{key}\t{token}\n")
                                facts.provides_pairs += 1
                        elif arch:
                            # Dependency identities are minted with the
                            # DECLARING package's arch, not the target's.
                            facts.expected_identity_keys.add((arch, token))
                            facts.expected_identity_names.add(token)
            elem.clear()

    return facts


class GraphFacts:
    """What the graph asserts, read as bytes on disk.

    `asserted` holds distinct nodes per class. Counting type assertions
    instead reported 11,984 "typed packages" for 2,996 package nodes and
    27,850 "identities" for 3,787, because the collector writes an identity's
    type once per reference.
    """

    def __init__(self):
        self.triples = 0
        self.unparsed = 0
        self.unparsed_sample = []
        self.undecoded_name_count = 0
        self.undecoded_literals = []
        self.entity_in_prose = 0
        self.prohibited = {p: 0 for p in PROHIBITED_PREDICATES}
        self.suppressed_token_count = 0
        self.suppressed_tokens_present = []
        self.provides_edge_lines = 0
        self.asserted = {c: set() for c in COVERED_CLASSES + REPORTED_CLASSES}
        self.capability_token = {}
        self.package_name = {}
        self.has_version = {}
        self.version_literal = {}
        self.identity_names = set()
        self.labelled = set()
        self.named = set()


def stream_graph_facts(rdf_path):
    """First pass: every fact that does not need the URI maps to exist yet."""
    facts = GraphFacts()

    with open(rdf_path, "r", encoding="utf-8") as handle:
        for raw in handle:
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            facts.triples += 1

            iri = NT_IRI.match(line)
            if iri:
                subject, predicate, obj = iri.groups()
                subject = subject[1:-1] if subject.startswith("<") else subject
                obj = obj[1:-1] if obj.startswith("<") else obj
                if predicate in facts.prohibited:
                    facts.prohibited[predicate] += 1
                elif predicate == RDF_TYPE:
                    if obj in facts.asserted:
                        facts.asserted[obj].add(subject)
                elif predicate == f"{PKG}providesCapability":
                    facts.provides_edge_lines += 1
                elif predicate == f"{PKG}hasVersion":
                    facts.has_version[subject] = obj
                continue

            literal = NT_LITERAL.match(line)
            if not literal:
                # Neither form matched. Counted and gated rather than skipped:
                # a line the audit cannot read is a line it is not checking.
                facts.unparsed += 1
                if len(facts.unparsed_sample) < 5:
                    facts.unparsed_sample.append(line[:200])
                continue

            subject, predicate, value = literal.groups()
            subject = subject[1:-1] if subject.startswith("<") else subject

            if UNDECODED_ENTITY.search(value):
                if predicate in NAME_PREDICATES:
                    facts.undecoded_name_count += 1
                    if len(facts.undecoded_literals) < 20:
                        facts.undecoded_literals.append(
                            {
                                "subject": subject,
                                "predicate": predicate,
                                "value": value,
                            }
                        )
                else:
                    facts.entity_in_prose += 1

            if predicate == f"{PKG}capabilityName":
                token = unescape_nt(value)
                facts.capability_token[subject] = token
                facts.named.add(subject)
                if is_rpm_internal_token(token):
                    facts.suppressed_token_count += 1
                    if len(facts.suppressed_tokens_present) < 20:
                        facts.suppressed_tokens_present.append(token)
            elif predicate == f"{PKG}packageName":
                facts.package_name[subject] = unescape_nt(value)
            elif predicate == f"{PKG}versionString":
                facts.version_literal[subject] = unescape_nt(value)
            elif predicate == f"{PKG}identityName":
                facts.identity_names.add(unescape_nt(value))
            elif predicate == RDFS_LABEL:
                facts.labelled.add(subject)

    return facts


def stream_graph_pairs(rdf_path, facts, pair_out):
    """Second pass: resolve each provides edge to a (package key, token) pair.

    Two passes rather than one because an edge can appear before the literals
    naming either end, and the alternative -- buffering every edge -- is the
    3.2M-element structure the disk-backed comparison exists to avoid.

    An edge whose ends cannot be named is counted, not dropped. Silently
    skipping it would let a package with no `packageName` hide from the
    comparison entirely.
    """
    written = 0
    unresolved = {"package": 0, "capability": 0}

    with open(rdf_path, "r", encoding="utf-8") as handle:
        for raw in handle:
            iri = NT_IRI.match(raw.strip())
            if not iri:
                continue
            subject, predicate, obj = iri.groups()
            if predicate != f"{PKG}providesCapability":
                continue
            subject = subject[1:-1] if subject.startswith("<") else subject
            obj = obj[1:-1] if obj.startswith("<") else obj

            name = facts.package_name.get(subject)
            version = facts.version_literal.get(facts.has_version.get(subject, ""))
            token = facts.capability_token.get(obj)
            if name is None or version is None:
                unresolved["package"] += 1
                continue
            if token is None:
                unresolved["capability"] += 1
                continue
            pair_out.write(f"{name}\t{version}\t{token}\n")
            written += 1

    return written, unresolved


def sorted_unique(path):
    """External sort, so neither pair stream has to fit in memory."""
    out = path + ".sorted"
    env = dict(os.environ, LC_ALL="C")
    subprocess.run(["sort", "-u", "-o", out, path], check=True, env=env)
    return out


def line_count(path):
    with open(path, "rb") as handle:
        return sum(1 for _ in handle)


def compare_sorted(left_path, right_path, limit=20):
    """Merge-walk two sorted files; return counts and a bounded sample."""
    only_left, only_right, both = 0, 0, 0
    sample_left, sample_right = [], []

    with open(left_path, encoding="utf-8") as left, open(
        right_path, encoding="utf-8"
    ) as right:
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


def run_shacl(rdf_path, ontology_root, regime):
    """Validate under one entailment regime, reporting the focus set it saw.

    A conformance boolean is not self-describing, so this returns the focus
    count beside it. Two regimes, because they see different focus sets -- in
    both directions.

    `none` is the honest reading of the bytes on disk. A corpus with its type
    triples deleted has zero `pkg:Capability` focus nodes, and every
    class-targeted shape over it conforms vacuously. It is blind a second way
    too: `sh:targetClass pkg:Package` reaches no `pkg:BinaryPackage` node
    without subclass entailment, so `PackageShape` never runs against a
    package in the corpus.

    `rdfs` is the regime the ontology declares, and it is not a weaker version
    of the same check. `pkg:capabilityName` has `rdfs:domain pkg:Capability`
    (core.ttl:186) and `pkg:providesCapability` has `rdfs:range
    pkg:Capability` (core.ttl:726), so RDFS *restores* capability membership
    for every node whose type triple was deleted. Measured on the type-erased
    fixture: 0 asserted, 6 effective, and validation then conforms because
    those nodes really do carry the fields the shape requires. Deleting a
    label as well makes it fail -- which is how we know the focus nodes are
    real rather than absent.

    Neither regime can therefore say whether the type triples were written
    down. That is a serialization question, and `coverage` is what answers it.
    """
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
    corpus_triples = len(data)
    capability = rdflib.URIRef(f"{PKG}Capability")
    type_iri = rdflib.URIRef(RDF_TYPE)
    asserted_focus = len(set(data.subjects(type_iri, capability)))

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

    # `inplace` so entailed triples land in `data` and the effective focus set
    # can be counted rather than assumed.
    conforms, _results, text = pyshacl.validate(
        data, shacl_graph=shapes, inference=regime, inplace=True, abort_on_first=False
    )
    effective_focus = len(set(data.subjects(type_iri, capability)))

    return {
        "status": "ran",
        "regime": regime,
        "conforms": bool(conforms),
        "violations": text.count("Constraint Violation"),
        "corpus_triples": corpus_triples,
        "capability_focus_asserted": asserted_focus,
        "capability_focus_effective": effective_focus,
        "shape_files": list(SHAPE_FILES),
        "axiom_files": list(AXIOM_FILES) if regime != "none" else [],
        "report_head": text[:2000],
    }


def check_mirrored_keys(repo_root):
    """Hold the audit's copies of collector facts to the collector.

    Three derivations above are transcriptions of Rust source, mirrored rather
    than imported so that drift is detectable. This is what detects it.
    Missing source is reported as `not_run`, never treated as agreement.
    """
    rpm_rs = Path(repo_root) / "etl" / "pg-collect" / "src" / "rpm.rs"
    extract_rs = Path(repo_root) / "etl" / "pg-collect" / "src" / "extract.rs"

    if not rpm_rs.is_file() or not extract_rs.is_file():
        return {
            "pass": None,
            "detail": f"not run: no collector source under {repo_root}",
        }

    rpm_src = rpm_rs.read_text(encoding="utf-8")
    extract_src = extract_rs.read_text(encoding="utf-8")
    problems = []

    match = re.search(
        r"RPM_INTERNAL_TOKEN_PREFIXES: &\[&str\] = &\[([^\]]*)\];", rpm_src
    )
    if match is None:
        problems.append("RPM_INTERNAL_TOKEN_PREFIXES not found in rpm.rs")
    else:
        declared = tuple(sorted(re.findall(r'"([^"]*)"', match.group(1))))
        if declared != tuple(sorted(RPM_INTERNAL_TOKEN_PREFIXES)):
            problems.append(f"suppression prefixes drifted: rpm.rs has {declared}")

    # `version_string` mirrors this. A change here silently breaks the pairs
    # gate's key, so it is pinned rather than trusted.
    if 'format!("{}-{}.{}", ver, rel, arch)' not in rpm_src:
        problems.append("rpm.rs no longer builds version_str as {ver}-{rel}.{arch}")

    for field in (
        "total_triples",
        "max_triples",
        "graphs_over_budget",
        "classes_over_budget",
    ):
        if f"pub {field}:" not in extract_src:
            problems.append(f"extract.rs no longer has {field}")
    if "serde(rename" in extract_src:
        problems.append("a serde rename in extract.rs may have moved the manifest keys")

    return {
        "pass": not problems,
        "detail": problems
        or "suppression prefixes, version format and manifest keys all match",
    }


def audit(
    primary_path, rdf_path, ontology_root=None, manifest_path=None, repo_root=None
):
    report = {
        "source": {"path": str(primary_path)},
        "graph": {"path": str(rdf_path)},
        "gates": {},
    }

    workdir = tempfile.mkdtemp(prefix="rpm-contract-audit.")
    source_pairs = os.path.join(workdir, "source-pairs.txt")
    graph_pairs = os.path.join(workdir, "graph-pairs.txt")

    report["source"]["sha256"] = sha256_file(primary_path)
    report["source"]["uncompressed_sha256"] = uncompressed_sha256(primary_path)
    report["graph"]["sha256"] = sha256_file(rdf_path)

    with open(source_pairs, "w", encoding="utf-8") as handle:
        src = stream_source(primary_path, handle)

    expected_edges = src.occurrences["provides"] - src.suppressed["provides"]
    report["source"].update(
        {
            "packages": src.packages,
            "section_occurrences": src.occurrences,
            "suppressed_by_policy": src.suppressed,
            "supported_occurrences": sum(src.occurrences.values()),
            "expected_provides_occurrences": expected_edges,
            "expected_capability_tokens": len(src.expected_tokens),
            "expected_binary_packages": len(src.expected_package_keys),
            "expected_identity_nodes": len(src.expected_identity_keys),
            "expected_identity_names": len(src.expected_identity_names),
        }
    )

    graph = stream_graph_facts(rdf_path)
    with open(graph_pairs, "w", encoding="utf-8") as handle:
        pair_lines, unresolved = stream_graph_pairs(rdf_path, graph, handle)

    asserted = {c: len(graph.asserted[c]) for c in graph.asserted}
    report["graph"].update(
        {
            "triples": graph.triples,
            "unparsed": graph.unparsed,
            "provides_edge_lines": graph.provides_edge_lines,
            "provides_pairs_resolved": pair_lines,
            "provides_pairs_unnameable": unresolved,
            "asserted_nodes": {c.split("#")[-1]: n for c, n in asserted.items()},
            "distinct_identity_names": len(graph.identity_names),
            "undecoded_name_count": graph.undecoded_name_count,
            "entity_in_prose": graph.entity_in_prose,
            "prohibited": graph.prohibited,
            "suppressed_token_count": graph.suppressed_token_count,
        }
    )

    # --- pairs: which package provides which capability -------------------
    source_sorted = sorted_unique(source_pairs)
    graph_sorted = sorted_unique(graph_pairs)
    comparison = compare_sorted(source_sorted, graph_sorted)
    report["pairs"] = dict(
        comparison,
        source_occurrences=src.provides_pairs,
        source_distinct=line_count(source_sorted),
        graph_lines=pair_lines,
        graph_distinct=line_count(graph_sorted),
    )
    unnameable = sum(unresolved.values())
    report["gates"]["pairs"] = {
        "pass": comparison["source_only"] == 0
        and comparison["graph_only"] == 0
        and unnameable == 0,
        "detail": (
            f"{comparison['in_both']} (package, capability) relations agree; "
            f"{comparison['source_only']} in source only, "
            f"{comparison['graph_only']} in graph only, "
            f"{unnameable} graph edges whose ends could not be named"
        ),
    }

    # --- edges: serialized occurrences, which pairs cannot see ------------
    # Distinct relations agreeing is not enough. `providesCapability` is
    # documented as per (package, capability) and never deduplicated, and
    # BaseOS states 566 pairs twice, so the line count is a separate fact
    # from the relation count.
    report["gates"]["edges"] = {
        "pass": graph.provides_edge_lines == expected_edges,
        "detail": (
            f"{graph.provides_edge_lines} providesCapability lines against "
            f"{src.occurrences['provides']} source occurrences less "
            f"{src.suppressed['provides']} suppressed = {expected_edges}; "
            f"{report['pairs']['source_distinct']} distinct relations"
        ),
    }

    # --- coverage: every expected subject carries its type explicitly -----
    # Asserted subjects, before any inference, counted as distinct nodes.
    # RDFS entails capability membership from capabilityName and
    # providesCapability, so no conformance result can establish this.
    observed_tokens = {
        graph.capability_token[uri]
        for uri in graph.asserted[f"{PKG}Capability"]
        if uri in graph.capability_token
    }
    observed_keys = set()
    for uri in graph.asserted[f"{PKG}BinaryPackage"]:
        name = graph.package_name.get(uri)
        version = graph.version_literal.get(graph.has_version.get(uri, ""))
        if name is not None and version is not None:
            observed_keys.add(f"{name}\t{version}")

    missing_tokens = src.expected_tokens - observed_tokens
    missing_keys = src.expected_package_keys - observed_keys
    missing_names = src.expected_identity_names - graph.identity_names
    identity_shortfall = len(src.expected_identity_keys) - asserted[
        f"{PKG}PackageIdentity"
    ]

    report["coverage_detail"] = {
        "capability_tokens_missing_count": len(missing_tokens),
        "capability_tokens_missing": sorted(missing_tokens)[:20],
        "binary_packages_missing_count": len(missing_keys),
        "binary_packages_missing": sorted(missing_keys)[:20],
        "identity_names_missing_count": len(missing_names),
        "identity_names_missing": sorted(missing_names)[:20],
        "identity_node_shortfall": identity_shortfall,
    }
    report["gates"]["coverage"] = {
        "pass": not missing_tokens
        and not missing_keys
        and not missing_names
        and identity_shortfall == 0,
        # Counts alone mislead here: against the pre-fix collector the
        # cardinalities all matched while the token SETS differed, because
        # each escaped variant substituted for a real one. The missing counts
        # are what the failure means, so they lead.
        "detail": (
            f"missing {len(missing_tokens)} capability types, "
            f"{len(missing_keys)} binary package types, "
            f"{len(missing_names)} identity names, "
            f"{identity_shortfall} identity nodes "
            f"(expected {len(src.expected_tokens)} tokens / "
            f"{len(src.expected_package_keys)} packages / "
            f"{len(src.expected_identity_keys)} identity nodes / "
            f"{len(src.expected_identity_names)} identity names; "
            f"asserted {len(observed_tokens)} / {len(observed_keys)} / "
            f"{asserted[f'{PKG}PackageIdentity']} / "
            f"{len(graph.identity_names)})"
        ),
    }

    # --- types: completeness among the nodes that are typed ---------------
    typed_caps = graph.asserted[f"{PKG}Capability"]
    unlabelled = typed_caps - graph.labelled
    unnamed = typed_caps - graph.named
    report["gates"]["types"] = {
        "pass": len(typed_caps) > 0
        and asserted[f"{PKG}BinaryPackage"] > 0
        and asserted[f"{PKG}PackageIdentity"] > 0
        and not unlabelled
        and not unnamed,
        "detail": (
            f"{asserted[f'{PKG}BinaryPackage']} package nodes, "
            f"{asserted[f'{PKG}PackageIdentity']} identity nodes, "
            f"{len(typed_caps)} capability nodes, "
            f"{len(unlabelled)} without a label, "
            f"{len(unnamed)} without a capabilityName"
        ),
    }

    report["gates"]["encoding"] = {
        "pass": graph.undecoded_name_count == 0,
        "detail": (
            f"{graph.undecoded_name_count} name literals carry an XML entity "
            f"reference; {graph.entity_in_prose} more outside the name "
            f"predicates, which do not gate"
        ),
        "sample": graph.undecoded_literals,
    }

    emitted = {p: n for p, n in graph.prohibited.items() if n}
    report["gates"]["prohibited"] = {
        "pass": not emitted,
        "detail": emitted or "no retired predicate is emitted",
    }

    report["gates"]["parse"] = {
        "pass": graph.unparsed == 0,
        "detail": f"{graph.unparsed} lines matched neither N-Triples form",
        "sample": graph.unparsed_sample,
    }

    report["gates"]["policy"] = {
        "pass": graph.suppressed_token_count == 0,
        "detail": (
            f"{graph.suppressed_token_count} capabilities carry a suppressed "
            f"prefix {RPM_INTERNAL_TOKEN_PREFIXES}; "
            f"{sum(src.suppressed.values())} such occurrences in the source"
        ),
        "sample": graph.suppressed_tokens_present,
    }

    report["gates"]["keys"] = check_mirrored_keys(
        repo_root or Path(__file__).resolve().parent.parent.parent
    )

    if ontology_root:
        report["shacl"] = {}
        for regime, gate in (("none", "shacl"), ("rdfs", "shacl_rdfs")):
            result = run_shacl(rdf_path, ontology_root, regime)
            report["shacl"][regime] = result
            if result["status"] == "ran":
                report["gates"][gate] = {
                    "pass": result["conforms"],
                    "detail": (
                        f"{result['violations']} violations, inference={regime}, "
                        f"{result['capability_focus_asserted']} asserted / "
                        f"{result['capability_focus_effective']} effective "
                        f"capability focus nodes"
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
        report["manifest"] = {
            "total_triples": manifest.get("total_triples"),
            "max_triples": manifest.get("max_triples"),
            "graphs_over_budget": len(manifest.get("graphs_over_budget", [])),
            "classes_over_budget": len(manifest.get("classes_over_budget", [])),
            "fan_out_cuts": manifest.get("selection", {}).get("fan_out_cuts"),
        }
        # Both keys are non-Option fields on extract.rs's `Manifest`, so a
        # missing one means the manifest is not the one this audit
        # understands. Treating that as "within budget" would be a silent pass.
        absent = [k for k in ("total_triples", "max_triples") if k not in manifest]
        if absent:
            report["gates"]["budget"] = {
                "pass": False,
                "detail": f"manifest has no {', '.join(absent)}",
            }
        else:
            report["gates"]["budget"] = {
                "pass": manifest["total_triples"] <= manifest["max_triples"],
                "detail": (
                    f"{manifest['total_triples']} triples against "
                    f"{manifest['max_triples']}"
                ),
            }

    # --- deferred, reported rather than silently passed -------------------
    # `declared` is null, not 0: a consumer reading 0 cannot tell "none found"
    # from "never looked", and there is no term to count against yet.
    report["per_kind"] = {
        kind: {
            "source": src.occurrences[kind],
            "rejected_by_policy": src.suppressed[kind],
            "expected": src.occurrences[kind] - src.suppressed[kind],
            "declared_lines": graph.provides_edge_lines if kind == "provides" else None,
            "declared_relations": (
                report["pairs"]["graph_distinct"] if kind == "provides" else None
            ),
            "status": "compared" if kind == "provides" else "not_run",
            "reason": (
                None
                if kind == "provides"
                else "rpm:DependencyDeclaration is undecided (ontology#19)"
            ),
        }
        for kind in SECTIONS
    }
    report["deferred"] = {
        "declarations": {
            "status": "not_run",
            "reason": "rpm:DependencyDeclaration is undecided (ontology#19)",
            "source_occurrences": {
                k: v for k, v in src.occurrences.items() if k != "provides"
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
        "classes_held_to_source": [c.split("#")[-1] for c in COVERED_CLASSES],
        "classes_counted_only": [c.split("#")[-1] for c in REPORTED_CLASSES],
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
    parser.add_argument(
        "--primary", required=True, help="primary.xml or primary.xml.gz"
    )
    parser.add_argument("--rdf", required=True, help="N-Triples graph to audit")
    parser.add_argument("--ontology-root", help="ontology checkout, for SHACL shapes")
    parser.add_argument(
        "--manifest", help="extraction manifest.json, for the budget gate"
    )
    parser.add_argument(
        "--repo-root", help="platform checkout, for the mirrored-key gate"
    )
    parser.add_argument("--report", help="write the JSON report here")
    args = parser.parse_args(argv)

    report = audit(
        args.primary, args.rdf, args.ontology_root, args.manifest, args.repo_root
    )

    text = json.dumps(report, indent=2, sort_keys=True)
    if args.report:
        Path(args.report).write_text(text + "\n", encoding="utf-8")
    else:
        print(text)

    for name, gate in sorted(report["gates"].items()):
        mark = {True: "PASS", False: "FAIL", None: "not run"}[gate["pass"]]
        print(f"{mark:>8}  {name}: {gate['detail']}", file=sys.stderr)

    if report["failed_gates"]:
        print(f"\nFAILED: {', '.join(report['failed_gates'])}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
