#!/usr/bin/env python3
"""The rebuild must read graphs through their commit manifests (#72).

Drives the real qlever-rebuild-index.sh against a local fake object store,
reusing the harness in test_rebuild_retry_and_preservation.py so both suites
exercise the same script the same way.

Three properties, in descending order of how badly they fail:

1. **A graph is manifest-backed or legacy, never both.** The old stable-key
   scheme let one graph URI have two discoverable payloads, and the rebuild
   indexed both into the same named graph -- measured live on 2026-09-11 as 24
   graphs and ~10GB of duplicate quads. So the assertions here are on the
   N-Quads the script actually produced, not on its log.

2. **Payload bytes are verified against the manifest.** Nothing verified
   anything before. A truncated or swapped object was simply indexed.

3. **"The listing failed" and "nothing is published" must not look alike.**
   Treating a transport error as an empty manifest set would fall back to the
   legacy copy of every graph and rebuild a stale corpus while reporting
   success -- the worst outcome available, because it is silent.

Legacy-only corpora must keep building unchanged throughout; that is what
test_rebuild_retry_and_preservation.py covers, plus
`test_a_legacy_only_corpus_of_both_formats_still_builds` here.
"""

import gzip
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from test_rebuild_retry_and_preservation import (  # noqa: E402
    BUCKET, MARKER, RebuildHarness,
)

GRAPH_BASE = "https://packagegraph.github.io/graph"
LEGACY_GRAPHS = 8
MANIFEST_GRAPHS = 4


def body_for(tag, count=50):
    return "".join(
        f"<https://pkg/{tag}/{n}> <https://pkg/p> <https://pkg/o{n}> .\n"
        for n in range(count)
    )


class ManifestHarness(RebuildHarness):
    """Twelve graphs: eight legacy pairs and four manifest-backed."""

    def seed_corpus(self):
        self.uris = []

        for i in range(LEGACY_GRAPHS):
            uri = f"{GRAPH_BASE}/L{i}"
            self.uris.append(uri)
            self.put(f"nt-output/L{i}.nt", body_for(f"L{i}"))
            self.put(f"nt-output/L{i}.nt.graph", uri + "\n")

        for i in range(MANIFEST_GRAPHS):
            uri = f"{GRAPH_BASE}/M{i}"
            self.uris.append(uri)
            self.publish(f"M{i}", uri, body_for(f"M{i}"))

        self.put("qlever-index/last-success.json", json.dumps({
            "status": "success",
            "timestamp": "2026-09-01T00:00:00+00:00",
            "content_hash": "0000000000000000",
            "triple_count": 500,
            "index_size": "1M",
            "graphs": self.uris,
        }))
        self.put("qlever-index/latest", "0000000000000000")

    # ---- fixture construction -------------------------------------------

    def publish(self, slug, uri, body, encoding="gzip", **override):
        """Commit a generation the way upload-nt.sh does, then return it."""
        if encoding == "gzip":
            payload = gzip.compress(body.encode(), mtime=0)
            ext = ".nt.gz"
        else:
            payload = body.encode()
            ext = ".nt"
        digest = hashlib.sha256(payload).hexdigest()
        generation = f"{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{digest[:12]}"
        key = f"graphs/{slug}/generations/{generation}{ext}"
        self.put(key, payload)
        manifest = {
            "schema": 1,
            "graph": uri,
            "generation": generation,
            "key": key,
            "encoding": encoding,
            "size_bytes": len(payload),
            "sha256": digest,
            "data_triples": body.count("\n"),
            "committed_at": "2026-09-24T00:00:00Z",
        }
        manifest.update(override)
        self.put(f"graphs/{slug}/manifest.json", json.dumps(manifest))
        return manifest

    def amend(self, slug, **fields):
        path = os.path.join(self.store, BUCKET, "graphs", slug, "manifest.json")
        with open(path) as fh:
            manifest = json.load(fh)
        for key, value in fields.items():
            if value is None:
                manifest.pop(key, None)
            else:
                manifest[key] = value
        with open(path, "w") as fh:
            json.dump(manifest, fh)
        return manifest

    # ---- inspection -------------------------------------------------------

    def quads(self):
        """The N-Quads this run actually handed to the indexer."""
        path = os.path.join(self.scratch, "packagegraph.nq")
        if not os.path.isfile(path):
            return []
        with open(path) as fh:
            return [line for line in fh.read().splitlines() if line.strip()]

    def quads_in(self, uri):
        return [q for q in self.quads() if q.endswith(f"<{uri}> .")]

    def payload_dir(self, slug):
        return os.path.join(self.scratch, "graph-payloads", slug)

    def assertRefused(self, proc, pattern):
        self.assertNotEqual(
            proc.returncode, 0,
            "the rebuild accepted a corpus it cannot vouch for:\n" + proc.stdout)
        self.assertRegex(proc.stdout + proc.stderr, pattern)
        self.assertEqual(self.qlever_invocations(), 0,
                         "the indexer ran on an unverified corpus")
        self.assertMarkerAbsent(proc, "a refused corpus")


