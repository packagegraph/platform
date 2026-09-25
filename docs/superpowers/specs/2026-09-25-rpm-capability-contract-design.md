# RPM capability and dependency contract

Status: proposed, 2026-09-25. Covers #62 and the emission half of the
RPM modelling work. Written against ontology `v0.15.0`
(`60017af`, the revision `etl/ONTOLOGY_VERSION` pins) and measured
against the production endpoint after the 2026-09-25 rebuild.

This is the contract document. It settles what RPM metadata maps to,
what terms carry it, and what has to hold before the emission change
ships.

## What the corpus actually contains

All `COUNT(*)` against the served index. The corpus serves
**386,964,067 distinct triples**. (The 2026-09-25 rebuild reported
454,980,557; that is the count it *ingested*, which double-counts
triples appearing in more than one named graph. The served distinct
count is the right denominator and is what is used below.)

These are corpus-wide totals; per-graph attribution is in
"Attribution" below.

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
| … whose target is typed `PackageIdentity` | **81,197,282 (100%)** | |
| `pkg:providesCapability` | 79,953,540 | yes |
| `pkg:requiresCapability` | **0** | yes |
| `pkg:Capability` instances | 1,013,026 | yes |

**The collector has never used the declared spellings.** It writes
`rpmProvides`/`rpmRequires`/`rpmConflicts`/`rpmObsoletes`; the ontology
declares `provides`/`requires`/`conflicts`/`obsoletes`. All four
declared properties sit at zero.

These four are **not invisible to validation**. The vocabulary gate in
`etl/pg-collect/src/vocab.rs` detects exactly this class of defect —
undeclared terms, and declared terms used in the wrong role — at `cargo
test` time. All four are listed in its `KNOWN_BAD` table
(`vocab.rs:169-173`), one of 60 entries. That table is a ratchet: it
lets the gate pass on what was already wrong while refusing anything
new, and its own doc comment calls it the work queue. So this is
**recorded, exempted technical debt, not an undetected hole**. The
contract's job is to retire five of those entries, not to discover them.

## The reasoning that actually justifies the repair

Three arguments that look available here are not, and the contract does
not rest on them.

**`?o a pkg:PackageIdentity` does not establish that `?o` is not a
`Package`.** The measurement establishes identity *typing*. Nothing in
the ontology forbids a node from being both, and `rdfs:range :Package`
on `directlyProvides` means an RDFS reasoner would *infer* `:Package`
membership for every one of those 81,197,282 targets rather than flag
them.

**`PackageEntity` is not a closed union.** `core.ttl:1659` declares it
as a plain `owl:Class`, and `Package` (`:1666`) and `PackageIdentity`
(`:1471`) declare themselves `rdfs:subClassOf` it. There is no
`owl:unionOf`, `owl:equivalentClass`, or disjointness closure. Two
declared subclasses do not prohibit a third member.

**`Package`/`Capability` disjointness is real, and is a separate
argument.** `core.ttl` asserts `owl:AllDisjointClasses ( :Package
:Distribution :Repository :License :Architecture :Capability )`. That
one holds and is load-bearing below.

So the case against the current emission is the entailment it causes,
not a violation it commits:

1. `directlyProvides` has `rdfs:range :Package`, so asserting it
   *entails* that every provided name is a `:Package`.
2. `:Package` carries `rdfs:subClassOf [ owl:cardinality 1 ; owl:onProperty
   :packageName ]` (`core.ttl:1671`).
3. `write_package_identity` (`emit/rdf.rs:57`) emits `identityName` and
   `rdfs:label`, never `packageName`.

The emission therefore manufactures 81,197,282 entailed `:Package`
instances that cannot satisfy `:Package`'s own cardinality restriction.
A capability token is asserted to be a package by the act of being
provided. That is the defect, stated as what the ontology actually says.

The SHACL side is a second, independent argument and is where the
`rpm:Dependency` decision below comes from.

## Constraints that decide the design

Read from `core/core.ttl` and `core/core.shacl.ttl` at v0.15.0, and
`ecosystems/rpm/rpm.ttl`:

| term | domain | range |
|---|---|---|
| `pkg:directlyProvides` | `Package` | `Package` |
| `pkg:provides` | `Package` | `Package` |
| `pkg:directlyDependsOn` | `Package` | `PackageEntity` |
| `pkg:dependencyTarget` | `Dependency` | `PackageEntity` |
| `pkg:providesCapability` | `Package` | `Capability` |
| `pkg:requiresCapability` | `Package` | `Capability` |
| `rpm:versionConstraint` | `rpm:Dependency` | `xsd:string` |
| `rpm:onPackage` | `rpm:Dependency` | `rpm:RPMPackage` |

