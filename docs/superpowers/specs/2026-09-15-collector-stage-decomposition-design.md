# Collector stage decomposition with triggered hand-offs

**Status:** DRAFT — design for review. Nothing here is implemented. Open
decisions are marked **[DECIDE]** and should be settled before a plan is
written.

**Problem this solves:** one `TimeoutStartSec` governs an entire multi-stage
pipeline, so the ceiling has to be sized for the sum of the stages. That makes
it useless as a hang detector — the value that lets `fedora-44-full` finish
(14h) is ~28x longer than a healthy `npm` run needs.

## Why now

`fedora-44-full` was measured on 2026-09-15 at ~12h50m: ~5h for the RPM and
spec stages, then 23,641 Koji NVRs at a measured 39/min. The 8h ceiling killed
it around the halfway point of the Koji stage, every time. Raising the shared
ceiling to 14h unblocks it, and that is the committed short-term fix — but it
buys completion by giving up hang detection for the other 42 collectors. A
genuinely stuck `npm` now burns 14h.

Per-stage timeouts are the actual answer. A stage-1 hang should die in 30
minutes; a Koji stage legitimately needs 12h. One number cannot express both.

## Current shape

`rpm-full` is a single process performing five sequential phases against one
output file:

1. **RPM collection** — parse repodata per arch, emit packages. Writes
   `<collection>/<name>.nt`.
2. **Spec collection** — per SRPM, fetch the spec from dist-git. Appends.
   Checkpointed (`spec` stage).
3. **Koji enrichment** — per NVR, query the Koji hub. Appends via
   `<name>.nt.koji.tmp` (read-and-append). Checkpointed (`koji` stage).
4. **Upload** — `upload-nt.sh` publishes the `.nt` and its `.graph` sidecar.
5. **Retire** — `pg-collect checkpoint commit` retires the generation.

Phases 2 and 3 are toggled by `--with-spec` / `--with-koji`. Correctness today
rests on two properties of being one process: the stages share an in-memory
NVR/SRPM list derived from stage 1, and a single exit code gates the upload.
**Both properties are what a decomposition has to reconstruct.**

## What already exists

Four of the five stages have standalone entry points, which is most of the work:

| stage | entry point | notes |
|---|---|---|
| RPM collection | `pg-collect rpm` | exists |
| Spec collection | — | **gap** — only reachable via `rpm-full --with-spec` |
| Koji enrichment | `pg-collect enrich-koji --srpm-list <file>` | exists; the file-based hand-off this design needs |
| Upload | `etl/scripts/upload-nt.sh` | exists |
| Retire | `pg-collect checkpoint commit` | exists |

The run generation also already works across processes:
`checkpoint_generation::acquire` reuses a valid existing generation rather than
minting a second one, bounded by `MAX_GENERATION_AGE_DAYS`. It was built for
retries of one run, and a staged pipeline is the same shape — one logical run,
several processes. No new coordination primitive is required.

Host systemd is 257, so `OnSuccess=` is available (needs ≥249).

## Proposed decomposition

Five Quadlet template units per collector, chained by success:

```
pg-collect-rpm@%i.service      → OnSuccess=pg-collect-spec@%i.service
pg-collect-spec@%i.service     → OnSuccess=pg-collect-koji@%i.service
pg-collect-koji@%i.service     → OnSuccess=pg-collect-upload@%i.service
pg-collect-upload@%i.service   → OnSuccess=pg-collect-commit@%i.service
pg-collect-commit@%i.service
```

`pg-collect-<name>.timer` starts only `pg-collect-rpm@<name>.service`. Each
unit carries its own `TimeoutStartSec`, sized to its stage.

### Use `OnSuccess=`, not `Requires=` + `After=`

`Requires=` + `After=` is the obvious choice and the wrong one. With
`RemainAfterExit=no`, a completed stage goes inactive; starting the next stage
then re-pulls the dependency and **re-runs the previous stage**.
`RemainAfterExit=yes` avoids that but leaves every unit stuck `active` after a
run, requiring a reset before the next one and making "did this run?"
unreadable from `systemctl status`.

`OnSuccess=` states the intent directly — on successful completion, start the
next unit — with no dependency graph to re-trigger. Failure needs no special
handling: if a stage fails, `OnSuccess=` simply does not fire, and the chain
stops with the failed unit visible in `systemctl --failed`.

