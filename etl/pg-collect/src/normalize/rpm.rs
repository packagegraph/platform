//! RPM normalizer — converts parsed RPM primary.xml data into PackageIr records.

use crate::ir::*;
use crate::rpm::RpmPackageData;
use regex::Regex;
use std::collections::BTreeMap;
use std::io::Result;
use std::path::Path;

/// Convert an RpmPackageData into a PackageIr record.
pub fn normalize_rpm_package(
    pkg: &RpmPackageData,
    scope: &ScopeIr,
    source_artifact_hash: &str,
) -> Option<PackageIr> {
    let fields = &pkg.fields;

    let name = fields.get("name")?;
    let arch = fields.get("arch")?;
    let ver = fields.get("ver")?;
    let rel = fields.get("rel")?;
    let epoch_str = fields.get("epoch").map(|s| s.as_str()).unwrap_or("0");
    let epoch: u32 = epoch_str.parse().unwrap_or(0);
    let version_str = format!("{}-{}.{}", ver, rel, arch);

    // Maintainer
    let maintainers = if let Some(packager) = fields.get("packager") {
        parse_maintainer(packager)
    } else {
        vec![]
    };

    // Dependencies
    let dependencies: Vec<DependencyIr> = pkg
        .deps
        .iter()
        .map(|d| DependencyIr {
            name: d.name.clone(),
            dep_type: d.dep_type.clone(),
            version_constraint: d.ver.as_ref().map(|v| {
                let flags = d.flags.as_deref().unwrap_or("");
                let epoch = d.epoch.as_deref().unwrap_or("");
                let rel = d.rel.as_deref().unwrap_or("");
                if !epoch.is_empty() && epoch != "0" {
                    format!("{} {}:{}-{}", flags, epoch, v, rel)
                } else if !rel.is_empty() {
                    format!("{} {}-{}", flags, v, rel)
                } else {
                    format!("{} {}", flags, v)
                }
                .trim()
                .to_string()
            }),
            flags: d.flags.clone(),
        })
        .collect();

    // Source package
    let source_package = fields
        .get("rpm:sourcerpm")
        .or_else(|| fields.get("sourcerpm"))
        .and_then(|srpm| parse_source_rpm(srpm));

    // Metadata
    let metadata = PackageMetadataIr {
        summary: fields.get("summary").cloned(),
        description: fields.get("description").cloned(),
        homepage: fields.get("url").cloned(),
        license: fields.get("rpm:license").or_else(|| fields.get("license")).cloned(),
        checksum: fields.get("checksum").cloned(),
        size_bytes: fields
            .get("size")
            .or_else(|| fields.get("package_size"))
            .and_then(|s| s.parse().ok()),
    };

    // Collector-specific fields
    let mut collector_specific = serde_json::Map::new();
    if let Some(srpm) = fields.get("rpm:sourcerpm").or_else(|| fields.get("sourcerpm")) {
        collector_specific.insert("source_rpm".to_string(), serde_json::Value::String(srpm.clone()));
    }
    if let Some(group) = fields.get("rpm:group").or_else(|| fields.get("group")) {
        collector_specific.insert("rpm_group".to_string(), serde_json::Value::String(group.clone()));
    }
    if epoch != 0 {
        collector_specific.insert("rpm_epoch".to_string(), serde_json::Value::Number(epoch.into()));
    }

    let mut source_artifacts = BTreeMap::new();
    source_artifacts.insert("primary".to_string(), source_artifact_hash.to_string());

    Some(PackageIr {
        ir_schema: IR_SCHEMA_VERSION,
        scope: scope.clone(),
        source_artifacts,
        package: PackageInfo {
            kind: "binary".to_string(),
            name: name.clone(),
            epoch,
            version: ver.clone(),
            release: Some(rel.clone()),
            full_version: version_str,
            arch: arch.clone(),
        },
        source_package,
        maintainers,
        dependencies,
        metadata: Some(metadata),
        collector_specific: if collector_specific.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(collector_specific))
        },
    })
}

/// Normalize an entire shard of RPM packages to IR.
pub fn normalize_rpm_shard(
    packages: &[RpmPackageData],
    scope: &ScopeIr,
    source_artifact_hash: &str,
    output_path: &Path,
) -> Result<usize> {
    let mut writer = IrWriter::new(output_path)?;

    for pkg in packages {
        if let Some(ir) = normalize_rpm_package(pkg, scope, source_artifact_hash) {
            writer.write(&ir)?;
        }
    }

    writer.finish()
}