class TestManifestBackedGraphsAreIndexed(ManifestHarness):

    def test_a_mixed_corpus_promotes_and_indexes_every_graph(self):
        proc = self.run_rebuild()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("promoted to latest", proc.stdout)
        self.assertIn(f"{MANIFEST_GRAPHS} manifest-backed", proc.stdout)
        for uri in self.uris:
            self.assertTrue(self.quads_in(uri), f"<{uri}> was not indexed")

    def test_a_manifest_payload_is_verified_before_use(self):
        proc = self.run_rebuild()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertRegex(proc.stdout, r"manifest: M0 → <.*M0> \(.*verified\)")

    def test_an_uncompressed_generation_is_read_as_plain_ntriples(self):
        self.publish("M0", f"{GRAPH_BASE}/M0", body_for("M0-plain"),
                     encoding="none")
        proc = self.run_rebuild()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("<https://pkg/M0-plain/0> <https://pkg/p> "
                      f"<https://pkg/o0> <{GRAPH_BASE}/M0> .", self.quads())

    def test_an_uncommitted_generation_is_never_read(self):
        """Orphans are expected -- a failed manifest write leaves one behind
        every time. They must be inert, not merely unused."""
        self.put("graphs/M0/generations/99999999T999999Z-deadbeefcafe.nt.gz",
                 gzip.compress(b"<https://pkg/orphan> <https://pkg/p> "
                               b"<https://pkg/o> .\n", mtime=0))
        proc = self.run_rebuild()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertFalse([q for q in self.quads() if "orphan" in q],
                         "an uncommitted generation was indexed")


class TestOneGenerationPerGraph(ManifestHarness):
    """The union bug is the whole reason #72 exists."""

    def test_a_manifest_supersedes_the_legacy_copy_of_the_same_uri(self):
        # Exactly the shape the format migration left behind: every
        # manifest-backed graph ALSO has a legacy pair under the old stable
        # key, still carrying its own valid sidecar. All four are seeded so
        # this corpus clears the graph floor with the legacy copies alone --
        # a reader that ignores manifests entirely builds happily from them,
        # which is what makes the assertion below discriminating rather than
        # an accident of the fixture being too small to build.
        for i in range(MANIFEST_GRAPHS):
            self.put(f"nt-output/M{i}.nt", body_for(f"M{i}-legacy"))
            self.put(f"nt-output/M{i}.nt.graph", f"{GRAPH_BASE}/M{i}\n")

        proc = self.run_rebuild()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("legacy (superseded by manifest): M0.nt", proc.stdout)

        for i in range(MANIFEST_GRAPHS):
            uri = f"{GRAPH_BASE}/M{i}"
            indexed = self.quads_in(uri)
            self.assertTrue(indexed, f"<{uri}> vanished")
            self.assertFalse(
                [q for q in indexed if f"M{i}-legacy" in q],
                f"<{uri}> was unioned with its legacy copy: two generations "
                f"of one named graph, which is the whole of #72")
            self.assertTrue([q for q in indexed if f"/M{i}/" in q],
                            f"<{uri}> lost its committed generation")

    def test_a_legacy_only_graph_is_still_read_from_its_sidecar(self):
        proc = self.run_rebuild()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertTrue(self.quads_in(f"{GRAPH_BASE}/L0"))

    def test_a_legacy_only_corpus_of_both_formats_still_builds(self):
        """No graphs/ prefix at all, raw .nt and .nt.gz side by side --
        the corpus as it exists today, which must keep building."""
        shutil.rmtree(os.path.join(self.store, BUCKET, "graphs"))
        for i in range(MANIFEST_GRAPHS):
            uri = f"{GRAPH_BASE}/M{i}"
            if i % 2:
                self.put(f"nt-output/M{i}.nt.gz",
                         gzip.compress(body_for(f"M{i}").encode(), mtime=0))
                self.put(f"nt-output/M{i}.nt.gz.graph", uri + "\n")
            else:
                self.put(f"nt-output/M{i}.nt", body_for(f"M{i}"))
                self.put(f"nt-output/M{i}.nt.graph", uri + "\n")

        proc = self.run_rebuild()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("0 manifest-backed", proc.stdout)
        for uri in self.uris:
            self.assertTrue(self.quads_in(uri), f"<{uri}> was not indexed")

    def test_two_manifests_claiming_one_graph_uri_are_refused(self):
        """Slug is a display name; the graph URI is the identity. Two
        manifests for one URI means there is no answer to "which generation
        is current", and guessing is how the mtime heuristic got here."""
        self.publish("M0-alias", f"{GRAPH_BASE}/M0", body_for("M0-alias"))
        self.assertRefused(self.run_rebuild(), r"two manifests claim")


