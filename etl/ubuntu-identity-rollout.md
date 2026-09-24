# Correcting Ubuntu collector identity

The Ubuntu Noble entry points must pass `--distro ubuntu`. `pg-collect debian`
selects the repository format; its default **distribution** is `debian`, even
when the repository's Release file says `Origin: Ubuntu`. The host wrappers and
their Kubernetes counterparts previously omitted the distribution argument.

This fix changes package, identity, source, version, distribution and release
IRIs, as well as the PURL namespace. It is not a PURL-only rewrite. For example:

| Entity | Old, incorrect value | Correct value |
| --- | --- | --- |
| Identity IRI (under `https://packagegraph.github.io/d/`) | `pkg/debian/noble/amd64/acl` | `pkg/ubuntu/noble/amd64/acl` |
| Identity PURL | `pkg:deb/debian/acl?arch=amd64` | `pkg:deb/ubuntu/acl?arch=amd64` |

The graph URIs remain `https://packagegraph.github.io/graph/ubuntu/noble`,
`.../ubuntu/noble/arm64` and `.../ubuntu/noble/riscv64`. The collector's format
identifier in data-quality provenance may still be `debian`; that does not
identify the package distribution and is intentionally unchanged.

## Before deployment

1. Save the previous complete graph artifacts, sidecars, image revision and
   installed wrappers. Follow the coordinated collector maintenance procedure;
   a source merge does not install host-mounted scripts. The checkpoint release
   tool stages RPM wrappers, **not these Ubuntu wrappers**: explicitly stage and
   compare all three Ubuntu scripts from the reviewed revision as well. Preserve
   existing enablement/masks and private seed inputs.
