#!/usr/bin/env python3
"""Drive audit-rpm-contract.py against a clean fixture pair and mutations of it.

A gate that cannot fail is decoration. Every test below takes the clean pair,
breaks exactly one thing, and asserts the matching gate goes red.

Four of these mutations exist because an earlier version of the audit passed
them. They are kept as the regression set:

  * a capability credited to the wrong package (token inventories unchanged)
  * one capability's type triple deleted (five typed instead of six)
  * every PackageIdentity type deleted (zero typed identities)
  * every Capability type deleted, which RDFS then restores
"""

import json
import re
import subprocess
import sys
import tempfile
import unittest
from importlib.machinery import SourceFileLoader
from pathlib import Path
from urllib.parse import quote

TESTS_DIR = Path(__file__).resolve().parent
SCRIPTS_DIR = TESTS_DIR.parent
REPO_ROOT = SCRIPTS_DIR.parent.parent
AUDIT = SCRIPTS_DIR / "audit-rpm-contract.py"
PRIMARY = TESTS_DIR / "fixtures" / "audit-primary.xml"
GRAPH = TESTS_DIR / "fixtures" / "audit-graph.nt"

PKG = "https://purl.org/packagegraph/ontology/core#"
RPM = "https://purl.org/packagegraph/ontology/rpm#"
TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
LABEL = "http://www.w3.org/2000/01/rdf-schema#label"
DATA = "https://packagegraph.github.io/d/"

BASH = f"{DATA}pkg/almalinux/9/x86_64/bash/5.1.8-9.el9.x86_64"
GLIBC = f"{DATA}pkg/almalinux/9/x86_64/glibc/2.34-60.el9.x86_64"

audit_module = SourceFileLoader("audit_rpm_contract", str(AUDIT)).load_module()


def capability_uri(token):
    return f"{DATA}capability/{quote(token, safe='')}"


def ontology_root():
    import os

    root = os.environ.get("ONTOLOGY_REPO", str(REPO_ROOT.parent / "ontology"))
    return root if (Path(root) / "core" / "core.shacl.ttl").is_file() else None


def run_audit(graph_path, primary_path=PRIMARY, manifest=None, ontology_root_=None):
    """Call the audit in-process and return its report."""
    return audit_module.audit(
        str(primary_path),
        str(graph_path),
        ontology_root=ontology_root_,
        manifest_path=manifest,
        repo_root=str(REPO_ROOT),
    )