- `rpm:Dependency rdfs:subClassOf pkg:Dependency` (`rpm.ttl:497`).
- `pkg:DependencyShape` (`core.shacl.ttl:132`) targets class
  `pkg:Dependency` and requires **exactly one** `pkg:dependencyTarget`
  of `sh:class pkg:PackageEntity`, at most one `pkg:hasVersionConstraint`,
  and a `pkg:dependencyType` drawn from a fixed `sh:in` list.
- `pkg:CapabilityShape` (`core.shacl.ttl:54`) requires **at least one
  `rdfs:label`** and exactly one `pkg:capabilityName`.
- `pkg:directlyDependsOn` carries `owl:propertyChainAxiom ( :hasDependency
  :dependencyTarget )`, so anything reachable through a reified
  `pkg:Dependency` is projected onto it automatically.

## Contract

### Capabilities do not reuse the `pkg:Dependency` hierarchy

A capability declaration cannot be an `rpm:Dependency`. `rpm:Dependency
⊑ pkg:Dependency`, so `DependencyShape` applies to it and demands a
`dependencyTarget` that is a `PackageEntity`. A `Capability` is not one,
and `Package`/`Capability` disjointness means making it one is not
available either.

`rpm:versionConstraint` cannot be borrowed either: its `rdfs:domain` is
`rpm:Dependency`, so asserting it drags the node into that class and the
shape with it, whatever type the declaration was minted with.

So the contract introduces terms that sit **outside** that hierarchy.
`pkg:Dependency` and `rpm:Dependency` are retained unchanged, for
relationships that genuinely meet the package-target contract.

New in `ecosystems/rpm/rpm.ttl`:

| term | kind | domain | range |
|---|---|---|---|
| `rpm:DependencyDeclaration` | `owl:Class`, `⊑ owl:Thing` | — | — |
| `rpm:declaringPackage` | `owl:ObjectProperty` | `rpm:DependencyDeclaration` | `pkg:Package` |
| `rpm:declarationKind` | `owl:ObjectProperty` | `rpm:DependencyDeclaration` | `rpm:DeclarationKind` |
| `rpm:declaredCapability` | `owl:ObjectProperty` | `rpm:DependencyDeclaration` | `pkg:Capability` |
| `rpm:declarationExpression` | `owl:DatatypeProperty` | `rpm:DependencyDeclaration` | `xsd:string` |
| `rpm:declarationConstraint` | `owl:DatatypeProperty` | `rpm:DependencyDeclaration` | `xsd:string` |
| `rpm:declarationEpoch` | `owl:DatatypeProperty` | `rpm:DependencyDeclaration` | `xsd:string` |
| `rpm:declarationUnparsed` | `owl:DatatypeProperty` | `rpm:DependencyDeclaration` | `xsd:boolean` |
| `rpm:applicableArchitecture` | `owl:ObjectProperty` | `rpm:DependencyDeclaration` | `pkg:Architecture` |
| `rpm:hasDeclaration` | `owl:ObjectProperty` | `pkg:Package` | `rpm:DependencyDeclaration` |

`rpm:DeclarationKind` is a class with exactly five named individuals —
`rpm:Requires`, `rpm:Provides`, `rpm:Conflicts`, `rpm:Obsoletes`,
`rpm:Recommends` — so the relation kind is a first-class term and not a
string. Conflicts and Obsoletes are kinds in their own right, not
variants of "depends".

`rpm:DependencyDeclarationShape` (new, in `ecosystems/rpm/rpm.shacl.ttl`):

- exactly one `rpm:declaringPackage`, `sh:class pkg:Package`
- exactly one `rpm:declarationKind`, `sh:in` the five individuals
- exactly one `rpm:declarationExpression` (the verbatim RPM string)
- at most one `rpm:declaredCapability`, `sh:class pkg:Capability`,
  **absent** when the expression is boolean or otherwise unparsed
- at most one each of `rpm:declarationConstraint`, `rpm:declarationEpoch`
- `rpm:declarationUnparsed` `true` iff `rpm:declaredCapability` is absent
- at least one `rdfs:label`

Note the declaration deliberately does **not** carry a
`pkg:dependencyTarget`. It is a statement about a capability token, not
about a package, and it must not be typeable as a `pkg:Dependency`.

