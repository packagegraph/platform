//! Shared RDF emitter — generates N-Triples from PackageIr records.
//!
//! This module is where the ontology contract is applied. All URI construction,
//! type assignments, property choices, and inverse edge policy lives here.

use crate::ir::{MaintainerIr, PackageIr};
use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use std::collections::HashSet;
use std::io::Result;

/// Policy configuration for RDF emission.
#[derive(Debug, Clone)]
pub struct EmitPolicy {
    /// Ecosystem-specific RDF type prefix (e.g., RPM, DEB, APK namespace)
    pub ecosystem_type_uri: Option<String>,
    /// Whether to emit packaging repository links (e.g., dist-git)
    pub emit_packaging_repo: bool,
}

impl Default for EmitPolicy {
    fn default() -> Self {
        Self {
            ecosystem_type_uri: None,
            emit_packaging_repo: false,
        }
    }
}

/// Emit the shared `pkg:PackageIdentity` triples: `rdf:type`, `identityName`
/// and `rdfs:label`.
///
/// This is **not** a whole-node conformance guarantee.
/// `pkg:PackageIdentityShape` also requires exactly one `pkg:purl`, which only
/// the caller can supply — it needs the ecosystem, namespace and version — so
/// a node built solely from this helper is still short of the shape. An
/// earlier version of this doc called the output "complete, ontology-
/// conformant"; that was wrong, and independent review of PR #25 caught it.
///
/// Centralised because this exact triple set is emitted from 38 call sites
/// across the collectors, and every one of them had the same two defects
/// (production audit 2026-09-11):
///
/// * `pkg:packageName` was used instead of `pkg:identityName`. packageName is
///   `rdfs:domain pkg:Package`; identityName is `rdfs:domain
///   pkg:PackageIdentity` and is defined as "distinct from the versioned
///   packageName on Package instances". Emitting packageName here violates
///   that domain, and under RDFS entailment infers every identity to also be
///   a Package -- collapsing the version-agnostic/versioned distinction this
///   class exists to draw.
/// * No `rdfs:label`. In this ontology rdfs:label is a human-readable
///   description rather than a restatement of the name, so it is required
///   alongside identityName rather than instead of it.
///
/// Callers must not additionally write `pkg:packageName` on the identity URI.
/// The versioned package URI is a different subject and still carries it.
pub fn write_package_identity(
    writer: &mut NTriplesWriter,
    identity_uri: &str,
    name: &str,
) -> Result<usize> {
    writer.write_triple(identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
    writer.write_literal(identity_uri, &format!("{PKG}identityName"), name)?;
    writer.write_literal(identity_uri, RDFS_LABEL, &format!("{name} Package Identity"))?;
    Ok(3)
}

/// [`write_package_identity`], but deduplicated per output file.
///
/// For paths that reference the same identity from many packages — RPM
/// `Provides:` names the same capability from every provider — these
/// definition triples are a pure function of `identity_uri`, so emitting them
/// once per distinct identity rather than once per reference is a large
/// output-size win. Returns the number of triples actually written, which is
/// 0 for an identity already defined in this file.
pub fn write_package_identity_once(
    writer: &mut NTriplesWriter,
    identity_uri: &str,
    name: &str,
) -> Result<usize> {
    let mut n = 0;
    if writer.write_triple_once(identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))? {
        n += 1;
    }
    if writer.write_literal_once(identity_uri, &format!("{PKG}identityName"), name)? {
        n += 1;
    }
    if writer.write_literal_once(
        identity_uri,
        RDFS_LABEL,
        &format!("{name} Package Identity"),
    )? {
        n += 1;
    }
    Ok(n)
}