class TestPayloadsAreVerified(ManifestHarness):

    def test_a_digest_mismatch_is_refused(self):
        """Same length, different bytes: only the digest catches this."""
        manifest = self.amend("M0")
        self.put(manifest["key"], gzip.compress(
            body_for("M0").replace("https://pkg/M0/0", "https://pkg/M0/X").encode(),
            mtime=0))
        self.amend("M0", size_bytes=os.path.getsize(
            os.path.join(self.store, BUCKET, manifest["key"])))
        self.assertRefused(self.run_rebuild(), r"digest .* does not match")

    def test_a_digest_mismatch_does_not_leave_the_bad_copy_cached(self):
        """Otherwise every retry fails identically without re-fetching, and
        the rebuild is wedged until someone clears the volume by hand."""
        manifest = self.amend("M0")
        self.put(manifest["key"], gzip.compress(b"wrong bytes entirely\n", mtime=0))
        self.amend("M0", size_bytes=os.path.getsize(
            os.path.join(self.store, BUCKET, manifest["key"])))
        self.run_rebuild()
        cached = os.listdir(self.payload_dir("M0"))
        self.assertFalse([f for f in cached if not f.endswith(".graph")],
                         f"a rejected payload stayed cached: {cached}")

    def test_a_size_mismatch_is_refused(self):
        self.amend("M0", size_bytes=999999)
        self.assertRefused(self.run_rebuild(), r"bytes, manifest says")

    def test_a_missing_generation_is_refused(self):
        manifest = self.amend("M0")
        os.remove(os.path.join(self.store, BUCKET, manifest["key"]))
        self.assertRefused(self.run_rebuild(), r"(?i)nosuchkey|unable to read")

    def test_a_superseded_generation_is_pruned_from_local_scratch(self):
        """Generations are immutable and never deleted upstream, so the
        scratch volume has to be the thing that stays bounded."""
        first = self.run_rebuild()
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        old = [f for f in os.listdir(self.payload_dir("M0"))
               if not f.endswith(".graph")]
        self.assertEqual(len(old), 1, old)

        time.sleep(1)  # the generation name is second-resolution
        self.publish("M0", f"{GRAPH_BASE}/M0", body_for("M0", count=60))
        second = self.run_rebuild()
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)

        kept = [f for f in os.listdir(self.payload_dir("M0"))
                if not f.endswith(".graph")]
        self.assertEqual(len(kept), 1, f"superseded generations accumulated: {kept}")
        self.assertNotEqual(kept, old, "the new generation was not fetched")


