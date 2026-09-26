# Request to the ontology team: three decisions for RPM dependency declarations

From: packagegraph/platform
Date: 2026-09-25
Against: ontology `v0.15.0` (`60017af`), the revision
`etl/ONTOLOGY_VERSION` pins
Related: platform#62 (open), platform#109 (draft design document)

---

## 1. Summary

Three decisions are requested. Each is scoped to what the RPM collector
needs in order to stop emitting demonstrably wrong data in this
delivery. None requires changing an existing core class, property,
domain, range, or cardinality.

| # | Decision | Where it lands |
|---|---|---|
| 1 | A small RPM **declaration** representation — declaring package, relation kind, the repodata fields as received, constraints, and unsupported expressions — held outside `pkg:Dependency` for this delivery | `ecosystems/rpm/rpm.ttl`, `ecosystems/rpm/rpm.shacl.ttl`, fixtures |
| 2 | An RPM capability URI convention: **`d/capability/rpm/{encoded-token}`**, with the encoding and scope stated | `URI-POLICY.md`; one annotation on `pkg:Capability` |
| 3 | Correct the DD-VirtualPackage worked example to use `pkg:providesCapability` for `Capability`/`VirtualPackage` targets | `docs/design-decisions.md` |

**Keeping declarations outside `pkg:Dependency`, and leaving the
package-oriented property ranges untouched, is a resource-bounded
implementation choice for this release.** It is not a claim that the
current abstraction is optimal, and it is not an argument that a
unified dependency model is wrong. It is the smallest change that lets
the collector emit correct data now, while leaving the larger modelling
question open and unprejudiced. That larger question is
recorded as a deferred item (§8), not yet filed.

---

## 2. The technical recommendation, and the evidence for it

**Recommendation: give RPM a declaration node that records what the
repodata actually said, keep it out of `pkg:Dependency` for this
release, and reserve resolution for later work.**

The evidence is a single measurement. RPM rich (boolean) dependencies
are live in the corpus today, and the current model turns each one into
a package.

```sparql
# over { ?s rpm:rpmRequires ?o . ?o pkg:identityName ?nm . FILTER(STRSTARTS(?nm,"(")) }
COUNT(DISTINCT ?nm)  # 14,037  distinct expression texts
COUNT(DISTINCT ?o)   # 23,587  distinct identity nodes
COUNT(*)             # 58,865  rpmRequires assertions
```

The three figures differ and the distinction matters. Identity URIs are
qualified by distro, release and architecture, so one expression text
appearing in several repositories mints several nodes — 23,587 nodes is
an upper bound on, not a count of, distinct expressions. **14,037 is the
number of distinct rich expressions.** All 23,587 nodes are typed
`pkg:PackageIdentity` and are the target of 58,865
`pkg:directlyDependsOn` assertions. Sample texts, verbatim:

```
(ansible-core or ansible)
(adcli-selinux if selinux-policy-targeted)
((adobe-afdko >= 4.0.1) with (adobe-afdko < 5~~))
(alsa-plugins-pulseaudio if pulseaudio)
(ansible-core or (ansible < 2.10.0 with ansible >= 2.9.10))
```

Three things follow, and they are the whole case:

1. **A disjunction has been asserted as a package.** `(ansible-core or
   ansible)` is a choice between two providers. It is currently a
   `PackageIdentity` node, and something `directlyDependsOn` it. There
   is no package by that name and there never will be.
2. **The `if` / `unless` operators are conditional, not mandatory.**
   `(adcli-selinux if selinux-policy-targeted)` asserts a requirement
   only when a condition holds. Modelled as a dependency edge it is
   unconditional, which is a stronger claim than the metadata makes.
3. **Flattening is not an available fix.** Splitting
   `(ansible-core or ansible)` into two `directlyDependsOn` edges turns
   a disjunction into a conjunction — it would assert both are
   required. That is worse than the current state, not better.