**Declaration identity.** The declaration URI is derived from
`(declaring package URI, kind, verbatim expression)`, not from
`(package, name)`. The current reified-dependency path derives its blank
node from `bnode_id("dep", "{pkg_uri}_{dep.name}")` (`rpm.rs:1487`),
which collapses `Requires: foo >= 1` and `Requires: foo < 2` into one
node — and then hangs two `hasVersionConstraint` values off it, against
`DependencyShape`'s `sh:maxCount 1`. Repeated names with different
constraints must remain distinct declarations.

**Boolean dependencies must not be flattened.** RPM supports them, and
decomposing `(a or b)` into two independent mandatory edges changes a
choice into a conjunction. A declaration whose expression cannot be
represented is preserved verbatim in `rpm:declarationExpression` with
`rpm:declarationUnparsed true` and no `declaredCapability`, rather than
silently approximated.

**Epoch, version and release are preserved in full.**

### Identity minting

A `pkg:PackageIdentity` is minted **only from a collected package
record** — something we have a version of. It is never minted from a
dependency or provides string.

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

| today | contract |
|---|---|
| mint `PackageIdentity` for the provided name | **stop** |
| `pkg:directlyProvides` → identity | **stop** (entails an unsatisfiable `:Package`) |
| `rpm:rpmProvides` → identity | **stop** (retires a `KNOWN_BAD` entry) |
| `pkg:providesCapability` → `Capability` | **keep, with the fixes below** |
| `pkg:Capability` + `pkg:capabilityName` | **keep, with the fixes below** |

Keeping the existing capability emission unchanged is not sufficient.
Four defects in it have to be fixed in the same change:

1. **Self-provides are skipped.** `rpm.rs:1582` drops any provide whose
   name equals the package name, before the capability node is emitted.
   Every RPM implicitly provides its own name, and that is how an
   ordinary `Requires: bash` finds a provider. Skipping it means the
   capability graph has no provider for ordinary package-name
   requirements — the majority case. Self-provides must be emitted.
2. **Capability nodes have no label.** The loop writes
   `pkg:capabilityName` only. `CapabilityShape` requires at least one
   `rdfs:label`, so all 1,013,026 capability nodes violate it today.
   Emit a label.
3. **Versioned provides lose their version.** `Provides: foo = 1.2-3` is
   a different claim from `Provides: foo`, and `providesCapability`
   cannot carry the difference. Versioned provides get an
   `rpm:DependencyDeclaration` of kind `rpm:Provides` carrying the
   constraint and epoch, in addition to the capability edge.
4. **Filtered internal capabilities are undocumented.** `config(...)`,
   `rpmlib(...)` and `rtld(...)` are dropped on both the provides and
   requires paths (`rpm.rs:1467`, `:1574`). This is deliberate — they
   describe RPM's own machinery, not inter-package functionality — and
   the contract adopts it explicitly: **they are not emitted as
   capabilities and not emitted as declarations**, and the filter list
   is a named constant with a test, not three inline `starts_with` calls
   duplicated across two loops.

### `Requires:` / `Conflicts:` / `Obsoletes:`

Each declaration becomes an `rpm:DependencyDeclaration` plus, where the
expression names a single capability, a `pkg:requiresCapability` edge
(currently at **zero** — the property built for this has never been
used). `pkg:directlyDependsOn` and the reified `pkg:Dependency` are
retained **only** where the target is a collected package identity, so
they continue to satisfy `DependencyShape`.

### Predicate policy (settled, not deferred)

The four `KNOWN_BAD` spellings are **removed, not declared**.

Renaming the emission to the declared spellings is not an option on its
own: `rpm:provides ⊑ pkg:provides` inherits `rdfs:range :Package`, so it
would trade four exempted undeclared predicates for 80.9M assertions
carrying the same bad entailment the repair exists to remove. Declaring
`rpmProvides` in the ontology would bless the same thing.

So:

- `rpm:rpmProvides`, `rpm:rpmRequires`, `rpm:rpmConflicts`,
  `rpm:rpmObsoletes` — **deleted from emission**, and their `KNOWN_BAD`
  entries deleted with them.
- `rpm:provides` / `requires` / `conflicts` / `obsoletes` — retained in
  the ontology, used **only** for assertions that satisfy their
  published `rdfs:range :Package`, i.e. where the target is a collected
  package. They stay at zero until such an assertion exists.
- Capability relationships use `pkg:providesCapability` /
  `pkg:requiresCapability`, whose published semantics already match.
- Everything the shortcuts cannot express uses
  `rpm:DependencyDeclaration`.

`rpm:RPMGroup`-as-predicate (`vocab.rs:169`) is the fifth rpm entry in
`KNOWN_BAD` and is a distinct defect; it is out of scope here.

