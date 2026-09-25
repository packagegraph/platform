#!/usr/bin/env python3
"""A failed unit must leave a trace somewhere a human will find it.

Issue #59: three enrichers had been failing since 2026-09-14 and one
collector since 2026-09-12 with nothing reporting it. The units were not
silent -- `systemctl --failed` knew, and so did the journal. Nobody was
reading either. The fix is an `OnFailure=` notifier that writes to a fixed
address on disk, plus per-instance timeouts so a hung enricher is noticed
in its own timescale rather than eight hours later.

Two properties here, and the second is the one that decays quietly:

  1. The real `unit-failed.sh` records the failure -- the unit's name, how
     it ended, and the tail of its journal -- without depending on anything
     leaving the host.
  2. The wiring holds. Every template names the notifier; the notifier does
     not name itself; script, timer and drop-in exist together for every
     enricher, so a retired one leaves nothing behind that still reads as
     live; and nothing suppresses its own failure reporting.

The real script runs. `systemctl`, `journalctl` and `logger` are faked into
a temp bin dir, and the state directory comes from PG_FAILED_UNITS_DIR.

Run directly, not via `unittest discover` (#57).
"""

import os
import re
import shutil
import stat
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
QUADLET = os.path.dirname(HERE)
SCRIPT = os.path.join(QUADLET, "scripts", "unit-failed.sh")
NOTIFIER = os.path.join(QUADLET, "failure", "pg-unit-failed@.service")
ENRICH_TEMPLATE = os.path.join(QUADLET, "enrichers", "pg-enrich@.container")
COLLECT_TEMPLATES = [
    os.path.join(QUADLET, "collectors", "pg-collect@.container"),
    os.path.join(QUADLET, "collectors", "pg-collect-rhel@.container"),
]
ENRICH_SCRIPTS = os.path.join(QUADLET, "enrichers", "scripts")
DROPINS = os.path.join(QUADLET, "enrichers", "dropins")
TIMERS = os.path.join(QUADLET, "enrichers", "timers")

FAKE_SYSTEMCTL = """#!/bin/sh
# `systemctl show <unit> --property=X --value` and the multi-property form.
for arg in "$@"; do
  if [ "$arg" = "--value" ]; then
    echo timeout
    exit 0
  fi
done
echo "Result=timeout"
echo "ExecMainStatus=0"
echo "ExecMainCode=2"
echo "NRestarts=0"
echo "InactiveEnterTimestamp=Mon 2026-09-14 20:32:05 UTC"
echo "ActiveEnterTimestamp=Mon 2026-09-14 12:30:34 UTC"
"""

FAKE_JOURNALCTL = """#!/bin/sh
echo "20:31:54  Progress: 45100 packages checked"
echo "20:32:05  container stop"
"""

FAKE_LOGGER = """#!/bin/sh
echo "$@" >> "$LOGGER_LOG"
"""


def read(path):
    with open(path) as fh:
        return fh.read()


def enricher_names():
    return sorted(
        f[:-3] for f in os.listdir(ENRICH_SCRIPTS) if f.endswith(".sh")
    )


class RecordsTheFailure(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="unit-failed-test-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.state = os.path.join(self.tmp, "state")
        self.bin = os.path.join(self.tmp, "bin")
        os.makedirs(self.bin)
        self.logger_log = os.path.join(self.tmp, "logger.log")
        for name, body in (
            ("systemctl", FAKE_SYSTEMCTL),
            ("journalctl", FAKE_JOURNALCTL),
            ("logger", FAKE_LOGGER),
        ):
            path = os.path.join(self.bin, name)
            with open(path, "w") as fh:
                fh.write(body)
            os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)

    def run_script(self, *args):
        env = dict(os.environ)
        env["PATH"] = self.bin + os.pathsep + env["PATH"]
        env["PG_FAILED_UNITS_DIR"] = self.state
        env["LOGGER_LOG"] = self.logger_log
        return subprocess.run(
            ["/bin/sh", SCRIPT, *args],
            env=env,
            capture_output=True,
            text=True,
        )

    def test_a_failed_unit_leaves_its_name_verdict_and_journal_tail(self):
        proc = self.run_script("pg-enrich@security.service")
        self.assertEqual(proc.returncode, 0, proc.stderr)

        # '@' is awkward in a filename; the unit name is still recorded
        # verbatim inside, which is what a reader greps for.
        report = os.path.join(self.state, "pg-enrich_security.service.txt")
        self.assertTrue(os.path.exists(report), os.listdir(self.state))
        body = read(report)
        self.assertIn("pg-enrich@security.service", body)
        self.assertIn("Result=timeout", body)
        # The kill's evidence is in the last lines before SIGKILL, and the
        # journal rotates -- a pointer to journalctl would not survive.
        self.assertIn("Progress: 45100 packages checked", body)

    def test_every_failure_is_appended_so_a_repeat_offender_looks_different(self):
        self.run_script("pg-enrich@security.service")
        self.run_script("pg-enrich@koji.service")
        self.run_script("pg-enrich@security.service")

        lines = read(os.path.join(self.state, "history.tsv")).splitlines()
        self.assertEqual(len(lines), 3, lines)
        self.assertEqual(
            [line.split("\t")[1] for line in lines],
            [
                "pg-enrich@security.service",
                "pg-enrich@koji.service",
                "pg-enrich@security.service",
            ],
        )
        for line in lines:
            self.assertTrue(line.endswith("\ttimeout"), line)

    def test_the_journal_gets_a_tagged_line_too(self):
        # The directory can be lost; `journalctl -t pg-unit-failed` should
        # still be a complete list of what failed.
        self.run_script("pg-enrich@taxonomy.service")
        self.assertIn(
            "pg-enrich@taxonomy.service failed",
            read(self.logger_log),
        )

    def test_called_with_no_unit_it_complains_rather_than_writing_nonsense(self):
        proc = self.run_script()
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(os.path.exists(self.state))


