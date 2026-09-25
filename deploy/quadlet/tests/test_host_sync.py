#!/usr/bin/env python3
"""`host-sync.sh` must apply the whole delta, and must not rewrite in place.

The install steps for this directory were scattered across half a dozen
bash blocks in the README, and a partial application is worse than none:
#94's per-enricher timeouts do nothing without their drop-ins, and the
`OnFailure=` in both templates points at a unit a host may not have.
`host-sync.sh` collapses those blocks into one idempotent script.

Two properties, and the second is the one that decays quietly:

  1. It applies everything -- every drop-in, script, timer and template in
     the repo lands, and the retired repology files are actively removed
     rather than just no longer installed. Deleting a file from the repo
     uninstalls nothing from a host that already has it.
  2. It replaces files by rename, never by truncate-and-rewrite. Scripts
     here are bind-mounted into running containers; `install` truncates in
     place, so a container reading one at that moment sees a truncated
     script. That hazard is invisible in any output -- the only evidence is
     that the destination inode changes -- so nothing but a test stops
     someone simplifying `install_atomic` back to `install`.

The real script runs, against a throwaway tree via PG_SYNC_ROOT. There is
no systemd in CI, so its `systemctl` calls are stubbed by the script
itself under that variable.

Run directly, not via `unittest discover` (#57).
"""

import os
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
QUADLET = os.path.dirname(HERE)
SYNC = os.path.join(QUADLET, "host-sync.sh")
DROPINS = os.path.join(QUADLET, "enrichers", "dropins")
ESCRIPTS = os.path.join(QUADLET, "enrichers", "scripts")
TIMERS = os.path.join(QUADLET, "enrichers", "timers")


def run_sync(root):
    env = dict(os.environ, PG_SYNC_ROOT=root)
    return subprocess.run(
        [SYNC, QUADLET], env=env, capture_output=True, text=True, check=True
    )


def seed_host(root):
    """A tree shaped like the host before the sync: repology installed,
    no drop-ins, no notifier, an older enricher template."""
    sd = os.path.join(root, "etc", "systemd", "system")
    qd = os.path.join(root, "etc", "containers", "systemd")
    os.makedirs(os.path.join(sd, "pg-enrich@repology.service.d"))
    os.makedirs(os.path.join(qd, "scripts", "enrichers"))
    for path in [
        os.path.join(sd, "pg-enrich-repology.timer"),
        os.path.join(sd, "pg-enrich@repology.service.d", "timeout.conf"),
        os.path.join(qd, "scripts", "enrichers", "repology.sh"),
        os.path.join(qd, "pg-enrich@.container"),
    ]:
        with open(path, "w") as f:
            f.write("stale content from before the sync\n")
    return sd, qd


class HostSync(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="pg-host-sync-test.")
        self.addCleanup(subprocess.run, ["rm", "-rf", self.tmp])
        self.sd, self.qd = seed_host(self.tmp)

    def test_every_repo_dropin_is_installed(self):
        run_sync(self.tmp)
        for name in sorted(os.listdir(DROPINS)):
            with self.subTest(dropin=name):
                self.assertTrue(
                    os.path.isfile(os.path.join(self.sd, name, "timeout.conf")),
                    f"{name} exists in the repo but the sync did not install it",
                )

    def test_every_enricher_script_and_timer_is_installed(self):
        run_sync(self.tmp)
        for f in sorted(os.listdir(ESCRIPTS)):
            with self.subTest(script=f):
                self.assertTrue(
                    os.path.isfile(os.path.join(self.qd, "scripts", "enrichers", f))
                )
        for f in sorted(os.listdir(TIMERS)):
            with self.subTest(timer=f):
                self.assertTrue(os.path.isfile(os.path.join(self.sd, f)))

    def test_the_failure_notifier_is_installed(self):
        # #94's whole point. Both templates name it in OnFailure=, so a host
        # without it turns every failure into a second, confusing failure.
        run_sync(self.tmp)
        self.assertTrue(
            os.path.isfile(os.path.join(self.sd, "pg-unit-failed@.service"))
        )
        self.assertTrue(
            os.path.isfile(os.path.join(self.qd, "scripts", "unit-failed.sh"))
        )
        self.assertTrue(
            os.path.isdir(
                os.path.join(self.tmp, "var", "lib", "packagegraph", "failed-units")
            )
        )

    def test_retired_repology_files_are_removed_from_the_host(self):
        # Removing them from the repo uninstalls nothing. An orphaned timer
        # keeps firing a unit whose script is gone.
        run_sync(self.tmp)
        for leftover in [
            os.path.join(self.sd, "pg-enrich-repology.timer"),
            os.path.join(self.sd, "pg-enrich@repology.service.d"),
            os.path.join(self.qd, "scripts", "enrichers", "repology.sh"),
        ]:
            with self.subTest(path=os.path.basename(leftover)):
                self.assertFalse(
                    os.path.exists(leftover),
                    f"{leftover} survived the sync",
                )

    def test_an_existing_file_is_replaced_not_written_through(self):
        # The bind-mount hazard, and the reason this script does not simply
        # `cp` or `scp` onto the destination path. Bash re-reads its script
        # from disk while executing it, so writing through an existing inode
        # splices old and new content together for anything mid-read -- that
        # cost this host ~50 minutes of a QLever index build once.
        #
        # A surviving inode is the signature of a write-through. Both `mv`
        # and GNU `install` replace the inode, so this does not discriminate
        # between those two; it discriminates against the shapes that
        # actually caused the incident.
        targets = [
            os.path.join(self.qd, "pg-enrich@.container"),
            os.path.join(self.qd, "scripts", "enrichers", "repology.sh"),
        ]
        before = {t: os.stat(t).st_ino for t in targets}
        run_sync(self.tmp)
        # repology.sh is removed outright rather than replaced, so its old
        # inode must not still be sitting at that path either.
        self.assertFalse(os.path.exists(targets[1]))
        after = os.stat(targets[0]).st_ino
        self.assertNotEqual(
            before[targets[0]],
            after,
            "the destination kept its inode, so it was written through "
            "rather than replaced -- a container executing it would have "
            "read spliced content",
        )

    def test_no_staging_files_are_left_behind(self):
        run_sync(self.tmp)
        stragglers = [
            os.path.join(dirpath, f)
            for dirpath, _, files in os.walk(self.tmp)
            for f in files
            if f.startswith(".pg-host-sync.")
        ]
        self.assertEqual(stragglers, [])

    def test_it_is_idempotent(self):
        first = run_sync(self.tmp)
        second = run_sync(self.tmp)
        # The retirement block is the part that could fail on a second pass,
        # by trying to remove files it already removed.
        self.assertEqual(second.returncode, 0)
        self.assertEqual(
            first.stdout.split("== verification")[-1],
            second.stdout.split("== verification")[-1],
        )


if __name__ == "__main__":
    unittest.main()
