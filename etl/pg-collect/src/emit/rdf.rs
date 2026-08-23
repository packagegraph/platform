//! Shared RDF emitter — generates N-Triples from PackageIr records.
//!
//! This module is where the ontology contract is applied. All URI construction,
//! type assignments, property choices, and inverse edge policy lives here.

use crate::ir::{PackageIr, MaintainerIr};
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

/// Emit RDF triples for a single PackageIr record.
///
/// Returns the number of triples written.
pub fn emit_rdf(
    ir: &PackageIr,
    writer: &mut NTriplesWriter,
    policy: &EmitPolicy,
) -> Result<usize> {
    let scope = &ir.scope;
    let pkg = &ir.package;
    let mut triples = 0;

    let release_name = &scope.release;
    let pkg_uri = package_uri(&scope.distro, release_name, &pkg.arch, &pkg.name, &pkg.full_version);

    // === Package type ===
    writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
    triples += 1;
    if let Some(ref eco_type) = policy.ecosystem_type_uri {
        writer.write_triple(&pkg_uri, RDF_TYPE, eco_type)?;
        triples += 1;
    }

    // === PackageIdentity ===
    let identity_uri = package_identity_uri(&scope.distro, release_name, &pkg.arch, &pkg.name);
    writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
    writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &pkg.name)?;
    writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
    triples += 3;

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
            if let Some(upstream_uri) = normalize_forge_url(homepage) {
                writer.write_triple(&identity_uri, &format!("{PKG}upstreamRepository"), &upstream_uri)?;
                writer.write_triple(&upstream_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
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
        triples += emit_dependency(writer, &pkg_uri, dep, &scope.distro, release_name, &pkg.arch)?;
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
    writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyType"), &dep_type_uri(&dep.dep_type))?;
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
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Core assertions
        assert!(content.contains("core#BinaryPackage"), "Should type as BinaryPackage");
        assert!(content.contains("core#PackageIdentity"), "Should create PackageIdentity");
        assert!(content.contains("core#isVersionOf"), "Should link isVersionOf");
        assert!(content.contains("core#hasVersion"), "Should link hasVersion");
        assert!(content.contains("core#Person"), "Should type maintainer as Person");
        assert!(content.contains("foaf/0.1/name"), "Should emit foaf:name");
        assert!(content.contains("core#maintainedBy"), "Should emit maintainedBy");
        assert!(content.contains("core#SourcePackage"), "Should emit SourcePackage");
        assert!(content.contains("core#builtFromSource"), "Should emit builtFromSource");
        assert!(content.contains("core#hasDependency"), "Should emit dependency");
        assert!(content.contains("core#directlyDependsOn"), "Should emit directlyDependsOn");
        assert!(content.contains("core#partOfDistribution"), "Should emit partOfDistribution");
    }

    #[test]
    fn test_emit_distribution_metadata() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let count = emit_distribution_metadata(&mut writer, "fedora", "43", "Fedora").unwrap();
        writer.flush().unwrap();

        assert!(count >= 6, "Should emit at least 6 triples");

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Distribution"), "Should type as Distribution");
        assert!(content.contains("core#DistributionRelease"), "Should type as DistributionRelease");
        assert!(content.contains("core#releaseVersion"), "43 is numeric → releaseVersion");
        assert!(content.contains("core#partOfDistribution"), "Should link partOfDistribution");
        assert!(content.contains("core#hasRelease"), "Should auto-emit hasRelease inverse");
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
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Must type as Person, NOT Maintainer (SD-3)
        assert!(content.contains("core#Person"), "Must type as Person");
        assert!(!content.contains("core#Maintainer"), "Must NOT type as Maintainer");
        assert!(content.contains("\"Fedora Project\""), "Should emit name");
        assert!(content.contains("mailto:admin@fedoraproject.org"), "Should emit mbox");
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
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // The URI must use the /maintainer/name/ path from maintainer_name_uri()
        let expected_uri = maintainer_name_uri("Debian QA Group");
        assert!(
            content.contains(&expected_uri),
            "RDF emitter must use maintainer_name_uri() for name-only entries.\n\
             Expected URI: {}\nContent:\n{}",
            expected_uri, content
        );
        assert!(content.contains("core#Person"), "Must type as Person");
        assert!(content.contains("\"Debian QA Group\""), "Must emit name");
        // Must NOT contain foaf:mbox for name-only
        assert!(
            !content.contains("mailto:"),
            "Name-only maintainer must not have mbox"
        );
    }
}
