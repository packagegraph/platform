"""upload-nt.sh must refuse to publish a graph that carries no data.

Runs the REAL etl/scripts/upload-nt.sh against a stubbed mc, because the
defect in #58 is a property of that script's control flow, not of any
collector: every one of the 44 collectors and 11 enrichers reaches Minio
through it, and none of them checks its own output first.

Why refusing matters more than reporting: an upload is a whole-graph
replacement. Publishing an empty file does not merely record nothing, it
deletes the graph's contents on the next rebuild. cpan, hex and nuget have
been doing exactly that, green, for as long as the journal retains.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
UPLOAD = REPO / "etl/scripts/upload-nt.sh"
GRAPH = "https://packagegraph.github.io/graph/test/floor"

# A real data triple, and the shape of a DataSnapshot line upload-nt.sh
# itself appends. The second must NOT be mistaken for data -- see
# test_prior_snapshot_block_alone_is_not_data.
DATA = ('<https://packagegraph.github.io/d/pkg/test/zlib> '
        '<https://purl.org/packagegraph/ontology/core#packageName> "zlib" .\n')
SNAPSHOT = ('<https://packagegraph.github.io/d/snapshot/collector/test-floor/20260916T000000Z> '
            '<http://www.w3.org/1999/02/22-rdf-syntax-ns#type> '
            '<https://purl.org/packagegraph/ontology/core#DataSnapshot> .\n')

# Verbatim from the live hex.nt.gz on 2026-09-16. This is the whole graph: a
# Distribution node and one DistributionRelease, emitted unconditionally by the
# collector, and NOT ONE PACKAGE. It is why #58's zeroes were invisible -- the
# file is not empty, so every size or line-count check calls it healthy.
HEX_SCAFFOLDING = (
    '<https://packagegraph.github.io/d/distro/hex> '
    '<http://www.w3.org/1999/02/22-rdf-syntax-ns#type> '
    '<https://purl.org/packagegraph/ontology/core#Distribution> .\n'
    '<https://packagegraph.github.io/d/distro/hex> '
    '<https://purl.org/packagegraph/ontology/core#projectName> "Hex.pm" .\n'
    '<https://packagegraph.github.io/d/release/hex/pm> '
    '<http://www.w3.org/1999/02/22-rdf-syntax-ns#type> '
    '<https://purl.org/packagegraph/ontology/core#DistributionRelease> .\n'
    '<https://packagegraph.github.io/d/release/hex/pm> '
    '<https://purl.org/packagegraph/ontology/core#releaseCodename> "pm" .\n'
    '<https://packagegraph.github.io/d/release/hex/pm> '
    '<https://purl.org/packagegraph/ontology/core#partOfDistribution> '
    '<https://packagegraph.github.io/d/distro/hex> .\n'
    '<https://packagegraph.github.io/d/distro/hex> '
    '<https://purl.org/packagegraph/ontology/core#hasRelease> '
    '<https://packagegraph.github.io/d/release/hex/pm> .\n')

# Verbatim shape from the live enrichment-forge-version.nt.gz: only 14 triples,
# but every one is real content. A legitimately small graph must still publish.
FORGE_ENRICHMENT = (
    '<https://packagegraph.github.io/d/forge-version/forgejo/1.27.0> '
    '<http://www.w3.org/1999/02/22-rdf-syntax-ns#type> '
    '<https://purl.org/packagegraph/ontology/vcs#ForgeSoftwareVersion> .\n'
    '<https://packagegraph.github.io/d/forge-obs/gitea.com/2026-09-14> '
    '<https://purl.org/packagegraph/ontology/vcs#observedAt> '
    '"2026-09-14"^^<http://www.w3.org/2001/XMLSchema#date> .\n'
    '<https://packagegraph.github.io/d/forge/gitea.com> '
    '<https://purl.org/packagegraph/ontology/vcs#hasVersionObservation> '
    '<https://packagegraph.github.io/d/forge-obs/gitea.com/2026-09-14> .\n')


class UploadFloorTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="upload-floor-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        # Reuse the repo's existing mc adapter rather than inventing a second
        # one; it records every call to mc-calls and sandboxes writes.
        adapter = self.bin / "adapter"
        adapter.write_bytes((HERE / "checkpoint_commands.py").read_bytes())
        adapter.chmod(0o755)
        (self.bin / "mc").symlink_to(adapter)
        self.env = dict(os.environ,
                        PATH=f"{self.bin}:{os.environ['PATH']}",
                        CHECKPOINT_TEST_ROOT=str(self.root),
                        MINIO_BUCKET="test", MINIO_ENDPOINT="http://127.0.0.1:1",
                        MINIO_ACCESS_KEY="test", MINIO_SECRET_KEY="test")

    def upload(self, content, **extra_env):
        target = self.root / "graph.nt"
        target.write_text(content)
        process = subprocess.run(
            ["bash", str(UPLOAD), str(target), GRAPH],
            env=dict(self.env, **extra_env), capture_output=True, text=True, timeout=60)
        return process, target

    def mc_calls(self):
        log = self.root / "mc-calls"
        if not log.exists():
            return []
        return [json.loads(line) for line in log.read_text().splitlines()]

    def assertNothingPublished(self, process):
        """No object may reach the bucket, and the failure must be loud."""
        self.assertNotEqual(process.returncode, 0,
                            f"expected refusal, got success:\n{process.stdout}{process.stderr}")
        verbs = [call[0] for call in self.mc_calls()]
        self.assertNotIn("cp", verbs, "an empty graph was uploaded")
        self.assertNotIn("pipe", verbs, "a sidecar (commit marker) was written")
        self.assertFalse((self.root / "remote").exists(), "something reached the bucket")
        combined = process.stdout + process.stderr
        self.assertRegex(combined, r"(?i)no data|empty|data triple",
                         "refusal gave no diagnosable reason")

    # --- the reported defect -------------------------------------------------

    def test_zero_byte_file_refuses_and_publishes_nothing(self):
        process, _ = self.upload("")
        self.assertNothingPublished(process)

    def test_whitespace_and_comments_are_not_data(self):
        process, _ = self.upload("\n   \n# a comment\n\t\n")
        self.assertNothingPublished(process)

    def test_prior_snapshot_block_alone_is_not_data(self):
        """The case a naive line-count check passes.

        upload-nt.sh appends its own DataSnapshot block before uploading and
        strips a prior one on retry. So a retried run that collected nothing
        still presents a file with lines in it. Counting lines would call that
        a healthy graph; only counting DATA lines catches it.
        """
        process, _ = self.upload(SNAPSHOT)
        self.assertNothingPublished(process)

    def test_distro_release_scaffolding_alone_is_not_data(self):
        """The actual hex/nuget defect, using their real published content.

        Both publish exactly six triples describing the Distribution and its
        release, and zero packages. A floor of "at least one triple" passes
        this and leaves #58 unfixed, which is why the taxonomy nodes -- distro,
        release, arch -- do not count as collected data.
        """
        process, _ = self.upload(HEX_SCAFFOLDING)
        self.assertNothingPublished(process)

    def test_scaffolding_plus_one_real_package_uploads(self):
        """One genuine package is enough; the scaffolding just cannot stand alone."""
        process, _ = self.upload(HEX_SCAFFOLDING + DATA)
        self.assertEqual(process.returncode, 0,
                         f"a graph with a real package was refused:\n{process.stdout}{process.stderr}")
        self.assertIn("cp", [call[0] for call in self.mc_calls()])

    def test_small_enricher_graph_still_uploads(self):
        """Enrichers emit no taxonomy nodes, so the exclusion cannot starve them.

        enrichment-forge-version publishes ~14 triples legitimately. If the
        floor mistook small for empty it would erase that graph every run.
        """
        process, _ = self.upload(FORGE_ENRICHMENT)
        self.assertEqual(process.returncode, 0,
                         f"a small but real enricher graph was refused:\n{process.stdout}{process.stderr}")
        self.assertIn("cp", [call[0] for call in self.mc_calls()])

    # --- the floor must not block healthy graphs -----------------------------

    def test_single_data_triple_uploads(self):
        process, _ = self.upload(DATA)
        self.assertEqual(process.returncode, 0,
                         f"a real graph was refused:\n{process.stdout}{process.stderr}")
        verbs = [call[0] for call in self.mc_calls()]
        self.assertIn("cp", verbs, "the .nt.gz was not uploaded")
        self.assertIn("pipe", verbs, "the .graph sidecar was not written")
        remote = self.root / "remote/nt-output"
        self.assertTrue((remote / "test-floor.nt.gz").exists())
        self.assertTrue((remote / "test-floor.nt.gz.graph").exists())

    def test_data_plus_prior_snapshot_still_uploads(self):
        """A retry of a run that DID collect must still publish."""
        process, _ = self.upload(SNAPSHOT + DATA)
        self.assertEqual(process.returncode, 0,
                         f"a real graph was refused on retry:\n{process.stdout}{process.stderr}")
        self.assertIn("cp", [call[0] for call in self.mc_calls()])

    # --- configurability, so the exemption is explicit rather than silent ----

    def test_explicit_opt_out_allows_an_empty_publish(self):
        """#58 requires an explicit opt-out, not a silent exemption.

        A caller that genuinely intends to publish an empty graph must say so,
        so the intent is greppable in review.
        """
        process, _ = self.upload("", PG_UPLOAD_ALLOW_EMPTY="1")
        self.assertEqual(process.returncode, 0,
                         f"opt-out did not permit the upload:\n{process.stdout}{process.stderr}")
        self.assertIn("cp", [call[0] for call in self.mc_calls()])

    def test_minimum_is_configurable_upward(self):
        process, _ = self.upload(DATA, PG_UPLOAD_MIN_TRIPLES="2")
        self.assertNothingPublished(process)

    def test_minimum_satisfied_uploads(self):
        process, _ = self.upload(DATA * 2, PG_UPLOAD_MIN_TRIPLES="2")
        self.assertEqual(process.returncode, 0,
                         f"floor rejected a file that meets it:\n{process.stdout}{process.stderr}")
        self.assertIn("cp", [call[0] for call in self.mc_calls()])

    def test_non_numeric_minimum_fails_closed(self):
        """A typo in configuration must not silently disable the floor."""
        process, _ = self.upload(DATA, PG_UPLOAD_MIN_TRIPLES="lots")
        self.assertNotEqual(process.returncode, 0,
                            "a malformed floor was ignored instead of failing closed")
        self.assertNotIn("cp", [call[0] for call in self.mc_calls()])

    # --- the local file must not be mutated when we refuse ------------------

    def test_refusal_does_not_append_a_snapshot_to_the_local_file(self):
        """Refuse before mutating, so a caller can inspect what it produced.

        upload-nt.sh appends to its input in place. If the floor rejected the
        file after appending, the operator investigating an empty collection
        would find a file that looks like it had content.
        """
        process, target = self.upload("")
        self.assertNothingPublished(process)
        self.assertEqual(target.read_text(), "",
                         "the rejected file was mutated before the check")


if __name__ == "__main__":
    unittest.main()
