#!/usr/bin/env python3
"""Drive the real qlever-refresh-if-changed.sh against local fakes.

Issue #73: the script used to decide whether to reload by reading
`.status` out of last-run.json. That object is written best-effort from an
EXIT trap with `|| true`, so a run could promote a new index and then die
before recording it -- after which the refresh read a missing or stale
status, concluded nothing had been promoted, and left the old index serving
with no reload attempted and nothing in the logs resembling a failure.

The property under test is that the decision follows STATE, not a report:
reload whenever what is promoted differs from what has been confirmed
serving, whatever last-run.json happens to say, and advance the serving
marker only once a reloaded qlever has actually answered.

The real script runs. `podman`, `systemctl`, `curl` and `mc` are faked, and
the podman fake executes the inner container scripts for real so the marker
reads and writes stay under test. No `/tmp` rewriting is needed here: unlike
the collector wrappers, this script takes every path from the environment.

Run directly, not via `unittest discover` (#57).
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(os.path.dirname(HERE), "scripts", "qlever-refresh-if-changed.sh")
FAKES = os.path.join(HERE, "fakes")
BUCKET = "test-bucket"

LOADER = "qlever-index-load.service"
SERVER = "qlever.service"


class RefreshHarness(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="qlever-refresh-test-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.store = os.path.join(self.tmp, "store")
        self.data = os.path.join(self.tmp, "data")
        self.bin = os.path.join(self.tmp, "bin")
        os.makedirs(os.path.join(self.data, "index"))
        os.makedirs(self.bin)
        for name in ("podman", "systemctl", "curl", "mc"):
            shutil.copy2(os.path.join(FAKES, name), os.path.join(self.bin, name))
            os.chmod(os.path.join(self.bin, name), 0o755)

        self.systemctl_log = os.path.join(self.tmp, "systemctl.log")
        self.ready_flag = os.path.join(self.tmp, "ready")
        self.env_file = os.path.join(self.tmp, "minio.env")
        with open(self.env_file, "w") as fh:
            fh.write(
                "MINIO_ENDPOINT=http://fake\n"
                "MINIO_ACCESS_KEY=fake\n"
                "MINIO_SECRET_KEY=fake\n"
                f"MINIO_BUCKET={BUCKET}\n"
            )

    # ---- fixture state ---------------------------------------------------

    def promote(self, content_hash):
        path = os.path.join(self.store, BUCKET, "qlever-index", "latest")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as fh:
            fh.write(content_hash)

    def write_last_run(self, payload):
        """payload=None removes the object entirely."""
        path = os.path.join(self.store, BUCKET, "qlever-index", "last-run.json")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        if payload is None:
            if os.path.exists(path):
                os.remove(path)
            return
        with open(path, "w") as fh:
            fh.write(payload if isinstance(payload, str) else json.dumps(payload))

    def set_serving(self, content_hash):
        path = os.path.join(self.data, "index", ".serving")
        if content_hash is None:
            if os.path.exists(path):
                os.remove(path)
            return
        with open(path, "w") as fh:
            fh.write(content_hash)

    def serving(self):
        path = os.path.join(self.data, "index", ".serving")
        if not os.path.isfile(path):
            return None
        with open(path) as fh:
            return fh.read().strip()

    def set_ready(self, ready):
        if ready:
            open(self.ready_flag, "w").close()
        elif os.path.exists(self.ready_flag):
            os.remove(self.ready_flag)

    def restarts(self):
        if not os.path.isfile(self.systemctl_log):
            return []
        with open(self.systemctl_log) as fh:
            return [l.strip() for l in fh if l.strip()]

    # ---- execution -------------------------------------------------------

    def run_refresh(self, **inject):
        env = dict(os.environ)
        env.update({
            "PATH": self.bin + os.pathsep + env["PATH"],
            "FAKE_S3_ROOT": self.store,
            "FAKE_DATA": self.data,
            "FAKE_SYSTEMCTL_LOG": self.systemctl_log,
            "FAKE_READY_FLAG": self.ready_flag,
            "QLEVER_ENV_FILE": self.env_file,
            "QLEVER_REBUILD_IMAGE": "localhost/fake-qlever-rebuild:test",
            "QLEVER_DATA_VOLUME": "fake-qlever-data",
            "QLEVER_READY_ATTEMPTS": "3",
            "QLEVER_READY_INTERVAL": "0.05",
        })
        for key in ("FAKE_SYSTEMCTL_FAIL", "FAKE_PODMAN_FAIL"):
            env.pop(key, None)
        env.update(inject)
        return subprocess.run(["bash", SCRIPT], env=env,
                              capture_output=True, text=True, timeout=120)


class TestHarnessIsHonest(RefreshHarness):
    def test_a_normal_reload_restarts_both_units_and_records(self):
        """Without a working happy path, every failure test below is vacuous."""
        self.promote("aaaaaaaaaaaaaaaa")
        self.set_serving("bbbbbbbbbbbbbbbb")
        self.write_last_run({"status": "success"})
        self.set_ready(True)
        proc = self.run_refresh()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.restarts(), [f"restart {LOADER}", f"restart {SERVER}"])
        self.assertEqual(self.serving(), "aaaaaaaaaaaaaaaa")


class TestDecisionFollowsStateNotStatus(RefreshHarness):
    """The core of #73: run status must not gate the transition."""

    def _promoted_not_serving(self):
        self.promote("cafecafecafecafe")
        self.set_serving("0000000000000000")
        self.set_ready(True)

    def test_reloads_when_status_object_is_missing(self):
        self._promoted_not_serving()
        self.write_last_run(None)
        proc = self.run_refresh()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(f"restart {SERVER}", self.restarts(),
                      "a promoted-but-not-serving index must reload with no status object")
        self.assertEqual(self.serving(), "cafecafecafecafe")

    def test_reloads_when_status_says_the_run_failed(self):
        """A run can promote and then fail before writing its own status."""
        self._promoted_not_serving()
        self.write_last_run({"status": "failure"})
        proc = self.run_refresh()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(f"restart {SERVER}", self.restarts())
        self.assertEqual(self.serving(), "cafecafecafecafe")

    def test_reloads_when_status_says_unchanged(self):
        self._promoted_not_serving()
        self.write_last_run({"status": "unchanged"})
        proc = self.run_refresh()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(f"restart {SERVER}", self.restarts())

    def test_reloads_when_status_is_unparseable(self):
        self._promoted_not_serving()
        self.write_last_run("{not json at all")
        proc = self.run_refresh()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(f"restart {SERVER}", self.restarts())

    def test_does_not_reload_when_already_serving_the_promotion(self):
        """Even when the status object claims a fresh success."""
        self.promote("dededededededede")
        self.set_serving("dededededededede")
        self.write_last_run({"status": "success"})
        self.set_ready(True)
        proc = self.run_refresh()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.restarts(), [],
                         "nothing changed, so qlever must not be bounced")

    def test_a_missing_serving_marker_counts_as_not_serving(self):
        self.promote("feedfeedfeedfeed")
        self.set_serving(None)
        self.write_last_run({"status": "unchanged"})
        self.set_ready(True)
        proc = self.run_refresh()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.serving(), "feedfeedfeedfeed")

    def test_an_empty_promotion_is_an_error_not_a_no_op(self):
        self.promote("")
        self.set_serving("0000000000000000")
        proc = self.run_refresh()
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(self.restarts(), [])
        self.assertEqual(self.serving(), "0000000000000000")


