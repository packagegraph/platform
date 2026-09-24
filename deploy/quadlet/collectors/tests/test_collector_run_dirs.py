#!/usr/bin/env python3
"""Static contract: every collector wrapper isolates its ephemeral scratch.

The collector scratch volume is mounted at /tmp and shared by every
concurrently running instance, so a fixed path there is a shared mailbox,
not scratch space (issue #69). This checks the property that makes sharing
safe, on every wrapper, rather than trusting a convention:

  * the run-directory preamble is present and correctly ordered;
  * no ephemeral /tmp path escapes $RUN_DIR;
  * the only persistent /tmp roots are the allowlisted cache ones.

Static analysis is the right shape here because the failure is silent: a
wrapper that publishes another collector's bytes exits 0 and uploads a
well-formed file. There is nothing to notice at runtime.

Run directly (`python3 .../test_collector_run_dirs.py -v`), not via
`unittest discover` -- discover exits 0 when it matches nothing, which would
turn a renamed file into a vacuous pass (#57).
"""

import glob
import os
import re
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPTS = os.path.join(os.path.dirname(HERE), "scripts")

# Not a collector: it is the contract checker for the checkpoint lifecycle.
NOT_A_WRAPPER = {"test-wrapper-checkpoint-contract.sh"}

# The only /tmp roots a wrapper may reference outside $RUN_DIR. These are
# persistent by design -- the HTTP/Koji cache and the checkpoint generations
# that make a timed-out run resumable -- and are mirrored to object storage.
# Adding to this list means committing to state that outlives the run.
PERSISTENT_ROOTS = ("/tmp/cache/",)

MKTEMP = re.compile(r"^RUN_DIR=\$\(mktemp -d /tmp/run-(?P<name>[A-Za-z0-9._-]+)\.X{6,}\)$")
SWEEP = re.compile(r"^find /tmp -maxdepth 1 -type d -name 'run-\*\.\*' -mmin \+(?P<age>\d+) ")
CLEANUP_TRAP = "trap 'rm -rf \"$RUN_DIR\"' EXIT"

# Longest a collector may run before systemd SIGKILLs it, from
# pg-collect@.container's TimeoutStartSec. A sweep threshold at or below this
# could delete a live run's directory out from under it.
TIMEOUT_MINUTES = 50400 // 60


def wrappers():
    found = sorted(
        p for p in glob.glob(os.path.join(SCRIPTS, "*.sh"))
        if os.path.basename(p) not in NOT_A_WRAPPER
    )
    return found


def code_lines(text):
    """Lines with comments and blanks dropped.

    Comments legitimately mention the paths under discussion, and flagging
    them would push the next author into deleting the explanation.
    """
    out = []
    for line in text.split("\n"):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        out.append(line)
    return out


class TestWrappersExist(unittest.TestCase):
    def test_wrappers_are_found(self):
        """Without this the whole suite passes on an empty set."""
        self.assertTrue(os.path.isdir(SCRIPTS), f"{SCRIPTS} is missing")
        self.assertGreaterEqual(
            len(wrappers()), 40,
            "found suspiciously few collector wrappers -- did the directory move?",
        )


