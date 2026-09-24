#!/usr/bin/env python3
"""Run two real collector wrappers at once and check what each publishes.

The static contract in test_collector_run_dirs.py checks the shape of every
wrapper. This checks the behaviour the shape is for, on the actual scripts,
by forcing the exact interleaving that issue #69 describes:

    A: collect  -> writes its triples to the output path
    B: collect  -> writes ITS triples to the same path
    A: upload   -> reads the path ... and publishes B's bytes

A barrier holds both collectors inside the window between write and upload
until both have written, so the race is not raced -- it happens every run.

Both real wrappers are executed. Two prefix rewrites make that possible
without touching the host: `/tmp/` becomes a sandbox that stands in for the
shared scratch volume, and `/app/scripts/` becomes a directory of stubs.
test_the_sandbox_rewrite_is_prefix_only proves those rewrites change
nothing else, so a regression cannot hide in the transformation.

Run directly, not via `unittest discover` (#57).
"""

import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPTS = os.path.join(os.path.dirname(HERE), "scripts")

# Two of the fifteen wrappers that shared /tmp/packages.nt, chosen because
# they are the minimal shape: collect to a path, upload that same path.
PAIR = ("arch", "nix")

BARRIER_WAIT_SECONDS = 30

FAKE_PG_COLLECT = r"""#!/bin/sh
# Stand-in for pg-collect. Writes a marker naming this invocation to the
# -o/--output path, then blocks until every participant has written, so the
# collect phases of all participants provably overlap.
set -eu
out=""
prev=""
for arg in "$@"; do
    case "$prev" in
        -o|--output) out="$arg" ;;
    esac
    prev="$arg"
done
[ -n "$out" ] || { echo "fake pg-collect: no -o in: $*" >&2; exit 64; }

printf '<https://pkg/%s> <https://pkg/from> <https://pkg/%s> .\n' \
    "$PG_RUN_ID" "$PG_RUN_ID" > "$out"

touch "$PG_BARRIER_DIR/$PG_RUN_ID.written"
waited=0
while [ "$(ls "$PG_BARRIER_DIR" | grep -c '\.written$')" -lt "$PG_BARRIER_N" ]; do
    sleep 0.05
    waited=$((waited + 1))
    if [ "$waited" -gt __SPINS__ ]; then
        echo "fake pg-collect: barrier timed out" >&2
        exit 75
    fi
done
"""

FAKE_UPLOAD = r"""#!/bin/sh
# Stand-in for upload-nt.sh. Records what this invocation actually read from
# the path its wrapper handed over, keyed by graph URI.
set -eu
src="$1"
graph="$2"
printf '%s\t%s\t%s\n' "$PG_RUN_ID" "$graph" "$(cat "$src")" \
    >> "$PG_PUBLISH_LOG"
"""


def sandboxed(wrapper_name, dest, tmp_root, app_root):
    """Copy a real wrapper, repointing only its /tmp/ and /app/scripts/ prefixes."""
    with open(os.path.join(SCRIPTS, wrapper_name + ".sh")) as fh:
        original = fh.read()
    rewritten = (original
                 .replace("/tmp/", tmp_root.rstrip("/") + "/")
                 .replace("/app/scripts/", app_root.rstrip("/") + "/"))
    with open(dest, "w") as fh:
        fh.write(rewritten)
    os.chmod(dest, 0o755)
    return original, rewritten