### Capability identity is an RPM-scoped symbol

`{DATA}capability/{encoded-name}` — one global node per capability name,
no distro, release or arch qualifier, against arch-qualified package
identities.

The contract adopts this URI shape, and **narrows what it means**. The
node denotes *this capability token in the RPM naming system*. It does
**not** assert that every occurrence denotes interchangeable
functionality.

- The same SONAME does not establish ABI compatibility across
  architectures, distributions, or repositories. `libc.so.6()(64bit)` in
  Fedora 43 aarch64 and in RHEL 9 x86_64 are the same token, not the
  same binary contract.
- Provision and requirement assertions stay contextual: they are made by
  an arch- and release-qualified package, and that context is what
  carries the compatibility claim.
- **Global name equality must not be used to establish provider
  suitability.** Any query or enricher that joins
  `providesCapability`/`requiresCapability` on the capability node alone
  is computing candidate providers, not compatible ones, and must say
  so.

**Namespace audit.** `etl/pg-collect/src/debian.rs:1004` mints into the
same `{DATA}capability/` namespace with the same `uris::encode`, so
Debian and RPM capability tokens already collide by construction — a
Debian virtual package named `foo` and an RPM `Provides: foo` are one
node today. No other ecosystem in the tree uses the namespace. Calling
this node "RPM-scoped" while Debian shares it is a contradiction that
must be resolved in the ontology PR: either the namespace is qualified
per naming system, or the node is redefined as a cross-ecosystem token
with the compatibility caveats above applying equally. This is a
1,013,026-node decision and changing it later is a migration.

## Consumers

Not out of scope. The emission change removes inputs these read.

**`revdeps` does not resolve providers.**
`etl/pg-collect/src/enrich_revdeps.rs:103` counts, per target identity,
the distinct `?depIdentity` of packages with
`pkg:directlyDependsOn ?targetIdentity`, filtered to
`?targetIdentity a pkg:PackageIdentity`. `blast-radius`
(`enrich_blast_radius.rs:84`) consumes `met:reverseDependencyCount` and
performs no resolution either. The earlier draft of this document
described both as treating every provider as a required package; that is
not what the code does.

The impact is on the **requires** side, not the provides side. The RPM
requires loop mints a `PackageIdentity` for every required name
(`rpm.rs:1476`) including capability tokens, then points
`directlyDependsOn` at it. Under this contract those identities are no
longer minted, so those `directlyDependsOn` edges disappear and the
counts fall.

This release must state which of three it does, and the choice is a
release gate, not an implementation detail:

1. **Withhold** — stop publishing `met:reverseDependencyCount` for RPM
   identities until a resolver exists. Honest, and visibly a gap.
2. **Replace** — publish explicitly named declaration metrics
   (`met:reverseDeclarationCount` over `rpm:declaredCapability`)
   alongside the package-target counts, so the two are never summed by
   accident.
3. **Approximate** — keep a single count over capability-name joins,
   documented at the term as candidate-provider-based and not
   resolution-based.

Whichever is chosen, **existing derived graphs must be regenerated or
retired**, not left in place. A stale `revdeps` graph computed over
manufactured identities is indistinguishable from a fresh one.

## Out of scope

**Resolution.** "Requires capability X ≥ Y" is true regardless of which
provider is selected, and stays a declaration. Candidate providers,
resolver-selected providers, no-provider-found, and
resolution-not-performed are four distinct outcomes needing an evidenced
result structure. That is its own design.

## Attribution

Measured per graph against the public endpoint, 2026-09-25
(`~/.cache/capgap.sh`). Quad counts — each occurrence in each named
graph, so these sum higher than the distinct totals above.

**`directlyProvides` is exactly `rpmProvides` + `debProvides`, in every
graph and in total.** 81,936,448 = 81,572,865 + 363,583. `debian.rs:1000`
is the only other writer; no third source exists.

**The `providesCapability` shortfall is four graphs, and only four.**

| graph | rpmProvides | providesCapability | gap |
|---|---|---|---|
| `fedora/rawhide` | 538,544 | 0 | 538,544 |
| `fedora/42` | 450,931 | 0 | 450,931 |
| `fedora/42/aarch64` | 425,070 | 0 | 425,070 |
| `fedora/44/aarch64` | 423,827 | 0 | 423,827 |
| **every other graph (19)** | | | **0** |
| total | | | **1,838,372** |