class TestFailuresLeaveTheMarkerAlone(RefreshHarness):
    """Every failure must be retryable on the next run."""

    def setUp(self):
        super().setUp()
        self.promote("1111111111111111")
        self.set_serving("0000000000000000")
        self.write_last_run({"status": "success"})

    def assertMarkerUnmoved(self, proc):
        self.assertEqual(
            self.serving(), "0000000000000000",
            f"the serving marker advanced without a confirmed reload.\n"
            f"exit={proc.returncode}\n{proc.stdout}\n{proc.stderr}",
        )

    def test_loader_failure_does_not_restart_qlever_or_advance(self):
        self.set_ready(True)
        proc = self.run_refresh(FAKE_SYSTEMCTL_FAIL=LOADER)
        self.assertNotEqual(proc.returncode, 0)
        self.assertNotIn(f"restart {SERVER}", self.restarts(),
                         "a failed load must not bounce qlever onto nothing new")
        self.assertMarkerUnmoved(proc)

    def test_server_restart_failure_does_not_advance(self):
        self.set_ready(True)
        proc = self.run_refresh(FAKE_SYSTEMCTL_FAIL=SERVER)
        self.assertNotEqual(proc.returncode, 0)
        self.assertMarkerUnmoved(proc)

    def test_readiness_timeout_does_not_advance(self):
        self.set_ready(False)
        proc = self.run_refresh()
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(f"restart {SERVER}", self.restarts())
        self.assertMarkerUnmoved(proc)

    def test_an_unwritable_marker_is_reported_rather_than_assumed(self):
        self.set_ready(True)
        proc = self.run_refresh(FAKE_PODMAN_FAIL=".serving")
        self.assertNotEqual(proc.returncode, 0)
        self.assertMarkerUnmoved(proc)

    def test_a_failed_run_is_retried_and_then_succeeds(self):
        self.set_ready(False)
        first = self.run_refresh()
        self.assertNotEqual(first.returncode, 0)
        self.assertMarkerUnmoved(first)

        self.set_ready(True)
        second = self.run_refresh()
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual(self.serving(), "1111111111111111")
        self.assertEqual(self.restarts().count(f"restart {SERVER}"), 2,
                         "the retry must actually attempt the reload again")

    def test_the_recorded_identifier_is_the_one_that_was_loaded(self):
        """A promotion that lands mid-restart must not be credited to it."""
        self.set_ready(True)

        # Stand in for a rebuild promoting again while qlever restarts: move
        # `latest` on after the script has already read it, via the readiness
        # probe, which is the last thing to run before the marker is written.
        racer = os.path.join(self.bin, "curl")
        with open(racer, "w") as fh:
            fh.write(
                "#!/usr/bin/env python3\n"
                "import os, sys\n"
                f"open({os.path.join(self.store, BUCKET, 'qlever-index', 'latest')!r}, 'w')"
                ".write('2222222222222222')\n"
                "sys.exit(0 if os.path.isfile(os.environ['FAKE_READY_FLAG']) else 22)\n"
            )
        os.chmod(racer, 0o755)

        proc = self.run_refresh()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(
            self.serving(), "1111111111111111",
            "recorded a promotion this reload never loaded",
        )


