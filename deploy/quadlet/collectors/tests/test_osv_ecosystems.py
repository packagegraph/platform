#!/usr/bin/env python3
"""Static contract: the OSV job asks for ecosystems OSV actually publishes.

Issue #97 measured what happens when it does not. `security.sh` looped
eleven names through the OSV API and four of them -- `deb`, `apk`, `rpm`,
`fedora` -- are not OSV ecosystems at all. Every request they made was
answered HTTP 400, for months, and the only trace was a warning per batch.
The RPM corpus got nothing from any of it.

The bulk collector fails louder -- an unknown name is a 404 on
`{ecosystem}/all.zip`, `pg-collect` exits 1, and `set -eu` takes the whole
job down -- but loud on the next scheduled run is still later than loud in
CI, and taking down a job that collects sixteen working ecosystems to
report the seventeenth's typo is not a good trade.

So two properties, checked statically because both failures are about a
string nobody reads:

  * the RPM-side ecosystems OSV does publish, and this corpus collects
    packages for, are in the collector's list;
  * the four names #97 found are not passed as `--ecosystem` by anything.

Static analysis is the right shape: these scripts run in a container with
`/app/scripts` on it, and the thing under test is which strings the list
holds, not what the network says about them.

Run directly (`python3 .../test_osv_ecosystems.py -v`), not via
`unittest discover` -- discover exits 0 when it matches nothing, which would
turn a renamed file into a vacuous pass (#57).
"""

import glob
import os
import re
import shlex
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
COLLECTORS = os.path.join(os.path.dirname(HERE), "scripts")
ENRICHERS = os.path.normpath(
    os.path.join(os.path.dirname(HERE), "..", "enrichers", "scripts")
)
OSV_SH = os.path.join(COLLECTORS, "osv.sh")

# OSV publishes one archive per distro, not one per release: unlike
# `Debian:13` and `Alpine:v3.20`, these three have no release-qualified
# prefixes in the bucket. The release is inside each record instead
# (`AlmaLinux:9`, `Rocky Linux:10`, `Red Hat:enterprise_linux:9::appstream`),
# so the bare name is the whole archive -- spaces and all.
#
# Verified against the live archives on 2026-09-24: AlmaLinux 5,935
# advisories, Rocky Linux 4,276, Red Hat 23,285.
RPM_ECOSYSTEMS = ("AlmaLinux", "Rocky Linux", "Red Hat")

# The corpus graphs each one covers. Named here so that dropping the last
# Rocky collector and leaving `Rocky Linux` in the list reads as the
# contradiction it is.
COLLECTED_FOR = {
    "AlmaLinux": ("alma-9-full.sh", "alma-10-full.sh"),
    "Rocky Linux": ("rocky-9-full.sh", "rocky-10-full.sh"),
    "Red Hat": ("rhel-9-full.sh", "rhel-10-full.sh"),
}

# #97's table, verbatim: every one of these was answered HTTP 400.
DEAD_NAMES = ("deb", "apk", "rpm", "fedora")

FOR_SPEC = re.compile(r"^for spec in (?P<specs>.*); do$", re.MULTILINE)
ECOSYSTEM_ARG = re.compile(r"--ecosystem\s+(?P<value>\"[^\"]*\"|'[^']*'|\S+)")
ANY_FOR_LIST = re.compile(r"^\s*for\s+\w+\s+in\s+(?P<words>.*?);?\s*do\s*$", re.MULTILINE)


def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def code_lines(text):
    """Lines with comments and blanks dropped.

    The comments legitimately name `fedora` and `rpm` while explaining why
    they are absent; flagging them would push the next author into deleting
    the explanation.
    """
    out = []
    for line in text.split("\n"):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        out.append(line)
    return out


def osv_specs():
    """The `eco:slug` pairs osv.sh loops over, as (ecosystem, slug)."""
    body = "\n".join(code_lines(read(OSV_SH)))
    match = FOR_SPEC.search(body)
    if match is None:
        raise AssertionError(
            "osv.sh has no `for spec in ...; do` line -- the loop this test "
            "reads was renamed or restructured"
        )
    pairs = []
    for spec in shlex.split(match.group("specs")):
        # `${spec%%:*}` / `${spec#*:}`: split on the FIRST colon, as the
        # script does. None of these ecosystem names contain one.
        ecosystem, _, slug = spec.partition(":")
        pairs.append((ecosystem, slug))
    return pairs


def ecosystem_arguments(directory):
    """Every ecosystem name a script under `directory` can pass to OSV.

    Yields (path, value), for scripts that mention `--ecosystem` at all.

    The literal form is the easy half. The half that matters is the one
    #97 actually found: both the old `security.sh` and today's `osv.sh`
    pass `--ecosystem "$VAR"` and hold the real names in a `for` list
    above it, so a checker that only reads the argument sees `$ECO` and
    passes on a script requesting `deb`, `apk`, `rpm` and `fedora`.

    So every word of every `for ... in` list in such a script counts as a
    candidate, up to the first colon (`osv.sh` packs `ecosystem:slug`).
    """
    for path in sorted(glob.glob(os.path.join(directory, "*.sh"))):
        body = "\n".join(code_lines(read(path)))
        if "--ecosystem" not in body:
            continue
        for match in ECOSYSTEM_ARG.finditer(body):
            value = match.group("value").strip("\"'")
            if "$" not in value:
                yield path, value
        for match in ANY_FOR_LIST.finditer(body):
            for word in shlex.split(match.group("words")):
                if "$" in word:
                    continue
                yield path, word.partition(":")[0]