So the collector needs somewhere to put an expression it cannot resolve
into a single target, without asserting a package relationship it
cannot support. That is what decision 1 provides, and it is why the
declaration node must be able to exist *without* a resolved target.

Two supporting measurements, both re-runnable at
`https://packagegraph.di.riseproject.dev/`:

| fact | figure |
|---|---|
| `pkg:requiresCapability` assertions corpus-wide | **0** — the requires half of the capability model has never been exercised |
| `pkg:Capability` nodes carrying `rdfs:label` | **0 of 1,013,026** — every node fails `pkg:CapabilityShape`; this is a producer fix, no ontology change needed |
| distinct `pkg:directlyProvides` targets | 3,611,932, of which 1,545,695 carry no `pkg:packageName` |

On that last row: the accurate statement is that these assertions
**infer `pkg:Package` membership the producer did not intend**, and the
completeness of the inferred members is a separate SHACL question. It is
not a range violation and the targets are not unsatisfiable —
`owl:cardinality 1` is not breached by an unstated value under the
open-world assumption.

---

## 3. Decision 1 — an RPM declaration representation

### 3.1 Supported RPM metadata sections, stated explicitly

The collector streams `repodata/primary.xml` and reads exactly four
dependency sections (`etl/pg-collect/src/rpm.rs:765`):

| repodata element | `rpm:DeclarationKind` |
|---|---|
| `<rpm:provides>` | `rpm:Provides` |
| `<rpm:requires>` | `rpm:Requires` |
| `<rpm:conflicts>` | `rpm:Conflicts` |
| `<rpm:obsoletes>` | `rpm:Obsoletes` |

**Out of scope for this delivery**, because the parser does not read
them: `<rpm:recommends>`, `<rpm:suggests>`, `<rpm:supplements>`,
`<rpm:enhances>`. Adding them later is one new `rpm:DeclarationKind`
individual each, which is additive — deferring them does not constrain
the design. They are recorded with the deferred items in §8.

Also out of scope: `<rpm:entry pre="1"/>` (pre-install ordering) and
`filelists.xml` / `other.xml`, neither of which the dependency path
touches.

### 3.2 The fields actually received

There is **no original RPM spec string**. `primary.xml` gives one XML
element per declaration with up to five attributes, and the collector's
`RpmDep` (`rpm.rs:39`) holds exactly those:

| repodata attribute | field | notes |
|---|---|---|
| `name` | `name: String` | the capability token — **or**, for a rich dependency, the entire boolean expression, as shown in §2 |
| `flags` | `flags: Option<String>` | `EQ`, `GE`, `GT`, `LE`, `LT`; absent when unversioned |
| `epoch` | `epoch: Option<String>` | |
| `ver` | `ver: Option<String>` | |
| `rel` | `rel: Option<String>` | |

So the term below is named for what it holds — the **`name` attribute
as received**, which is the only field that can carry an expression —
rather than implying a spec-file string we never see. The version
constraint is reassembled from `flags`, `epoch`, `ver` and `rel`, and
those components are recorded separately so the reassembly is
reversible.

### 3.3 Terms — 2 classes, 4 individuals, 9 properties, 2 node shapes

```turtle
rpm:DependencyDeclaration a owl:Class ;
    rdfs:label "RPM Dependency Declaration"@en ;
    IAO:0000115 "A single dependency-related entry as published in RPM repository metadata — one rpm:entry element within a provides, requires, conflicts or obsoletes section — recorded as declared, independently of whether any package satisfies it. Its declared token is a capability name or, for a rich dependency, a boolean expression; in neither case is it a package, which is why a declaration is not a pkg:Dependency."@en ;
    rdfs:isDefinedBy rpm: ;
    rdfs:subClassOf owl:Thing .

rpm:DeclarationKind a owl:Class ;
    rdfs:label "RPM Declaration Kind"@en ;
    IAO:0000115 "The kind of RPM dependency entry: Provides, Requires, Conflicts or Obsoletes. Each has distinct semantics and none is a specialisation of another."@en ;
    rdfs:isDefinedBy rpm: ;
    rdfs:subClassOf owl:Thing .

rpm:Provides  a rpm:DeclarationKind ; rdfs:label "Provides"@en  ; rdfs:isDefinedBy rpm: .
rpm:Requires  a rpm:DeclarationKind ; rdfs:label "Requires"@en  ; rdfs:isDefinedBy rpm: .
rpm:Conflicts a rpm:DeclarationKind ; rdfs:label "Conflicts"@en ; rdfs:isDefinedBy rpm: .
rpm:Obsoletes a rpm:DeclarationKind ; rdfs:label "Obsoletes"@en ; rdfs:isDefinedBy rpm: .
```