class MutationCase(unittest.TestCase):
    """Base: hand each test a private copy of the clean graph to break."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.graph = Path(self.tmp.name) / "graph.nt"
        self.graph.write_text(GRAPH.read_text(encoding="utf-8"), encoding="utf-8")

    def lines(self):
        return self.graph.read_text(encoding="utf-8").splitlines()

    def rewrite(self, lines):
        self.graph.write_text("\n".join(lines) + "\n", encoding="utf-8")

    def drop_matching(self, predicate, expected=None):
        """Drop every line matching a predicate; assert how many went."""
        before = self.lines()
        kept = [line for line in before if predicate(line) is not True]
        gone = len(before) - len(kept)
        if expected is not None:
            self.assertEqual(expected, gone, "mutation did not remove what it meant to")
        else:
            self.assertGreater(gone, 0, "mutation removed nothing")
        self.rewrite(kept)
        return gone

    def drop_first(self, predicate):
        """Drop only the first matching line.

        Distinct from drop_matching: "one capability has no label" and "no
        capability has a label" are different mutations, and conflating them
        hid which one the gate was catching.
        """
        before = self.lines()
        for index, line in enumerate(before):
            if predicate(line) is True:
                self.rewrite(before[:index] + before[index + 1 :])
                return line
        self.fail("mutation matched nothing")

    def assertGateFails(self, report, gate):
        self.assertIn(gate, report["failed_gates"], report["gates"][gate]["detail"])
        self.assertFalse(report["pass"])

    def assertOnlyGateFails(self, report, gate):
        self.assertEqual([gate], report["failed_gates"], report["gates"])

    def assertGatesFail(self, report, gates):
        self.assertEqual(sorted(gates), sorted(report["failed_gates"]), report["gates"])


class CleanPair(unittest.TestCase):
    def test_the_clean_fixture_pair_passes_every_gate(self):
        report = run_audit(GRAPH)
        self.assertEqual([], report["failed_gates"], report["gates"])
        self.assertTrue(report["pass"])

    def test_relations_and_serialized_occurrences_are_reported_separately(self):
        # RDF is a set of triples; the collector writes one line per source
        # occurrence without deduplicating. On the fixture the two agree, but
        # they are distinct facts and the report keeps them apart. BaseOS
        # states 566 pairs twice, so there they differ.
        report = run_audit(GRAPH)
        self.assertEqual(7, report["graph"]["provides_edge_lines"])
        self.assertEqual(7, report["pairs"]["graph_lines"])
        self.assertEqual(7, report["pairs"]["graph_distinct"])
        self.assertEqual(
            report["per_kind"]["provides"]["declared_lines"],
            report["graph"]["provides_edge_lines"],
        )
        self.assertEqual(
            report["per_kind"]["provides"]["declared_relations"],
            report["pairs"]["graph_distinct"],
        )

    def test_the_source_side_counts_what_it_suppresses_separately(self):
        report = run_audit(GRAPH)
        self.assertEqual(2, report["source"]["suppressed_by_policy"]["provides"])
        self.assertEqual(1, report["source"]["suppressed_by_policy"]["requires"])

    def test_undecided_terms_report_not_run_rather_than_zero(self):
        report = run_audit(GRAPH)
        for key in ("declarations", "declaration_pre"):
            self.assertEqual("not_run", report["deferred"][key]["status"])
        for kind in ("requires", "conflicts", "obsoletes"):
            entry = report["per_kind"][kind]
            self.assertIsNone(entry["declared_lines"], kind)
            self.assertIsNone(entry["declared_relations"], kind)
            self.assertEqual("not_run", entry["status"])
            self.assertGreater(entry["source"], 0, kind)

    def test_asserted_nodes_are_counted_as_nodes_not_type_assertions(self):
        # The collector writes an identity's type once per reference: 27,850
        # assertions for 3,787 nodes on BaseOS. Counting assertions reported
        # four times as many "packages" as there were packages.
        report = run_audit(GRAPH)
        nodes = report["graph"]["asserted_nodes"]
        self.assertEqual(2, nodes["BinaryPackage"])
        self.assertEqual(2, nodes["BinaryRPM"])
        self.assertEqual(6, nodes["Capability"])
        # basesystem is required by both packages, and the collector re-emits
        # an identity definition per reference (write_package_identity, not
        # the _once variant), so assertions exceed nodes here just as they do
        # at corpus scale: 27,850 over 3,787.
        raw = GRAPH.read_text(encoding="utf-8")
        self.assertEqual(7, raw.count(f"<{TYPE}> <{PKG}PackageIdentity>"))
        self.assertEqual(6, nodes["PackageIdentity"])


class WrongProvider(MutationCase):
    """Review finding 1. Passed every gate before the pairs gate existed."""

    def test_reassigning_a_capability_to_another_package_fails_pairs(self):
        token = "bash(x86-64)"
        edge = f"<{BASH}> <{PKG}providesCapability> <{capability_uri(token)}> ."
        lines = self.lines()
        self.assertIn(edge, lines)
        self.rewrite([edge.replace(BASH, GLIBC) if l == edge else l for l in lines])

        report = run_audit(self.graph)
        # Only pairs: the token inventory, every count, every type and label
        # are untouched. That is what made this invisible.
        self.assertOnlyGateFails(report, "pairs")
        self.assertEqual(1, report["pairs"]["source_only"])
        self.assertEqual(1, report["pairs"]["graph_only"])
        self.assertTrue(report["gates"]["edges"]["pass"])
        self.assertTrue(report["gates"]["coverage"]["pass"])
        self.assertTrue(report["gates"]["types"]["pass"])

    def test_the_pair_key_carries_the_version_not_just_the_name(self):
        # BaseOS ships several versions of 475 (name, arch) pairs, so a
        # name-only key collapsed 3,209,727 relations into 107,256. The
        # samples must therefore name a version.
        token = "bash(x86-64)"
        edge = f"<{BASH}> <{PKG}providesCapability> <{capability_uri(token)}> ."
        self.rewrite(
            [edge.replace(BASH, GLIBC) if l == edge else l for l in self.lines()]
        )
        report = run_audit(self.graph)
        self.assertEqual(
            [f"bash\t5.1.8-9.el9.x86_64\t{token}"], report["pairs"]["source_only_sample"]
        )
        self.assertEqual(
            [f"glibc\t2.34-60.el9.x86_64\t{token}"],
            report["pairs"]["graph_only_sample"],
        )

    def test_an_edge_whose_ends_cannot_be_named_fails_rather_than_vanishing(self):
        # Dropping a package's name makes its edges unpairable. Skipping them
        # silently would let the package disappear from the comparison.
        self.drop_matching(
            lambda l: l.startswith(f"<{BASH}>") and f"<{PKG}packageName>" in l, 1
        )
        report = run_audit(self.graph)
        self.assertGateFails(report, "pairs")
        self.assertEqual(4, report["graph"]["provides_pairs_unnameable"]["package"])


class ExplicitTypeCoverage(MutationCase):
    """Review finding 2. Both mutations passed every gate before `coverage`."""

    def test_removing_one_capability_type_fails_coverage(self):
        token = "bash(x86-64)"
        self.drop_matching(
            lambda l: l == f"<{capability_uri(token)}> <{TYPE}> <{PKG}Capability> .", 1
        )
        report = run_audit(self.graph)
        self.assertOnlyGateFails(report, "coverage")
        self.assertEqual(5, report["graph"]["asserted_nodes"]["Capability"])
        self.assertEqual(1, report["coverage_detail"]["capability_tokens_missing_count"])
        self.assertEqual([token], report["coverage_detail"]["capability_tokens_missing"])
        # Everything else still holds: the node keeps its name, label and
        # edge, so only the serialization of its type is gone.
        self.assertTrue(report["gates"]["pairs"]["pass"])
        self.assertTrue(report["gates"]["types"]["pass"])

    def test_removing_every_identity_type_fails_coverage(self):
        self.drop_matching(lambda l: f"<{TYPE}> <{PKG}PackageIdentity>" in l)
        report = run_audit(self.graph)
        self.assertGateFails(report, "coverage")
        self.assertEqual(0, report["graph"]["asserted_nodes"]["PackageIdentity"])
        self.assertGreater(report["coverage_detail"]["identity_node_shortfall"], 0)

    def test_removing_one_identitys_only_type_assertion_fails_coverage(self):
        # The partial case, which a nonzero check cannot see at all. Picks an
        # identity referenced exactly once, so dropping one line really does
        # leave the node untyped.
        self.drop_first(
            lambda l: l
            == f"<{DATA}pkg/almalinux/9/x86_64/bash-completion> <{TYPE}> "
            f"<{PKG}PackageIdentity> ."
        )
        report = run_audit(self.graph)
        self.assertGateFails(report, "coverage")
        self.assertEqual(1, report["coverage_detail"]["identity_node_shortfall"])

    def test_removing_a_binary_package_type_fails_coverage(self):
        self.drop_matching(
            lambda l: l == f"<{BASH}> <{TYPE}> <{PKG}BinaryPackage> .", 1
        )
        report = run_audit(self.graph)
        self.assertGateFails(report, "coverage")
        self.assertEqual(1, report["coverage_detail"]["binary_packages_missing_count"])
        self.assertEqual(
            ["bash\t5.1.8-9.el9.x86_64"],
            report["coverage_detail"]["binary_packages_missing"],
        )

    def test_the_expected_sets_come_from_the_source_not_from_the_graph(self):
        # A coverage gate that derived its expectation from the graph would
        # pass any graph. The counts have to match the source's own numbers.
        report = run_audit(GRAPH)
        self.assertEqual(6, report["source"]["expected_capability_tokens"])
        self.assertEqual(2, report["source"]["expected_binary_packages"])
        self.assertEqual(
            report["source"]["expected_capability_tokens"],
            report["graph"]["asserted_nodes"]["Capability"],
        )


class TypeErasureAndInference(MutationCase):
    """Review finding 3. RDFS restores what the serialization dropped."""

    def erase_capability_types(self):
        return self.drop_matching(lambda l: f"<{TYPE}> <{PKG}Capability>" in l, 6)

    def test_erasing_every_capability_type_fails_coverage(self):
        before = run_audit(self.graph)
        self.assertEqual([], before["failed_gates"])
        self.erase_capability_types()
        report = run_audit(self.graph)
        self.assertEqual(0, report["graph"]["asserted_nodes"]["Capability"])
        self.assertGateFails(report, "coverage")
        self.assertEqual(6, report["coverage_detail"]["capability_tokens_missing_count"])

    def test_the_erased_corpus_still_carries_its_names_and_edges(self):
        self.erase_capability_types()
        report = run_audit(self.graph)
        self.assertEqual(7, report["graph"]["provides_edge_lines"])
        self.assertTrue(report["gates"]["pairs"]["pass"])
        self.assertTrue(report["gates"]["edges"]["pass"])

    def test_rdfs_restores_the_membership_the_serialization_dropped(self):
        # The correction to this file's earlier claim. `capabilityName` has
        # rdfs:domain Capability (core.ttl:186) and `providesCapability` has
        # rdfs:range Capability (core.ttl:726), so the declared regime entails
        # membership for every node whose type triple was deleted. The audit
        # must MEASURE the focus sets, not read them off a conformance bool.
        try:
            import pyshacl  # noqa: F401
        except ImportError as exc:
            self.skipTest(f"pySHACL unavailable ({exc.name})")
        root = ontology_root()
        if root is None:
            self.skipTest("no ontology checkout; the declared regime needs its axioms")

        self.erase_capability_types()
        report = run_audit(self.graph, ontology_root_=root)

        none_run = report["shacl"]["none"]
        rdfs_run = report["shacl"]["rdfs"]
        self.assertEqual(0, none_run["capability_focus_asserted"])
        self.assertEqual(0, none_run["capability_focus_effective"])
        self.assertEqual(0, rdfs_run["capability_focus_asserted"])
        self.assertEqual(
            6,
            rdfs_run["capability_focus_effective"],
            "RDFS should restore all six; if not, the axioms changed",
        )
        # Conformance under `none` is vacuous -- no focus nodes at all.
        # Conformance under `rdfs` is genuine -- six real focus nodes that
        # satisfy the shape. Neither says the types were written down.
        self.assertTrue(none_run["conforms"])
        self.assertTrue(rdfs_run["conforms"])
        self.assertGateFails(report, "coverage")

    def test_erasing_a_label_too_makes_the_declared_regime_fail(self):
        # The negative control. If RDFS validation still passed here, the
        # restored nodes would not be real focus nodes and the reading above
        # would be wrong.
        try:
            import pyshacl  # noqa: F401
        except ImportError as exc:
            self.skipTest(f"pySHACL unavailable ({exc.name})")
        root = ontology_root()
        if root is None:
            self.skipTest("no ontology checkout")

        self.erase_capability_types()
        self.drop_first(lambda l: "/capability/" in l and f"<{LABEL}>" in l)
        report = run_audit(self.graph, ontology_root_=root)
        self.assertTrue(report["shacl"]["none"]["conforms"], "none stays vacuous")
        self.assertFalse(
            report["shacl"]["rdfs"]["conforms"],
            "the restored focus nodes must be reachable by the shape",
        )
        self.assertEqual(6, report["shacl"]["rdfs"]["capability_focus_effective"])


class LabelAndNameCompleteness(MutationCase):
    def test_a_typed_capability_without_a_label_fails_the_type_gate(self):
        self.drop_first(lambda l: "/capability/" in l and f"<{LABEL}>" in l)
        report = run_audit(self.graph)
        self.assertGateFails(report, "types")
        self.assertIn("1 without a label", report["gates"]["types"]["detail"])

    def test_a_typed_capability_without_a_name_fails_both_gates(self):
        token = "glibc"
        self.drop_matching(
            lambda l: l.startswith(f"<{capability_uri(token)}>")
            and f"<{PKG}capabilityName>" in l,
            1,
        )
        report = run_audit(self.graph)
        # types: the typed node lost a required field. coverage: the expected
        # token no longer has a typed node the audit can identify. pairs: its
        # edge can no longer be named.
        self.assertGatesFail(report, ["pairs", "coverage", "types"])
        self.assertIn("1 without a capabilityName", report["gates"]["types"]["detail"])


class Encoding(MutationCase):
    def test_an_undecoded_entity_in_a_name_fails_the_encoding_gate(self):
        lines = [
            line.replace(
                '"(shell-if-bash >= 5.0 with shell-if-bash < 6)"',
                '"(shell-if-bash &gt;= 5.0 with shell-if-bash &lt; 6)"',
            )
            for line in self.lines()
        ]
        self.rewrite(lines)
        report = run_audit(self.graph)
        self.assertGateFails(report, "encoding")
        self.assertEqual(2, report["graph"]["undecoded_name_count"])

    def test_an_entity_in_a_prose_literal_is_reported_but_does_not_gate(self):
        lines = self.lines()
        lines.append(f'<{BASH}> <{PKG}description> "prints a &gt; prompt" .')
        self.rewrite(lines)
        report = run_audit(self.graph)
        self.assertEqual([], report["failed_gates"])
        self.assertEqual(1, report["graph"]["entity_in_prose"])
        self.assertEqual(0, report["graph"]["undecoded_name_count"])


class Edges(MutationCase):
    def test_a_lost_provider_edge_fails_edges_and_pairs(self):
        token = "bundled(gnulib)"
        self.drop_matching(
            lambda l: l
            == f"<{BASH}> <{PKG}providesCapability> <{capability_uri(token)}> .",
            1,
        )
        report = run_audit(self.graph)
        self.assertGatesFail(report, ["pairs", "edges"])
        self.assertEqual(6, report["graph"]["provides_edge_lines"])

    def test_a_duplicated_edge_line_fails_edges_but_not_pairs(self):
        # The distinction that needs two gates. A repeated line changes the
        # serialized occurrence count while leaving the relation set alone.
        lines = self.lines()
        edge = f"<{BASH}> <{PKG}providesCapability> <{capability_uri('bash')}> ."
        self.assertIn(edge, lines)
        self.rewrite(lines + [edge])
        report = run_audit(self.graph)
        self.assertOnlyGateFails(report, "edges")
        self.assertEqual(8, report["graph"]["provides_edge_lines"])
        self.assertEqual(7, report["pairs"]["graph_distinct"])


class Policy(MutationCase):
    def test_emitting_a_suppressed_token_fails_the_policy_gate(self):
        token = "rtld(GNU_HASH)"
        uri = capability_uri(token)
        self.rewrite(
            self.lines()
            + [
                f"<{uri}> <{TYPE}> <{PKG}Capability> .",
                f'<{uri}> <{PKG}capabilityName> "{token}" .',
                f'<{uri}> <{LABEL}> "{token}" .',
            ]
        )
        report = run_audit(self.graph)
        self.assertGateFails(report, "policy")
        self.assertEqual(1, report["graph"]["suppressed_token_count"])
        # Not blamed on pairs: a suppressed token is in the source, so calling
        # it "graph only" would point at the wrong bug.
        self.assertTrue(report["gates"]["pairs"]["pass"])


class MirroredKeys(unittest.TestCase):
    def test_the_mirrored_derivations_match_the_collector(self):
        report = run_audit(GRAPH)
        self.assertTrue(report["gates"]["keys"]["pass"], report["gates"]["keys"])

    def test_the_version_key_matches_what_rpm_rs_builds(self):
        rust = (REPO_ROOT / "etl" / "pg-collect" / "src" / "rpm.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn('format!("{}-{}.{}", ver, rel, arch)', rust)
        self.assertEqual(
            "2.34-60.el9.x86_64",
            audit_module.version_string("2.34", "60.el9", "x86_64"),
        )

    def test_the_prefix_list_matches_the_collector(self):
        rust = (REPO_ROOT / "etl" / "pg-collect" / "src" / "rpm.rs").read_text(
            encoding="utf-8"
        )
        match = re.search(
            r"RPM_INTERNAL_TOKEN_PREFIXES: &\[&str\] = &\[([^\]]*)\];", rust
        )
        self.assertIsNotNone(match)
        self.assertEqual(
            tuple(sorted(re.findall(r'"([^"]*)"', match.group(1)))),
            tuple(sorted(audit_module.RPM_INTERNAL_TOKEN_PREFIXES)),
        )

    def test_a_missing_collector_source_reports_not_run(self):
        # Never treated as agreement.
        gate = audit_module.check_mirrored_keys("/nonexistent")
        self.assertIsNone(gate["pass"])
        self.assertIn("not run", gate["detail"])


class Parse(MutationCase):
    def test_a_line_the_audit_cannot_read_fails_the_parse_gate(self):
        self.rewrite(self.lines() + ["this is not n-triples"])
        report = run_audit(self.graph)
        self.assertGateFails(report, "parse")
        self.assertEqual(1, report["graph"]["unparsed"])

    def test_blank_node_subjects_are_read_rather_than_skipped(self):
        # 88,932 such triples in BaseOS were silently skipped, and the audit
        # still reported zero undecoded entities.
        self.rewrite(
            self.lines()
            + [
                f"_:dep_abc <{TYPE}> <{PKG}Dependency> .",
                f'_:constraint_abc <{PKG}versionConstraintValue> "5.0 &gt; 4" .',
            ]
        )
        report = run_audit(self.graph)
        self.assertEqual(0, report["graph"]["unparsed"])
        self.assertEqual(1, report["graph"]["entity_in_prose"])


class Prohibited(MutationCase):
    def test_a_retired_predicate_fails_the_prohibited_gate(self):
        cap = capability_uri("bash")
        self.rewrite(
            self.lines()
            + [
                f"<{BASH}> <{RPM}rpmProvides> <{cap}> .",
                f"<{BASH}> <{PKG}directlyProvides> <{cap}> .",
            ]
        )
        report = run_audit(self.graph)
        self.assertOnlyGateFails(report, "prohibited")
        self.assertEqual(
            {f"{RPM}rpmProvides": 1, f"{PKG}directlyProvides": 1},
            report["gates"]["prohibited"]["detail"],
        )

    def test_the_legacy_kind_predicates_are_not_prohibited_yet(self):
        # rpmRequires/rpmConflicts/rpmObsoletes are still part of today's
        # contract. Retiring them is blocked on ontology#19, and the fixture
        # carries them so this stays honest about what is decided.
        report = run_audit(GRAPH)
        self.assertEqual([], report["failed_gates"])
        self.assertIn(f"{RPM}rpmObsoletes", GRAPH.read_text(encoding="utf-8"))


class Budget(MutationCase):
    def write_manifest(self, body):
        path = Path(self.tmp.name) / "manifest.json"
        path.write_text(json.dumps(body), encoding="utf-8")
        return str(path)

    def test_a_corpus_over_its_ceiling_fails_the_budget_gate(self):
        report = run_audit(
            self.graph,
            manifest=self.write_manifest(
                {"total_triples": 10_608_101, "max_triples": 3_000_000}
            ),
        )
        self.assertGateFails(report, "budget")

    def test_a_manifest_missing_the_keys_fails_rather_than_passing(self):
        report = run_audit(self.graph, manifest=self.write_manifest({"files": []}))
        self.assertGateFails(report, "budget")
        self.assertIn("total_triples", report["gates"]["budget"]["detail"])
        self.assertIn("max_triples", report["gates"]["budget"]["detail"])

    def test_a_corpus_within_its_ceiling_passes(self):
        report = run_audit(
            self.graph,
            manifest=self.write_manifest(
                {"total_triples": 2_000_000, "max_triples": 3_000_000}
            ),
        )
        self.assertEqual([], report["failed_gates"])


class ShaclReporting(MutationCase):
    def test_both_regimes_run_and_report_their_focus_sets(self):
        try:
            import pyshacl  # noqa: F401
        except ImportError as exc:
            self.skipTest(f"pySHACL unavailable ({exc.name})")
        root = ontology_root()
        if root is None:
            self.skipTest("no ontology checkout")

        report = run_audit(GRAPH, ontology_root_=root)
        self.assertEqual({"none", "rdfs"}, set(report["shacl"]))
        for regime in ("none", "rdfs"):
            run = report["shacl"][regime]
            self.assertEqual(6, run["capability_focus_asserted"], regime)
            self.assertEqual(6, run["capability_focus_effective"], regime)
            self.assertEqual(list(audit_module.SHAPE_FILES), run["shape_files"])
        self.assertEqual([], report["shacl"]["none"]["axiom_files"])
        self.assertEqual(
            list(audit_module.AXIOM_FILES), report["shacl"]["rdfs"]["axiom_files"]
        )

    def test_a_shacl_run_that_did_not_happen_is_not_a_pass(self):
        report = run_audit(
            self.graph, ontology_root_=str(Path(self.tmp.name) / "no-such-ontology")
        )
        for name in ("shacl", "shacl_rdfs"):
            gate = report["gates"][name]
            self.assertIsNone(gate["pass"], name)
            self.assertIn("not run", gate["detail"])
            self.assertNotIn(name, report["failed_gates"])
            self.assertIn(name, report["coverage"]["gates_not_run"])


class ReportScope(unittest.TestCase):
    def test_the_report_names_what_it_did_and_did_not_hold_to_source(self):
        report = run_audit(GRAPH)
        self.assertEqual(["provides"], report["coverage"]["kinds_compared"])
        self.assertEqual(
            ["requires", "conflicts", "obsoletes"],
            report["coverage"]["kinds_not_compared"],
        )
        self.assertEqual(
            ["Capability", "BinaryPackage", "PackageIdentity"],
            report["coverage"]["classes_held_to_source"],
        )
        self.assertIn("SourcePackage", report["coverage"]["classes_counted_only"])
        self.assertEqual("not_requested", report["coverage"]["shacl"])
        self.assertEqual("no manifest given", report["coverage"]["budget"])


class CommandLine(MutationCase):
    def test_a_failing_gate_exits_nonzero_and_writes_the_report(self):
        token = "bash(x86-64)"
        edge = f"<{BASH}> <{PKG}providesCapability> <{capability_uri(token)}> ."
        self.rewrite(
            [edge.replace(BASH, GLIBC) if l == edge else l for l in self.lines()]
        )
        out = Path(self.tmp.name) / "report.json"
        proc = subprocess.run(
            [
                sys.executable,
                str(AUDIT),
                "--primary",
                str(PRIMARY),
                "--rdf",
                str(self.graph),
                "--repo-root",
                str(REPO_ROOT),
                "--report",
                str(out),
            ],
            capture_output=True,
            text=True,
            check=False,  # the returncode is what this test asserts on
        )
        self.assertEqual(1, proc.returncode, proc.stderr)
        self.assertIn("FAILED: pairs", proc.stderr)
        self.assertEqual(
            ["pairs"], json.loads(out.read_text(encoding="utf-8"))["failed_gates"]
        )

    def test_the_clean_pair_exits_zero(self):
        proc = subprocess.run(
            [
                sys.executable,
                str(AUDIT),
                "--primary",
                str(PRIMARY),
                "--rdf",
                str(GRAPH),
                "--repo-root",
                str(REPO_ROOT),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(0, proc.returncode, proc.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
