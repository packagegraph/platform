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
from "empty". (`sh:targetNode` and `sh:targetSubjectsOf` reach focus nodes
another way and are not affected by this; the claim is about class-targeted
shapes only.)

The audit therefore counts focus nodes before it trusts a conformance result,
and reads `primary.xml` with the Python standard library rather than through
the collector — comparing two outputs of the same parser is not independence.

## Release gates

Each gate fails the run and exits nonzero. Run:

```sh
etl/scripts/audit-rpm-contract.py \
    --primary <repodata>/primary.xml.gz \
    --rdf <graph>.nt \
    --ontology-root ../ontology \
    --manifest <extract>/manifest.json \
    --report acceptance-report.json
```

| Gate | Passes when | Defect it would have caught |
|---|---|---|
| `fidelity` | Every non-suppressed `Provides` token in the source is a capability in the graph, and every capability in the graph traces back to a source token | A capability layer silently narrower than the source |
| `edges` | The `providesCapability` edge count equals the source's `Provides` occurrences less the suppressed ones, exactly | A lost per-provider edge. Distinct tokens can all agree while an edge is missing, because another package still provides the same token |
| `types` | `rdf:type` is present for packages, identities and capabilities; no capability lacks an `rdfs:label` | All 1,013,026 capability nodes failed `CapabilityShape`'s label requirement while SHACL reported conformance |
| `encoding` | No name-bearing literal carries an XML entity reference | 9,919 of `fedora/43`'s identity names contained `&gt;` |
| `prohibited` | Neither `rpm:rpmProvides` nor `pkg:directlyProvides` is emitted on the provides path | `rpmProvides` was undeclared in every ontology module; `directlyProvides` has `rdfs:range :Package`, so pointing it at a capability token entailed `:Package` membership |
| `policy` | The tokens the collector documents as suppressed are absent, and the audit's mirrored prefix list still matches `rpm.rs` | The collector drifting from its own stated policy in either direction |
| `budget` | `total_triples <= max_triples` in the manifest | A run wrote 10,608,101 triples against a 3,000,000 ceiling and exited 0 |
| `shacl` | Conforms with `inference="none"` | — reported as `not run`, never as a pass, when pySHACL or the shapes are unavailable |

`encoding` deliberately gates only on `capabilityName`, `identityName`,
`packageName` and `rdfs:label`. A description can legitimately contain the text
`&gt;`; entity references outside the name predicates are counted as
`entity_in_prose` and gate nothing.

`policy` covers `config(`, `rpmlib(` and `rtld(`. Suppressing them is a bounded
policy choice, not a semantic claim: `glibc` really does provide
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
| `fidelity` | 46,547 tokens agree; 0 source-only, 0 graph-only |
| `edges` | 3,210,293 edges = 3,210,726 source occurrences − 433 suppressed, exactly |
| `types` | 11,984 typed packages, 27,850 typed identities, 46,547 typed capabilities, 0 unlabelled |
| `encoding` | 0 name literals carry an entity reference; 0 outside the name predicates |
| `prohibited` | 0 `rpm:rpmProvides`, 0 `pkg:directlyProvides` |
| `policy` | 1,632 suppressed occurrences in the source, 0 in the graph |
| `parse` | 0 of 3,755,319 lines matched neither N-Triples form |

`parse` exists because the first version of this audit did not have it. Its
subject pattern accepted only IRIs, so it silently skipped the 88,932
blank-node triples the RPM collector writes for reified dependencies and
version constraints — whose literals come from the same `ver`/`rel` source
attributes the decoding bug affected — and still reported zero undecoded
entities. A line the audit cannot read is a line it is not checking, so an
unreadable line now fails the run.

### Against the pre-fix collector

The same audit was run against output from `1aa0f12`, the commit before the XML
decoding fix, over identical repodata. It exits 1:

