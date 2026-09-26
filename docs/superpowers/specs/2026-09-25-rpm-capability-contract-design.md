# RPM capability and dependency emission: platform design

Status: proposed, 2026-09-25 (revised 2026-09-26). Covers the
platform-side half of #62.

**The ontology contract lives in
[`docs/ontology-request-rpm-capability-contract.md`](../../ontology-request-rpm-capability-contract.md),
filed as packagegraph/ontology#19.** This document does not restate it.
Where the two overlap, the request document is authoritative — term
names, shapes and fixtures are settled there, and duplicating them here
would guarantee drift.

This document covers what the platform does: what to stop emitting,
what to start emitting, how consumers are handled, and how the
transition runs.

## The problem, in one measurement

RPM rich (boolean) dependencies are live in the corpus and the current
model turns each one into a package.

| | |
|---|---|
| distinct rich-expression texts | 14,037 |
| distinct `pkg:PackageIdentity` nodes minted from them | 23,587 |
| `pkg:directlyDependsOn` assertions targeting those nodes | 58,865 |

Their `identityName` values are, verbatim, `(ansible-core or ansible)`,
`(adcli-selinux if selinux-policy-targeted)`,
`((adobe-afdko >= 4.0.1) with (adobe-afdko < 5~~))`. A disjunction and a
conditional, each asserted as a package that something depends on.

Flattening is not available as a fix: splitting `(a or b)` into two
`directlyDependsOn` edges asserts both are required, which is a stronger
and different claim than the metadata makes.

## What is wrong today, stated accurately

Three claims about this that appear in #62 and in earlier drafts of this
document are **withdrawn**, and are recorded here so they are not
recycled:

- *"100% range violation"* — `rdfs:range` assertions **infer** class
  membership; they do not fail. The accurate statement is that
  `pkg:directlyProvides` infers `pkg:Package` membership the producer
  did not intend.
- *"Unsatisfiable Packages, 81,197,282 of them"* — wrong on both counts.
  Under the open-world assumption a missing `packageName` may exist
  unstated, so `owl:cardinality 1` is not breached. And 81,197,282 edges
  resolve to **3,611,932 distinct targets**, of which **1,545,695**
  carry no `packageName`. Completeness of the inferred members is a
  separate SHACL question.
- *"A `Capability` cannot be a `pkg:PackageEntity`"* — does not follow
  from `Package`/`Capability` disjointness, which says nothing about
  their shared superclass. Keeping declarations out of the generic
  dependency hierarchy is a **design choice** resting on
  `pkg:DependencyShape` and on a resource bound for this release, not a
  logical prohibition.

What does hold, and is measured:

| fact | figure |
|---|---|
| `pkg:requiresCapability` assertions | **0** — never exercised |
| `pkg:Capability` nodes with `rdfs:label` | **0 of 1,013,026** — all fail `pkg:CapabilityShape` |
| `rpm:provides`/`requires`/`conflicts`/`obsoletes` | 0 each; the collector uses four undeclared spellings instead |
| `pkg:directlyProvides` | = `rpm:rpmProvides` + `deb:debProvides`, exactly, per graph |

## Emission changes

### Stop

| today | why |
|---|---|
| `pkg:directlyProvides` → `PackageIdentity` for a provided name | infers unintended `pkg:Package` membership |
| `rpm:rpmProvides` / `rpmRequires` / `rpmConflicts` / `rpmObsoletes` | four undeclared spellings; retires four `vocab.rs` `KNOWN_BAD` entries (`rpm:RPMGroup`, the fifth RPM entry, is a separate defect and out of scope) |
| minting a `PackageIdentity` from any dependency or provides string | identities come from collected package records only |

### Start

- `rpm:DependencyDeclaration` per `rpm:entry`, per
  packagegraph/ontology#19 §3.
