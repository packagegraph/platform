//! Debian-specific emission extensions.
//!
//! Emits triples that only apply to Debian packages: suite, component,
//! installed size, Vcs-Git packaging repository.

use crate::ir::PackageIr;
use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use std::io::Result;

/// Emit Debian-specific triples from the collector_specific field.
pub fn emit_debian_extras(ir: &PackageIr, writer: &mut NTriplesWriter) -> Result<usize> {
    let cs = match &ir.collector_specific {
        Some(v) => v,
        None => return Ok(0),
    };

    let scope = &ir.scope;
    let pkg = &ir.package;
    let pkg_uri = package_uri(
        &scope.distro,
        &scope.release,
        &pkg.arch,
        &pkg.name,
        &pkg.full_version,
    );
    let identity_uri = package_identity_uri(&scope.distro, &scope.release, &pkg.arch, &pkg.name);
    let mut triples = 0;

    // Debian-specific type
    writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{DEB}BinaryPackage"))?;
    triples += 1;

    // Suite
    if let Some(suite) = cs.get("suite").and_then(|v| v.as_str()) {
        writer.write_literal(&pkg_uri, &format!("{DEB}inSuite"), suite)?;
        triples += 1;
    }

    // Component
    if let Some(component) = cs.get("component").and_then(|v| v.as_str()) {
        writer.write_literal(&pkg_uri, &format!("{DEB}inComponent"), component)?;
        triples += 1;
    }

    // Installed size
    if let Some(installed_size) = cs.get("installed_size_kb").and_then(|v| v.as_str()) {
        if let Ok(size) = installed_size.parse::<i64>() {
            writer.write_integer(&pkg_uri, &format!("{DEB}installedSize"), size)?;
            triples += 1;
        }
    }

    // Vcs-Git packaging repository
    if let Some(vcs_git) = cs.get("vcs_git").and_then(|v| v.as_str()) {
        let vcs_url = vcs_git.split_whitespace().next().unwrap_or(vcs_git);
        if let Some(pkg_repo_uri) = normalize_forge_url(vcs_url) {
            writer.write_triple(
                &identity_uri,
                &format!("{PKG}packagingRepository"),
                &pkg_repo_uri,
            )?;
            writer.write_triple(&pkg_repo_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
            triples += 2;
        }
    }

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
    fn test_emit_debian_extras() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let ir = PackageIr {
            ir_schema: 1,
            scope: ScopeIr {
                collector: "debian".to_string(),
                distro: "debian".to_string(),
                release: "trixie".to_string(),
                repo: Some("main".to_string()),
                arch: "amd64".to_string(),
            },
            source_artifacts: BTreeMap::new(),
            package: PackageInfo {
                kind: "binary".to_string(),
                name: "libc6".to_string(),
                epoch: 0,
                version: "2.36-9".to_string(),
                release: None,
                full_version: "2.36-9+deb13u1".to_string(),
                arch: "amd64".to_string(),
            },
            source_package: None,
            maintainers: vec![],
            dependencies: vec![],
            metadata: None,
            collector_specific: Some(serde_json::json!({
                "suite": "testing",
                "component": "main",
                "installed_size_kb": "13456",
                "vcs_git": "https://salsa.debian.org/glibc-team/glibc.git -b trixie"
            })),
        };

        let count = emit_debian_extras(&ir, &mut writer).unwrap();
        writer.flush().unwrap();

        assert!(count >= 4, "Should emit at least 4 Debian-specific triples");

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("deb#BinaryPackage"),
            "Should type as BinaryPackage"
        );
        assert!(content.contains("deb#inSuite"), "Should emit suite");
        assert!(content.contains("deb#inComponent"), "Should emit component");
        assert!(
            content.contains("deb#installedSize"),
            "Should emit installed size"
        );
        assert!(
            content.contains("packagingRepository"),
            "Should emit VCS repo"
        );
    }
}
