# Offline PURL fixtures

The `*-two-versions.nt` files are emitted by the real RPM, Debian, and Maven
emitters from two versions of each package. The `*-ir.nt` files exercise
RPM/Debian replay of normalized intermediate records. They exercise versionless identities,
versioned packages, Debian source versions, RPM epochs and source RPMs, Maven
dependency identities, and percent encoding. Duplicate statements are kept as
emitted; RDF graph loading deduplicates them. Debian `dq:detectedAt` values are
the fixture generation time.

From `etl/pg-collect`, regenerate without contacting package repositories:

```sh
PURL_FIXTURE_DIR=tests/fixtures/purls cargo test --locked --test test_purls
PURL_FIXTURE_DIR=tests/fixtures/purls cargo test --locked --lib maven_two_versions
```

The tests parse every emitted PURL with the `packageurl` Rust library and compare
the complete subject-to-PURL mapping with independently specified values.
All literals use `xsd:anyURI`. The fixtures can also be loaded into the ontology
repository's PURL validator with the matching `rpm`, `deb`, or `maven` profile.

See [PURL collection and recollection](../../../../purl-recollection.md) for the
supported completeness boundary and deployment requirements.
