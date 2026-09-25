# RPM capability and dependency contract

Status: proposed, 2026-09-25. Covers #62 and the emission half of the
RPM modelling work. Written against ontology `v0.15.0`
(`60017af`, the revision `etl/ONTOLOGY_VERSION` pins) and measured
against the production endpoint after the 2026-09-25 rebuild.

This is the contract document. It settles what RPM metadata maps to
before any emission changes, because the emission currently writes
predicates the ontology does not declare and edges that violate the
ranges it does.

## What the corpus actually contains

All `COUNT(*)` against the served index, 454,980,557 triples total.

| predicate | count | declared in ontology? |
|---|---|---|
| `rpm:rpmProvides` | 80,894,928 | **no** |
| `rpm:provides` | 0 | yes, `⊑ pkg:provides` |
| `rpm:rpmRequires` | 5,403,816 | **no** |
| `rpm:requires` | 0 | yes, `⊑ pkg:dependsOn` |
| `rpm:rpmConflicts` | 30,716 | **no** |
| `rpm:conflicts` | 0 | yes, `⊑ pkg:conflicts` |
| `rpm:rpmObsoletes` | 92,803 | **no** |
| `rpm:obsoletes` | 0 | yes, `⊑ pkg:replaces` |
| `pkg:directlyProvides` | 81,197,282 | yes |
| … targeting a `PackageIdentity` | **81,197,282 (100%)** | |
| `pkg:providesCapability` | 79,953,540 | yes |
| `pkg:requiresCapability` | **0** | yes |
| `pkg:Capability` instances | 1,013,026 | yes |

Two facts dominate everything below.

**The collector has never used the declared spellings.** It writes
`rpmProvides`/`rpmRequires`/`rpmConflicts`/`rpmObsoletes`; the ontology
declares `provides`/`requires`/`conflicts`/`obsoletes`. All four
declared properties sit at zero. 86.4M triples are on predicates no
module defines, which means no range applies to them and **no
conformance check can see them** — they are invisible to validation
rather than failing it.

**Every `directlyProvides` edge is out of range.** `rdfs:domain :Package
; rdfs:range :Package`, and 100% of its 81,197,282 targets are
`PackageIdentity`. `PackageIdentity ⊑ PackageEntity`, and `PackageEntity`
is *not* `Package`. This is the live violation #62 describes; #62's
figure of 6,558,436 comes from a stale code comment and is low by an
order of magnitude.

Together `directlyProvides` and `rpmProvides` are **162,092,210 triples,
36% of the corpus**, spent on two edges that are both wrong.

## The constraints that decide the design

Read from `core/core.ttl` at v0.15.0:

| term | domain | range |
|---|---|---|
| `pkg:directlyProvides` | `Package` | `Package` |
| `pkg:provides` | `Package` | `Package` |
| `pkg:directlyDependsOn` | `Package` | `PackageEntity` |
| `pkg:dependencyTarget` | `Dependency` | `PackageEntity` |
| `pkg:providesCapability` | `Package` | `Capability` |
| `pkg:requiresCapability` | `Package` | `Capability` |

Plus two axioms that constrain what is even expressible:

- `owl:AllDisjointClasses ( :Package :Distribution :Repository :License
  :Architecture :Capability )` — a Capability can never also be a
  Package.
- `PackageEntity` has exactly two subclasses, `Package` and
  `PackageIdentity`. **Capability is not a `PackageEntity`.**

Therefore a Capability cannot be a `dependencyTarget`, and cannot be the
object of `directlyDependsOn`. `pkg:directlyDependsOn` additionally
carries `owl:propertyChainAxiom ( :hasDependency :dependencyTarget )`,
so anything reachable through the reified `Dependency` is projected onto
it automatically under OWL 2 reasoning. Routing capabilities through the
generic Dependency model therefore means changing `dependencyTarget`'s
range *and* accepting what the property chain then materialises — a
coordinated OWL/SHACL/collector/query change, not an emission tweak.

**This contract does not do that.** Capabilities stay out of the generic
Dependency model.

## Contract

### Identity minting

A `pkg:PackageIdentity` is minted **only from a collected package
record** — something we have a version of. It is never minted from a
dependency string.

Shape-based filtering (exclude parenthesised names, sonames, absolute
paths) is explicitly rejected as the mechanism. `mail-transport-agent`
has no parenthesis, no soname and no slash, and is not a package; a
shape filter admits it. The test is provenance, not syntax.