| property | type | domain | range | cardinality |
|---|---|---|---|---|
| `rpm:hasDeclaration` | ObjectProperty | `pkg:Package` | `rpm:DependencyDeclaration` | 0..n |
| `rpm:declaringPackage` | ObjectProperty, `owl:inverseOf rpm:hasDeclaration` | `rpm:DependencyDeclaration` | `pkg:Package` | 1 |
| `rpm:declarationKind` | ObjectProperty | `rpm:DependencyDeclaration` | `rpm:DeclarationKind` | 1 |
| `rpm:declaredToken` | DatatypeProperty | `rpm:DependencyDeclaration` | `xsd:string` | 1 |
| `rpm:declaredCapability` | ObjectProperty | `rpm:DependencyDeclaration` | `pkg:Capability` | 0..1 |
| `rpm:declarationFlags` | DatatypeProperty | `rpm:DependencyDeclaration` | `xsd:string` | 0..1 |
| `rpm:declarationEpoch` | DatatypeProperty | `rpm:DependencyDeclaration` | `xsd:string` | 0..1 |
| `rpm:declarationVersion` | DatatypeProperty | `rpm:DependencyDeclaration` | `xsd:string` | 0..1 |
| `rpm:declarationRelease` | DatatypeProperty | `rpm:DependencyDeclaration` | `xsd:string` | 0..1 |

`rpm:declaredToken` is the `name` attribute exactly as received.
`rpm:declaredCapability` is present only when that token is a plain
capability name; for a rich dependency it is **absent**, and its absence
is the machine-readable signal that the expression was not resolved into
a target. Flags, epoch, version and release are the repodata attributes,
kept separate rather than concatenated so no parsing is required to
recover them.

`rpm:declarationArchitecture` is deliberately **not** proposed:
architecture is a property of the declaring package, which is already
arch-qualified, so a per-declaration copy would be redundant. This
differs from an earlier draft of this request.

### 3.4 Declaration identity

Declarations must stay distinct when a package declares the same name
more than once under different constraints. This is not hypothetical:
the current collector derives its reified node from
`bnode_id("dep", "{pkg_uri}_{dep.name}")` (`rpm.rs:1487`), keyed on the
declaring package and the dependency **name only**. `Requires: foo >= 1`
and `Requires: foo < 2` collapse into one node, which then carries two
version constraints.

**Requested identity:** a declaration node is identified by the
declaring package URI, the relation kind, and **all five preserved
attributes** — `name`, `flags`, `epoch`, `ver`, `rel`.

Absent values must be distinguishable from present-but-empty. An
attribute that did not appear in the `rpm:entry` element is not the same
as one that appeared with an empty value, and a key that joins them
would silently merge two different declarations. The platform will use a
canonical encoding in which absence is a distinct sentinel rather than
the empty string; the ontology does not need to mandate the encoding,
only that identity covers all six components and that absence is
distinguished.

This is stated here because it is a contract the fixtures must test —
see the repeated-name fixture in §3.7 — not because it constrains the
ontology terms.

### 3.5 Shape — enforcing the conditions, not just naming them

Two node shapes. The permitted attribute combinations are enumerated
first, because the shape is only as good as the rule it encodes and an
earlier version of this section checked one prohibited combination out
of four.

