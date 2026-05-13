//! RPM-specific emission extensions.
//!
//! Emits triples that only apply to RPM packages: sourceRPM, RPMGroup,
//! epoch as RPM integer, dist-git packaging repository.

use crate::ir::PackageIr;
use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use std::io::Result;

/// Emit RPM-specific triples from the collector_specific field.
pub fn emit_rpm_extras(
    ir: &PackageIr,
    writer: &mut NTriplesWriter,
) -> Result<usize> {
    let cs = match &ir.collector_specific {
        Some(v) => v,
        None => return Ok(0),
    };

    let scope = &ir.scope;
    let pkg = &ir.package;
    let pkg_uri = package_uri(&scope.distro, &scope.release, &pkg.arch, &pkg.name, &pkg.full_version);
    let mut triples = 0;

    // RPM-specific type
    writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{RPM}BinaryRPM"))?;
    triples += 1;

    // sourceRPM
    if let Some(srpm) = cs.get("source_rpm").and_then(|v| v.as_str()) {
        writer.write_literal(&pkg_uri, &format!("{RPM}sourceRPM"), srpm)?;
        triples += 1;
    }

    // RPMGroup
    if let Some(group) = cs.get("rpm_group").and_then(|v| v.as_str()) {
        writer.write_literal(&pkg_uri, &format!("{RPM}RPMGroup"), group)?;
        triples += 1;
    }

    // epoch as RPM integer
    if let Some(epoch) = cs.get("rpm_epoch").and_then(|v| v.as_i64()) {
        if epoch != 0 {
            writer.write_integer(&pkg_uri, &format!("{RPM}epoch"), epoch)?;
            triples += 1;
        }
    }

    // Dist-git packaging repository
    let identity_uri = package_identity_uri(&scope.distro, &scope.release, &pkg.arch, &pkg.name);
    let distgit = fedora_distgit_uri(&scope.distro, &pkg.name);
    writer.write_triple(&identity_uri, &format!("{PKG}packagingRepository"), &distgit)?;
    writer.write_triple(&distgit, RDF_TYPE, &format!("{VCS}Repository"))?;
    triples += 2;

    Ok(triples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::*;
    use std::collections::BTreeMap;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_emit_rpm_extras() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let ir = PackageIr {
            ir_schema: 1,
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
                name: "gcc".to_string(),
                epoch: 0,
                version: "14.0.1".to_string(),
                release: Some("1.fc43".to_string()),
                full_version: "14.0.1-1.fc43.x86_64".to_string(),
                arch: "x86_64".to_string(),
            },
            source_package: None,
            maintainers: vec![],
            dependencies: vec![],
            metadata: None,
            collector_specific: Some(serde_json::json!({
                "source_rpm": "gcc-14.0.1-1.fc43.src.rpm",
                "rpm_group": "Development/Languages",
                "rpm_epoch": 0
            })),
        };

        let count = emit_rpm_extras(&ir, &mut writer).unwrap();
        writer.flush().unwrap();

        assert!(count >= 3, "Should emit at least 3 RPM-specific triples");

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("rpm#BinaryRPM"), "Should type as BinaryRPM");
        assert!(content.contains("rpm#sourceRPM"), "Should emit sourceRPM");
        assert!(content.contains("rpm#RPMGroup"), "Should emit RPMGroup");
        assert!(content.contains("packagingRepository"), "Should emit dist-git repo");
    }

    #[test]
    fn test_emit_rpm_extras_no_collector_specific() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let ir = PackageIr {
            ir_schema: 1,
            scope: ScopeIr {
                collector: "rpm".to_string(),
                distro: "fedora".to_string(),
                release: "43".to_string(),
                repo: None,
                arch: "x86_64".to_string(),
            },
            source_artifacts: BTreeMap::new(),
            package: PackageInfo {
                kind: "binary".to_string(),
                name: "test".to_string(),
                epoch: 0,
                version: "1.0".to_string(),
                release: None,
                full_version: "1.0".to_string(),
                arch: "x86_64".to_string(),
            },
            source_package: None,
            maintainers: vec![],
            dependencies: vec![],
            metadata: None,
            collector_specific: None,
        };

        let count = emit_rpm_extras(&ir, &mut writer).unwrap();
        assert_eq!(count, 0, "No collector_specific → 0 extra triples");
    }
}
