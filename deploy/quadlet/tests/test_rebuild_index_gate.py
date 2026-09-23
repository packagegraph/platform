"""The graph identity gate in qlever-rebuild-index.sh must compare SETS.

`comm` is a line-by-line multiset merge, not a set-membership test. A
duplicate graph URI in last-success.json's `graphs[]` with no matching
duplicate in the current run makes `comm -23` report the extra copy as
missing, the gate `exit 1`s, and the index is never promoted.

That deadlocks: last-success.json is only rewritten *after* a successful
promotion, and the gate runs *before* it, so a bad baseline blocks the very
run that would replace it. See issue #64.

These tests run the real block extracted from the real script, so they fail
if someone drops the `-u` rather than passing against a copy.
"""
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE.parent / "scripts" / "qlever-rebuild-index.sh"


def gate_block() -> str:
    """Pull the three comparison lines out of the live script."""
    src = SCRIPT.read_text()
    m = re.search(
        r"^(\s*PREV_GRAPHS=.*\n\s*CURR_GRAPHS=.*\n\s*MISSING=.*)$",
        src,
        re.MULTILINE,
    )
    assert m, f"gate block not found in {SCRIPT} -- did it move or get rewritten?"
    return m.group(1)


def run_gate(prev_graphs, curr_graphs):
    """Return the gate's MISSING output for a given baseline and current run."""
    with tempfile.TemporaryDirectory() as d:
        uris = Path(d) / "graph-uris.txt"
        uris.write_text("".join(g + "\n" for g in curr_graphs))
        prev_run = '{"graphs":[' + ",".join(f'"{g}"' for g in prev_graphs) + "]}"
        # The script reads a fixed path; point it at the fixture instead.
        block = gate_block().replace("/tmp/graph-uris.txt", str(uris))
        script = f"set -euo pipefail\nPREV_RUN={prev_run!r}\n{block}\nprintf '%s' \"$MISSING\"\n"
        r = subprocess.run(
            ["bash", "-c", script], capture_output=True, text=True, check=True
        )
        return r.stdout.strip()


class RebuildIndexGateTest(unittest.TestCase):
    def test_a_duplicated_baseline_entry_is_not_reported_missing(self):
        """The deadlock case: the graph is present, the baseline counted it twice."""
        missing = run_gate(
            prev_graphs=["graph/debian/trixie", "graph/fedora/42", "graph/fedora/42"],
            curr_graphs=["graph/debian/trixie", "graph/fedora/42"],
        )
        self.assertEqual(
            missing, "", "a duplicated baseline entry must not read as data loss"
        )

    def test_a_genuinely_missing_graph_is_still_caught(self):
        """The gate must keep doing its actual job."""
        missing = run_gate(
            prev_graphs=["graph/debian/trixie", "graph/fedora/42"],
            curr_graphs=["graph/fedora/42"],
        )
        self.assertEqual(missing, "graph/debian/trixie")

    def test_a_duplicate_does_not_mask_a_real_loss(self):
        """Dedup must not swallow a graph that really did disappear."""
        missing = run_gate(
            prev_graphs=["graph/fedora/42", "graph/fedora/42", "graph/debian/trixie"],
            curr_graphs=["graph/fedora/42"],
        )
        self.assertEqual(missing, "graph/debian/trixie")

    def test_a_new_graph_appearing_is_not_an_error(self):
        """comm -23 is one-directional by design: additions are fine."""
        missing = run_gate(
            prev_graphs=["graph/fedora/42"],
            curr_graphs=["graph/fedora/42", "graph/fedora/43"],
        )
        self.assertEqual(missing, "")

    def test_the_persisted_baseline_is_written_as_a_set(self):
        """Write-side guard: no future run may persist a multiset baseline.

        Without this, the read-side `sort -u` only papers over a baseline the
        script itself is still capable of creating.
        """
        src = SCRIPT.read_text()
        m = re.search(r"^GRAPHS_JSON=.*$", src, re.MULTILINE)
        self.assertIsNotNone(m, "GRAPHS_JSON assignment not found")
        self.assertIn(
            "sort -u",
            m.group(0),
            "graphs[] must be deduped before being persisted as the next baseline",
        )


if __name__ == "__main__":
    unittest.main()