**Permitted combinations of the four version attributes.** In
`primary.xml` a version constraint is `flags` plus `ver`, with `epoch`
and `rel` as optional refinements of that version. So exactly two shapes
are legal:

| # | flags | epoch | ver | rel | meaning |
|---|---|---|---|---|---|
| A | absent | absent | absent | absent | unversioned — `Requires: foo` |
| B | present | optional | **present** | optional | versioned — `Requires: foo >= 1.2-3` |

Everything else is prohibited, and each prohibited case gets its own
constraint and its own negative fixture:

| prohibited | why |
|---|---|
| `flags` without `ver` | a comparison operator with nothing to compare |
| `ver` without `flags` | a version with no operator states no constraint |
| `epoch` without `ver` | epoch qualifies a version that is not present |
| `rel` without `ver` | release qualifies a version that is not present |

**The capability link is required for plain tokens.** An earlier version
treated a missing `rpm:declaredCapability` as evidence that the token
was an unparsed expression. It is not — the shape accepted a plain `foo`
declaration with no capability link at all, so absence proved nothing.
In this bounded profile the link is **mandatory when the token is not a
rich expression**, which is what makes its absence meaningful.

```turtle
rpm:DependencyDeclarationShape a sh:NodeShape ;
    sh:targetClass rpm:DependencyDeclaration ;

    sh:property [ sh:path rpm:declaringPackage ; sh:class pkg:Package ;
                  sh:minCount 1 ; sh:maxCount 1 ;
                  sh:message "A declaration must have exactly one declaring package."@en ] ,
                [ sh:path rpm:declarationKind ;
                  sh:in ( rpm:Provides rpm:Requires rpm:Conflicts rpm:Obsoletes ) ;
                  sh:minCount 1 ; sh:maxCount 1 ;
                  sh:message "A declaration must have exactly one kind from the enumeration."@en ] ,
                [ sh:path rpm:declaredToken ; sh:datatype xsd:string ;
                  sh:minCount 1 ; sh:maxCount 1 ;
                  sh:message "A declaration must record the name attribute as received."@en ] ,
                [ sh:path rpm:declaredCapability ; sh:class pkg:Capability ; sh:maxCount 1 ] ,
                [ sh:path rpm:declarationFlags ;
                  sh:datatype xsd:string ; sh:maxCount 1 ;
                  sh:in ( "EQ" "NE" "GE" "GT" "LE" "LT" ) ;
                  sh:message "Flags must be an RPM comparison operator."@en ] ,
                [ sh:path rpm:declarationEpoch ;   sh:datatype xsd:string ; sh:maxCount 1 ] ,
                [ sh:path rpm:declarationVersion ; sh:datatype xsd:string ; sh:maxCount 1 ] ,
                [ sh:path rpm:declarationRelease ; sh:datatype xsd:string ; sh:maxCount 1 ] ,
                [ sh:path rdfs:label ; sh:minCount 1 ;
                  sh:message "A declaration must have at least one label."@en ] ;

    # 1. A rich expression must not claim a capability link.
    sh:sparql [ sh:message "A declaration whose token is a rich expression must not have a declaredCapability."@en ;
                sh:select """
                  SELECT $this WHERE {
                    $this <https://purl.org/packagegraph/ontology/rpm#declaredToken> ?t .
                    $this <https://purl.org/packagegraph/ontology/rpm#declaredCapability> ?c .
                    FILTER(STRSTARTS(?t, "("))
                  }""" ] ,

    # 2. A plain token must have one.
              [ sh:message "A declaration whose token is not a rich expression must have exactly one declaredCapability."@en ;
                sh:select """
                  SELECT $this WHERE {
                    $this <https://purl.org/packagegraph/ontology/rpm#declaredToken> ?t .
                    FILTER(!STRSTARTS(?t, "("))
                    FILTER NOT EXISTS { $this <https://purl.org/packagegraph/ontology/rpm#declaredCapability> ?c }
                  }""" ] ,

    # 3. flags without ver.
              [ sh:message "A comparison flag requires a version."@en ;
                sh:select """
                  SELECT $this WHERE {
                    $this <https://purl.org/packagegraph/ontology/rpm#declarationFlags> ?f .
                    FILTER NOT EXISTS { $this <https://purl.org/packagegraph/ontology/rpm#declarationVersion> ?v }
                  }""" ] ,

    # 4. ver without flags.
              [ sh:message "A version requires a comparison flag."@en ;
                sh:select """
                  SELECT $this WHERE {
                    $this <https://purl.org/packagegraph/ontology/rpm#declarationVersion> ?v .
                    FILTER NOT EXISTS { $this <https://purl.org/packagegraph/ontology/rpm#declarationFlags> ?f }
                  }""" ] ,

    # 5. epoch without ver.
              [ sh:message "An epoch requires a version."@en ;
                sh:select """
                  SELECT $this WHERE {
                    $this <https://purl.org/packagegraph/ontology/rpm#declarationEpoch> ?e .
                    FILTER NOT EXISTS { $this <https://purl.org/packagegraph/ontology/rpm#declarationVersion> ?v }
                  }""" ] ,

    # 6. rel without ver.
              [ sh:message "A release requires a version."@en ;
                sh:select """
                  SELECT $this WHERE {
                    $this <https://purl.org/packagegraph/ontology/rpm#declarationRelease> ?r .
                    FILTER NOT EXISTS { $this <https://purl.org/packagegraph/ontology/rpm#declarationVersion> ?v }
                  }""" ] .

# A declaration is not a dependency, in this delivery.
rpm:DeclarationNotDependencyShape a sh:NodeShape ;
    sh:targetClass rpm:DependencyDeclaration ;
    sh:not [ sh:class pkg:Dependency ] ;
    sh:message "An RPM declaration must not be typed as a pkg:Dependency in this release."@en .
```