In all nineteen graphs the current collector produces — `rhel/9`,
`rhel/10`, `almalinux/*`, `rocky/*`, `centos-stream/*`,
`opensuse/tumbleweed`, `fedora/43`, `fedora/44`, `fedora/44/riscv64`,
`debian/*`, `ubuntu/*` — `rpmProvides` (or `debProvides`),
`directlyProvides` and `providesCapability` agree **exactly, to the
triple**.

The four outliers are a pre-capability generation. `fedora/42` has
450,931 `directlyProvides`, 648,661 `directlyDependsOn` and 440,264
`PackageIdentity` nodes, and **zero** `pkg:Capability` instances and
zero `providesCapability`. `fedora/43`, from the current collector, has
373,333 capabilities and `providesCapability` equal to
`directlyProvides` at 734,484. The capability layer was added in
`74fcf0a`; these four predate it.

**None of the four has a collector.** `deploy/quadlet/collectors/scripts/`
contains `fedora-43-full.sh` and `fedora-44-full.sh` and nothing else
for Fedora. `fedora/42`, `fedora/42/aarch64`, `fedora/44/aarch64` and
`fedora/rawhide` are legacy graph files, sole copies, with nothing able
to regenerate them — the population already tracked as stale duplicate
graph files.

So the subtractive change has two different consequences, and the
contract must not average them:

- **For the nineteen regenerable graphs, removal loses nothing.**
  `providesCapability` is a verified one-for-one replacement, per graph.
  This is what the earlier draft asserted without evidence; it now has
  evidence, scoped to where it holds.
- **For the four legacy graphs, removal has no replacement.** They have
  no capability layer and cannot be rebuilt. Their 1,838,372 provides
  and 81,197,282-corpus-wide share of the range problem would persist
  after the emission fix, because the emission fix cannot reach a static
  file.

That makes it a retire-or-except decision, not a correctness blocker:
either the four graphs are dropped from the served union, or the
release gates below are scoped to regenerated graphs and the four are
carried as a named, documented exception. **They must not be silently
excluded from the gate query.** Note also that they contribute
`directlyDependsOn` (648,661 from `fedora/42` alone), so they feed
`revdeps` today and retiring them changes those counts independently of
anything in this contract.

## Release gates

- No `pkg:directlyProvides` assertion whose target lacks `pkg:packageName`.
  Currently 81,197,282.
- The four `rpm:rpm*` entries are gone from `vocab.rs` `KNOWN_BAD`,
  because the emission is gone — not because the ontology declared them.
- A capability declaration does not mint a `pkg:PackageIdentity`.
- Every `pkg:Capability` node satisfies `CapabilityShape`, label
  included. Currently none do.
- The retire-or-except decision on `fedora/42`, `fedora/42/aarch64`,
  `fedora/44/aarch64` and `fedora/rawhide` is made and recorded. If
  excepted, the gate query names them explicitly rather than filtering
  them out by a predicate that happens to exclude them.
- The consumer decision (withhold / replace / approximate) is
  implemented, and every dependent derived graph is regenerated or
  retired.
- Prior generations and index retained for rollback.

## Release fixtures

Acceptance fixtures, each a repodata sample plus expected N-Triples:

| fixture | proves |
|---|---|
| self-provide | `Requires: bash` finds a provider through `bash`'s own implicit provide |
| versioned provide | `Provides: foo = 1.2-3` keeps the version and is distinguishable from `Provides: foo` |
| epoch | `Requires: foo >= 2:1.0-1` preserves the epoch |
| repeated name, different constraints | `Requires: foo >= 1` and `Requires: foo < 2` are two declarations, not one node with two constraints |
| boolean expression | `Requires: (a or b)` round-trips verbatim with `declarationUnparsed true`, and emits no mandatory edge to either operand |
| unresolved requirement | a required capability with no provider in the corpus yields a declaration and no `PackageIdentity` |
| architecture context | the same capability name required by an x86_64 and an aarch64 package yields two contextual declarations against one capability node |
| filtered internals | `rpmlib(...)`, `config(...)`, `rtld(...)` produce neither capability nor declaration |

Plus two kinds of test the fixtures alone do not cover:

- **Inference tests.** Run an RDFS/OWL reasoner over a fixture's output
  and assert that no `pkg:Capability` node is entailed to be a
  `pkg:Package`, and that no node is entailed into `:Package` without a
  `:packageName`. This is the test that would have caught the current
  defect.
- **Consumer tests.** Assert that changed coverage cannot appear
  silently as a lower dependency count: given a fixture whose required
  names are all capabilities, the chosen consumer strategy either
  withholds the metric, emits a distinctly-named one, or emits the
  documented approximation — and never emits `met:reverseDependencyCount`
  reduced without a signal.