/// Emit RDF triples for a single PackageIr record.
///
/// Returns the number of triples written.
pub fn emit_rdf(ir: &PackageIr, writer: &mut NTriplesWriter, policy: &EmitPolicy) -> Result<usize> {
    let scope = &ir.scope;
    let pkg = &ir.package;
    let mut triples = 0;

    let release_name = &scope.release;
    let pkg_uri = package_uri(
        &scope.distro,
        release_name,
        &pkg.arch,
        &pkg.name,
        &pkg.full_version,
    );

    // === Package type ===
    writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
    triples += 1;
    if let Some(ref eco_type) = policy.ecosystem_type_uri {
        writer.write_triple(&pkg_uri, RDF_TYPE, eco_type)?;
        triples += 1;
    }

    // === PackageIdentity ===
    let identity_uri = package_identity_uri(&scope.distro, release_name, &pkg.arch, &pkg.name);
    triples += write_package_identity(writer, &identity_uri, &pkg.name)?;
    writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
    triples += 1;

    // === Package name ===
    writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.name)?;
    triples += 1;

    // === Version ===
    let ver_uri = version_uri(&scope.distro, release_name, &pkg.name, &pkg.full_version);
    writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
    writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &pkg.full_version)?;
    writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
    triples += 3;

    if pkg.epoch != 0 {
        writer.write_integer(&ver_uri, &format!("{PKG}epoch"), pkg.epoch as i64)?;
        triples += 1;
    }

    // === Architecture ===
    let arch_uri_val = arch_uri(&pkg.arch);
    writer.write_triple(&pkg_uri, &format!("{PKG}targetArchitecture"), &arch_uri_val)?;
    triples += 1;

    // === Distribution + Release ===
    let dist_uri_val = distro_uri(&scope.distro);
    writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri_val)?;
    triples += 1;

    if !release_name.is_empty() {
        let rel_uri = release_uri(&scope.distro, release_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
        triples += 1;
    }

    // === Maintainers ===
    for maint in &ir.maintainers {
        triples += emit_maintainer(writer, &pkg_uri, maint)?;
    }

    // === Description / Homepage ===
    if let Some(ref meta) = ir.metadata {
        if let Some(ref desc) = meta.description.as_ref().or(meta.summary.as_ref()) {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(ref homepage) = meta.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
            // Upstream repository from homepage (if forge URL)
            if let Some(canonical_url) = normalize_forge_url_canonical(homepage) {
                let upstream_repo_iri = repo_uri(&canonical_url);
                writer.write_triple(
                    &identity_uri,
                    &format!("{PKG}upstreamRepository"),
                    &upstream_repo_iri,
                )?;
                writer.write_triple(&upstream_repo_iri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
                triples += crate::forge::emit_upstream_project(writer, &canonical_url)?;
            }
        }
    }

    // === Source package ===
    if let Some(ref src) = ir.source_package {
        let src_version = src.version.as_deref().unwrap_or(&pkg.full_version);
        let src_uri = source_uri(&scope.distro, release_name, &src.name, src_version);
        writer.write_triple(&src_uri, RDF_TYPE, &format!("{PKG}SourcePackage"))?;
        writer.write_literal(&src_uri, &format!("{PKG}packageName"), &src.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}builtFromSource"), &src_uri)?;
        triples += 3;
    }

    // === Dependencies ===
    for dep in &ir.dependencies {
        triples += emit_dependency(
            writer,
            &pkg_uri,
            dep,
            &scope.distro,
            release_name,
            &pkg.arch,
        )?;
    }

    Ok(triples)
}

/// Emit distribution-level metadata (called once per shard).
pub fn emit_distribution_metadata(
    writer: &mut NTriplesWriter,
    distro: &str,
    release: &str,
    display_name: &str,
) -> Result<usize> {
    let dist_uri_val = distro_uri(distro);
    let rel_uri = release_uri(distro, release);
    let mut triples = 0;

    writer.write_triple(&dist_uri_val, RDF_TYPE, &format!("{PKG}Distribution"))?;
    writer.write_literal(&dist_uri_val, &format!("{PKG}distributionName"), distro)?;
    writer.write_literal(&dist_uri_val, RDFS_LABEL, display_name)?;
    triples += 3;

    writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
    if is_numeric_release(release) {
        writer.write_literal(&rel_uri, &format!("{PKG}releaseVersion"), release)?;
    } else {
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), release)?;
    }
    // partOfDistribution auto-emits hasRelease inverse via ntriples.rs
    writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri_val)?;
    triples += 3;

    Ok(triples)
}