Constraints 3 to 6 together admit exactly rows A and B of the table
above. If you prefer `sh:xone` over six `sh:sparql` constraints, or
node-shape composition, the form is yours — the requirement is that all
four prohibited combinations, and both capability-link rules, **fail
validation** rather than being discouraged in prose.

### 3.6 Emission rules by kind — and what does not happen

Stated explicitly because an earlier draft of the platform design got
this wrong and said every simple declaration would also emit
`pkg:requiresCapability`, which would have turned "conflicts with X"
into "requires X".

| kind | capability shortcut emitted | declaration node |
|---|---|---|
| `rpm:Provides` | `pkg:providesCapability` | yes |
| `rpm:Requires` | `pkg:requiresCapability` | yes |
| `rpm:Conflicts` | **none** | yes |
| `rpm:Obsoletes` | **none** | yes |

**`rpm:Conflicts` and `rpm:Obsoletes` do not produce
`pkg:requiresCapability`, `pkg:providesCapability`, or any other
capability shortcut.** Conflicting with a capability is not requiring
it, and obsoleting a package is not depending on it. Their meaning is
carried by `rpm:declarationKind` alone. The same applies to
`rpm:Recommends` and the other weak-dependency kinds if and when they
are added: a weak dependency is not `requiresCapability`, and a separate
term or an explicit modality will be requested at that time rather than
folded into the strong one.

Capability shortcuts are emitted **only** when
`rpm:declaredCapability` is present — never for a rich dependency.

### 3.7 Fixtures requested

**Positive**, each must validate:

- an unversioned `Requires` — token only, no flags/epoch/ver/rel, with a
  `declaredCapability` (row A)
- a versioned `Requires` carrying `flags`, `epoch`, `ver` and `rel`
  (row B, fully populated)
- a versioned `Requires` carrying `flags` and `ver` only (row B, minimal)
- a `Provides` naming the declaring package's own name (self-provide)
- a `Conflicts` and an `Obsoletes`, each with no capability shortcut
  emitted alongside
