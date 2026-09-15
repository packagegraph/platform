# Collector seed lists

Host-installed inputs for collectors that seed from a curated set rather than
from SPARQL auto-discovery. **Nothing in this directory is tracked** — see
`.gitignore` for why, and treat that as binding rather than advisory.

Currently one consumer: `maven.sh` reads `/seeds/maven-roots.txt`.

## Format

One `groupId:artifactId` per line. Blank lines and `#` comments are ignored.

Roots must be **unpinned**. A `groupId:artifactId:version` line skips version
resolution entirely, which freezes that coordinate at whatever was written down
and, for vendor-suffixed builds (`…redhat-00001` and similar), 404s outright —
those publish to vendor repositories, never to Maven Central.

## Failure modes worth knowing

These are silent, which is why they are written down:

- `MavenSeed::parse` returns `None` for a malformed line and
  `read_maven_seed_file` skips it. A typo does not error — it shrinks the
  published graph, and nothing reports that.
- An unresolvable root spends its full retry budget and counts against
  `pg-collect`'s 20% error-rate guard. Enough of them abort the run and publish
  nothing. That guard is correct and should not be loosened to accommodate a
  bad list; verify roots resolve before adding them.

`etl/pg-collect/tests/test_maven_seed_roots.rs` checks both, but only when
pointed at a list via `PG_COLLECT_MAVEN_SEEDS` — it cannot run in CI, because
CI has no seed list.

## Deployment

Install to `/etc/containers/systemd/seeds/`, mode 644.
`pg-collect@.container` bind mounts that directory read-only at `/seeds`.

**Do not delete the directory on the host.** Podman refuses to start a
container whose bind-mount source is missing (`statfs …: no such file or
directory`), and the template is shared by every collector — so an absent
`/etc/containers/systemd/seeds` fails all of them, not just the ones that read
a list. `checkpoint-release.py verify` checks the directory is present and
non-empty for exactly this reason.
