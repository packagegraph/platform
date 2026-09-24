"""upload-nt.sh must commit a graph through a manifest, not a stable key.

Runs the REAL etl/scripts/upload-nt.sh against the same stubbed mc the other
collector tests use, because the defect in #72 is a property of that script's
publication order and nothing else.

The old scheme wrote a stable `nt-output/<slug>.nt.gz` and then its `.graph`
sidecar, and called the sidecar a commit marker. That holds for a graph's
FIRST upload and never again: on every later upload the sidecar is already
there, so the new payload becomes discoverable the instant its PUT lands --
unverified, and with nothing left to gate it. Whether the sidecar write then
succeeds or fails changes nothing, because it rewrites bytes that were already
correct.

So the properties under test are about ordering and about what survives each
failure, not about any single call succeeding:

  * the payload goes to a key that has never existed, so it cannot replace
    anything;
  * it is read back and checked before anything points at it;
  * the manifest PUT is the commit, and it is last.

See docs/GRAPH-PUBLICATION.md.
"""
import gzip
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
UPLOAD = REPO / "etl/scripts/upload-nt.sh"

GRAPH = "https://packagegraph.github.io/graph/test/manifest"
SLUG = "test-manifest"
OTHER_GRAPH = "https://packagegraph.github.io/graph/test-manifest"

DATA = ('<https://packagegraph.github.io/d/pkg/test/zlib> '
        '<https://purl.org/packagegraph/ontology/core#packageName> "zlib" .\n')


class UploadManifestTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="upload-manifest-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        adapter = self.bin / "adapter"
        adapter.write_bytes((HERE / "checkpoint_commands.py").read_bytes())
        adapter.chmod(0o755)
        (self.bin / "mc").symlink_to(adapter)
        self.remote = self.root / "remote"
        self.env = dict(os.environ,
                        PATH=f"{self.bin}:{os.environ['PATH']}",
                        CHECKPOINT_TEST_ROOT=str(self.root),
                        MINIO_BUCKET="test", MINIO_ENDPOINT="http://127.0.0.1:1",
                        MINIO_ACCESS_KEY="test", MINIO_SECRET_KEY="test")

    # ---- helpers ---------------------------------------------------------

    def upload(self, content, graph=GRAPH, name="graph.nt", **extra_env):
        target = self.root / name
        target.write_text(content)
        return subprocess.run(
            ["bash", str(UPLOAD), str(target), graph],
            env=dict(self.env, **extra_env),
            capture_output=True, text=True, timeout=60)

    def mc_calls(self):
        log = self.root / "mc-calls"
        if not log.exists():
            return []
        return [json.loads(line) for line in log.read_text().splitlines()]

    def manifest(self, slug=SLUG):
        path = self.remote / "graphs" / slug / "manifest.json"
        if not path.exists():
            return None
        return json.loads(path.read_text())

    def generations(self, slug=SLUG):
        directory = self.remote / "graphs" / slug / "generations"
        if not directory.exists():
            return []
        return sorted(p.name for p in directory.iterdir())

    def assertCommitted(self, process):
        self.assertEqual(process.returncode, 0,
                         f"upload failed:\n{process.stdout}{process.stderr}")
        manifest = self.manifest()
        self.assertIsNotNone(manifest, "no manifest was committed")
        return manifest

    # ---- the commit itself ----------------------------------------------

    def test_a_first_upload_commits_a_manifest_describing_its_generation(self):
        process = self.upload(DATA)
        manifest = self.assertCommitted(process)

        self.assertEqual(manifest["schema"], 1)
        self.assertEqual(manifest["graph"], GRAPH)
        self.assertEqual(manifest["encoding"], "gzip")
        self.assertEqual(manifest["data_triples"], 1)
        self.assertEqual(manifest["key"],
                         f"graphs/{SLUG}/generations/{manifest['generation']}.nt.gz")

        payload = self.remote / manifest["key"]
        self.assertTrue(payload.exists(), "the manifest points at nothing")
        raw = payload.read_bytes()
        self.assertEqual(manifest["size_bytes"], len(raw))
        self.assertEqual(manifest["sha256"], hashlib.sha256(raw).hexdigest())
        # The digest has to describe the bytes a reader will actually gunzip.
        self.assertIn(DATA.strip(), gzip.decompress(raw).decode())

    def test_the_generation_name_carries_its_own_digest(self):
        """So a generation key is self-identifying and collision-resistant.

        Two uploads a second apart would otherwise share a name.
        """
        manifest = self.assertCommitted(self.upload(DATA))
        stamp, _, short = manifest["generation"].partition("-")
        self.assertRegex(stamp, r"^\d{8}T\d{6}Z$")
        self.assertEqual(short, manifest["sha256"][:12])

    def test_the_payload_is_uploaded_before_the_manifest_commits(self):
        """Ordering IS the protocol: everything before the manifest PUT is
        invisible to readers, everything after it is cleanup."""
        self.assertCommitted(self.upload(DATA))
        destinations = [call[-1] for call in self.mc_calls()
                        if call[0] in ("cp", "pipe")]
        generation = next(i for i, d in enumerate(destinations)
                          if "/generations/" in d)
        commit = next(i for i, d in enumerate(destinations)
                      if d.endswith("manifest.json"))
        self.assertLess(generation, commit,
                        f"the manifest was committed first: {destinations}")

    def test_the_stored_payload_is_read_back_before_the_commit(self):
        self.assertCommitted(self.upload(DATA))
        verbs = [call[0] for call in self.mc_calls()]
        self.assertIn("ls", verbs,
                      "nothing read the uploaded object back before committing it")

    # ---- replacement, which is where the old scheme broke ----------------

    def test_republishing_writes_a_new_generation_and_keeps_the_old_object(self):
        first = self.assertCommitted(self.upload(DATA))
        second = self.assertCommitted(self.upload(DATA * 3, name="graph2.nt"))

        self.assertNotEqual(first["generation"], second["generation"])
        self.assertEqual(len(self.generations()), 2,
                         "a generation key was reused, so a payload was replaced")
        self.assertIn(f"{first['generation']}.nt.gz", self.generations())
        self.assertEqual(self.manifest()["generation"], second["generation"])

    def test_a_failed_payload_upload_leaves_the_old_manifest_authoritative(self):
        first = self.assertCommitted(self.upload(DATA))
        payload = self.remote / first["key"]
        before = payload.read_bytes()

        process = self.upload(DATA * 3, name="graph2.nt",
                              FAIL_MC_DEST="/generations/")
        self.assertNotEqual(process.returncode, 0,
                            "a failed payload upload reported success")
        self.assertEqual(self.manifest()["generation"], first["generation"],
                         "the manifest moved to a generation that was never stored")
        self.assertEqual(payload.read_bytes(), before,
                         "the previously committed payload was disturbed")

    def test_a_failed_manifest_commit_leaves_an_ignored_orphan_generation(self):
        first = self.assertCommitted(self.upload(DATA))

        process = self.upload(DATA * 3, name="graph2.nt",
                              FAIL_MC_DEST="manifest.json")
        self.assertNotEqual(process.returncode, 0,
                            "a failed commit reported success")
        self.assertEqual(self.manifest()["generation"], first["generation"],
                         "the manifest changed despite its write failing")
        # The orphan is left in place deliberately -- #72 says do not delete
        # generations here -- but nothing references it, so nothing reads it.
        self.assertEqual(len(self.generations()), 2)
        self.assertIn("orphan", process.stderr.lower(),
                      "the orphan was not reported")

    def test_a_short_stored_payload_aborts_before_the_commit(self):
        """The read-back check is the only thing standing between a partial
        object and a manifest that vouches for its digest."""
        first = self.assertCommitted(self.upload(DATA))

        process = self.upload(DATA * 3, name="graph2.nt",
                              TRUNCATE_MC_DEST="/generations/")
        self.assertNotEqual(process.returncode, 0,
                            "a short object was committed as if it were whole")
        self.assertEqual(self.manifest()["generation"], first["generation"])
        self.assertRegex(process.stderr, r"(?i)bytes, expected|not committing")

    # ---- slug collisions --------------------------------------------------

    def test_a_slug_collision_is_refused_rather_than_overwritten(self):
        """`graph/test/manifest` and `graph/test-manifest` both slug to
        `test-manifest`. Publishing one over the other would make two graphs
        take turns erasing each other, invisibly."""
        first = self.assertCommitted(self.upload(DATA))

        process = self.upload(DATA * 2, graph=OTHER_GRAPH, name="graph2.nt")
        self.assertNotEqual(process.returncode, 0,
                            "one graph URI published over another's manifest")
        self.assertEqual(self.manifest()["graph"], GRAPH)
        self.assertEqual(self.manifest()["generation"], first["generation"])
        self.assertIn("slug", process.stderr.lower())

    def test_republishing_the_same_graph_is_not_a_collision(self):
        self.assertCommitted(self.upload(DATA))
        self.assertCommitted(self.upload(DATA * 2, name="graph2.nt"))

    # ---- the transitional legacy mirror ----------------------------------

    def test_the_legacy_pair_is_refreshed_by_default(self):
        """Host readers are installed files and writers ship in an image, so
        they do not move together. Until every reader is synced, an upgraded
        writer that stopped refreshing nt-output/ would freeze those graphs at
        their last legacy upload -- silently, because stale-but-present passes
        every gate."""
        self.assertCommitted(self.upload(DATA))
        legacy = self.remote / "nt-output"
        self.assertTrue((legacy / f"{SLUG}.nt.gz").exists())
        self.assertEqual((legacy / f"{SLUG}.nt.gz.graph").read_text(), GRAPH)

    def test_the_legacy_mirror_can_be_turned_off(self):
        self.assertCommitted(self.upload(DATA, PG_UPLOAD_LEGACY_MIRROR="0"))
        self.assertFalse((self.remote / "nt-output").exists(),
                         "the legacy mirror ran despite being disabled")

    def test_a_failed_legacy_mirror_does_not_fail_a_committed_upload(self):
        """It runs after the commit, so its failure cannot un-publish
        anything. Reporting the graph as failed would be a lie that triggers
        a pointless recollection."""
        process = self.upload(DATA, FAIL_MC_DEST="nt-output/")
        manifest = self.assertCommitted(process)
        self.assertTrue((self.remote / manifest["key"]).exists())
        self.assertIn("warning", process.stderr.lower())

    # ---- optional fields ---------------------------------------------------

    def test_a_source_url_is_recorded_and_omitted_when_absent(self):
        target = self.root / "graph.nt"
        target.write_text(DATA)
        process = subprocess.run(
            ["bash", str(UPLOAD), str(target), GRAPH, "http://example.invalid/repo"],
            env=self.env, capture_output=True, text=True, timeout=60)
        manifest = self.assertCommitted(process)
        self.assertEqual(manifest["source_url"], "http://example.invalid/repo")

        self.assertNotIn("source_url", self.assertCommitted(
            self.upload(DATA * 2, name="graph2.nt")),
            "an absent source URL was recorded as an empty string")


if __name__ == "__main__":
    unittest.main(verbosity=2)