class TestMalformedManifests(ManifestHarness):

    def test_an_unparseable_manifest_is_refused(self):
        self.put("graphs/M0/manifest.json", "{not json at all")
        self.assertRefused(self.run_rebuild(), r"not valid JSON")

    def test_a_manifest_missing_its_graph_uri_is_refused(self):
        self.amend("M0", graph=None)
        self.assertRefused(self.run_rebuild(), r"missing required field 'graph'")

    def test_a_manifest_missing_its_digest_is_refused(self):
        self.amend("M0", sha256=None)
        self.assertRefused(self.run_rebuild(), r"missing required field 'sha256'")

    def test_an_unknown_encoding_is_refused_rather_than_guessed(self):
        self.amend("M0", encoding="zstd")
        self.assertRefused(self.run_rebuild(), r"unknown encoding 'zstd'")

    def test_a_manifest_may_not_vouch_for_another_graphs_generation(self):
        """The key and the digest it is checked against both come from the
        same manifest, so they will agree happily about the wrong object.
        Only the key's scope can catch it."""
        other = self.amend("M1")
        self.amend("M0", key=other["key"], generation=other["generation"],
                   size_bytes=other["size_bytes"], sha256=other["sha256"])
        proc = self.run_rebuild()
        self.assertRefused(proc, r"points outside its own generations")
        self.assertFalse([q for q in self.quads() if "M1" in q
                          and q.endswith(f"<{GRAPH_BASE}/M0> .")],
                         "one graph's bytes were published under another's URI")

    def test_an_unusable_generation_name_is_refused(self):
        """It becomes a local filename; a bucket key is not a promise."""
        self.amend("M0", generation="../../escape")
        self.assertRefused(self.run_rebuild(), r"(?i)unusable generation name|points outside")


class TestConcurrencyAndTransport(ManifestHarness):

    def test_a_manifest_committed_during_the_download_is_detected(self):
        """A collector publishing while the rebuild reads its corpus. The
        before/after listing has to cover graphs/ as well as nt-output/, and
        it has to include .json keys there -- the manifests ARE the race."""
        store = os.path.join(self.store, BUCKET)
        hook = (
            f"mkdir -p {store}/graphs/M9/generations && "
            f"printf '%s' '{{\"schema\":1,\"graph\":\"{GRAPH_BASE}/M9\"}}' "
            f"> {store}/graphs/M9/manifest.json"
        )
        proc = self.run_rebuild(FAKE_MC_HOOK=hook)
        self.assertNotEqual(proc.returncode, 0,
                            "a mid-download commit went unnoticed:\n" + proc.stdout)
        self.assertIn("changed during download", proc.stdout)
        self.assertMarkerAbsent(proc, "a run that raced a publisher")

    def test_a_failed_listing_is_fatal_not_an_empty_manifest_set(self):
        """The silent-staleness case. If a transport error reads as "no
        manifests", every manifest-backed graph falls back to whatever legacy
        object happens to still be there and the rebuild reports success."""
        self.put("nt-output/M0.nt", body_for("M0-legacy"))
        self.put("nt-output/M0.nt.graph", f"{GRAPH_BASE}/M0\n")

        proc = self.run_rebuild(FAKE_MC_FAIL="graphs/")
        self.assertNotEqual(proc.returncode, 0,
                            "a listing failure was read as an empty corpus:\n"
                            + proc.stdout)
        self.assertRegex(proc.stdout + proc.stderr, r"listing .* failed")
        self.assertFalse([q for q in self.quads() if "M0-legacy" in q],
                         "a stale legacy copy was indexed in place of a manifest")
        self.assertMarkerAbsent(proc, "a run whose corpus could not be listed")

    def test_an_unchanged_corpus_still_skips_with_manifests_present(self):
        """The early-exit fingerprint now spans both prefixes; it must still
        recognise an unchanged corpus, or every night is a full rebuild."""
        first = self.run_rebuild()
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.assertIsNotNone(self.get(MARKER))

        second = self.run_rebuild()
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertIn("corpus unchanged", second.stdout)

    def test_a_new_manifest_alone_defeats_the_skip(self):
        """The mirror image: a commit under graphs/ changes nothing under
        nt-output/, so a fingerprint that only watched the old prefix would
        skip the rebuild that publishes it."""
        first = self.run_rebuild()
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)

        time.sleep(1)
        self.publish("M0", f"{GRAPH_BASE}/M0", body_for("M0", count=70))
        second = self.run_rebuild()
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertNotIn("corpus unchanged", second.stdout)
        self.assertEqual(self.qlever_invocations(), 2,
                         "a newly committed manifest did not trigger a rebuild")


if __name__ == "__main__":
    unittest.main(verbosity=2)
