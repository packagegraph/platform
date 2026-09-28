#!/usr/bin/env python3
"""Drive audit-rpm-contract.py against a clean fixture pair and mutations of it.

A gate that cannot fail is decoration. Every test below takes the clean pair,
breaks exactly one thing, and asserts the matching gate goes red -- including
the case the whole exercise exists for: a corpus with every `pkg:Capability`
type deleted, which a class-targeted SHACL run reports as conforming because
the shape has no focus nodes left to check.
"""

import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

TESTS_DIR = Path(__file__).resolve().parent
SCRIPTS_DIR = TESTS_DIR.parent
AUDIT = SCRIPTS_DIR / "audit-rpm-contract.py"
PRIMARY = TESTS_DIR / "fixtures" / "audit-primary.xml"
GRAPH = TESTS_DIR / "fixtures" / "audit-graph.nt"

PKG = "https://purl.org/packagegraph/ontology/core#"
RPM = "https://purl.org/packagegraph/ontology/rpm#"
TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"

sys.path.insert(0, str(SCRIPTS_DIR))
audit_module = __import__("importlib").machinery.SourceFileLoader(
    "audit_rpm_contract", str(AUDIT)
).load_module()


def run_audit(graph_path, primary_path=PRIMARY, manifest=None, ontology_root=None):
    """Call the audit in-process and return its report."""
    return audit_module.audit(
        str(primary_path),
        str(graph_path),
        ontology_root=ontology_root,
        manifest_path=manifest,
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

    def assertGateFails(self, report, gate):
        self.assertIn(gate, report["failed_gates"], report["gates"][gate]["detail"])
        self.assertFalse(report["pass"])

    def assertOnlyGateFails(self, report, gate):
        self.assertEqual([gate], report["failed_gates"], report["gates"])

    def assertGatesFail(self, report, gates):
        self.assertEqual(sorted(gates), sorted(report["failed_gates"]), report["gates"])

    def drop_capability(self, token):
        """Remove a capability node whole -- type, name, label, and the edge.

        Dropping only the lines that mention the token as a literal leaves the
        `rdf:type` triple behind, which fails the type gate as well and points
        at the wrong bug. A capability the collector simply never emitted is
        absent entirely.
        """
        from urllib.parse import quote

        uri = "https://packagegraph.github.io/d/capability/" + quote(token, safe="")
        kept = [line for line in self.lines() if f"<{uri}>" not in line]
        self.assertLess(len(kept), len(self.lines()))
        self.rewrite(kept)


class CleanPair(unittest.TestCase):
    def test_the_clean_fixture_pair_passes_every_gate(self):
        report = run_audit(GRAPH)
        self.assertEqual([], report["failed_gates"], report["gates"])
        self.assertTrue(report["pass"])

    def test_the_source_side_counts_what_it_suppresses_separately(self):
        # config(bash) and rtld(GNU_HASH) in provides, rpmlib(...) in requires.
        # Reported, not discarded: the policy is a choice, and a choice needs a
        # number attached to be reviewable.
        report = run_audit(GRAPH)
        self.assertEqual(2, report["source"]["suppressed_by_policy"]["provides"])
        self.assertEqual(1, report["source"]["suppressed_by_policy"]["requires"])

    def test_undecided_terms_report_not_run_rather_than_zero(self):
        # Reporting `declared: 0` would be a measurement of an absence. These
        # terms do not exist yet, so there is nothing to measure.
        report = run_audit(GRAPH)
        for key in ("declarations", "declaration_pre"):
            self.assertEqual("not_run", report["deferred"][key]["status"])
        self.assertIsNone(report["deferred"]["declarations"]["declared"])
        self.assertEqual(
            {"requires": 3, "conflicts": 1, "obsoletes": 1},
            report["deferred"]["declarations"]["source_occurrences"],
        )


class ReportScope(unittest.TestCase):
    """The report has to say what it did not check, not only that it passed."""

    def test_only_provides_is_reported_as_compared(self):
        report = run_audit(GRAPH)
        self.assertEqual(["provides"], report["coverage"]["kinds_compared"])
        self.assertEqual(
            ["requires", "conflicts", "obsoletes"],
            report["coverage"]["kinds_not_compared"],
        )

    def test_the_uncompared_kinds_declare_null_not_zero(self):
        # A consumer reading 0 cannot tell "none found" from "never looked".
        report = run_audit(GRAPH)
        for kind in ("requires", "conflicts", "obsoletes"):
            entry = report["per_kind"][kind]
            self.assertIsNone(entry["declared"], kind)
            self.assertEqual("not_run", entry["status"])
            self.assertIn("ontology#19", entry["reason"])
            self.assertGreater(entry["source"], 0, kind)

    def test_the_provides_kind_carries_real_numbers(self):
        report = run_audit(GRAPH)
        entry = report["per_kind"]["provides"]
        self.assertEqual("compared", entry["status"])
        self.assertEqual(9, entry["source"])
        self.assertEqual(2, entry["rejected_by_policy"])
        self.assertEqual(7, entry["expected"])
        self.assertEqual(entry["expected"], entry["declared"])

    def test_a_shacl_run_that_did_not_happen_shows_in_the_coverage_block(self):
        report = run_audit(GRAPH)
        self.assertEqual("not_requested", report["coverage"]["shacl"])
        self.assertEqual("no manifest given", report["coverage"]["budget"])
        self.assertNotIn("shacl", report["coverage"]["gates_run"])


class TypeErasure(MutationCase):
    """The mutation the reviewer asked for by name."""

    def erase_capability_types(self):
        kept = [
            line
            for line in self.lines()
            if f"<{TYPE}> <{PKG}Capability>" not in line
        ]
        self.rewrite(kept)
        return kept

    def test_deleting_every_capability_type_fails_the_type_gate(self):
        before = run_audit(self.graph)
        self.assertEqual([], before["failed_gates"])

        self.erase_capability_types()
        after = run_audit(self.graph)
        self.assertEqual(0, after["graph"]["capability_types"])
        self.assertGateFails(after, "types")

    def test_the_erased_corpus_still_carries_its_capability_names(self):
        # This is what makes the mutation interesting. The names and the
        # provides edges survive, so the graph still looks populated and the
        # fidelity gate still passes -- only the types are gone.
        self.erase_capability_types()
        report = run_audit(self.graph)
        self.assertEqual(6, report["graph"]["capability_names"])
        self.assertEqual(7, report["graph"]["provides_edges"])
        self.assertTrue(report["gates"]["fidelity"]["pass"])
        self.assertOnlyGateFails(report, "types")

    def test_a_class_targeted_shacl_run_alone_calls_the_erased_corpus_conforming(self):
        # The claim the type gate exists to cover. Skipped rather than assumed
        # when pySHACL is absent: a check that did not run is not a check that
        # passed, and this is the one assertion that cannot be faked.
        try:
            import pyshacl  # noqa: F401
            import rdflib
        except ImportError as exc:
            self.skipTest(f"pySHACL/rdflib unavailable ({exc.name})")

        shapes, source = self.locate_shapes()
        self.erase_capability_types()
        data = rdflib.Graph()
        data.parse(str(self.graph), format="nt")
        shape_graph = rdflib.Graph()
        shape_graph.parse(str(shapes), format="turtle")
        conforms, _, _text = pyshacl.validate(
            data, shacl_graph=shape_graph, inference="none"
        )

        self.assertTrue(
            conforms,
            f"the erased corpus was expected to conform vacuously against "
            f"{source}; if it does not, the shapes now reach these nodes "
            f"another way and this test needs rewriting rather than deleting",
        )
        # The point: SHACL says yes, the audit says no, and the audit is right.
        self.assertGateFails(run_audit(self.graph), "types")

    def test_the_vacuity_shape_copy_still_matches_the_ontology(self):
        # The copy exists so the test above can run without a checkout. When a
        # checkout is here, hold the copy to it.
        real = self.ontology_shapes()
        if real is None:
            self.skipTest("no ontology checkout to compare the shape copy to")
        copied = (TESTS_DIR / "fixtures" / "capability-shape.ttl").read_text(
            encoding="utf-8"
        )
        body = copied.split("@prefix", 1)[1]
        target = re.search(
            r"pkg:CapabilityShape a sh:NodeShape ;.*?sh:targetClass pkg:Capability \.",
            real.read_text(encoding="utf-8"),
            re.S,
        )
        self.assertIsNotNone(target, "CapabilityShape is no longer in core.shacl.ttl")
        self.assertIn(
            target.group(0),
            "@prefix" + body,
            "fixtures/capability-shape.ttl has drifted from the ontology",
        )

    @staticmethod
    def ontology_shapes():
        import os

        root = os.environ.get(
            "ONTOLOGY_REPO", str(SCRIPTS_DIR.parent.parent.parent / "ontology")
        )
        path = Path(root) / "core" / "core.shacl.ttl"
        return path if path.is_file() else None

    def locate_shapes(self):
        """Prefer the real shapes; fall back to the committed copy."""
        real = self.ontology_shapes()
        if real is not None:
            return real, f"the ontology's own {real}"
        return (
            TESTS_DIR / "fixtures" / "capability-shape.ttl",
            "the committed CapabilityShape copy",
        )


class LabelErasure(MutationCase):
    def test_a_capability_without_a_label_fails_the_type_gate(self):
        # pkg:CapabilityShape requires at least one rdfs:label. Every one of
        # the corpus's capability nodes failed it before #110.
        kept, dropped = [], 0
        for line in self.lines():
            if "/capability/" in line and "rdf-schema#label" in line and dropped == 0:
                dropped += 1
                continue
            kept.append(line)
        self.assertEqual(1, dropped)
        self.rewrite(kept)

        report = run_audit(self.graph)
        self.assertEqual(1, report["graph"]["capabilities_without_label"])
        self.assertOnlyGateFails(report, "types")


class Encoding(MutationCase):
    def test_an_undecoded_entity_in_a_name_fails_the_encoding_gate(self):
        # The live defect: reading the raw attribute bytes left `&gt;` in
        # 9,919 of fedora/43's identity names.
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
        self.assertTrue(
            any("&gt;" in hit["value"] for hit in report["gates"]["encoding"]["sample"])
        )

    def test_an_entity_in_a_prose_literal_is_reported_but_does_not_gate(self):
        # A description may legitimately contain the text `&gt;`. Failing a
        # release on that would be a false positive, so it is counted only.
        lines = self.lines()
        lines.append(
            f'<https://packagegraph.github.io/d/pkg/almalinux/9/x86_64/bash> '
            f'<{PKG}description> "prints a &gt; prompt" .'
        )
        self.rewrite(lines)

        report = run_audit(self.graph)
        self.assertEqual([], report["failed_gates"])
        self.assertEqual(1, report["graph"]["entity_in_prose"])
        self.assertEqual(0, report["graph"]["undecoded_name_count"])


class Fidelity(MutationCase):
    def test_a_source_token_missing_from_the_graph_fails_fidelity(self):
        # Removing the node also removes its edge, so both gates fire. That is
        # the point of having two: they fail together on an omitted capability
        # and separately on an omitted edge (see Edges below).
        self.drop_capability("bash(x86-64)")

        report = run_audit(self.graph)
        self.assertGatesFail(report, ["fidelity", "edges"])
        self.assertEqual(1, report["fidelity"]["source_only"])
        self.assertEqual(["bash(x86-64)"], report["fidelity"]["source_only_sample"])

    def test_a_capability_the_source_never_declared_fails_fidelity(self):
        lines = self.lines()
        invented = "https://packagegraph.github.io/d/capability/invented"
        lines += [
            f"<{invented}> <{TYPE}> <{PKG}Capability> .",
            f'<{invented}> <{PKG}capabilityName> "invented" .',
            f'<{invented}> <http://www.w3.org/2000/01/rdf-schema#label> "invented" .',
        ]
        self.rewrite(lines)

        report = run_audit(self.graph)
        self.assertGateFails(report, "fidelity")
        self.assertEqual(1, report["fidelity"]["graph_only"])
        self.assertEqual(["invented"], report["fidelity"]["graph_only_sample"])


class Edges(MutationCase):
    def test_a_lost_provider_edge_fails_while_fidelity_still_passes(self):
        # The defect distinct-token agreement cannot see. bash and glibc both
        # provide bundled(gnulib); drop one edge and the token still has a
        # provider, so fidelity is satisfied while the relationship is gone.
        from urllib.parse import quote

        cap = "https://packagegraph.github.io/d/capability/" + quote(
            "bundled(gnulib)", safe=""
        )
        before = self.lines()
        kept = [
            line
            for line in before
            if not (f"<{PKG}providesCapability> <{cap}>" in line and "/bash/" in line)
        ]
        self.assertEqual(len(before) - 1, len(kept), "no bash edge was dropped")
        self.rewrite(kept)

        report = run_audit(self.graph)
        self.assertOnlyGateFails(report, "edges")
        self.assertTrue(report["gates"]["fidelity"]["pass"])
        self.assertEqual(6, report["graph"]["provides_edges"])


class Policy(MutationCase):
    def test_emitting_a_suppressed_token_fails_the_policy_gate(self):
        # The collector documents that it drops these. If it stops dropping
        # them, the documented policy and the output disagree and a human has
        # to decide which is wrong.
        lines = self.lines()
        uri = "https://packagegraph.github.io/d/capability/rtld%28GNU_HASH%29"
        lines += [
            f"<{uri}> <{TYPE}> <{PKG}Capability> .",
            f'<{uri}> <{PKG}capabilityName> "rtld(GNU_HASH)" .',
            f'<{uri}> <http://www.w3.org/2000/01/rdf-schema#label> "rtld(GNU_HASH)" .',
        ]
        self.rewrite(lines)

        report = run_audit(self.graph)
        self.assertGateFails(report, "policy")
        self.assertEqual(1, report["graph"]["suppressed_token_count"])
        # It must not also be blamed on fidelity: a suppressed token is in the
        # source, so counting it as "graph only" would point at the wrong bug.
        self.assertTrue(report["gates"]["fidelity"]["pass"])

    def test_the_mirrored_prefix_list_matches_the_collector(self):
        # Mirrored deliberately, so it can drift. This is the test that
        # notices, and it reads the Rust source as text rather than importing.
        rust = (
            SCRIPTS_DIR.parent / "pg-collect" / "src" / "rpm.rs"
        ).read_text(encoding="utf-8")
        match = re.search(
            r"RPM_INTERNAL_TOKEN_PREFIXES: &\[&str\] = &\[([^\]]*)\];", rust
        )
        self.assertIsNotNone(match, "could not find the collector's prefix list")
        declared = tuple(sorted(re.findall(r'"([^"]*)"', match.group(1))))
        self.assertEqual(
            declared,
            tuple(sorted(audit_module.RPM_INTERNAL_TOKEN_PREFIXES)),
            "the audit's mirrored suppression list has drifted from rpm.rs",
        )


class Prohibited(MutationCase):
    def test_a_retired_predicate_fails_the_prohibited_gate(self):
        # rpm:rpmProvides was undeclared in every ontology module;
        # directlyProvides has rdfs:range :Package, so pointing it at a
        # capability token entailed :Package membership for that token.
        lines = self.lines()
        pkg = "https://packagegraph.github.io/d/pkg/almalinux/9/x86_64/bash/0%3A5.1.8-9.el9"
        cap = "https://packagegraph.github.io/d/capability/bash"
        lines += [
            f"<{pkg}> <{RPM}rpmProvides> <{cap}> .",
            f"<{pkg}> <{PKG}directlyProvides> <{cap}> .",
        ]
        self.rewrite(lines)

        report = run_audit(self.graph)
        self.assertOnlyGateFails(report, "prohibited")
        self.assertEqual(
            {f"{RPM}rpmProvides": 1, f"{PKG}directlyProvides": 1},
            report["gates"]["prohibited"]["detail"],
        )

    def test_the_legacy_kind_predicates_are_not_prohibited_yet(self):
        # rpmRequires/rpmConflicts/rpmObsoletes are still part of today's
        # contract. Retiring them is Task 3, blocked on ontology#19, and the
        # fixture carries them so this stays honest about what is decided.
        report = run_audit(GRAPH)
        self.assertEqual([], report["failed_gates"])
        self.assertIn(f"{RPM}rpmObsoletes", GRAPH.read_text(encoding="utf-8"))


class Budget(MutationCase):
    def write_manifest(self, total, ceiling):
        path = Path(self.tmp.name) / "manifest.json"
        path.write_text(
            json.dumps(
                {
                    "total_triples": total,
                    "max_triples": ceiling,
                    "graphs_over_budget": [],
                    "classes_over_budget": [],
                    "selection": {"fan_out_cuts": 0},
                }
            ),
            encoding="utf-8",
        )
        return str(path)

    def test_a_corpus_over_its_ceiling_fails_the_budget_gate(self):
        # The live run wrote 10,608,101 triples against a 3,000,000 ceiling
        # and exited 0.
        report = run_audit(self.graph, manifest=self.write_manifest(10_608_101, 3_000_000))
        self.assertGateFails(report, "budget")

    def test_a_manifest_missing_the_keys_fails_rather_than_passing(self):
        # extract.rs declares both as non-Option, so an absent key means this
        # is not the manifest the audit understands. "Within budget" would be
        # the wrong conclusion to draw from a manifest it cannot read.
        path = Path(self.tmp.name) / "partial.json"
        path.write_text(json.dumps({"files": []}), encoding="utf-8")
        report = run_audit(self.graph, manifest=str(path))
        self.assertGateFails(report, "budget")
        self.assertIn("total_triples", report["gates"]["budget"]["detail"])
        self.assertIn("max_triples", report["gates"]["budget"]["detail"])

    def test_the_gate_reads_the_key_names_extract_rs_writes(self):
        # A gate that reads a key nobody writes is a gate that always passes.
        rust = (
            SCRIPTS_DIR.parent / "pg-collect" / "src" / "extract.rs"
        ).read_text(encoding="utf-8")
        for field in ("total_triples", "max_triples", "graphs_over_budget",
                      "classes_over_budget"):
            self.assertIn(
                f"pub {field}:", rust, f"extract.rs no longer has {field}"
            )
        self.assertNotIn(
            "serde(rename", rust, "a serde rename would change the JSON keys"
        )

    def test_a_corpus_within_its_ceiling_passes(self):
        report = run_audit(self.graph, manifest=self.write_manifest(2_000_000, 3_000_000))
        self.assertEqual([], report["failed_gates"])


class ShaclReporting(MutationCase):
    def test_a_shacl_run_that_did_not_happen_is_not_a_pass(self):
        # `pass: None` prints as "not run" and is excluded from failed_gates.
        # It must never read as true.
        report = run_audit(
            self.graph, ontology_root=str(Path(self.tmp.name) / "no-such-ontology")
        )
        gate = report["gates"]["shacl"]
        self.assertIsNone(gate["pass"])
        self.assertIn("not run", gate["detail"])
        self.assertNotIn("shacl", report["failed_gates"])


class CommandLine(MutationCase):
    def test_a_failing_gate_exits_nonzero_and_writes_the_report(self):
        self.drop_capability("bash(x86-64)")
        out = Path(self.tmp.name) / "report.json"
        expected = ["edges", "fidelity"]

        proc = subprocess.run(
            [
                sys.executable,
                str(AUDIT),
                "--primary",
                str(PRIMARY),
                "--rdf",
                str(self.graph),
                "--report",
                str(out),
            ],
            capture_output=True,
            text=True,
        )
        self.assertEqual(1, proc.returncode, proc.stderr)
        self.assertIn("FAILED: ", proc.stderr)
        self.assertEqual(
            expected,
            sorted(json.loads(out.read_text(encoding="utf-8"))["failed_gates"]),
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
            ],
            capture_output=True,
            text=True,
        )
        self.assertEqual(0, proc.returncode, proc.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