class TheWiringHolds(unittest.TestCase):
    """The static half. A notifier nothing invokes is worth nothing."""

    def test_every_collector_and_enricher_template_invokes_the_notifier(self):
        for path in [ENRICH_TEMPLATE, *COLLECT_TEMPLATES]:
            with self.subTest(template=os.path.basename(path)):
                self.assertIn(
                    "OnFailure=pg-unit-failed@%n.service",
                    read(path),
                    f"{path} does not report its failures",
                )

    def test_the_notifier_does_not_notify_about_itself(self):
        # A notifier that failed would otherwise trigger the notifier.
        body = read(NOTIFIER)
        self.assertRegex(body, r"(?m)^OnFailure=\s*$")

    def test_the_notifier_runs_the_script_this_test_drives(self):
        self.assertIn(
            "/etc/containers/systemd/scripts/unit-failed.sh",
            read(NOTIFIER),
        )

    def test_every_enricher_has_a_timeout_drop_in_and_every_drop_in_an_enricher(self):
        have = sorted(
            d[len("pg-enrich@"):-len(".service.d")]
            for d in os.listdir(DROPINS)
            if d.startswith("pg-enrich@") and d.endswith(".service.d")
        )
        self.assertEqual(
            have,
            enricher_names(),
            "a new enricher needs its own timeout, and a removed one should "
            "not leave a drop-in behind",
        )

    def test_every_drop_in_states_a_timeout_and_the_basis_for_it(self):
        for name in enricher_names():
            path = os.path.join(
                DROPINS, f"pg-enrich@{name}.service.d", "timeout.conf"
            )
            with self.subTest(enricher=name):
                body = read(path)
                self.assertRegex(body, r"(?m)^TimeoutStartSec=\d+$")
                # A number with no stated basis is the thing this issue is
                # about: nobody can tell later whether it was measured or
                # invented.
                self.assertRegex(body, r"(?m)^# Basis: (MEASURED|ESTIMATED|UNMEASURED)")

    def test_no_enricher_silences_the_notifier(self):
        # repology used to be exempt, because a ~90h pass meant a weekly
        # timeout kill was expected and a weekly failure report would have
        # trained the reader to ignore reports. It is retired rather than
        # exempted now, so nothing left here should be suppressing its own
        # failures -- an enricher worth running is one worth hearing about.
        for name in enricher_names():
            body = read(
                os.path.join(DROPINS, f"pg-enrich@{name}.service.d", "timeout.conf")
            )
            with self.subTest(enricher=name):
                self.assertNotRegex(body, r"(?m)^OnFailure=")

    def test_every_enricher_script_still_has_a_timer(self):
        # A script with no timer is dead weight that reads as live: still
        # installed, still mounted by the template, still looking like
        # something that runs. Retiring an enricher means removing the
        # script, the timer and the drop-in together.
        timers = sorted(
            f[len("pg-enrich-"):-len(".timer")]
            for f in os.listdir(TIMERS)
            if f.startswith("pg-enrich-") and f.endswith(".timer")
        )
        self.assertEqual(timers, enricher_names())

    def test_a_drop_in_timeout_is_not_longer_than_the_template_default(self):
        # The template's number is the conservative ceiling. A drop-in above
        # it would be a silent raise nobody asked for; raising the ceiling
        # belongs in the template, where its comment lives.
        template = read(ENRICH_TEMPLATE)
        ceiling = int(re.search(r"(?m)^TimeoutStartSec=(\d+)$", template).group(1))
        for name in enricher_names():
            body = open(
                os.path.join(DROPINS, f"pg-enrich@{name}.service.d", "timeout.conf")
            ).read()
            value = int(re.search(r"(?m)^TimeoutStartSec=(\d+)$", body).group(1))
            with self.subTest(enricher=name):
                self.assertLessEqual(value, ceiling)


if __name__ == "__main__":
    unittest.main()