- a rich-expression `Requires` — `declaredToken` beginning `(`, **no**
  `declaredCapability`
- **repeated name, different constraints**: one package with
  `Requires: foo >= 1` and `Requires: foo < 2`. Both declarations must
  be present and distinct, and each must carry its own single
  `declarationFlags`/`declarationVersion` pair. This is the fixture that
  pins §3.4; the current collector collapses exactly this case.

**Negative**, each must fail:

- a rich expression that also carries `rpm:declaredCapability`
  (constraint 1)
- a plain token with **no** `rpm:declaredCapability` (constraint 2)
- `declarationFlags` with no `declarationVersion` (constraint 3)
- `declarationVersion` with no `declarationFlags` (constraint 4)
- `declarationEpoch` with no `declarationVersion` (constraint 5)
- `declarationRelease` with no `declarationVersion` (constraint 6)
- a declaration typed both `rpm:DependencyDeclaration` and
  `pkg:Dependency`
- a declaration with no `declaredToken`
- a declaration with two `declarationKind` values
- a `declarationFlags` value outside `EQ`/`NE`/`GE`/`GT`/`LE`/`LT`

Reasoner check: `rpm:DependencyDeclaration` must not be entailed to be
a `pkg:Dependency`.

**Competency questions** worth representing, because they are what the
platform will run:

- *Which packages declare a requirement on capability X, and under what
  constraints?* — kind, capability, flags, epoch, version, release.
- *Which declarations record a rich expression rather than a capability
  link?* — the query that makes the 14,037 distinct rich expressions
  retrievable instead of lost.

The second question is deliberately phrased as **"no capability link
recorded"**, not "unresolved". Provider resolution is out of scope for
this delivery (§8), and a declaration with no capability link is a
statement about the token's syntax, not about whether any package
satisfies it.

---

## 4. Decision 2 — the RPM capability URI convention

**Requested: approve `d/capability/rpm/{encoded-token}`.**

`URI-POLICY.md`'s pattern table covers `d/pkg`, `d/src`, `d/ver`,
`d/distro`, `d/release` and `d/arch`. It has **no capability row**. The
platform has been minting `d/capability/{encoded-name}` without policy
cover, from two collectors — `rpm.rs:1606` and `debian.rs:1004` — using
the same encoder, so an RPM `Provides: foo` and a Debian virtual package
`foo` are one node today.

Proposed row:

| Type | Pattern | Example |
|---|---|---|
| Capability (RPM) | `d/capability/rpm/{encoded-token}` | `libssl.so.3()(64bit)` → `d/capability/rpm/libssl.so.3%28%29%2864bit%29` |

**Encoding:** percent-encoding with the unreserved set `A-Z a-z 0-9 - _
. ~` — everything else encoded, equivalent to Python
`quote(token, safe="")`. This is the platform's existing
`uris::encode` (`etl/pg-collect/src/uris.rs:51`), already used for every
other path segment in the policy table, so capabilities gain no special
case. Parentheses in SONAME tokens are encoded; nothing is lowercased,
normalised, or stripped, so the mapping is reversible.

**Scope.** The `URI-POLICY.md` row governs the RPM ingestion
convention: a node minted under `d/capability/rpm/` identifies *this
capability token within the RPM naming system*.

The accompanying note on `pkg:Capability` should be scoped to that
convention — an `rdfs:comment` or `skos:scopeNote` recording that **RPM
ingestion** mints naming-system-qualified capability identifiers and
what they do and do not assert. It must **not** amend
`pkg:Capability`'s `IAO:0000115` in a way that redefines every
capability, in every ecosystem, as an RPM token. `pkg:Capability` stays
general; only the RPM minting convention is pinned here. **Matching this identifier does not establish that a
provider is compatible with a requirer.** The same SONAME across
architectures, distributions or repositories is the same token and not
the same binary contract. Provider suitability is established by
resolution against a repository snapshot, which is out of scope for this
release.