- `rdfs:label` on every `pkg:Capability` node — today none have one.
- Self-provides. `rpm.rs:1582` currently drops a provide whose name
  equals the package name, *before* the capability node is minted. Every
  RPM implicitly provides its own name, and that is how `Requires: bash`
  finds a provider, so the capability graph has no provider for ordinary
  package-name requirements today. This is the majority case.
- Versioned provides carry their version on the declaration.

### By kind

| kind | capability shortcut | declaration node |
|---|---|---|
| `rpm:Provides` | `pkg:providesCapability` | yes |
| `rpm:Requires` | `pkg:requiresCapability` | yes |
| `rpm:Conflicts` | **none** | yes |
| `rpm:Obsoletes` | **none** | yes |

**Conflicts and Obsoletes emit no capability shortcut.** An earlier
draft of this document said every simple declaration also emits
`pkg:requiresCapability`, which would have turned "conflicts with X"
into "requires X". Their meaning is carried by `rpm:declarationKind`
alone.

Capability shortcuts are emitted **only** when a declaration has a
`rpm:declaredCapability` — never for a rich expression.

Weak dependencies (`recommends`, `suggests`, `supplements`, `enhances`)
are not parsed by the collector and are out of scope. When added, a weak
dependency will not be folded into `pkg:requiresCapability`.

### Identity minting

A `pkg:PackageIdentity` is minted **only from a collected package
record** — something we have a version of. Never from a dependency or
provides string.

Shape-based filtering is rejected as the mechanism: `mail-transport-agent`
has no parenthesis, no soname and no slash, and is not a package. The
test is provenance, not syntax.

Consequence: the 3,815,106 identities that nothing `isVersionOf` are an
**investigation population, not a deletion list**. An identity with no
collected version may be a real package in a repository we have not
collected.

### Filtered internals

`config(...)`, `rpmlib(...)` and `rtld(...)` are dropped on both the
provides and requires paths (`rpm.rs:1467`, `rpm.rs:1574`). This is
deliberate — they describe RPM's own machinery — and is adopted
explicitly: **no capability node and no declaration node**. The filter
list becomes a named constant with a test, rather than three inline
`starts_with` calls duplicated across two loops.

### Declaration identity

Per packagegraph/ontology#19 §3.4: declaring package URI, relation kind,
and all five preserved attributes, with absence distinguished from
present-but-empty.

This is a behaviour change, not a restatement. `rpm.rs:1487` derives its
reified node from `bnode_id("dep", "{pkg_uri}_{dep.name}")`, keyed on
package and name only, so `Requires: foo >= 1` and `Requires: foo < 2`
collapse into one node carrying two version constraints — against
`pkg:DependencyShape`'s `sh:maxCount 1`.

## Capability identity

`d/capability/rpm/{encoded-token}`, pending packagegraph/ontology#19
decision 2.

The node identifies *this capability token within the RPM naming
system*. **Matching the identifier does not establish that a provider is
compatible with a requirer** — the same SONAME across architectures,
distributions or repositories is the same token, not the same binary
contract.

Note for the migration: `rpm.rs:1606` and `debian.rs:1004` currently
mint into the same unqualified `{DATA}capability/` namespace with the
same encoder, so an RPM `Provides: foo` and a Debian virtual package
`foo` are one node today. Only the RPM side is qualified by this
delivery; Debian's convention is a deferred item.

## Consumers

`revdeps` does not resolve providers. `enrich_revdeps.rs:103` counts
distinct dependents with `pkg:directlyDependsOn` into a
`pkg:PackageIdentity`; `enrich_blast_radius.rs:84` consumes
`met:reverseDependencyCount`.

The impact is on the **requires** side. `rpm.rs:1476` mints an identity
for every required name — including all 23,587 rich-expression nodes —
and points `directlyDependsOn` at it. When that stops, the counts fall.