**[DECIDE]** Whether `%i` expands inside `OnSuccess=`. systemd expands
specifiers in many `[Unit]` directives, but this must be **verified on 257
before the design is committed to**, not assumed. If it does not expand, the
fallback is a generated non-template unit per collector per stage (10 × 5 = 50
files for the rpm-full set), which is materially worse and might change the
recommendation.

### Hand-offs

Everything crosses process boundaries as files under the generation directory,
which is already the checkpoint mechanism:

- **Stage 1 → stages 2/3:** stage 1 writes the SRPM/NVR list to a known path.
  `enrich-koji --srpm-list` already consumes exactly this. **Code gap:** today
  `rpm-full` builds `nvr_list` in memory; something must persist it.
  Iteration order must stay sorted, since that is what makes a checkpointed
  stage's fragments replay identically.
- **Stages → upload:** the `.nt` file, as today.
- **Generation:** stage 1 acquires; stages 2–4 reuse; stage 5 retires.

### Gating the upload — the real correctness risk

Today a partially enriched `.nt` can never be uploaded, because one exit code
covers all enrichment. Split into units, that guarantee disappears, and the
`.nt` lives *outside* the generation directory so checkpoint machinery does not
protect it.

The failure to design against: stage 3 fails, then something starts
`pg-collect-upload@%i` — directly, or via a later timer firing — and publishes
a `.nt` containing stage-1 data only. It would look like a successful run that
lost 90% of its triples, and the loss gate cannot see it, because a
corpus-wide percentage check does not notice one graph shrinking.

**[DECIDE]** the gating mechanism. Leading option: each enrichment stage
writes a completion marker into the generation directory on success, and the
upload unit refuses to run unless every marker for the stages enabled for that
collector is present. This keeps the invariant checkable by a third party
rather than implied by control flow — the same reasoning that made
`checkpoint commit` explicit rather than inferred.

### Per-stage timeouts

Indicative, from the 2026-09-15 measurement:

| stage | proposed `TimeoutStartSec` | basis |
|---|---|---|
| rpm | 1h | stage 1 of fedora-44-full is minutes; streaming (#52) removed the memory pressure that made it slow |
| spec | 8h | ~5h measured for RPM+spec combined, plus margin for a cold cache |
| koji | 14h | 23,641 NVRs at 39/min ≈ 10h, plus margin; the degradation from 73/min to 39/min under load is not yet explained |
| upload | 1h | 178 MB compressed today |
| commit | 5m | a directory rename |

A hung stage 1 then dies in 1h instead of 14h, which is the whole point.

## Costs and open questions

**The template fork.** `pg-collect@.container`'s stated virtue is "one shared
template file, one shared timeout" across 43 collectors. Five templates for
the rpm-full set means those 10 diverge structurally from the other 33 — two
deployment shapes, two things to keep in step, and `checkpoint-release.py`
needs to stage and verify both.

**[DECIDE]** scope. Three options:

1. **rpm-full only** (10 collectors). Smallest change, solves the actual
   problem, accepts the fork.
2. **All 43.** Single-stage collectors become collect → upload → commit, which
   also buys independent upload retry. Uniform, much larger, and most of those
   collectors have no timeout problem at all.
3. **Generalize.** Each collector declares its own stage list and the units are
   generated from it. Most flexible, most machinery, and it invents a workflow
   engine inside systemd — which is the thing to be most suspicious of.

The stated intent is to decompose everything with triggered hand-offs, i.e.
option 2 or 3. This design argues for **staging the work as option 1 first**:
it is the case with a measured failure, it proves the `OnSuccess=` chain and
the upload-gating invariant on real data, and the pattern it establishes is
what option 2 or 3 would generalize anyway. Committing to option 3 before the
five-unit chain has run once in production is designing on paper.

**Concurrency.** Five units per collector × 43 collectors sharing one scratch
volume, with the timers unchanged. Whether stage units of *different*
collectors may overlap needs a decision; today the shared template lets them.

**Observability.** Per-stage timing in journald is a real gain. Against it, "is
fedora-44-full currently running?" stops being one `systemctl is-active` and
becomes a question about five units — the same class of confusion that made me
misread a `Type=oneshot` unit in `activating` as finished earlier today.

## Out of scope

- Raising the shared `TimeoutStartSec` to 14h. Already done as the short-term
  fix; this design is what eventually makes it unnecessary.
- Why the Koji rate degraded from ~73/min to 39/min within one run. Worth
  investigating on its own; it changes the stage-3 timeout but not this shape.
- `hex` and `nuget` publishing zero package data (a seed-source problem, not a
  staging one).