class TestTheUnitActuallyInvokesIt(unittest.TestCase):
    """The best decision logic is useless if systemd never runs it.

    These are static checks on qlever-rebuild-index.container, because both
    properties look like style and are not.
    """

    UNIT = os.path.join(os.path.dirname(HERE), "qlever-rebuild-index.container")

    def setUp(self):
        self.assertTrue(os.path.isfile(self.UNIT), f"{self.UNIT} is missing")
        with open(self.UNIT) as fh:
            self.lines = [l.strip() for l in fh
                          if l.strip() and not l.strip().startswith("#")]

    def directive(self, key):
        return [l.split("=", 1)[1] for l in self.lines if l.startswith(key + "=")]

    def test_the_refresh_runs_on_the_failure_path_too(self):
        """systemd runs ExecStartPost= only after a successful ExecStart=.

        The run that most needs a reload is one that promoted and then failed
        -- the rebuild exits 1 if it cannot persist last-success.json, after
        `latest` has already moved. ExecStartPost= never fires there.
        """
        self.assertEqual(
            self.directive("ExecStartPost"), [],
            "the refresh is back on ExecStartPost=, so it will not run after a "
            "rebuild that promoted and then failed -- the #73 case",
        )
        post = self.directive("ExecStopPost")
        self.assertEqual(len(post), 1, f"expected one ExecStopPost=, got {post}")
        self.assertTrue(post[0].endswith("qlever-refresh-if-changed.sh"), post[0])

    def test_the_stop_timeout_outlasts_the_readiness_poll(self):
        """ExecStopPost= is bounded by TimeoutStopSec=, default 90s. The
        refresh polls readiness for up to 30 x 10s, so the default would kill
        a reload that was working and leave the marker unadvanced."""
        stop = self.directive("TimeoutStopSec")
        self.assertEqual(len(stop), 1, f"expected one TimeoutStopSec=, got {stop}")
        with open(SCRIPT) as fh:
            script = fh.read()
        attempts = int(re.search(r'QLEVER_READY_ATTEMPTS:-(\d+)', script).group(1))
        interval = int(re.search(r'QLEVER_READY_INTERVAL:-(\d+)', script).group(1))
        budget = attempts * interval
        self.assertGreater(
            int(stop[0]), budget,
            f"TimeoutStopSec={stop[0]} does not outlast the script's own "
            f"{attempts} x {interval}s = {budget}s readiness poll",
        )


if __name__ == "__main__":
    if not os.path.isfile(SCRIPT):
        sys.exit(f"FATAL: {SCRIPT} not found -- this suite would pass vacuously")
    unittest.main(verbosity=2)