This is deliberately narrow. It settles the collector's immediate
identity contract and makes no claim about what a capability means in
general, or about how Debian, Alpine, Arch or language ecosystems should
scope theirs. Those can be added as further policy rows when each is
needed. The Debian collector's existing use of the unqualified path is
noted here for your awareness and is not part of this request.

Deciding this late is expensive: 1,013,026 nodes and 79,953,540 edges
carry the current shape.

---

## 5. Decision 3 — correct the DD-VirtualPackage example

`docs/design-decisions.md:218` specifies `:VirtualPackage rdfs:subClassOf
:Capability` (`core/core.ttl:86`), then gives a worked example
(`design-decisions.md:232`) in which `pkg:provides` — `rdfs:domain
:Package ; rdfs:range :Package` (`core/core.ttl:1274`) — targets "a
VirtualPackage that is ALSO typed as Capability".

`AllDisjointClasses ( :Package … :Capability )` holds
(`core/core.ttl:1714`), so that example entails the target is both
`:Package` and `:Capability`. It is not expressible under the
ontology's own axioms.

It is latent, not live. I checked before reporting:

```sparql
SELECT (COUNT(DISTINCT ?v) AS ?n) WHERE { ?v a pkg:VirtualPackage }   # 0
SELECT (COUNT(*)  AS ?n) WHERE { ?s pkg:provides ?o . ?o a pkg:Capability }  # 0
```

**Requested: correct the example to use `pkg:providesCapability` for
`Capability` and `VirtualPackage` targets, and retain the existing
package-oriented property ranges in this release.** `pkg:provides` keeps
`rdfs:range :Package` and remains correct for genuine package-to-package
substitution. No axiom changes.

This matters to decision 1 because DD-VirtualPackage's worked example is
`mail-transport-agent` — the same case that drives the platform's rule
that identities are minted only from collected package records. If your
correction reads as above, the two designs agree.

Separately and not part of this request: the 107,822 existing
`pkg:provides` assertions are a platform bug — `alpine.rs:351` and
`arch.rs:343` write a **literal** object on an `owl:ObjectProperty`.
Ours to fix.

---

## 6. What is not being requested

- No change to `pkg:directlyProvides`. platform#62 proposes retargeting
  it at capability nodes; that would change a shared core term's meaning
  for every consumer, and is unnecessary — `pkg:providesCapability`
  already exists with the right range and 79,953,540 assertions. The
  producer stops emitting the wrong edge.
- No widening of `pkg:dependencyTarget`'s range.
- No declaration of the `rpm:rpmProvides` / `rpmRequires` /
  `rpmConflicts` / `rpmObsoletes` spellings. The collector stops
  emitting them; **four** entries leave the platform's `vocab.rs`
  `KNOWN_BAD` ratchet. (`rpm:RPMGroup`, the fifth RPM entry there, is an
  unrelated class-used-as-predicate defect and is not in this delivery.)
- No relaxation of `pkg:CapabilityShape`. The producer starts emitting
  `rdfs:label`.
- No metrics-module change. Affected RPM metrics are withheld, not
  redefined (§7).

---

## 7. Sequencing: what proceeds in parallel, and what gates publication

**G1 (decision 1 released, `ONTOLOGY_VERSION` bumped) is a publication
gate, not a development blocker.** Collector development proceeds
against the agreed proposal as soon as the terms are agreed in the
issue. Publishing changed RDF requires the released pin and passing
integration checks.

**G2 (decision 2) gates final identifiers and publication, not
acquisition.** Source repodata can be captured and preserved
immediately; only the minted capability URIs and the published graph
wait on the decision.

**G3 (decision 3)** needs an answer, not a release.

