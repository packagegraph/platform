#!/usr/bin/env python3
"""Every reader of the published corpus must apply the same rules (#72).

Five programs decide which bytes belong to which named graph, and four of
them are inline shell inside Kubernetes manifests. They have drifted before:
the host rebuild grew a dedup step for duplicate graph URIs and the
Kubernetes readers never did, so the same bucket produced a deduplicated
QLever index and a TDB2 dataset that loaded both copies of 24 graphs.

This is not a style check. Two readers that disagree about which generation
of a graph is current publish two different corpora from one bucket, and
nothing downstream can tell -- both look healthy, both pass their gates.

reader_parity.py holds the single copy, taken from the host script between
its parity markers. Here we assert the Kubernetes readers carry it verbatim,
and that the rules it encodes are actually present rather than merely
identical to each other.
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from reader_parity import (  # noqa: E402
    CLOSE, HOST_CLOSE, HOST_OPEN, HOST_SCRIPT, OPEN, REPO,
    extract, shared_block,
)

READERS = [
    f"deploy/overlays/{overlay}/jobs/{job}.yaml"
    for overlay in ("dev", "prod")
    for job in ("rebuild-qlever-index", "rebuild-tdb2")
]

# Each rule, and a fragment that can only be there if the rule is implemented.
# Checked against the shared block itself, so it cannot be satisfied by a
# comment in one reader that another reader lacks.
RULES = {
    "reads the commit manifest":
        'mc_cat_check "pgraph/${MINIO_BUCKET}/graphs/${slug}/manifest.json"',
    "verifies the payload digest":
        'have_sha=$(sha256sum "$local_payload" | cut -d\' \' -f1)',
    "verifies the payload size":
        'have_size=$(stat -c \'%s\' "$local_payload")',
    "refuses an unknown encoding":
        "declares unknown encoding",
    "refuses two manifests for one graph URI":
        "two manifests claim",
    "drops the legacy copy of a manifest-backed graph":
        "legacy (superseded by manifest)",
    "snapshots the manifests, not just the legacy prefix":
        'mc_list_json "pgraph/${MINIO_BUCKET}/graphs/"',
    "fails rather than treating a listing error as an empty corpus":
        'echo "ERROR: listing $target failed',
    "keeps the legacy sidecar path readable":
        "for graph_file in /tmp/nt-output/*.graph; do",
}


def read(path):
    with open(os.path.join(REPO, path)) as fh:
        return fh.read()


class TestTheSharedBlockIsWhatItClaims(unittest.TestCase):

    def test_the_host_script_still_carries_its_parity_markers(self):
        """Without these, every assertion below passes vacuously."""
        source = read(os.path.relpath(HOST_SCRIPT, REPO))
        for marker in (OPEN, CLOSE, HOST_OPEN, HOST_CLOSE):
            self.assertIn(marker, source, f"{marker!r} was removed")
        self.assertLess(source.index(OPEN), source.index(HOST_OPEN))
        self.assertLess(source.index(HOST_CLOSE), source.index(CLOSE))

    def test_the_host_only_region_is_excluded_from_the_shared_block(self):
        """The early-exit skip reads a marker the Kubernetes jobs never
        write. Copying it there would skip every rebuild."""
        block = extract()
        self.assertNotIn("CORPUS_LISTING_HASH", block)
        self.assertNotIn("advance_corpus_marker", block)
        self.assertIn("CORPUS_LISTING_HASH",
                      read(os.path.relpath(HOST_SCRIPT, REPO)),
                      "the host lost its early-exit skip")

    def test_every_rule_is_present_in_the_shared_block(self):
        block = extract()
        for rule, fragment in RULES.items():
            self.assertIn(fragment, block, f"the shared block no longer {rule}")


class TestTheKubernetesReadersMatch(unittest.TestCase):

    def test_each_reader_embeds_the_generated_block_verbatim(self):
        block = shared_block(18)
        for path in READERS:
            with self.subTest(reader=path):
                self.assertIn(
                    block, read(path),
                    f"{path} has drifted from the host reader. Regenerate it:\n"
                    f"  python3 deploy/quadlet/tests/reader_parity.py 18\n"
                    f"and splice the output in, or fix the host script if the "
                    f"change belongs there.")

    def test_no_reader_still_globs_the_legacy_sidecars_to_pick_winners(self):
        """The dedup exists because the glob returns two payloads for one
        graph URI. Selecting from it directly is the original bug."""
        for path in READERS:
            with self.subTest(reader=path):
                body = read(path)
                self.assertIn("done < /tmp/winning-graphs.txt", body,
                              f"{path} does not consume the winner list")

    def test_no_reader_feeds_a_compressed_payload_to_sed_unread(self):
        """Committed generations are gzip. A reader that sed-substitutes a
        .gz file produces plausible garbage rather than an error."""
        for path in READERS:
            with self.subTest(reader=path):
                self.assertIn("*.gz)", read(path),
                              f"{path} does not branch on compression")

    def test_the_two_overlays_differ_only_in_pinned_versions(self):
        """dev and prod are the same reader; a fix applied to one and not the
        other is the same divergence in a smaller box."""
        for job in ("rebuild-qlever-index", "rebuild-tdb2"):
            with self.subTest(job=job):
                dev = read(f"deploy/overlays/dev/jobs/{job}.yaml").splitlines()
                prod = read(f"deploy/overlays/prod/jobs/{job}.yaml").splitlines()
                self.assertEqual(len(dev), len(prod),
                                 f"{job} overlays have diverged structurally")
                differing = [d for d, p in zip(dev, prod) if d != p]
                self.assertTrue(
                    all("image" in d or "version" in d for d in differing),
                    f"{job} overlays differ in logic, not just pins:\n"
                    + "\n".join(differing))


if __name__ == "__main__":
    unittest.main(verbosity=2)
