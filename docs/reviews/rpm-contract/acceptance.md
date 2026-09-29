# RPM capability contract — acceptance criteria

**Status: proposed.** These are the criteria the platform side offers for
replacing the live RPM graphs. They are not an approved specification. The
ontology-side questions they depend on are open in
[ontology#19](https://github.com/packagegraph/ontology/issues/19); the audit
evidence and the proposed plan are in
[ontology#20](https://github.com/packagegraph/ontology/pull/20).

Scope: platform#62, PR #110. Owner: platform. Every criterion below is either
checked mechanically by `etl/scripts/audit-rpm-contract.py` or named here as
explicitly unchecked.

## Why a separate audit at all

A SHACL run over the corpus was reporting conformance while the corpus was
missing the capability types the shapes target. `sh:targetClass` with no focus
nodes conforms, so a class-targeted validation cannot distinguish "correct"
from "empty".

But that is only one of the ways a conformance boolean fails to answer the
question, and the audit needs all three kept apart.

### Two distinctions the audit is built around

**Relations are not serialized occurrences.** RDF is a set of triples. The
collector writes one `providesCapability` line per source occurrence without
deduplicating, and AlmaLinux 9 BaseOS declares 566 `(package, token)` pairs
twice — so 3,210,293 edge lines carry 3,209,727 distinct relations. Checking
only the total catches a lost line but not a capability credited to the wrong
package; checking only the relation set catches the wrong provider but not a
lost duplicate. `edges` and `pairs` are separate gates for that reason.

**Explicit types are not entailed membership.** `pkg:capabilityName` has
`rdfs:domain pkg:Capability` (`core.ttl:186`) and `pkg:providesCapability` has
`rdfs:range pkg:Capability` (`core.ttl:726`). Under the regime the ontology
declares, those axioms **restore** capability membership for every node whose
type triple was deleted. Measured on the type-erased fixture:

| Mutation | Inference | Asserted focus | Effective focus | Conforms |
|---|---|---:|---:|---|
| Erase all 6 capability types | `none` | 0 | 0 | true |
| Erase all 6 capability types | `rdfs` | 0 | **6** | true |
| Erase those types and one label | `none` | 0 | 0 | true |
| Erase those types and one label | `rdfs` | 0 | **6** | **false** |

The third row is why the second row conforms: under `rdfs` the nodes come
back and genuinely satisfy the shape. Remove a required field as well and
validation fails, which is how we know the focus nodes are real rather than
absent. `none` conforms for the opposite reason — there is nothing to check.

So neither regime can say whether the type triples were **written down**. That
is a serialization question. The `coverage` gate is what answers it, by
comparing source-derived expected subject sets against asserted subjects
before any inference. The audit reports the asserted and effective focus
counts beside every conformance result rather than inferring either from a
boolean.

The audit also reads `primary.xml` with the Python standard library rather
than through the collector — comparing two outputs of the same parser is not
independence.

## Release gates

Each gate fails the run and exits nonzero. Run:

```sh
etl/scripts/audit-rpm-contract.py \
    --primary <repodata>/primary.xml.gz \
    --rdf <graph>.nt \
    --ontology-root ../ontology \
    --manifest <extract>/manifest.json \
    --repo-root . \
    --report acceptance-report.json
```

| Gate | Passes when | Defect it would have caught |
|---|---|---|
| `pairs` | Every `(declaring package, capability token)` relation the source states is in the graph and the reverse, with no edge whose ends cannot be named | A capability credited to the wrong package. Token inventories and all counts are unchanged by that mutation |
| `edges` | `providesCapability` line count equals the source's non-suppressed occurrences, exactly | A lost duplicate occurrence, which the relation set cannot see |
| `coverage` | Every capability token, binary package and package identity the source implies carries its `rdf:type` explicitly — expected sets derived from the source, compared against asserted subjects before inference, counting distinct nodes | One capability's type triple deleted, or every `PackageIdentity` type deleted. A nonzero check sees neither |
| `types` | Nonzero asserted nodes per gated class, and every asserted capability has both an `rdfs:label` and a `capabilityName` | All 1,013,026 capability nodes failed `CapabilityShape`'s label requirement while SHACL reported conformance |
| `encoding` | No name-bearing literal carries an XML entity reference | 9,919 of `fedora/43`'s identity names contained `&gt;` |
| `prohibited` | Neither `rpm:rpmProvides` nor `pkg:directlyProvides` is emitted | `rpmProvides` was undeclared in every ontology module; `directlyProvides` has `rdfs:range :Package`, so pointing it at a capability token entailed `:Package` membership |
| `parse` | Every line matched one of the two N-Triples forms | 88,932 blank-node triples silently skipped while the audit reported zero undecoded entities |
| `policy` | The tokens the collector documents as suppressed are absent | The collector drifting from its own stated policy |
| `keys` | The audit's mirrored copies of collector facts still match `rpm.rs` and `extract.rs` | A silently-broken derivation. Missing collector source reports `not_run`, never agreement |
| `budget` | `total_triples <= max_triples` in the manifest, both keys present | A run wrote 10,608,101 triples against a 3,000,000 ceiling and exited 0. A manifest missing either key fails rather than reading as unlimited |
| `shacl` / `shacl_rdfs` | Conforms under each regime, reported with asserted and effective focus counts | — `not_run`, never a pass, when pySHACL or the shapes are unavailable |

### Counting nodes, not type assertions

The collector re-emits an identity's definition on every reference
(`write_package_identity`, not the `_once` variant), so BaseOS carries 27,850
`PackageIdentity` type assertions over 3,787 nodes, and 11,984 package-class
assertions over 2,996 package nodes. Counting assertions reported four times
as many packages as there were packages. Every class count in the report is
distinct nodes.

### Mirrored derivations

Three facts about the collector are transcribed rather than imported, because
an audit that reads the collector's own definitions cannot detect the
collector drifting from its stated behaviour:

- the suppression prefixes `config(`, `rpmlib(`, `rtld(`;
- the version-string format `{ver}-{rel}.{arch}` (`rpm.rs:1061`), which the
  `pairs` key depends on — the package name alone is not a key, because BaseOS
  ships several versions of 475 `(name, arch)` pairs and a name-keyed
  comparison collapsed 3,209,727 relations into 107,256;
- the `Manifest` field names in `extract.rs`.

The `keys` gate holds all three to the source.

`encoding` deliberately gates only on `capabilityName`, `identityName`,
`packageName` and `rdfs:label`. A description can legitimately contain the
text `&gt;`; entity references outside the name predicates are counted as
`entity_in_prose` and gate nothing.

`policy` covers `config(`, `rpmlib(` and `rtld(`. Suppressing them is a
bounded policy choice, not a semantic claim: `glibc` really does provide
`rtld(GNU_HASH)` and 1,199 packages really do require it. The gate checks that
the code and the documented policy agree, not that the policy is right.

## Measured against real collector output

The fixture pair proves the gates can fail. It does not prove the collector
passes them, so the audit was run against the current collector's output over
the preserved AlmaLinux 9 BaseOS repodata (`primary.xml.gz`
`efbe328b6173f71e…`, uncompressed `2991b157fe0a1edc…`, both matching
`MANIFEST.sha256` in the evidence archive).

2,996 packages, 3,755,319 triples, 890 MB. Every gate passed:

| Gate | Result |
|---|---|
| `pairs` | 3,209,727 relations agree; 0 source-only, 0 graph-only, 0 unnameable edges |
| `edges` | 3,210,293 lines = 3,210,726 source occurrences − 433 suppressed, exactly |
| `coverage` | 46,547/46,547 capability types, 2,996/2,996 binary packages, 3,787/3,787 identity nodes, 3,232/3,232 identity names |
| `types` | 2,996 package nodes, 3,787 identity nodes, 46,547 capability nodes, 0 unlabelled, 0 unnamed |
| `encoding` | 0 name literals carry an entity reference |
| `prohibited` | 0 `rpm:rpmProvides`, 0 `pkg:directlyProvides` |
| `parse` | 0 of 3,755,319 lines unreadable |
| `policy` | 1,632 suppressed occurrences in the source, 0 in the graph |
| `keys` | all three mirrored derivations match |

Two passes over the corpus, 37 seconds, 651 MB peak RSS. The pair comparison
is an external sorted merge, so the 3.2M-relation sets never sit in memory;
the in-memory maps are bounded by distinct names, not occurrences.

### Against the pre-fix collector

The same audit against output from `1aa0f12`, the commit before the XML
decoding fix, over identical repodata. It exits 1:

| Gate | Result at `1aa0f12` |
|---|---|
| `pairs` | **FAIL** — 3,204,577 agree; 5,150 source-only and 5,150 graph-only |
| `coverage` | **FAIL** — 1,726 capability types and 8 identity names missing |
| `encoding` | **FAIL** — 3,606 name literals carry an entity reference |
| `edges`, `types`, `policy`, `prohibited`, `parse`, `keys` | pass |

Two things are worth noting. `edges` and `types` pass, because the line count
and the type coverage are identical either way — only the names were wrong.
And `coverage` fails while every **cardinality** matches (46,547 asserted for
46,547 expected): each escaped variant substitutes for a real token, so the
sets differ where the counts agree. That is why the gate compares sets and why
its message leads with the missing counts rather than the totals.

## What the report says about its own scope

`pass: true` on its own invites the reading that everything was checked, so the
report carries the scope as data:

- `per_kind` gives `source`, `rejected_by_policy`, `expected`,
  `declared_lines`, `declared_relations` and `status` for all four sections.
  The two `declared_*` fields are `null` — not `0` — for the three kinds with
  no term to assert, because a consumer cannot tell "none found" from "never
  looked" otherwise.
- `coverage` lists `kinds_compared`, `kinds_not_compared`,
  `classes_held_to_source`, `classes_counted_only`, `gates_run`,
  `gates_not_run`, and whether SHACL and the budget check ran at all.

On the BaseOS run: `provides` compared at 3,210,293 lines / 3,209,727
relations; `requires` 24,619 in source with 1,199 rejected and `null`
declared; `conflicts` 560; `obsoletes` 874.

## What is not checked

Reported as `not_run` with a reason, never as a pass:

- **Declaration occurrences.** `Requires`, `Conflicts` and `Obsoletes` source
  counts are reported; the graph side reads `null`. There is no term to count
  against until `rpm:DependencyDeclaration` is decided.
- **`pre` on a declaration.** Same reason; `rpm:declarationPre` is proposed,
  not agreed.
- **Obsoletes semantics.** `Obsoletes` matches package Name/EVR, not arbitrary
  `Provides`.
- **Dependency resolution of any kind.** Nothing here establishes that a
  requirement has a provider or that a version constraint is satisfiable. See
  the next section.
- **`SourcePackage`, `SourceRPM`, `BinaryRPM`, `Version` coverage.** Counted
  and reported, not held to an expectation: a source RPM is shared across its
  subpackages (545 nodes for 2,996 packages) and the audit has no independent
  derivation of the version-node set.
- **GLSA.** `collect_glsa.rs` has the same undecoded-attribute pattern that
  `rpm.rs` was fixed for. Tracked separately.
- **`pkg:provides` as a literal.** `alpine.rs:351` and `arch.rs:343` call
  `write_literal` on an `owl:ObjectProperty`. Different collectors, tracked
  separately.

## Reverse-dependency metrics are not addressed here

The consumer guard that was previously on this branch has been removed from
it (`85388b8`) and re-prepared, corrected, in
[#111](https://github.com/packagegraph/platform/pull/111). Its criterion —
whether some package links to a target identity via `pkg:isVersionOf` —
detects an **orphan target** and nothing more. It does not establish
resolution: a requirement `foo >= 2` against a graph containing only `foo`
version 1 satisfies that check, so the guard reported complete support and
emitted a count.

Calling that "resolution" would justify a metric nobody measured. Until a
bounded policy for unsupported RPM impact metrics is agreed, the RPM
reverse-dependency numbers stay unaddressed rather than approximated, and
nothing in this branch changes them.

## Test coverage of the gates

`etl/scripts/tests/test_audit_rpm_contract.py` takes a clean fixture pair and
breaks exactly one thing per test, asserting the matching gate goes red. A gate
that cannot fail is decoration. 40 tests.

Four of those mutations are regression cases: each passed every gate of an
earlier version of this audit.

- a capability reassigned from `bash` to `glibc` — now fails `pairs` **only**,
  since nothing else about the graph changes;
- one capability's type triple deleted — now fails `coverage`;
- every `PackageIdentity` type deleted — now fails `coverage` and `types`;
- every `Capability` type deleted — now fails `coverage`, with the RDFS focus
  restoration measured rather than assumed, and the missing-label negative
  control retained.

The fixture is a faithful miniature in the respects the gates check: it
carries a capability two packages both provide, a dependency target two
packages both name (so identity type assertions exceed identity nodes, as they
do at corpus scale), the `pkg:hasVersion` and `pkg:Version` nodes the
collector emits for both binary and source packages, and the legacy
`rpm:rpm*` predicates that are still part of today's contract. A fixture that
is not faithful produces findings about the fixture.

The SHACL tests require pySHACL and an ontology checkout; without either they
**skip**, they do not pass. CI installs pySHACL, fetches the shapes at the
`etl/ONTOLOGY_VERSION` pin, and fails the step on any skip.

## What acceptance does not authorise

Passing every gate on a freshly collected corpus is the evidence for replacing
the live graphs. It is not the decision. Production replacement stays gated on:

1. the ontology#19 amendments being decided, not just proposed;
2. a full collection run of the affected RPM graphs, audited green;
3. the existing rollback pair retained until the replacement has been queried.