class TestRunDirContract(unittest.TestCase):
    """One assertion set, applied to every wrapper."""

    def each(self):
        for path in wrappers():
            with self.subTest(wrapper=os.path.basename(path)):
                with open(path) as fh:
                    yield path, fh.read()

    def test_every_wrapper_mints_a_run_dir_named_for_itself(self):
        for path, text in self.each():
            name = os.path.basename(path)[:-3]
            hits = [m for m in (MKTEMP.match(l) for l in code_lines(text)) if m]
            self.assertEqual(
                len(hits), 1,
                f"{name}: expected exactly one RUN_DIR=$(mktemp -d ...) line",
            )
            self.assertEqual(
                hits[0].group("name"), name,
                f"{name}: run directory is named for a different collector, so "
                f"two wrappers would share a sweep namespace",
            )

    def test_the_cleanup_trap_follows_the_mktemp_immediately(self):
        for path, text in self.each():
            name = os.path.basename(path)[:-3]
            lines = code_lines(text)
            idx = [i for i, l in enumerate(lines) if MKTEMP.match(l)]
            self.assertTrue(idx, f"{name}: no RUN_DIR line")
            nxt = lines[idx[0] + 1].strip()
            self.assertEqual(
                nxt, CLEANUP_TRAP,
                f"{name}: the cleanup trap must be the very next statement, or "
                f"a `set -e` exit before it leaks the run directory",
            )

    def test_a_later_trap_still_cleans_up(self):
        """Wrappers that kill a background cache-sync loop install a second
        trap, which *replaces* the first. It has to carry both actions."""
        for path, text in self.each():
            name = os.path.basename(path)[:-3]
            traps = re.findall(r"trap '([^']*)' EXIT", text)
            for body in traps[1:]:
                self.assertIn(
                    'rm -rf "$RUN_DIR"', body,
                    f"{name}: a later EXIT trap replaces the cleanup trap "
                    f"without cleaning up: trap '{body}'",
                )

    def test_every_wrapper_sweeps_orphans_beyond_the_run_timeout(self):
        """SIGKILL runs no trap, so orphans need a floor -- one that cannot
        collide with a live run."""
        for path, text in self.each():
            name = os.path.basename(path)[:-3]
            hits = [m for m in (SWEEP.match(l.strip()) for l in code_lines(text)) if m]
            self.assertEqual(len(hits), 1, f"{name}: expected exactly one orphan sweep")
            self.assertGreater(
                int(hits[0].group("age")), TIMEOUT_MINUTES,
                f"{name}: the sweep threshold is not longer than the "
                f"{TIMEOUT_MINUTES}-minute run timeout, so it can delete a "
                f"directory a running collector still owns",
            )

    def test_no_ephemeral_tmp_path_escapes_the_run_dir(self):
        """The heart of #69: a fixed /tmp path is shared state."""
        allowed = re.compile(
            r"^(?:/tmp/run-[A-Za-z0-9._-]+\.X{6,}$|/tmp$)"
        )
        for path, text in self.each():
            name = os.path.basename(path)[:-3]
            offenders = []
            for line in code_lines(text):
                if SWEEP.match(line.strip()) or MKTEMP.match(line.strip()):
                    continue
                for ref in re.findall(r"/tmp/[^\s\"';)|&]*", line):
                    if ref.startswith(PERSISTENT_ROOTS):
                        continue
                    if allowed.match(ref):
                        continue
                    offenders.append((line.strip(), ref))
            self.assertEqual(
                offenders, [],
                f"{name}: ephemeral path(s) outside $RUN_DIR -- the scratch "
                f"volume is shared, so these can be another run's bytes: "
                f"{offenders}",
            )

    def test_persistent_roots_are_only_the_allowlisted_caches(self):
        for path, text in self.each():
            name = os.path.basename(path)[:-3]
            for line in code_lines(text):
                for ref in re.findall(r"/tmp/[^\s\"';)|&]*", line):
                    if not ref.startswith(PERSISTENT_ROOTS):
                        continue
                    self.assertRegex(
                        ref, r"^/tmp/cache/[A-Za-z0-9._${}-]+$",
                        f"{name}: {ref} sits under a persistent root but is "
                        f"not a per-collector cache directory",
                    )

    def test_cleanup_never_touches_a_persistent_cache(self):
        """Run cleanup must not be able to cold-start the next run."""
        for path, text in self.each():
            name = os.path.basename(path)[:-3]
            for body in re.findall(r"trap '([^']*)' EXIT", text):
                for ref in re.findall(r"/tmp/[^\s\"';)|&]*", body):
                    self.assertFalse(
                        ref.startswith(PERSISTENT_ROOTS),
                        f"{name}: EXIT trap removes persistent cache state: {ref}",
                    )

    def test_checkpoint_commit_still_reads_the_persistent_cache(self):
        """Guards the acceptance criterion that checkpoint lifecycle is
        unchanged: the commit must target CACHE_DIR, never $RUN_DIR."""
        for path, text in self.each():
            name = os.path.basename(path)[:-3]
            for line in code_lines(text):
                if "checkpoint commit" not in line:
                    continue
                self.assertIn(
                    '--cache-dir "${CACHE_DIR}"', line,
                    f"{name}: checkpoint commit must target the persistent "
                    f"cache: {line.strip()}",
                )
                self.assertNotIn("RUN_DIR", line, f"{name}: {line.strip()}")


if __name__ == "__main__":
    if not os.path.isdir(SCRIPTS):
        sys.exit(f"FATAL: {SCRIPTS} not found -- this suite would pass vacuously")
    unittest.main(verbosity=2)
