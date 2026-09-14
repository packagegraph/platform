"""Real wrapper -> CLI -> upload-nt.sh -> commit, with local external services.

Run via cargo test --test test_checkpoint_lifecycle (sets PG_COLLECT_BIN).
No production paths, credentials, private scratch files or network services.
"""
import gzip
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import unittest

from checkpoint_fixture import Hub

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]


class LifecycleTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="checkpoint-lifecycle-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.hub = Hub()
        self.addCleanup(self.hub.close)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        adapter = self.bin / "adapter"
        adapter.write_bytes((HERE / "checkpoint_commands.py").read_bytes())
        adapter.chmod(0o755)
        for name in ("mc", "pg-collect", "sleep"):
            (self.bin / name).symlink_to(adapter)
        # Relocate ONLY absolute filesystem paths. Shell control flow, commit
        # arguments, mirroring and invocation of the real upload script stay intact.
        self.wrapper = self.root / "wrapper.sh"
        source = Path(os.environ.get("CHECKPOINT_TEST_WRAPPER",
                                    HERE.parent / "scripts/fedora-44-full.sh")).read_text()
        self.wrapper.write_text(source.replace("/tmp/", str(self.root) + "/")
                                .replace("/app/scripts/", str(REPO / "etl/scripts") + "/"))
        self.cache = self.root / "cache/fedora-44-full"
        self.state_path = self.cache / "output/GENERATION"
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                        CHECKPOINT_TEST_ROOT=str(self.root), CHECKPOINT_TEST_HUB=self.hub.url,
                        MINIO_BUCKET="test", MINIO_ENDPOINT=self.hub.url,
                        MINIO_ACCESS_KEY="test", MINIO_SECRET_KEY="test")
        # Fail rather than accidentally using a globally installed old binary.
        self.assertTrue(Path(self.env["PG_COLLECT_BIN"]).is_file())

    def run_wrapper(self, fail_upload="", fail_collect=False):
        (self.root / "periodic-started").unlink(missing_ok=True)
        process = subprocess.Popen(
            ["sh", str(self.wrapper)], env=dict(self.env, FAIL_UPLOAD=fail_upload,
                                              FAIL_COLLECT="1" if fail_collect else ""),
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True,
        )
        try:
            output, _ = process.communicate(timeout=30)
            return process.returncode, output
        finally:
            # Clean up the periodic-loop descendants even on timeout/assertion.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()

    def state(self):
        return json.loads(self.state_path.read_text())

    def fragment(self):
        text = (self.root / "collection/fedora-44.nt").read_text()
        return "\n".join(line for line in text.splitlines() if "/d/build/fedora/" in line)

    def test_failed_upload_replays_then_success_retires_and_rederives(self):
        # Removing/hoisting commit or swallowing an upload error must fail this.
        code, output = self.run_wrapper(fail_upload="cp")
        self.assertEqual(code, 42, output)
        old = self.state()
        self.assertEqual(old["status"], "active")
        self.assertIn("0 hits, 1 misses", output)
        self.assertEqual(self.hub.rpcs, ["getBuild", "listBuildRPMs", "queryRPMSigs"])
        fragment = self.fragment()
        self.assertIn("rehearsal", fragment)
        self.assertIn("cafebabe", fragment)
        self.hub.rpcs.clear()

        code, output = self.run_wrapper()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.state(), {"id": old["id"], "status": "complete"})
        self.assertIn("1 hits, 0 misses", output)
        self.assertEqual(self.hub.rpcs, [])
        self.assertEqual(self.fragment(), fragment)
        uploaded = self.root / "remote/nt-output/fedora-44.nt.gz"
        self.assertIn("cafebabe", gzip.decompress(uploaded.read_bytes()).decode())
        self.assertEqual(uploaded.with_suffix(".gz.graph").read_text(),
                         "https://packagegraph.github.io/graph/fedora/44")
        remote_cache = self.root / "remote/collector-cache/fedora-44-full"
        self.assertTrue(list(remote_cache.rglob("*.json")), "source files must actually mirror")
        self.assertFalse((remote_cache / "output").exists())

        code, output = self.run_wrapper()
        self.assertEqual(code, 0, output)
        self.assertNotEqual(self.state()["id"], old["id"])
        self.assertEqual(self.state()["status"], "complete")
        self.assertIn("0 hits, 1 misses", output)
        self.assertEqual(self.hub.rpcs, [], "fresh generation still reuses valid source cache")
        self.assertFalse((self.cache / "output" / old["id"]).exists())

    def test_failed_sidecar_does_not_retire_generation(self):
        code, output = self.run_wrapper(fail_upload="pipe")
        self.assertEqual(code, 42, output)
        self.assertEqual(self.state()["status"], "active")
        self.assertTrue((self.root / "remote/nt-output/fedora-44.nt.gz").is_file())
        self.assertFalse((self.root / "remote/nt-output/fedora-44.nt.gz.graph").exists())

    def test_failed_collect_neither_uploads_nor_commits(self):
        self.assertEqual(self.run_wrapper(fail_upload="cp")[0], 42)
        old = self.state()
        (self.root / "mc-calls").unlink()
        code, output = self.run_wrapper(fail_collect=True)
        self.assertEqual(code, 43, output)
        self.assertEqual(self.state(), old)
        calls = [json.loads(line) for line in (self.root / "mc-calls").read_text().splitlines()]
        self.assertFalse(any(call[0] in ("cp", "pipe") for call in calls))

    def test_retryable_rpc_is_not_checkpointed_and_retries_on_resume(self):
        self.hub.fail_signatures = True
        code, output = self.run_wrapper(fail_upload="cp")
        self.assertEqual(code, 42, output)
        self.assertIn("1 retryable", output)
        generation = self.state()["id"]
        self.assertEqual(list((self.cache / "output" / generation).rglob("koji-v1/*")), [])
        self.hub.fail_signatures = False
        self.hub.rpcs.clear()
        code, output = self.run_wrapper()
        self.assertEqual(code, 0, output)
        self.assertIn("0 hits, 1 misses", output)
        self.assertIn("queryRPMSigs", self.hub.rpcs)
        self.assertIn("cafebabe", self.fragment())

    def test_disabled_checkpoint_setup_still_uploads_and_finishes(self):
        self.cache.mkdir(parents=True)
        (self.cache / "output").write_text("not a directory")
        code, output = self.run_wrapper()
        self.assertEqual(code, 0, output)
        self.assertIn("checkpointing disabled", output)
        self.assertTrue((self.root / "remote/nt-output/fedora-44.nt.gz.graph").is_file())


if __name__ == "__main__":
    unittest.main()