| Gate | Result at `1aa0f12` |
|---|---|
| `encoding` | **FAIL** — 3,606 name literals carry an entity reference, e.g. `identityName "(NetworkManager &gt;= 1.20 or dhclient)"` |
| `fidelity` | **FAIL** — 44,821 tokens agree; 1,726 source-only and 1,726 graph-only, the escaped variants standing in for the real ones |
| `edges`, `types`, `policy`, `prohibited`, `parse` | pass |

That `edges` and `types` pass here is the useful part: the edge count and the
type coverage are identical either way, because only the names were wrong. One
gate is not enough.

The audit reads the 890 MB corpus in 21 seconds with the token comparison done
as an external sorted merge, so neither side has to fit in memory.

Two caveats on that run, stated because they are substitutions:

- `filelists.xml.gz` was not preserved and 404s. The collector degrades past it
  with a warning, losing only phantom detection.
- `updateinfo.xml.gz` was not preserved either, and the collector does **not**
  degrade past a 404 on it — the graceful path covers repomd.xml having no
  `updateinfo` entry, not the download failing. An empty `<updates/>` document
  was synthesized for that href. It is inert for every gate here, all of which
  concern the provides/capability layer, but it is not the repository's bytes.

SHACL was not run against this corpus. 3.7M triples through rdflib is not
practical, and the report records `shacl` as absent rather than passing. The
class-targeted vacuity problem is covered by the `types` gate, which is the
point of having it.

## What is not checked

Reported as `not_run` with a reason, never as a pass:

- **Declaration occurrences.** `Requires`, `Conflicts` and `Obsoletes` source
  counts are reported; the graph side reads `null`, not `0`. There is no term
  to count against until `rpm:DependencyDeclaration` is decided.
- **`pre` on a declaration.** Same reason; `rpm:declarationPre` is proposed,
  not agreed.
- **Obsoletes semantics.** `Obsoletes` matches package Name/EVR, not arbitrary
  `Provides`. The current emission treats it as an identity reference, which is
  closer to right than a capability would be but is not the kind-specific
  handling the plan proposes.
- **GLSA.** `collect_glsa.rs` has the same undecoded-attribute pattern that
  `rpm.rs` was fixed for. Tracked separately; not a blocker for the RPM graphs.
- **`pkg:provides` as a literal.** `alpine.rs:351` and `arch.rs:343` call
  `write_literal` on an `owl:ObjectProperty`. Different collectors, tracked
  separately.

## Test coverage of the gates

`etl/scripts/tests/test_audit_rpm_contract.py` takes a clean fixture pair and
breaks exactly one thing per test, asserting the matching gate goes red. A gate
that cannot fail is decoration. 22 tests, all of which fail if the gate they
target is removed.

The fixture deliberately contains a capability two packages both provide
(`bundled(gnulib)`). Without one, no mutation can express a lost provider edge
whose token still has a provider, which is the only defect `edges` catches on
its own.

The mutation that motivates the whole exercise deletes every
`rdf:type pkg:Capability` triple and asserts two things at once: pySHACL
reports the result as **conforming** against the real `pkg:CapabilityShape`,
and the `types` gate **fails**. The fixture retains its capability names and
provides edges through that mutation, so the graph still looks populated —
which is exactly the state the live corpus was in.

That test requires pySHACL. Without it the test **skips**; it does not pass. CI
installs pySHACL so it runs there.

## What acceptance does not authorise

Passing every gate on a freshly collected corpus is the evidence for replacing
the live graphs. It is not the decision. Production replacement stays gated on:

1. the ontology#19 amendments being decided, not just proposed;
2. a full collection run of the affected RPM graphs, audited green;
3. the existing rollback pair retained until the replacement has been queried.

The reverse-dependency consumer guard (`rpm_contract_guard.rs`) is prepared but
not deployed: `withhold_unresolved` defaults to `false`, so today it reports and
changes nothing. Turning it on is a separate decision, because on measured data
it withholds 85–92% of RPM reverse-dependency results against 1.0% for Alpine.