2. Re-run the reference audit. It has been run once already -- see
   [Audit result](#audit-result-2026-09-24) for the method, the queries and
   the measured numbers -- but the served index moves, so repeat it against
   current data immediately before replacement. Audit **both directions**: the
   object-position query returns zero here while the subject-position one
   returns 43,559 derived facts, so an object-only audit reads as "safe" and
   is not. A timeout or error is **unknown**, not evidence of zero references.
   The served index may be older than the retained artifacts, so check both.
3. Recollect the three Ubuntu graphs using the same repositories, release,
   component and architecture, with the corrected distribution argument. Each
   wrapper now mints its own run directory (#69), so concurrent runs no longer
   share `/tmp/packages.nt` -- but that fix is host-installed, not baked into
   the image, so confirm the deployed wrappers carry it before relying on it.
   Do not upload until complete-graph validation passes.
4. Run the pinned ontology's `python -m packagegraph.purls --profile deb` on
   each complete candidate. That profile checks `deb` syntax and roles, **not
   whether the vendor is Ubuntu**. Also require Ubuntu package/source/version/
   release IRIs, `pkg:deb/ubuntu/` PURLs, and no old Noble coordinates. Compare
   binary, source, identity, architecture and dependency counts with the old
   graph and current upstream inputs; investigate unexplained losses.
5. Regenerate `enrichment/taxonomy` and `enrichment/revdeps` after the three
   replacements and before the result is treated as complete. This step is
   **required**, not conditional: the audit found 26,064 classifications and
   17,495 reverse-dependency counts attached to the old identities, and
   replacing the Ubuntu graphs alone strands every one of them. Do not
   globally replace
   `/debian/` (real Debian data exists), invent `owl:sameAs` aliases, or retain
   stale links by pretending the namespace change is cosmetic. If a consumer
   cannot be safely updated, stop and agree the migration with its owner.
6. Replace entire graphs through the existing upload path; do not append the
   new namespace alongside the old one. Rebuild only after all required graph
   replacements and the matching ontology have been validated. Verify the
   promoted index is actually loaded, repeat the reference/identity checks,
   and restore only the previously active schedules. Keep rollback exports.

## Audit result, 2026-09-24

Run against the served endpoint at QLever index date; **58 graphs, 374,402,409
triples**. Both directions were checked, because they do not agree.

### Object position: zero

No graph outside the three Ubuntu graphs references the old
`d/{pkg,src,ver,release}/debian/noble*` identities as an object.

```sparql
SELECT ?g2 ?p (COUNT(*) AS ?refs) WHERE {
  { SELECT DISTINCT ?old WHERE {
      GRAPH <https://packagegraph.github.io/graph/ubuntu/noble> { ?old ?p1 ?o1 }
      FILTER(STRSTARTS(STR(?old), "https://packagegraph.github.io/d/pkg/debian/noble/")
          || STRSTARTS(STR(?old), "https://packagegraph.github.io/d/src/debian/noble/")
          || STRSTARTS(STR(?old), "https://packagegraph.github.io/d/ver/debian/noble/")
          || STRSTARTS(STR(?old), "https://packagegraph.github.io/d/release/debian/noble"))
  } }
  GRAPH ?g2 { ?s ?p ?old }
  FILTER(?g2 NOT IN (
    <https://packagegraph.github.io/graph/ubuntu/noble>,
    <https://packagegraph.github.io/graph/ubuntu/noble/arm64>,
    <https://packagegraph.github.io/graph/ubuntu/noble/riscv64>))
} GROUP BY ?g2 ?p ORDER BY DESC(?refs)
```

Bind the graph explicitly and run one Ubuntu graph at a time. An unbound
`GRAPH ?g` with a `VALUES` list makes the planner scan the whole dataset and
the proxy returns 504 at 60s — which is **unknown**, not zero. Bound, each of
the three completes in 44-47s.

### Subject position: 43,559 derived facts

**This is the direction that matters, and the object-position query above
cannot see it.** The enrichers do not point *at* package identities; they
attach facts *to* them, as subjects:

```sparql
  # ... same DISTINCT ?old subquery ...
  GRAPH ?g2 { ?old ?p ?o }          # subject, not object
```

| Source graph | `enrichment/taxonomy` `hasClassification` | `enrichment/revdeps` `reverseDependencyCount` |
| --- | --- | --- |
| `ubuntu/noble` | 8,960 | 5,969 |
| `ubuntu/noble/arm64` | 8,789 | 5,884 |
| `ubuntu/noble/riscv64` | 8,315 | 5,642 |
| **total** | **26,064** | **17,495** |

No other graph holds subject-side facts about these identities.
`enrichment/blast-radius` keys on `d/cve/*` and is unaffected;
`enrichment/{epss,advisory-dsa,advisory-rhsa,npm-provenance,forge-version}`
returned nothing.

### What this means for the migration

Replacing the three Ubuntu graphs alone **orphans all 43,559 facts**. They stay
in `enrichment/taxonomy` and `enrichment/revdeps`, attached to identities that
no longer exist anywhere, and every classification and reverse-dependency count
for Ubuntu Noble silently disappears from any query that starts at a package.
So step 5 of the procedure below is not conditional here: it is required.

`enrichment/taxonomy` and `enrichment/revdeps` must be regenerated after the
three graphs are replaced and before the result is treated as complete. Both
are whole-graph replacements through the same upload path, so re-running their
enrichers is sufficient; no surgical deletion is needed, and none should be
attempted.

### What this audit does NOT cover

- **Retained source artifacts in object storage.** Checked from the served
  index only; this session had no object-store credentials. The index can lag
  `nt-output/`, so re-run the same two queries, or inspect the artifacts
  directly, immediately before replacement.
- **Consumers outside the served dataset.** External caches, exports and
  downstream copies cannot be certified by SPARQL over this endpoint.
- A timeout or error is **unknown**, never zero. Every figure above came back
  as a completed result; none was inferred from a failed query.

The audit deliberately excludes the shared `distro/debian` node: it also
describes genuine Debian releases and must not be renamed globally.

## Regression gate

```sh
python3 -m pip install PyYAML==6.0.3
cargo build --manifest-path etl/pg-collect/Cargo.toml --locked
PG_COLLECT_BIN="$PWD/etl/pg-collect/target/debug/pg-collect" \
  python3 deploy/quadlet/collectors/tests/test_ubuntu_identity.py -v
```

The gate executes all six real entry points against a local repository fixture
using the real collector binary. It checks versionless identity and versioned
binary/source PURLs, epoch encoding, architecture-independent packages and
Ubuntu resource namespaces. The adapter redirects repository/output paths and
stops before upload. Removing the distro argument from any entry point must
fail on its emitted RDF. CI invokes the test directly, so moving or deleting it
cannot silently produce an empty discovery run.

Implementation and passing tests do not constitute deployment or migration.
Historical RPM/Debian graphs without recoverable source mappings remain outside
this fix; see [PURL recollection](purl-recollection.md).
