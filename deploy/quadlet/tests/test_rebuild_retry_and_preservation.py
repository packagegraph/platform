#!/usr/bin/env python3
"""Drive the real qlever-rebuild-index.sh against a local fake object store.

Two invariants are under test, both from issue #75:

1. **A failed run must stay retryable.** The script's early-exit skip keys
   off a corpus fingerprint in the object store. If that marker advances on
   a run that did not produce an index, the next run sees an unchanged
   corpus, skips, and the deployment is frozen on a stale index until some
   collector happens to upload something new -- which for a quiet graph set
   can be never. So no injected failure may advance the marker, and a retry
   of the identical corpus must reach the indexer again.

2. **Building an index must not consume its own inputs.** The dedup step
   runs before conversion, the build, the gates and promotion, against an
   unversioned bucket. Anything it destroys there is destroyed for the
   retry too.

The script is exercised as-is apart from one mechanical rewrite: its
hard-coded `/tmp/` scratch prefix is repointed at a per-test sandbox.
`test_sandbox_rewrite_is_prefix_only` proves that rewrite changes nothing
else, so a regression cannot hide in the transformation.
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(os.path.dirname(HERE), "scripts", "qlever-rebuild-index.sh")
FAKES = os.path.join(HERE, "fakes")
BUCKET = "test-bucket"
MARKER = "qlever-index/last-corpus-listing-hash.txt"

# Comfortably over the script's own "minimum is 10" graph floor.
GRAPH_COUNT = 12


def sandboxed_script(dest_dir, scratch):
    """Copy the real script, repointing only its /tmp/ scratch prefix."""
    with open(SCRIPT) as fh:
        original = fh.read()
    rewritten = original.replace("/tmp/", scratch.rstrip("/") + "/")
    path = os.path.join(dest_dir, "qlever-rebuild-index.sh")
    with open(path, "w") as fh:
        fh.write(rewritten)
    os.chmod(path, 0o755)
    return path, original, rewritten


class RebuildHarness(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="qlever-rebuild-test-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.store = os.path.join(self.tmp, "store")
        self.scratch = os.path.join(self.tmp, "scratch")
        self.bin = os.path.join(self.tmp, "bin")
        os.makedirs(self.scratch)
        os.makedirs(self.bin)
        for name in ("mc", "qlever-index"):
            shutil.copy2(os.path.join(FAKES, name), os.path.join(self.bin, name))
            os.chmod(os.path.join(self.bin, name), 0o755)
        self.mc_log = os.path.join(self.tmp, "mc.log")
        self.qlever_log = os.path.join(self.tmp, "qlever.log")
        self.script, _, _ = sandboxed_script(self.tmp, self.scratch)
        self.seed_corpus()

    # ---- fixture construction -------------------------------------------

    def put(self, key, data):
        path = os.path.join(self.store, BUCKET, key)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        mode = "wb" if isinstance(data, bytes) else "w"
        with open(path, mode) as fh:
            fh.write(data)
        return path

    def get(self, key):
        path = os.path.join(self.store, BUCKET, key)
        if not os.path.isfile(path):
            return None
        with open(path) as fh:
            return fh.read()

    def seed_corpus(self):
        """A dozen complete .nt/.graph pairs, plus one losing duplicate."""
        for i in range(GRAPH_COUNT):
            uri = f"https://packagegraph.github.io/graph/g{i}"
            body = "".join(
                f"<https://pkg/s{i}/{n}> <https://pkg/p> <https://pkg/o{n}> .\n"
                for n in range(50)
            )
            # The sidecar name embeds the data file's full name, extension
            # included -- that is how the script pairs them.
            self.put(f"nt-output/g{i}.nt", body)
            self.put(f"nt-output/g{i}.nt.graph", uri + "\n")

        # An older upload of g0's graph URI under the pre-migration name.
        # Newer mtime must go to the winner, so age this one deliberately.
        self.loser_key = "nt-output/g0-legacy.nt"
        self.loser_body = "<https://pkg/legacy> <https://pkg/p> <https://pkg/o> .\n"
        loser = self.put(self.loser_key, self.loser_body)
        self.put("nt-output/g0-legacy.nt.graph",
                 "https://packagegraph.github.io/graph/g0\n")
        old = time.time() - 86400
        os.utime(loser, (old, old))

        # A previous successful run: makes the relative gates apply (the
        # absolute floor is 1,000,000 triples, far above a test fixture).
        self.put("qlever-index/last-success.json", json.dumps({
            "status": "success",
            "timestamp": "2026-09-01T00:00:00+00:00",
            "content_hash": "0000000000000000",
            "triple_count": 500,
            "index_size": "1M",
            "graphs": [f"https://packagegraph.github.io/graph/g{i}"
                       for i in range(GRAPH_COUNT)],
        }))
        self.put("qlever-index/latest", "0000000000000000")

    def drop_duplicate(self):
        """Remove the losing duplicate pair from the fixture.

        Needed by any test that reasons about the early-exit skip. The
        pre-fix script reclaimed the loser by overwriting it in place, which
        changed that object's ETag and so changed the corpus listing itself
        -- masking the poisoned marker on the very next run and making a
        retry test pass for a reason that has nothing to do with the marker.
        The two defects in #75 were entangled that way; a test that isolates
        one has to remove the other's trigger.
        """
        for key in (self.loser_key, self.loser_key + ".graph"):
            os.remove(os.path.join(self.store, BUCKET, key))

    def corpus_digest(self):
        """Hash every source object under nt-output/, path and bytes."""
        root = os.path.join(self.store, BUCKET, "nt-output")
        h = hashlib.sha256()
        for dirpath, _, names in os.walk(root):
            for name in sorted(names):
                full = os.path.join(dirpath, name)
                h.update(os.path.relpath(full, root).encode())
                with open(full, "rb") as fh:
                    h.update(fh.read())
        return h.hexdigest()

    # ---- execution -------------------------------------------------------

    def run_rebuild(self, **inject):
        env = dict(os.environ)
        env.update({
            "PATH": self.bin + os.pathsep + env["PATH"],
            "FAKE_S3_ROOT": self.store,
            "FAKE_MC_LOG": self.mc_log,
            "FAKE_QLEVER_LOG": self.qlever_log,
            "MINIO_ENDPOINT": "http://fake",
            "MINIO_ACCESS_KEY": "fake",
            "MINIO_SECRET_KEY": "fake",
            "MINIO_BUCKET": BUCKET,
            "TMPDIR": self.scratch,
        })
        env.pop("FAKE_MC_FAIL", None)
        env.pop("FAKE_QLEVER_FAIL", None)
        env.update({k: v for k, v in inject.items()})
        return subprocess.run(
            ["bash", self.script],
            env=env, capture_output=True, text=True, timeout=300,
        )

    def qlever_invocations(self):
        if not os.path.isfile(self.qlever_log):
            return 0
        with open(self.qlever_log) as fh:
            return len([line for line in fh if line.strip()])

    def assertMarkerAbsent(self, proc, why):
        self.assertIsNone(
            self.get(MARKER),
            f"{why}: corpus marker was advanced by a run that produced no "
            f"index -- the next run will skip and the deployment freezes.\n"
            f"exit={proc.returncode}\nstdout tail:\n"
            + "\n".join(proc.stdout.splitlines()[-15:]),
        )


class TestHarnessIsHonest(RebuildHarness):
    """Guard the harness itself, so a passing suite means something."""

    def test_sandbox_rewrite_is_prefix_only(self):
        _, original, rewritten = sandboxed_script(self.tmp, self.scratch)
        self.assertNotEqual(original, rewritten, "nothing was rewritten")
        # Undoing the rewrite must reproduce the script byte for byte: the
        # sandbox may relocate scratch paths and may do nothing else.
        self.assertEqual(
            rewritten.replace(self.scratch.rstrip("/") + "/", "/tmp/"),
            original,
            "the sandbox rewrite changed something other than the /tmp/ prefix",
        )
        self.assertEqual(original.count("/tmp/"), rewritten.count(self.scratch.rstrip("/") + "/"))

    def test_a_clean_run_promotes_and_records(self):
        """The happy path must actually reach promotion, or every
        failure-injection test below would pass vacuously."""
        proc = self.run_rebuild()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("promoted to latest", proc.stdout)
        self.assertEqual(self.qlever_invocations(), 1)
        self.assertIsNotNone(self.get(MARKER),
                             "a successful run must record the corpus marker")


class TestFailuresStayRetryable(RebuildHarness):
    """No failure may advance the marker (issue #75, first defect)."""

    def test_conversion_failure_does_not_advance_the_marker(self):
        # A corrupt .gz makes gunzip fail inside convert_one_graph.
        self.put("nt-output/g3.nt.gz", b"this is not gzip data")
        self.put("nt-output/g3.nt.gz.graph",
                 "https://packagegraph.github.io/graph/g3-gz\n")
        proc = self.run_rebuild()
        self.assertNotEqual(proc.returncode, 0,
                            "a corrupt input should fail the run")
        self.assertEqual(self.qlever_invocations(), 0,
                         "conversion failed, so the build must not have run")
        self.assertMarkerAbsent(proc, "conversion failure")

    def test_build_failure_does_not_advance_the_marker(self):
        proc = self.run_rebuild(FAKE_QLEVER_FAIL="1")
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(self.qlever_invocations(), 1)
        self.assertMarkerAbsent(proc, "index build failure")

    def test_triple_gate_failure_does_not_advance_the_marker(self):
        # Claim a far larger previous run so the 25%-loss gate trips.
        prev = json.loads(self.get("qlever-index/last-success.json"))
        prev["triple_count"] = 10_000_000
        self.put("qlever-index/last-success.json", json.dumps(prev))
        proc = self.run_rebuild()
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("below", proc.stdout)
        self.assertMarkerAbsent(proc, "triple-count gate failure")

    def test_graph_identity_gate_failure_does_not_advance_the_marker(self):
        prev = json.loads(self.get("qlever-index/last-success.json"))
        prev["graphs"].append("https://packagegraph.github.io/graph/vanished")
        self.put("qlever-index/last-success.json", json.dumps(prev))
        proc = self.run_rebuild()
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("graph set incomplete", proc.stdout)
        self.assertMarkerAbsent(proc, "graph identity gate failure")

    def test_promotion_failure_does_not_advance_the_marker(self):
        # The PUT that moves the `latest` pointer is refused.
        proc = self.run_rebuild(FAKE_MC_FAIL="qlever-index/latest")
        self.assertNotEqual(proc.returncode, 0)
        self.assertMarkerAbsent(proc, "promotion failure")

    def test_upload_failure_does_not_advance_the_marker(self):
        # The index archive itself cannot be stored.
        proc = self.run_rebuild(FAKE_MC_FAIL="index.tar.gz")
        self.assertNotEqual(proc.returncode, 0)
        self.assertMarkerAbsent(proc, "archive upload failure")

    def test_retry_after_a_failed_build_reaches_the_indexer_again(self):
        """The whole point of the marker fix: an unchanged corpus after a
        failure must not be treated as already-done."""
        self.drop_duplicate()
        first = self.run_rebuild(FAKE_QLEVER_FAIL="1")
        self.assertNotEqual(first.returncode, 0)
        self.assertEqual(self.qlever_invocations(), 1)

        second = self.run_rebuild()
        self.assertNotIn(
            "skipping download/build entirely", second.stdout,
            "the retry skipped -- a failed run poisoned the corpus marker",
        )
        self.assertEqual(self.qlever_invocations(), 2,
                         "the retry must invoke the indexer again")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertIn("promoted to latest", second.stdout)

    def test_an_unchanged_corpus_after_success_still_skips(self):
        """The fix must not cost the early-exit optimisation."""
        self.drop_duplicate()
        first = self.run_rebuild()
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        second = self.run_rebuild()
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertIn("skipping download/build entirely", second.stdout)
        self.assertEqual(self.qlever_invocations(), 1,
                         "an unchanged corpus must not rebuild")


class TestSourcePayloadsSurvive(RebuildHarness):
    """Dedup and indexing must not consume inputs (issue #75, second defect)."""

    def test_a_losing_duplicate_keeps_its_bytes_on_success(self):
        before = self.corpus_digest()
        proc = self.run_rebuild()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("duplicate (not used, retained)", proc.stdout,
                      "the fixture's duplicate was not exercised")
        self.assertEqual(self.get(self.loser_key), self.loser_body,
                         "the losing duplicate's payload was destroyed")
        self.assertEqual(before, self.corpus_digest(),
                         "a source object changed during a successful rebuild")

    def test_a_losing_duplicate_keeps_its_bytes_on_failure(self):
        before = self.corpus_digest()
        proc = self.run_rebuild(FAKE_QLEVER_FAIL="1")
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(self.get(self.loser_key), self.loser_body,
                         "a failed rebuild destroyed a source payload it "
                         "would need to retry")
        self.assertEqual(before, self.corpus_digest())

    def test_the_rebuild_never_writes_under_nt_output(self):
        """Strongest form: the source prefix is read-only to this script."""
        self.run_rebuild()
        self.run_rebuild(FAKE_QLEVER_FAIL="1")
        with open(self.mc_log) as fh:
            writes = [line.strip() for line in fh if line.strip()]
        offenders = [w for w in writes if "/nt-output/" in w]
        self.assertEqual(offenders, [],
                         f"rebuild wrote to the source prefix: {offenders}")


if __name__ == "__main__":
    if not os.path.isfile(SCRIPT):
        sys.exit(f"FATAL: {SCRIPT} not found -- this suite would pass vacuously")
    unittest.main(verbosity=2)
