# Collector wrappers

One `<name>.sh` per collector. `pg-collect@.container` bind-mounts exactly one
of them to `/usr/local/bin/collect.sh` and runs it; the unit file has no
per-collector knowledge at all. `%i` is the filename stem, so the name here is
the name in `pg-collect@<name>.service` and in `timers/`.

## Run directories

`pg-collect-scratch.volume` is mounted at `/tmp` and is **shared** — by every
collector running concurrently, and by repeated runs of any one collector. A
fixed path there is not scratch space, it is a shared mailbox.

That is what issue #69 found. Fifteen wrappers wrote their output to
`/tmp/packages.nt` and then handed the same path to `upload-nt.sh`. Between the
write and the read, any other one of those fifteen could truncate and rewrite
it, and the upload would publish the wrong collector's triples under this
collector's graph URI — silently, since the file exists and parses. Several
other wrappers had the narrower version of the same problem with fixed
intermediates (`/tmp/void-repo`, `/tmp/gentoo.tar.gz`, `/tmp/feeds/<feed>`,
`/tmp/osv-<slug>.nt`), which collide when one wrapper overlaps itself.

Every wrapper therefore opens with:

```sh
find /tmp -maxdepth 1 -type d -name 'run-*.*' -mmin +2880 -exec rm -rf {} + 2>/dev/null || true
RUN_DIR=$(mktemp -d /tmp/run-<name>.XXXXXXXX)
trap 'rm -rf "$RUN_DIR"' EXIT
```

and puts **everything ephemeral** under `$RUN_DIR`: the output `.nt`, archives,
extracted trees, per-ecosystem fragments. Producer and consumer of a file are
always given the same `$RUN_DIR` path, so they cannot drift apart.

Three details worth keeping:

- **The trap goes immediately after the `mktemp`**, not later. A few wrappers
  install a second trap further down to kill their background cache-sync loop;
  that one replaces this one and carries both actions. Installing only the
  later trap would leak a run directory whenever `set -e` fired in between —
  `mc alias set` failing, for instance.
- **The sweep exists because a trap is not guaranteed.** `TimeoutStartSec` is
  14h and systemd kills with `SIGKILL`, which runs no trap. 2880 minutes (48h)
  is comfortably longer than the longest permitted run, so the sweep can never
  delete a directory a live collector still owns.
- **`mktemp` is what makes two concurrent runs of the same wrapper safe.** A
  per-collector fixed directory would fix the cross-collector collision and
  leave the self-collision.

## What deliberately stays shared

`/tmp/cache/<collector>` is persistent state, not scratch: the HTTP/Koji cache
and the checkpoint generations that let a timed-out run resume instead of
restarting (see `../checkpoint-cutover.md`). It is mirrored to and from
`collector-cache/<collector>` in object storage and must survive the run that
created it, so it is **not** under `$RUN_DIR` and is never touched by cleanup.
Moving it would silently turn every resume into a cold run.

`tests/test_collector_run_dirs.py` enforces both halves of this: every
ephemeral path is under `$RUN_DIR`, and the only persistent roots are the
allowlisted cache ones.