**Decision: RPM package-level reverse-dependency metrics are withheld
visibly** until resolution semantics exist, rather than silently
recomputed over a changed input. Withholding needs no ontology change.
Derived graphs computed over manufactured identities are retired, not
left in place — a stale `revdeps` graph is indistinguishable from a
fresh one.

**The platform will not project `pkg:directlyDependsOn` on a name
match.** An earlier draft retained the projection wherever the target
was a collected package identity. That is insufficient: finding a
package with the same name does not establish that it satisfies the
declaration's version constraint, or that it applies in the declaring
package's architecture and repository context. Satisfying the target's
class constraint is not the same as satisfying the declaration. Until
resolution is modelled, the projection is withheld.

## Transition

Not a purge. **Collect replacements → validate → switch, retaining
rollback data.**

Four graphs — `fedora/42`, `fedora/42/aarch64`, `fedora/44/aarch64`,
`fedora/rawhide` — predate the capability emission and carry zero
`pkg:Capability` nodes, so their provenance and shape cannot be
attested. They account for the entire 1,838,372-quad discrepancy between
`rpmProvides` and `providesCapability`; the other nineteen graphs agree
exactly, to the triple.

1. Collect replacement generations from source, under the new contract.
2. Validate each replacement against the shapes **and against the legacy
   generation**, attributing every difference rather than accepting the
   delta wholesale.
3. Switch the served union to the replacement set.
4. Retain legacy generations and the prior index for rollback; remove
   only after the replacement set is verified in service.

At no point is the only copy of a graph deleted before its replacement
is validated.

## Sequencing

`G1` — packagegraph/ontology#19 decision 1 released, `ONTOLOGY_VERSION`
bumped — is a **publication gate, not a development blocker**. Collector
work proceeds against the agreed proposal; publishing changed RDF
requires the released pin and passing integration checks.

`G2` — decision 2 — gates **final identifiers and publication, not
acquisition**. Repodata capture and source-preserving fixtures start
immediately.

`G3` — decision 3 — needs an answer, not a release.

Work that starts now, before any gate: source-preserving fixtures from
real repodata; the capability-label fix; the self-provides fix; the
filtered-internals constant; removal of the four undeclared predicates;
consumer withholding; identification of stale derived results.

## Release gates

- No `pkg:directlyProvides` assertion whose target lacks
  `pkg:packageName`. Today 1,545,695 of 3,611,932 distinct targets.
- Four `rpm:rpm*` entries gone from `vocab.rs` `KNOWN_BAD` because the
  emission is gone, not because the ontology declared them.
- No `pkg:PackageIdentity` minted from a declaration.
- Every `pkg:Capability` node satisfies `pkg:CapabilityShape`, label
  included. Today none do.
- No rich expression appears as a `pkg:PackageIdentity`. Today 23,587
  do.
- `Requires: foo >= 1` and `Requires: foo < 2` on one package survive as
  two declarations.
- Conflicts and Obsoletes emit no capability shortcut.
- RPM reverse-dependency metrics are absent or distinctly named — never
  silently reduced.
- Replacement generations validated against their legacy counterparts,
  with differences attributed, before the switch. Rollback data
  retained.

## Deferred

Recorded, not filed. None has an issue yet.

- Resolution as an evidenced result — candidate providers,
  resolver-selected, no-provider-found, resolution-not-performed, with
  repository snapshot, architecture, resolver version and policy as
  evidence.
- Whether RPM declarations should eventually unify with
  `pkg:Dependency`. Deliberately left open; this delivery neither
  forecloses it nor argues against it.
- Weak dependency sections.
- Capability scoping for non-RPM ecosystems, including Debian's current
  use of the unqualified capability path.
- `rpm:RPMGroup` used as a predicate.
- `pkg:provides` emitted with a **literal** object by `alpine.rs:351`
  and `arch.rs:343` — 107,822 assertions putting a literal on an
  `owl:ObjectProperty`.

#62 stays open until the corrected collector output and the published
graph are both verified.