class TestFixtureIsNotVacuous(unittest.TestCase):
    """Without these the whole suite passes on a moved or empty file."""

    def test_osv_sh_exists(self):
        self.assertTrue(os.path.isfile(OSV_SH), f"{OSV_SH} is missing")

    def test_the_loop_list_parses(self):
        specs = osv_specs()
        self.assertGreaterEqual(
            len(specs), 15,
            "found suspiciously few ecosystems -- did the loop change shape?",
        )
        for ecosystem, slug in specs:
            self.assertTrue(ecosystem, f"empty ecosystem in {specs}")
            self.assertTrue(slug, f"{ecosystem} has no output slug")

    def test_enricher_scripts_are_found(self):
        self.assertTrue(os.path.isdir(ENRICHERS), f"{ENRICHERS} is missing")


class TestRpmEcosystemsAreCollected(unittest.TestCase):
    """#97: the RPM corpus got no OSV data, and three archives were waiting."""

    def test_each_rpm_ecosystem_is_in_the_loop(self):
        listed = [ecosystem for ecosystem, _ in osv_specs()]
        for ecosystem in RPM_ECOSYSTEMS:
            self.assertIn(
                ecosystem, listed,
                f"{ecosystem!r} is an OSV ecosystem this corpus collects "
                f"packages for ({', '.join(COLLECTED_FOR[ecosystem])}), but "
                f"osv.sh does not pull its archive (#97)",
            )

    def test_spelled_exactly_as_osv_names_them(self):
        """`RockyLinux` or `redhat` is a 404, not a near miss."""
        listed = [ecosystem for ecosystem, _ in osv_specs()]
        squashed = {e.replace(" ", "").lower(): e for e in listed}
        for ecosystem in RPM_ECOSYSTEMS:
            key = ecosystem.replace(" ", "").lower()
            self.assertEqual(
                squashed.get(key), ecosystem,
                f"osv.sh spells it {squashed.get(key)!r}; OSV publishes "
                f"{ecosystem}/all.zip",
            )

    def test_the_collectors_that_justify_them_still_exist(self):
        for ecosystem, wrappers in COLLECTED_FOR.items():
            for wrapper in wrappers:
                self.assertTrue(
                    os.path.isfile(os.path.join(COLLECTORS, wrapper)),
                    f"{wrapper} is gone, so pulling the {ecosystem} archive "
                    f"no longer has a corpus to be about",
                )

    def test_slugs_are_distinct(self):
        """Two ecosystems sharing a slug would overwrite each other's file."""
        slugs = [slug for _, slug in osv_specs()]
        self.assertEqual(
            len(slugs), len(set(slugs)),
            f"duplicate output slug in osv.sh: {sorted(slugs)}",
        )


class TestDeadEcosystemNames(unittest.TestCase):
    """#97's four HTTP 400s, locked out of both script trees.

    Removed by #99 when the security enricher stopped calling the API.
    Nothing but this stops them coming back -- they are plausible-looking
    packaging-system names, and three of them are what this repo calls those
    packaging systems everywhere else.
    """

    def test_no_script_passes_a_dead_name(self):
        for directory in (COLLECTORS, ENRICHERS):
            for path, value in ecosystem_arguments(directory):
                self.assertNotIn(
                    value, DEAD_NAMES,
                    f"{os.path.basename(path)} passes --ecosystem {value!r}, "
                    f"which OSV answers HTTP 400 (#97)",
                )

    def test_no_dead_name_in_the_osv_loop(self):
        listed = [ecosystem for ecosystem, _ in osv_specs()]
        for dead in DEAD_NAMES:
            self.assertNotIn(
                dead, listed,
                f"osv.sh loops {dead!r}, which is not an OSV ecosystem -- "
                f"{dead}/all.zip is a 404 and takes the whole job with it (#97)",
            )


class TestOneGraphOneUpload(unittest.TestCase):
    """The shared-GRAPH_URI hazard, re-checked because the list just grew.

    upload-nt.sh derives its object key from the graph URI, so calling it
    once per ecosystem against one GRAPH_URI has each upload silently
    replace the last -- seventeen ecosystems collected, one published. The
    accumulate-then-upload-once shape is what prevents it, and it has to
    survive every future addition to the loop.
    """

    def test_upload_is_called_exactly_once(self):
        body = "\n".join(code_lines(read(OSV_SH)))
        calls = [line for line in body.split("\n") if "upload-nt.sh" in line]
        self.assertEqual(
            len(calls), 1,
            f"osv.sh calls upload-nt.sh {len(calls)} times; all its "
            f"ecosystems share one GRAPH_URI, so all but the last would be "
            f"overwritten: {calls}",
        )

    def test_upload_is_after_the_loop_and_takes_the_combined_file(self):
        lines = code_lines(read(OSV_SH))
        upload_at = next(i for i, l in enumerate(lines) if "upload-nt.sh" in l)
        done_at = max(i for i, l in enumerate(lines) if l.strip() == "done")
        self.assertGreater(
            upload_at, done_at,
            "upload-nt.sh runs inside the ecosystem loop",
        )
        self.assertIn(
            "$COMBINED", lines[upload_at],
            "upload-nt.sh is not given the accumulated file",
        )

    def test_every_ecosystem_appends_to_the_combined_file(self):
        body = "\n".join(code_lines(read(OSV_SH)))
        self.assertIn(
            '>> "$COMBINED"', body,
            "nothing appends to $COMBINED -- per-ecosystem output is being "
            "dropped or uploaded separately",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2 if "-v" in sys.argv else 1)
