# PURL collection and recollection

The RPM, Debian, and Maven emitters put a canonical **versionless** `pkg:purl`
on each collected package identity and a canonical **versioned** PURL on its
concrete package. PURLs are typed `xsd:anyURI`. Loading several versions into
one graph therefore leaves one distinct identity PURL and separate package
PURLs. Existing resource URIs, version resources, source links, dependency
constraints and provenance remain intact.

The same placement applies to RPM/Debian `emit-from-ir` replay when source
coordinates are present. The IR path does not synthesize a missing source version.

| Collection profile | Completeness boundary | PURL convention |
| --- | --- | --- |
| `rpm` | `rpm:BinaryRPM`, its `pkg:isVersionOf` identity, and `pkg:builtFromSource` source | `pkg:rpm/vendor/name@version-release?arch=...&epoch=...`; omit a zero/unknown epoch; source architecture is `src` or `nosrc` |
| `deb` | `deb:BinaryPackage`, its identity, and its source | `pkg:deb/vendor/name@version?arch=...`; use declared package architecture (including `all`), falling back to repository architecture; retain the full Debian version including epoch and binNMU; source architecture is `source` and its version comes from `Source` when supplied |
| `maven` | `maven:MavenArtifact`, its identity, and resolved dependency identities | `pkg:maven/groupId/artifactId@version`; identity and dependency PURLs omit the version; group/artifact case is preserved |

The identity PURL excludes the RPM epoch as well as the version; otherwise
epoch changes would give the same identity multiple PURLs. RPM source filenames
do not supply a source epoch, so the collector does not guess one from the
binary epoch. Legacy `.nosrc.rpm` source URIs and `versionString` values are
preserved; their PURL uses the correct version-release and `arch=nosrc`.

Some existing subject keys are insufficient: RPM binary IRIs omit epoch, RPM IR
source IRIs omit release, and Debian direct-collector identities use repository
architecture. Distinct valid PURLs can therefore collide on a legacy subject.
The writer returns `InvalidData` on a second distinct PURL for a subject, so the
collection fails rather than publishing an ambiguous artifact. This guard
preserves the existing IRIs; it does **not** repair those identity-key defects.
It stores SHA-256 subject/value digests (64 bytes per entry before hash-table
overhead), applies within one writer/artifact, and allows identical statements.
Separate files, appended graphs, and raw checkpoint replay still need complete-
graph profile validation. Investigate collisions and retain the failed output
for diagnosis; do not upload it or discard one of its values to force validation.

RPM and Debian dependency/provides identities can denote capabilities, virtual
packages, or unresolved targets. Other ecosystem collectors also use the shared
identity helper without sufficient PURL coordinates. None of these receives a
fabricated PURL, and none is covered by the completeness requirement unless an
actual supported collected package identifies it. All existing PURLs remain
subject to semantic validation even outside these profiles.

## Deploy and recollect

Ship the collector with the matching ontology revision, where `pkg:purl` has
domain `pkg:PackageEntity`; the old identity-only domain would infer concrete
packages to be identities. The platform's `etl/ONTOLOGY_VERSION` pin must refer
to that revision.

This change pins ontology commit `5840cf33ada5bdf814b1dd6c811692cc3e30189e`
from the paired [ontology PR #17](https://github.com/packagegraph/ontology/pull/17).

Rebuild each affected RPM, Debian, and Maven named graph from a fresh collector
output using the same collection inputs and source repositories. Use the normal
wrapper upload path, which replaces a complete named graph after a successful
collection. Merely appending the new triples leaves the old versioned identity
PURLs in place. Retain the previous graph/export and image revision for rollback,
and validate PURL placement and the matching completeness profile before
publishing the rebuilt graph.

Ubuntu Noble additionally needs its corrected `--distro ubuntu` input. This
changes previously Debian-namespaced resource IRIs, not just PURLs; follow the
[Ubuntu identity rollout](ubuntu-identity-rollout.md) and audit retained
references before replacing those three graphs.

Raw source/HTTP caches may be reused: these emitters regenerate the RDF from the
cached package metadata. Existing generated `.nt`/`.nq` exports must be
regenerated, not republished. Current RPM output checkpoints cover only the
Koji/spec enrichment stages; neither emits the PURL triples changed here, so
their schema versions do not require a bump. Follow the normal
[checkpoint lifecycle](../deploy/quadlet/collectors/checkpoint-cutover.md) for
unfinished generations; do not delete or manually commit unpublished work.
Any downstream cache that stores complete generated package RDF must be
invalidated or regenerated before publication.

For retained historical versions that the current repository no longer exposes,
use the ontology repository's graph migration tooling on an exported copy,
review unresolved mappings, and preserve the original graph. Recollection alone
cannot recover packages absent from the upstream metadata. The migration must
move versioned identity PURLs only to matching concrete packages, canonicalize
the identity PURL, and preserve non-PURL triples and named-graph provenance.

## Offline verification

```sh
cd etl/pg-collect
cargo test --all-targets --locked --quiet
```

The [generated fixtures](pg-collect/tests/fixtures/purls/README.md) provide two
versions per ecosystem for downstream ontology/profile checks. Before/after
fixture comparisons for this change preserve all non-PURL statements, with
collection-time `dq:detectedAt` values compared independently.

With the pinned ontology checked out and its Python dependencies installed,
validate each full collector graph separately. From this platform repository,
set `ONTOLOGY_DIR` to that checkout and run:

```sh
PYTHONPATH="$ONTOLOGY_DIR" python -m packagegraph.purls --profile rpm etl/pg-collect/tests/fixtures/purls/rpm-two-versions.nt etl/pg-collect/tests/fixtures/purls/rpm-ir.nt
PYTHONPATH="$ONTOLOGY_DIR" python -m packagegraph.purls --profile deb etl/pg-collect/tests/fixtures/purls/debian-two-versions.nt etl/pg-collect/tests/fixtures/purls/debian-ir.nt
PYTHONPATH="$ONTOLOGY_DIR" python -m packagegraph.purls --profile maven etl/pg-collect/tests/fixtures/purls/maven-two-versions.nt
```

## Production rollout status

Implementation and offline verification do not deploy or rewrite production.
The following work remains:

- Review and integrate both PRs, publish the pinned ontology and build the
  corresponding collector image.
- Inventory and export affected named graphs and retain their graph names,
  collection inputs and previous image revisions for rollback.
- Recollect current data or rehearse the ontology migration on exported
  historical data; resolve legacy-key collisions and unsupported mappings.
- Validate every complete rebuilt named graph with its PURL profile, inspect
  the before/after counts and confirm preservation of all non-PURL triples.
- Publish validated replacements through the normal upload path; rebuild the
  served index or readers through their existing deployment workflow.
- Repeat the production PURL audit and record results before closing the
  operational rollout. Historical source metadata that cannot be recovered
  remains an explicit unresolved item, never a guessed identifier.

The ecosystem conventions follow the official
[RPM](https://github.com/package-url/purl-spec/blob/main/types/rpm-definition.json),
[Debian](https://github.com/package-url/purl-spec/blob/main/types/deb-definition.json),
and [Maven](https://github.com/package-url/purl-spec/blob/main/types/maven-definition.json)
PURL definitions.
