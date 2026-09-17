# Collector seed lists

Host-installed inputs for collectors that seed from a curated set rather than
from SPARQL auto-discovery. **Nothing in this directory is tracked** — see
`.gitignore` for why, and treat that as binding rather than advisory.

Currently one consumer: `maven.sh` reads `/seeds/maven-roots.txt`.

## Format

One `groupId:artifactId:version` per line, sorted. Blank lines and `#`
comments are ignored.

Roots must be **pinned**. A pinned line skips version resolution entirely,
which is the point: unpinned roots resolve through `maven-metadata.xml`, and
`parse_metadata_version` prefers `<release>` then `<latest>`. For some roots
Central reports a *pre-release milestone* in both fields — every
`org.springframework.boot:*` root resolved to `4.2.0-M1` on 2026-09-17, was
fetched with a clean 200, and was published as though it were shipped
software. Nothing in the pipeline flags that. Pinning removes the whole class
of error, and a seed list of shipped versions is what makes the graph describe
software that exists rather than whatever Central resolves to this week.

This reverses the previous rule. That rule existed because pinning
`…redhat-00001`-style vendor-suffixed strings 404s — those publish to vendor
repositories, never to Central. Strip any `.redhat-NNNNN` / `-redhat-NNNNN`
suffix to its upstream base version, which does publish to Central, and the
objection goes away. The list must therefore be
sanitized before install; `seed_roots_carry_no_vendor_suffix` in
`tests/test_maven_seed_roots.rs` checks it.

Verify every coordinate resolves before installing a list — see below.

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