fn parse_maintainer(packager: &str) -> Vec<MaintainerIr> {
    let re = Regex::new(r"^(.+?)\s*<(.+?)>$").unwrap();
    if let Some(caps) = re.captures(packager) {
        let name = caps.get(1).unwrap().as_str().trim().to_string();
        let email = caps.get(2).unwrap().as_str().trim().to_string();
        vec![MaintainerIr {
            name,
            email: Some(email),
            role_hint: Some("maintainer".to_string()),
        }]
    } else {
        let name = packager.trim();
        if name.is_empty() {
            vec![]
        } else {
            vec![MaintainerIr {
                name: name.to_string(),
                email: None,
                role_hint: Some("maintainer".to_string()),
            }]
        }
    }
}

fn parse_source_rpm(srpm: &str) -> Option<SourcePackageRef> {
    let name = srpm
        .trim_end_matches(".src.rpm")
        .trim_end_matches(".rpm");
    // Parse NVR: name-version-release
    let mut parts = name.rsplitn(3, '-');
    let release = parts.next()?;
    let version = parts.next()?;
    let src_name = parts.next()?;

    Some(SourcePackageRef {
        name: src_name.to_string(),
        version: Some(version.to_string()),
        release: Some(release.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpm::RpmDep;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn sample_rpm_data() -> RpmPackageData {
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), "glibc".to_string());
        fields.insert("arch".to_string(), "x86_64".to_string());
        fields.insert("ver".to_string(), "2.39".to_string());
        fields.insert("rel".to_string(), "17.fc43".to_string());
        fields.insert("epoch".to_string(), "0".to_string());
        fields.insert("packager".to_string(), "Fedora Project <admin@fedoraproject.org>".to_string());
        fields.insert("summary".to_string(), "GNU C Library".to_string());
        fields.insert("url".to_string(), "https://www.gnu.org/software/libc/".to_string());
        fields.insert("rpm:sourcerpm".to_string(), "glibc-2.39-17.fc43.src.rpm".to_string());

        RpmPackageData {
            fields,
            deps: vec![
                RpmDep {
                    name: "glibc-common".to_string(),
                    flags: Some("EQ".to_string()),
                    epoch: None,
                    ver: Some("2.39".to_string()),
                    rel: Some("17.fc43".to_string()),
                    dep_type: "requires".to_string(),
                },
            ],
        }
    }

    fn sample_scope() -> ScopeIr {
        ScopeIr {
            collector: "rpm".to_string(),
            distro: "fedora".to_string(),
            release: "43".to_string(),
            repo: Some("fedora".to_string()),
            arch: "x86_64".to_string(),
        }
    }

    #[test]
    fn test_normalize_rpm_package() {
        let pkg = sample_rpm_data();
        let scope = sample_scope();

        let ir = normalize_rpm_package(&pkg, &scope, "sha256:abc").unwrap();

        assert_eq!(ir.package.name, "glibc");
        assert_eq!(ir.package.version, "2.39");
        assert_eq!(ir.package.arch, "x86_64");
        assert_eq!(ir.package.kind, "binary");
        assert_eq!(ir.maintainers.len(), 1);
        assert_eq!(ir.maintainers[0].name, "Fedora Project");
        assert_eq!(ir.maintainers[0].email.as_deref(), Some("admin@fedoraproject.org"));
        assert_eq!(ir.dependencies.len(), 1);
        assert_eq!(ir.dependencies[0].name, "glibc-common");
        assert!(ir.source_package.is_some());
        assert_eq!(ir.source_package.as_ref().unwrap().name, "glibc");
    }

    #[test]
    fn test_normalize_rpm_shard() {
        let tmp = TempDir::new().unwrap();
        let ir_path = tmp.path().join("test.jsonl.zst");

        let packages = vec![sample_rpm_data()];
        let scope = sample_scope();

        let count = normalize_rpm_shard(&packages, &scope, "sha256:abc", &ir_path).unwrap();
        assert_eq!(count, 1);

        // Verify we can read it back
        let reader = IrReader::new(&ir_path).unwrap();
        let records: Vec<PackageIr> = reader
            .records()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].package.name, "glibc");
    }

    #[test]
    fn test_parse_source_rpm() {
        let result = parse_source_rpm("glibc-2.39-17.fc43.src.rpm");
        assert!(result.is_some());
        let ref_ = result.unwrap();
        assert_eq!(ref_.name, "glibc");
        assert_eq!(ref_.version.as_deref(), Some("2.39"));
        assert_eq!(ref_.release.as_deref(), Some("17.fc43"));
    }

    #[test]
    fn test_parse_maintainer_with_email() {
        let m = parse_maintainer("Fedora Project <admin@fedoraproject.org>");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "Fedora Project");
        assert_eq!(m[0].email.as_deref(), Some("admin@fedoraproject.org"));
    }

    #[test]
    fn test_parse_maintainer_name_only() {
        let m = parse_maintainer("CentOS BuildSystem");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "CentOS BuildSystem");
        assert!(m[0].email.is_none());
    }
}