fn emit_maintainer(
    writer: &mut NTriplesWriter,
    pkg_uri: &str,
    maint: &MaintainerIr,
) -> Result<usize> {
    let maint_uri_val = if let Some(ref email) = maint.email {
        maintainer_uri(email)
    } else {
        maintainer_name_uri(&maint.name)
    };
    let mut triples = 0;

    // Type as Person (SD-3 data contract)
    writer.write_triple(&maint_uri_val, RDF_TYPE, &format!("{PKG}Person"))?;
    writer.write_literal(&maint_uri_val, &format!("{FOAF}name"), &maint.name)?;
    writer.write_literal(&maint_uri_val, RDFS_LABEL, &maint.name)?;
    triples += 3;

    if let Some(ref email) = maint.email {
        if email.contains('@') {
            writer.write_triple(
                &maint_uri_val,
                &format!("{FOAF}mbox"),
                &format!("mailto:{email}"),
            )?;
            triples += 1;
        }
    }

    writer.write_triple(pkg_uri, &format!("{PKG}maintainedBy"), &maint_uri_val)?;
    triples += 1;

    Ok(triples)
}

fn emit_dependency(
    writer: &mut NTriplesWriter,
    pkg_uri: &str,
    dep: &crate::ir::DependencyIr,
    distro: &str,
    release: &str,
    arch: &str,
) -> Result<usize> {
    use crate::ntriples::bnode_id;

    let target_uri = package_identity_uri(distro, release, arch, &dep.name);
    let bnode = bnode_id("dep", &format!("{}{}{}", pkg_uri, dep.name, dep.dep_type));
    let mut triples = 0;

    writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
    writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
    writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
    writer.write_bnode_subject(
        &bnode,
        &format!("{PKG}dependencyType"),
        &dep_type_uri(&dep.dep_type),
    )?;
    triples += 4;

    if let Some(ref constraint) = dep.version_constraint {
        writer.write_bnode_literal(&bnode, &format!("{PKG}versionConstraint"), constraint)?;
        triples += 1;
    }

    // Direct dependency link
    writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
    triples += 1;

    Ok(triples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::*;
    use std::collections::BTreeMap;
    use std::io::Read;
    use tempfile::NamedTempFile;

    fn sample_ir() -> PackageIr {
        PackageIr {
            ir_schema: IR_SCHEMA_VERSION,
            scope: ScopeIr {
                collector: "rpm".to_string(),
                distro: "fedora".to_string(),
                release: "43".to_string(),
                repo: Some("fedora".to_string()),
                arch: "x86_64".to_string(),
            },
            source_artifacts: BTreeMap::new(),
            package: PackageInfo {
                kind: "binary".to_string(),
                name: "glibc".to_string(),
                epoch: 0,
                version: "2.39".to_string(),
                release: Some("17.fc43".to_string()),
                full_version: "2.39-17.fc43.x86_64".to_string(),
                arch: "x86_64".to_string(),
            },
            source_package: Some(SourcePackageRef {
                name: "glibc".to_string(),
                version: Some("2.39".to_string()),
                release: Some("17.fc43".to_string()),
            }),
            maintainers: vec![MaintainerIr {
                name: "Fedora Project".to_string(),
                email: Some("admin@fedoraproject.org".to_string()),
                role_hint: Some("maintainer".to_string()),
            }],
            dependencies: vec![DependencyIr {
                name: "glibc-common".to_string(),
                dep_type: "requires".to_string(),
                version_constraint: Some("= 2.39-17.fc43".to_string()),
                flags: None,
            }],
            metadata: Some(PackageMetadataIr {
                summary: Some("GNU C Library".to_string()),
                description: Some("The GNU libc libraries".to_string()),
                homepage: None,
                license: None,
                checksum: None,
                size_bytes: None,
            }),
            collector_specific: None,
        }
    }

    #[test]
    fn test_emit_rdf_produces_triples() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let ir = sample_ir();
        let policy = EmitPolicy::default();

        let count = emit_rdf(&ir, &mut writer, &policy).unwrap();
        writer.flush().unwrap();

        assert!(count > 10, "Should emit at least 10 triples, got {}", count);

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // The reported count must match what was actually written. Operators
        // read this number off the "N packages, M triples" log line, so a
        // hand-maintained `triples += N` that drifts from the writes around it
        // is silently wrong. Centralizing the identity block into
        // write_package_identity broke exactly this: the helper self-reports
        // its 3 triples while the caller's trailing `triples += N` still
        // counted the 2 writes the helper had absorbed, over-reporting by 2
        // per package at all 37 call sites.
        //
        // Output lines are not the tally: write_triple auto-emits inverse
        // statements that no caller counts, and silently drops triples with
        // invalid IRI characters. Both are accounted for here, so this
        // compares logical writes against a logical tally.
        assert_eq!(
            count + writer.skipped_invalid_iri,
            content.lines().count() - writer.auto_inverses,
            "reported tally ({count}) must match logical writes \
             ({} lines - {} auto-inverses, {} skipped)",
            content.lines().count(),
            writer.auto_inverses,
            writer.skipped_invalid_iri
        );

        // Core assertions
        assert!(
            content.contains("core#BinaryPackage"),
            "Should type as BinaryPackage"
        );
        assert!(
            content.contains("core#PackageIdentity"),
            "Should create PackageIdentity"
        );
        // pkg:identityName, not pkg:packageName. packageName is
        // rdfs:domain pkg:Package; identityName is rdfs:domain
        // pkg:PackageIdentity and is defined as "distinct from the versioned
        // packageName on Package instances". Emitting packageName on an
        // identity violates that domain and, under RDFS entailment, infers
        // every identity to also be a Package -- collapsing the
        // version-agnostic/versioned split the class exists to draw.
        assert!(
            content.contains("core#identityName"),
            "identity must carry pkg:identityName"
        );
        assert!(
            content.contains("Package Identity\""),
            "identity must carry a human-readable rdfs:label"
        );
        // Regression guard: the identity must NOT carry packageName. The
        // versioned pkg_uri still does, so assert on the identity line only.
        let identity_line = content
            .lines()
            .find(|l| l.contains("core#identityName"))
            .expect("identityName triple present");
        let identity_subject = identity_line.split_whitespace().next().unwrap();
        assert!(
            !content
                .lines()
                .any(|l| l.starts_with(identity_subject) && l.contains("core#packageName")),
            "identity subject {identity_subject} must not carry pkg:packageName"
        );
        assert!(
            content.contains("core#isVersionOf"),
            "Should link isVersionOf"
        );
        assert!(
            content.contains("core#hasVersion"),
            "Should link hasVersion"
        );
        assert!(
            content.contains("core#Person"),
            "Should type maintainer as Person"
        );
        assert!(content.contains("foaf/0.1/name"), "Should emit foaf:name");
        assert!(
            content.contains("core#maintainedBy"),
            "Should emit maintainedBy"
        );
        assert!(
            content.contains("core#SourcePackage"),
            "Should emit SourcePackage"
        );
        assert!(
            content.contains("core#builtFromSource"),
            "Should emit builtFromSource"
        );
        assert!(
            content.contains("core#hasDependency"),
            "Should emit dependency"
        );
        assert!(
            content.contains("core#directlyDependsOn"),
            "Should emit directlyDependsOn"
        );
        assert!(
            content.contains("core#partOfDistribution"),
            "Should emit partOfDistribution"
        );
    }

    /// The RPM `Provides:` path reaches identities through the `_once` writers,
    /// which is why the 2026-09-11 migration missed it and left `packageName`
    /// on 6,558,436 triples' worth of identities. Pin both halves: the right
    /// predicates, and the dedup that made this path use `_once` to begin with.
    #[test]
    fn write_package_identity_once_dedupes_and_avoids_package_name() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let uri = "https://packagegraph.github.io/data/identity/fedora/42/x86_64/libssl.so.3";
        let first = write_package_identity_once(&mut writer, uri, "libssl.so.3").unwrap();
        let second = write_package_identity_once(&mut writer, uri, "libssl.so.3").unwrap();
        writer.flush().unwrap();

        assert_eq!(first, 3, "first reference defines the identity");
        assert_eq!(
            second, 0,
            "a second reference to the same identity must write nothing"
        );

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert_eq!(
            content.lines().count(),
            3,
            "exactly three lines for two references"
        );
        assert!(content.contains("core#PackageIdentity"));
        assert!(content.contains("core#identityName"));
        assert!(content.contains("Package Identity\""), "needs rdfs:label");
        assert!(
            !content.contains("core#packageName"),
            "identity must not carry pkg:packageName -- it is rdfs:domain pkg:Package"
        );
    }

    #[test]
    fn test_emit_distribution_metadata() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let count = emit_distribution_metadata(&mut writer, "fedora", "43", "Fedora").unwrap();
        writer.flush().unwrap();

        assert!(count >= 6, "Should emit at least 6 triples");

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("core#Distribution"),
            "Should type as Distribution"
        );
        assert!(
            content.contains("core#DistributionRelease"),
            "Should type as DistributionRelease"
        );
        assert!(
            content.contains("core#releaseVersion"),
            "43 is numeric → releaseVersion"
        );
        assert!(
            content.contains("core#partOfDistribution"),
            "Should link partOfDistribution"
        );
        assert!(
            content.contains("core#hasRelease"),
            "Should auto-emit hasRelease inverse"
        );
        assert!(content.contains("\"Fedora\""), "Should have rdfs:label");
    }

    #[test]
    fn test_emit_maintainer_person_typing() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let ir = sample_ir();
        let policy = EmitPolicy::default();

        emit_rdf(&ir, &mut writer, &policy).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // Must type as Person, NOT Maintainer (SD-3)
        assert!(content.contains("core#Person"), "Must type as Person");
        assert!(
            !content.contains("core#Maintainer"),
            "Must NOT type as Maintainer"
        );
        assert!(content.contains("\"Fedora Project\""), "Should emit name");
        assert!(
            content.contains("mailto:admin@fedoraproject.org"),
            "Should emit mbox"
        );
    }

    #[test]
    fn test_emit_maintainer_name_only_uses_name_uri() {
        // Verifies that the RDF emitter uses maintainer_name_uri() for name-only
        // MaintainerIr entries, producing URIs consistent with the direct callers
        // in debian.rs and collect_sources.rs.
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut ir = sample_ir();
        ir.maintainers = vec![MaintainerIr {
            name: "Debian QA Group".to_string(),
            email: None,
            role_hint: Some("maintainer".to_string()),
        }];
        let policy = EmitPolicy::default();

        emit_rdf(&ir, &mut writer, &policy).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // The URI must use the /maintainer/name/ path from maintainer_name_uri()
        let expected_uri = maintainer_name_uri("Debian QA Group");
        assert!(
            content.contains(&expected_uri),
            "RDF emitter must use maintainer_name_uri() for name-only entries.\n\
             Expected URI: {}\nContent:\n{}",
            expected_uri,
            content
        );
        assert!(content.contains("core#Person"), "Must type as Person");
        assert!(content.contains("\"Debian QA Group\""), "Must emit name");
        // Must NOT contain foaf:mbox for name-only
        assert!(
            !content.contains("mailto:"),
            "Name-only maintainer must not have mbox"
        );
    }

    #[test]
    fn test_emit_rdf_upstream_repo_links_to_hub() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut ir = sample_ir();
        ir.metadata = Some(PackageMetadataIr {
            summary: Some("GNU C Library".to_string()),
            description: Some("The GNU libc libraries".to_string()),
            homepage: Some("https://github.com/owner/repo".to_string()),
            license: None,
            checksum: None,
            size_bytes: None,
        });
        let policy = EmitPolicy::default();

        emit_rdf(&ir, &mut writer, &policy).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("upstreamRepository"), "existing triple must still be emitted");
        assert!(content.contains(&format!("{VCS}Repository")), "existing repo typing must be preserved");
        assert!(content.contains("UpstreamProject"), "new hub triple");
        assert!(
            content.contains("\"owner/repo\""),
            "projectName should be derived from the repo URL"
        );
        // hasUpstreamProject is rdfs:domain :SourcePackage; identity_uri here
        // is a PackageIdentity, so this predicate must never appear on it.
        // The hub is discoverable via upstreamRepository/projectRepository
        // joining through the shared repo URI instead.
        assert!(!content.contains("hasUpstreamProject"));
    }
}