class ConcurrencyHarness(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="collector-concurrency-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.shared_tmp = os.path.join(self.tmp, "shared-scratch")
        self.app = os.path.join(self.tmp, "app-scripts")
        self.bin = os.path.join(self.tmp, "bin")
        self.barrier = os.path.join(self.tmp, "barrier")
        for d in (self.shared_tmp, self.app, self.bin, self.barrier):
            os.makedirs(d)
        self.publish_log = os.path.join(self.tmp, "published.tsv")

        spins = int(BARRIER_WAIT_SECONDS / 0.05)
        # str.replace, not %-formatting: the stub's own printf
        # format strings are full of %s.
        self.write_exe(os.path.join(self.bin, "pg-collect"),
                       FAKE_PG_COLLECT.replace("__SPINS__", str(spins)))
        self.write_exe(os.path.join(self.app, "upload-nt.sh"), FAKE_UPLOAD)

    @staticmethod
    def write_exe(path, body):
        with open(path, "w") as fh:
            fh.write(body)
        os.chmod(path, 0o755)

    def stage(self, wrapper_name, run_id):
        dest = os.path.join(self.tmp, f"{run_id}.sh")
        sandboxed(wrapper_name, dest, self.shared_tmp, self.app)
        return dest

    def run_concurrently(self, jobs):
        """jobs: [(run_id, script_path)]. Returns {run_id: CompletedProcess}."""
        results = {}

        def go(run_id, script):
            env = dict(os.environ)
            env.update({
                "PATH": self.bin + os.pathsep + env["PATH"],
                "PG_RUN_ID": run_id,
                "PG_BARRIER_DIR": self.barrier,
                "PG_BARRIER_N": str(len(jobs)),
                "PG_PUBLISH_LOG": self.publish_log,
            })
            results[run_id] = subprocess.run(
                ["sh", script], env=env, capture_output=True, text=True,
                timeout=BARRIER_WAIT_SECONDS * 3,
            )

        threads = [threading.Thread(target=go, args=j) for j in jobs]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        return results

    def published(self):
        """[(run_id, graph_uri, bytes_read)] in publication order."""
        if not os.path.isfile(self.publish_log):
            return []
        out = []
        with open(self.publish_log) as fh:
            for line in fh:
                if line.strip():
                    out.append(tuple(line.rstrip("\n").split("\t", 2)))
        return out


class TestHarnessIsHonest(ConcurrencyHarness):
    def test_the_sandbox_rewrite_is_prefix_only(self):
        for name in PAIR:
            with self.subTest(wrapper=name):
                dest = os.path.join(self.tmp, name + ".check.sh")
                original, rewritten = sandboxed(name, dest, self.shared_tmp, self.app)
                self.assertNotEqual(original, rewritten, "nothing was rewritten")
                undone = (rewritten
                          .replace(self.shared_tmp.rstrip("/") + "/", "/tmp/")
                          .replace(self.app.rstrip("/") + "/", "/app/scripts/"))
                self.assertEqual(
                    undone, original,
                    f"{name}: the sandbox changed something other than the "
                    f"/tmp/ and /app/scripts/ prefixes",
                )

    def test_the_barrier_actually_overlaps_the_collectors(self):
        """If the barrier did not hold, the race would not be forced and
        every test below would pass by luck."""
        jobs = [(PAIR[0], self.stage(PAIR[0], PAIR[0])),
                (PAIR[1], self.stage(PAIR[1], PAIR[1]))]
        self.run_concurrently(jobs)
        written = sorted(f for f in os.listdir(self.barrier) if f.endswith(".written"))
        self.assertEqual(
            written, sorted(f"{r}.written" for r, _ in jobs),
            "both collectors must have written before either was released",
        )


class TestConcurrentCollectorsDoNotCross(ConcurrencyHarness):
    def test_two_formerly_colliding_collectors_each_publish_their_own_bytes(self):
        jobs = [(PAIR[0], self.stage(PAIR[0], PAIR[0])),
                (PAIR[1], self.stage(PAIR[1], PAIR[1]))]
        procs = self.run_concurrently(jobs)
        for run_id, proc in procs.items():
            self.assertEqual(proc.returncode, 0,
                             f"{run_id} failed:\n{proc.stdout}\n{proc.stderr}")

        rows = self.published()
        self.assertEqual(len(rows), 2, f"expected two publications, got {rows}")

        graphs = {}
        for run_id, graph, payload in rows:
            self.assertIn(
                run_id, payload,
                f"{run_id} published bytes it did not produce -- payload "
                f"{payload!r} under graph {graph}. This is #69: a fixed "
                f"scratch path let another collector's output be uploaded "
                f"under this collector's graph URI.",
            )
            for other, _ in jobs:
                if other != run_id:
                    self.assertNotIn(other, payload,
                                     f"{run_id}'s upload carries {other}'s bytes")
            graphs[graph] = run_id
        self.assertEqual(len(graphs), 2, f"the two runs shared a graph URI: {graphs}")

    def test_two_invocations_of_one_wrapper_do_not_cross(self):
        """A fixed per-collector directory would fix the cross-collector case
        and leave this one, so it is checked separately."""
        name = PAIR[0]
        jobs = [("run-a", self.stage(name, "run-a")),
                ("run-b", self.stage(name, "run-b"))]
        procs = self.run_concurrently(jobs)
        for run_id, proc in procs.items():
            self.assertEqual(proc.returncode, 0,
                             f"{run_id} failed:\n{proc.stdout}\n{proc.stderr}")

        rows = self.published()
        self.assertEqual(len(rows), 2, f"expected two publications, got {rows}")
        for run_id, _graph, payload in rows:
            self.assertIn(
                run_id, payload,
                f"{run_id} published another invocation's bytes: {payload!r}. "
                f"Two concurrent runs of {name} shared a scratch path.",
            )

    def test_each_run_dir_is_removed_afterwards(self):
        """Cleanup must actually run, or the shared volume fills up."""
        jobs = [(PAIR[0], self.stage(PAIR[0], PAIR[0])),
                (PAIR[1], self.stage(PAIR[1], PAIR[1]))]
        self.run_concurrently(jobs)
        leftovers = [e for e in os.listdir(self.shared_tmp) if e.startswith("run-")]
        self.assertEqual(leftovers, [],
                         f"run directories survived their wrappers: {leftovers}")


if __name__ == "__main__":
    missing = [n for n in PAIR if not os.path.isfile(os.path.join(SCRIPTS, n + ".sh"))]
    if missing:
        sys.exit(f"FATAL: wrapper(s) not found: {missing} -- this suite would "
                 f"pass vacuously")
    unittest.main(verbosity=2)