| Team | Work that can start now |
|---|---|
| Ontology | File this as an issue; prepare terms, shapes, fixtures, the `URI-POLICY.md` row and the DD-VirtualPackage correction |
| Collectors | Build source-preserving fixtures from real repodata; implement against the agreed contract; fix capability labels, self-provides, versioned provides, and the incorrect predicate emission |
| Consumers | Withhold affected RPM package-level metrics **visibly**; identify and retire stale derived results |
| Collection / index | Capture immutable source metadata; attribute legacy differences; prepare replacement-generation validation |

### Transition plan

Not a purge. **Collect replacements → validate → switch, retaining
rollback data.**

Four Fedora graphs — `fedora/42`, `fedora/42/aarch64`,
`fedora/44/aarch64`, `fedora/rawhide` — predate the capability emission
and carry no `pkg:Capability` nodes at all, so their provenance and
shape cannot be attested. They are replaced, in this order:

1. Collect replacement generations from source, under the new contract.
2. Validate each replacement against the shapes and against the legacy
   generation, attributing every difference rather than accepting the
   delta wholesale.
3. Switch the served union to the replacement set.
4. Retain the legacy generations and the prior index for rollback; they
   are removed only after the replacement set has been verified in
   service.

At no point is the only copy of a graph deleted before its replacement
is validated.

### Consumer handling

RPM package-level reverse-dependency metrics are **withheld visibly**
until resolution semantics exist, rather than silently recomputed over a
changed input. `met:reverseDependencyCount` derives from
`pkg:directlyDependsOn` into `pkg:PackageIdentity`
(`etl/pg-collect/src/enrich_revdeps.rs:103`), and the requires path
currently mints an identity for every required name — including all
23,587 rich-expression identity nodes. When that stops, the counts change.
Withholding needs no ontology change.

The platform will **not** project `pkg:directlyDependsOn` merely because
a collected package identity shares the declared name. A name match
does not establish that the package satisfies the declaration's version
constraint or applies in its architecture and repository context. Until
resolution is modelled, that projection is withheld too.

platform#62 **stays open** until the corrected collector output and the
published graph are both verified. This request is not a closing
argument for it.

---

## 8. Deferred items

Not prerequisites for this delivery, and listed so they are not
mistaken for scope. **None of these has an issue yet** — there are no
open ontology issues, and no corresponding platform issue. They are
deferred items recorded here, not filed work; each needs an issue before
it can be tracked, and this document is not that issue.

- **Resolution as an evidenced result** — candidate providers,
  resolver-selected providers, no-provider-found, and
  resolution-not-performed are four distinct outcomes, with repository
  snapshot, architecture, resolver version and policy as evidence.
- **Whether declarations should eventually unify with `pkg:Dependency`.**
  Deliberately left open. This delivery neither forecloses it nor argues
  against it.
- **Weak dependencies** — `<rpm:recommends>`, `<rpm:suggests>`,
  `<rpm:supplements>`, `<rpm:enhances>`; additive when needed.
- **Capability scoping for non-RPM ecosystems**, including the Debian
  collector's current use of the unqualified capability path.
- **`rpm:RPMGroup` used as a predicate** — the remaining RPM entry in
  the platform's `KNOWN_BAD` ratchet.

---

## 9. Acceptance

Decision 1 is delivered when, at a tagged release: `make lint validate
reason test-cq` passes; the fifteen terms and two node shapes resolve, and the terms appear
in generated docs; the positive fixtures in §3.7 validate; every negative
fixture in §3.7 fails; and `rpm:DependencyDeclaration` is not entailed
to be a `pkg:Dependency`.

Decision 2 is delivered when `URI-POLICY.md` carries the capability row
with its encoding, and `pkg:Capability`'s definition states the scope
and the non-compatibility caveat.

Decision 3 is delivered when DD-VirtualPackage's example uses
`pkg:providesCapability`.

Every figure in this document is a `POST` to
`https://packagegraph.di.riseproject.dev/` with `Accept: text/csv`. If
a measurement does not reproduce, treat the argument resting on it as
withdrawn.