Consequence: the 3,815,106 identities that nothing `isVersionOf` are an
**investigation population, not a deletion list**. An identity with no
collected version may be a real package in a repository we have not
collected. Emission changes going forward; existing nodes are audited
separately.

### `Provides:`

Subtractive, and it resolves the range violation.

| today | contract |
|---|---|
| mint `PackageIdentity` for the provided name | **stop** |
| `pkg:directlyProvides` → identity | **stop** (100% range violation) |
| `rpm:rpmProvides` → identity | **stop** (undeclared predicate) |
| `pkg:providesCapability` → `Capability` | **keep** — already correct |
| `pkg:Capability` + `pkg:capabilityName` | **keep** |

`providesCapability` already carries 79,953,540 triples, essentially 1:1
with `rpmProvides`. The correct edge is already being written alongside
the incorrect ones, so this removes ~162M triples and loses no
information.

**Versioned provides** (`Provides: foo = 1.2-3`) is a different claim
from `Provides: foo` and must not be flattened into it. `providesCapability`
cannot carry the version, so a versioned provide needs a declaration
structure (below).

**Open:** the 941,388 gap between `rpmProvides` (80,894,928) and
`providesCapability` (79,953,540). Both are written per (package,
capability) in the same loop, so they should agree. Resolve before
relying on `providesCapability` as a complete replacement.

### `Requires:` / `Conflicts:` / `Obsoletes:`

Additive, and it needs terms that do not exist yet.

`pkg:requiresCapability` exists, has range `Capability`, and is at
**zero** — the requires path has never used the property built for it.
It is correct for the simple case (`Requires: foo`, no constraint) but
cannot carry:

- a version constraint (`Requires: foo >= 1.2`)
- a boolean expression (`Requires: (a or b)`)
- the distinction between Conflicts and Obsoletes, which have their own
  semantics and are not variants of "depends"

So each declaration keeps its **relation kind, target, version
constraint and source package together**, on a structure that can be
reassembled. `rpm:Dependency ⊑ pkg:Dependency` and `rpm:versionConstraint`
(domain `rpm:Dependency`, range `xsd:string`) already exist and are the
natural base.

**Boolean dependencies must not be flattened.** RPM supports them, and
decomposing `(a or b)` into two independent mandatory edges changes a
choice into a conjunction. That is a correctness bug, not a
simplification. A declaration whose expression cannot be represented
must be preserved as an unparsed expression rather than silently
approximated.

**Epoch, version and release are preserved in full.**

### Predicate naming

The four undeclared predicates must be reconciled with the four declared
ones. Either the collector adopts `rpm:provides` etc., or the ontology
declares the `rpmProvides` spelling. **Deciding this is in scope for the
ontology PR and out of scope for the emission PR** — but note that
`rpm:provides ⊑ pkg:provides` inherits `rdfs:range :Package`, so simply
renaming the emission would convert 80.9M invisible triples into 80.9M
*visible* range violations. The naming fix and the retargeting fix have
to land together.

### Capability identity

`{DATA}capability/{encoded-name}` — no distro, release or arch, so one
global node per capability name across the whole corpus, against
arch-qualified package identities. This is currently implicit. It is
defensible for sonames (`libc.so.6()(64bit)` means the same thing
everywhere) and arguable for virtual names. **The contract adopts it
explicitly** rather than leaving it inherited, and notes that changing
it later is a 1,013,026-node migration.

## Out of scope

**Resolution.** "Requires capability X ≥ Y" is true regardless of which
provider is selected, and stays a declaration. Candidate providers,
resolver-selected providers, no-provider-found, and
resolution-not-performed are four distinct outcomes needing an evidenced
result structure. That is its own design.

**`revdeps` and `blast-radius` semantics.** Both currently treat every
provider as a required package. Once declarations and resolutions are
distinguished they must distinguish potential from selected
relationships — otherwise this work changes what those two enrichers
mean without anyone noticing. `revdeps` currently produces 902,727
triples and `blast-radius` 1,848.

## Release gates

- No `pkg:directlyProvides` edge targets a non-`Package`. Currently
  81,197,282 do.
- No emitted predicate is undeclared in the pinned ontology revision.
  Currently four are, carrying 86,422,263 triples.
- A capability declaration does not manufacture a `PackageIdentity`.
- A boolean dependency expression round-trips, or is preserved unparsed
  — never silently flattened.
- Prior generations and index retained for rollback.
