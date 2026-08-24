//! Alpine normalizer — converts APKINDEX key=value data into PackageIr records.

use crate::ir::*;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Result};
use std::path::Path;

/// Normalize an Alpine APKINDEX entry into a PackageIr.
pub fn normalize_alpine_package(
    fields: &HashMap<String, String>,
    scope: &ScopeIr,
    source_artifact_hash: &str,
) -> Option<PackageIr> {
    let name = fields.get("P")?;
    let version = fields.get("V")?;
    let arch = fields.get("A").map(|s| s.as_str()).unwrap_or(&scope.arch);

    let maintainers = fields
        .get("m")
        .map(|m| {
            let re = regex::Regex::new(r"^(.+?)\s*<(.+?)>$").unwrap();
            if let Some(caps) = re.captures(m) {
                vec![MaintainerIr {
                    name: caps.get(1).unwrap().as_str().trim().to_string(),
                    email: Some(caps.get(2).unwrap().as_str().trim().to_string()),
                    role_hint: Some("maintainer".to_string()),
                }]
            } else {
                vec![MaintainerIr {
                    name: m.trim().to_string(),
                    email: None,
                    role_hint: Some("maintainer".to_string()),
                }]
            }
        })
        .unwrap_or_default();

    // Dependencies
    let dependencies: Vec<DependencyIr> = fields
        .get("D")
        .map(|deps_str| {
            deps_str
                .split_whitespace()
                .filter(|d| !d.starts_with("so:") && !d.starts_with("pc:"))
                .map(|d| {
                    let (name, constraint) = if let Some(idx) = d.find(|c: char| ">=<~".contains(c))
                    {
                        (&d[..idx], Some(d[idx..].to_string()))
                    } else {
                        (d, None)
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

    // Source package
    let source_package = fields.get("o").map(|origin| SourcePackageRef {
        name: origin.clone(),
        version: Some(version.clone()),
        release: None,
    });

    let metadata = PackageMetadataIr {
        summary: fields.get("T").cloned(),
        description: fields.get("T").cloned(),
        homepage: fields.get("U").cloned(),
        license: fields.get("L").cloned(),
        checksum: fields.get("C").cloned(),
        size_bytes: fields.get("S").and_then(|s| s.parse().ok()),
    };

    let mut source_artifacts = BTreeMap::new();
    source_artifacts.insert("apkindex".to_string(), source_artifact_hash.to_string());

    Some(PackageIr {
        ir_schema: IR_SCHEMA_VERSION,
        scope: scope.clone(),
        source_artifacts,
        package: PackageInfo {
            kind: "binary".to_string(),
            name: name.clone(),
            epoch: 0,
            version: version.clone(),
            release: None,
            full_version: version.clone(),
            arch: arch.to_string(),
        },
        source_package,
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
    fn test_normalize_alpine_package() {
        let mut fields = HashMap::new();
        fields.insert("P".to_string(), "musl".to_string());
        fields.insert("V".to_string(), "1.2.5-r0".to_string());
        fields.insert("A".to_string(), "x86_64".to_string());
        fields.insert(
            "m".to_string(),
            "Natanael Copa <ncopa@alpinelinux.org>".to_string(),
        );
        fields.insert("o".to_string(), "musl".to_string());
        fields.insert("T".to_string(), "musl C library".to_string());
        fields.insert("D".to_string(), "so:ld-musl-x86_64.so.1".to_string());

        let scope = ScopeIr {
            collector: "alpine".to_string(),
            distro: "alpine".to_string(),
            release: "v3.20".to_string(),
            repo: Some("main".to_string()),
            arch: "x86_64".to_string(),
        };

        let ir = normalize_alpine_package(&fields, &scope, "sha256:abc").unwrap();

        assert_eq!(ir.package.name, "musl");
        assert_eq!(ir.package.version, "1.2.5-r0");
        assert_eq!(ir.maintainers.len(), 1);
        assert_eq!(ir.maintainers[0].name, "Natanael Copa");
        assert!(ir.source_package.is_some());
        assert_eq!(ir.source_package.as_ref().unwrap().name, "musl");
    }
}
