//! Arch normalizer — converts %FIELD% format data into PackageIr records.

use crate::ir::*;
use std::collections::{BTreeMap, HashMap};

/// Normalize an Arch package entry into a PackageIr.
pub fn normalize_arch_package(
    fields: &HashMap<String, Vec<String>>,
    scope: &ScopeIr,
    source_artifact_hash: &str,
) -> Option<PackageIr> {
    let name = fields.get("%NAME%")?.first()?;
    let version = fields.get("%VERSION%")?.first()?;
    let arch = fields
        .get("%ARCH%")
        .and_then(|v| v.first())
        .map(|s| s.as_str())
        .unwrap_or(&scope.arch);

    let maintainers = fields
        .get("%PACKAGER%")
        .and_then(|v| v.first())
        .map(|packager| {
            let re = regex::Regex::new(r"^(.+?)\s*<(.+?)>$").unwrap();
            if let Some(caps) = re.captures(packager) {
                vec![MaintainerIr {
                    name: caps.get(1).unwrap().as_str().trim().to_string(),
                    email: Some(caps.get(2).unwrap().as_str().trim().to_string()),
                    role_hint: Some("maintainer".to_string()),
                }]
            } else {
                vec![MaintainerIr {
                    name: packager.trim().to_string(),
                    email: None,
                    role_hint: Some("maintainer".to_string()),
                }]
            }
        })
        .unwrap_or_default();

    let dependencies: Vec<DependencyIr> = fields
        .get("%DEPENDS%")
        .map(|deps| {
            deps.iter()
                .map(|d| {
                    let (name, constraint) = if let Some(idx) = d.find(|c: char| ">=<".contains(c))
                    {
                        (&d[..idx], Some(d[idx..].to_string()))
                    } else {
                        (d.as_str(), None)
                    };
                    DependencyIr {
                        name: name.to_string(),
                        dep_type: "requires".to_string(),
                        version_constraint: constraint,
                        flags: None,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let metadata = PackageMetadataIr {
        summary: fields.get("%DESC%").and_then(|v| v.first()).cloned(),
        description: fields.get("%DESC%").and_then(|v| v.first()).cloned(),
        homepage: fields.get("%URL%").and_then(|v| v.first()).cloned(),
        license: fields.get("%LICENSE%").and_then(|v| v.first()).cloned(),
        checksum: fields.get("%SHA256SUM%").and_then(|v| v.first()).cloned(),
        size_bytes: fields
            .get("%ISIZE%")
            .and_then(|v| v.first())
            .and_then(|s| s.parse().ok()),
    };

    let mut source_artifacts = BTreeMap::new();
    source_artifacts.insert("db".to_string(), source_artifact_hash.to_string());

    // Parse version: "1.0.0-1" → version "1.0.0", release "1"
    let (ver, rel) = if let Some(idx) = version.rfind('-') {
        (&version[..idx], Some(&version[idx + 1..]))
    } else {
        (version.as_str(), None)
    };

    Some(PackageIr {
        ir_schema: IR_SCHEMA_VERSION,
        scope: scope.clone(),
        source_artifacts,
        package: PackageInfo {
            kind: "binary".to_string(),
            name: name.clone(),
            epoch: 0,
            version: ver.to_string(),
            release: rel.map(String::from),
            full_version: version.clone(),
            arch: arch.to_string(),
        },
        source_package: None,
        maintainers,
        dependencies,
        metadata: Some(metadata),
        collector_specific: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_arch_package() {
        let mut fields: HashMap<String, Vec<String>> = HashMap::new();
        fields.insert("%NAME%".to_string(), vec!["gcc".to_string()]);
        fields.insert("%VERSION%".to_string(), vec!["14.1.1-2".to_string()]);
        fields.insert("%ARCH%".to_string(), vec!["x86_64".to_string()]);
        fields.insert(
            "%PACKAGER%".to_string(),
            vec!["Frederik Schwan <freswa@archlinux.org>".to_string()],
        );
        fields.insert(
            "%DEPENDS%".to_string(),
            vec!["glibc>=2.39".to_string(), "gcc-libs".to_string()],
        );
        fields.insert(
            "%DESC%".to_string(),
            vec!["The GNU Compiler Collection".to_string()],
        );
        fields.insert("%URL%".to_string(), vec!["https://gcc.gnu.org".to_string()]);

        let scope = ScopeIr {
            collector: "arch".to_string(),
            distro: "arch".to_string(),
            release: "arch".to_string(),
            repo: Some("core".to_string()),
            arch: "x86_64".to_string(),
        };

        let ir = normalize_arch_package(&fields, &scope, "sha256:xyz").unwrap();

        assert_eq!(ir.package.name, "gcc");
        assert_eq!(ir.package.version, "14.1.1");
        assert_eq!(ir.package.release.as_deref(), Some("2"));
        assert_eq!(ir.maintainers.len(), 1);
        assert_eq!(ir.maintainers[0].name, "Frederik Schwan");
        assert_eq!(ir.dependencies.len(), 2);
        assert_eq!(ir.dependencies[0].name, "glibc");
        assert_eq!(
            ir.dependencies[0].version_constraint.as_deref(),
            Some(">=2.39")
        );
    }
}
