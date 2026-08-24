//! Debian normalizer — converts parsed Packages.gz data into PackageIr records.

use crate::ir::*;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Result};
use std::path::Path;

/// Normalize a Debian package entry (HashMap from Packages.gz parsing) into a PackageIr.
pub fn normalize_debian_package(
    fields: &HashMap<String, String>,
    scope: &ScopeIr,
    source_artifact_hash: &str,
) -> Option<PackageIr> {
    let name = fields.get("Package")?;
    let version = fields.get("Version")?;
    let arch = fields
        .get("Architecture")
        .map(|s| s.as_str())
        .unwrap_or(&scope.arch);

    // Parse epoch from version string
    let (epoch, clean_version) = if let Some(idx) = version.find(':') {
        let e: u32 = version[..idx].parse().unwrap_or(0);
        (e, &version[idx + 1..])
    } else {
        (0, version.as_str())
    };

    // Maintainer
    let maintainers = if let Some(maint) = fields.get("Maintainer") {
        parse_maintainer(maint)
    } else {
        vec![]
    };

    // Dependencies
    let mut dependencies = Vec::new();
    for (field, dep_type) in &[
        ("Depends", "requires"),
        ("Pre-Depends", "pre-depends"),
        ("Recommends", "recommends"),
        ("Suggests", "suggests"),
        ("Enhances", "enhances"),
        ("Conflicts", "conflicts"),
        ("Breaks", "breaks"),
        ("Provides", "provides"),
        ("Replaces", "replaces"),
    ] {
        if let Some(dep_str) = fields.get(*field) {
            for dep in parse_debian_deps(dep_str, dep_type) {
                dependencies.push(dep);
            }
        }
    }

    // Source package
    let source_package = fields.get("Source").map(|src| {
        // "Source: name (version)" or just "Source: name"
        if let Some(paren) = src.find('(') {
            let src_name = src[..paren].trim();
            let src_ver = src[paren + 1..].trim_end_matches(')').trim();
            SourcePackageRef {
                name: src_name.to_string(),
                version: Some(src_ver.to_string()),
                release: None,
            }
        } else {
            SourcePackageRef {
                name: src.trim().to_string(),
                version: Some(version.clone()),
                release: None,
            }
        }
    });

    // Metadata
    let metadata = PackageMetadataIr {
        summary: fields
            .get("Description")
            .and_then(|d| d.lines().next().map(String::from)),
        description: fields.get("Description").cloned(),
        homepage: fields.get("Homepage").cloned(),
        license: None, // Debian doesn't have a license field in Packages
        checksum: fields.get("SHA256").cloned(),
        size_bytes: fields
            .get("Installed-Size")
            .and_then(|s| s.parse::<u64>().ok().map(|kb| kb * 1024)),
    };

    // Collector-specific
    let mut collector_specific = serde_json::Map::new();
    if let Some(suite) = fields.get("Suite") {
        collector_specific.insert(
            "suite".to_string(),
            serde_json::Value::String(suite.clone()),
        );
    }
    if let Some(component) = scope.repo.as_ref() {
        collector_specific.insert(
            "component".to_string(),
            serde_json::Value::String(component.clone()),
        );
    }
    if let Some(installed_size) = fields.get("Installed-Size") {
        collector_specific.insert(
            "installed_size_kb".to_string(),
            serde_json::Value::String(installed_size.clone()),
        );
    }
    if let Some(vcs_git) = fields.get("Vcs-Git") {
        collector_specific.insert(
            "vcs_git".to_string(),
            serde_json::Value::String(vcs_git.clone()),
        );
    }

    let mut source_artifacts = BTreeMap::new();
    source_artifacts.insert("packages_gz".to_string(), source_artifact_hash.to_string());

    Some(PackageIr {
        ir_schema: IR_SCHEMA_VERSION,
        scope: scope.clone(),
        source_artifacts,
        package: PackageInfo {
            kind: "binary".to_string(),
            name: name.clone(),
            epoch,
            version: clean_version.to_string(),
            release: None,
            full_version: version.clone(),
            arch: arch.to_string(),
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

/// Parse Packages.gz bytes into PackageIr records and write to an IR shard.
pub fn normalize_debian_shard(
    packages_bytes: &[u8],
    scope: &ScopeIr,
    source_artifact_hash: &str,
    output_path: &Path,
) -> Result<usize> {
    let mut writer = IrWriter::new(output_path)?;
    let reader = BufReader::new(packages_bytes);

    let mut current_pkg: HashMap<String, String> = HashMap::new();
    let mut last_key = String::new();

    for line in reader.lines() {
        let line = line?;

        if line.is_empty() {
            if !current_pkg.is_empty()
                && current_pkg.contains_key("Package")
                && current_pkg.contains_key("Version")
            {
                if let Some(ir) =
                    normalize_debian_package(&current_pkg, scope, source_artifact_hash)
                {
                    writer.write(&ir)?;
                }
            }
            current_pkg.clear();
            last_key.clear();
        } else if line.starts_with(' ') || line.starts_with('\t') {
            if !last_key.is_empty() {
                if let Some(value) = current_pkg.get_mut(&last_key) {
                    value.push(' ');
                    value.push_str(line.trim());
                }
            }
        } else if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            last_key = key.clone();
            current_pkg.insert(key, value.trim().to_string());
        }
    }

    // Process last package
    if !current_pkg.is_empty()
        && current_pkg.contains_key("Package")
        && current_pkg.contains_key("Version")
    {
        if let Some(ir) = normalize_debian_package(&current_pkg, scope, source_artifact_hash) {
            writer.write(&ir)?;
        }
    }

    writer.finish()
}

fn parse_maintainer(maint: &str) -> Vec<MaintainerIr> {
    use super::maintainer::{is_email_iri_safe, parse_mailbox_list};

    let parsed = parse_mailbox_list(maint);

    if parsed.malformed_count > 0 {
        eprintln!(
            "WARNING: {} malformed maintainer entries in: {}",
            parsed.malformed_count, maint
        );
    }

    let mut iri_unsafe_count = 0usize;
    let mut result = Vec::with_capacity(parsed.mailboxes.len());
    for m in parsed.mailboxes {
        let email = if let Some(e) = m.email {
            if is_email_iri_safe(&e) {
                Some(e)
            } else {
                iri_unsafe_count += 1;
                None
            }
        } else {
            None
        };
        result.push(MaintainerIr {
            name: m.name,
            email,
            role_hint: Some("maintainer".to_string()),
        });
    }

    if iri_unsafe_count > 0 {
        eprintln!(
            "WARNING: {} IRI-unsafe email addresses skipped in: {}",
            iri_unsafe_count, maint
        );
    }

    result
}

fn parse_debian_deps(dep_str: &str, dep_type: &str) -> Vec<DependencyIr> {
    dep_str
        .split(',')
        .filter_map(|dep| {
            let dep = dep.trim();
            // Take first alternative (before |)
            let dep = dep.split('|').next().unwrap_or(dep).trim();
            if dep.is_empty() {
                return None;
            }
            // Parse "name (>= version)" or just "name"
            let (name, constraint) = if let Some(paren) = dep.find('(') {
                let n = dep[..paren].trim();
                let c = dep[paren + 1..].trim_end_matches(')').trim();
                (n, Some(c.to_string()))
            } else {
                (dep.split_whitespace().next().unwrap_or(dep), None)
            };

            Some(DependencyIr {
                name: name.to_string(),
                dep_type: dep_type.to_string(),
                version_constraint: constraint,
                flags: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scope() -> ScopeIr {
        ScopeIr {
            collector: "debian".to_string(),
            distro: "debian".to_string(),
            release: "trixie".to_string(),
            repo: Some("main".to_string()),
            arch: "amd64".to_string(),
        }
    }

    #[test]
    fn test_normalize_debian_package() {
        let mut fields = HashMap::new();
        fields.insert("Package".to_string(), "libc6".to_string());
        fields.insert("Version".to_string(), "2.36-9+deb13u1".to_string());
        fields.insert("Architecture".to_string(), "amd64".to_string());
        fields.insert(
            "Maintainer".to_string(),
            "GNU Libc Maintainers <debian-glibc@lists.debian.org>".to_string(),
        );
        fields.insert("Depends".to_string(), "libgcc-s1, libc-l10n".to_string());
        fields.insert("Source".to_string(), "glibc".to_string());
        fields.insert(
            "Description".to_string(),
            "GNU C Library: Shared libraries".to_string(),
        );

        let ir = normalize_debian_package(&fields, &sample_scope(), "sha256:xyz").unwrap();

        assert_eq!(ir.package.name, "libc6");
        assert_eq!(ir.package.full_version, "2.36-9+deb13u1");
        assert_eq!(ir.maintainers.len(), 1);
        assert_eq!(ir.maintainers[0].name, "GNU Libc Maintainers");
        assert_eq!(ir.dependencies.len(), 2);
        assert_eq!(ir.dependencies[0].name, "libgcc-s1");
        assert!(ir.source_package.is_some());
        assert_eq!(ir.source_package.as_ref().unwrap().name, "glibc");
    }

    #[test]
    fn test_normalize_debian_shard() {
        let packages_text = b"Package: libc6\nVersion: 2.36-9\nArchitecture: amd64\n\nPackage: gcc\nVersion: 13.2.0-1\nArchitecture: amd64\n\n";

        let tmp = tempfile::TempDir::new().unwrap();
        let ir_path = tmp.path().join("test.jsonl.zst");

        let count =
            normalize_debian_shard(packages_text, &sample_scope(), "sha256:abc", &ir_path).unwrap();
        assert_eq!(count, 2);

        let reader = IrReader::new(&ir_path).unwrap();
        let records: Vec<PackageIr> = reader
            .records()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].package.name, "libc6");
        assert_eq!(records[1].package.name, "gcc");
    }

    #[test]
    fn test_parse_debian_deps() {
        let deps = parse_debian_deps(
            "libc6 (>= 2.17), libpthread-stubs0-dev, zlib1g (>= 1:1.1.4)",
            "requires",
        );
        assert_eq!(deps.len(), 3);
        assert_eq!(deps[0].name, "libc6");
        assert_eq!(deps[0].version_constraint.as_deref(), Some(">= 2.17"));
        assert_eq!(deps[1].name, "libpthread-stubs0-dev");
        assert!(deps[1].version_constraint.is_none());
    }

    #[test]
    fn test_parse_epoch_version() {
        let mut fields = HashMap::new();
        fields.insert("Package".to_string(), "emacs".to_string());
        fields.insert("Version".to_string(), "1:29.1+1-4".to_string());
        fields.insert("Architecture".to_string(), "amd64".to_string());

        let ir = normalize_debian_package(&fields, &sample_scope(), "sha256:abc").unwrap();
        assert_eq!(ir.package.epoch, 1);
        assert_eq!(ir.package.version, "29.1+1-4");
        assert_eq!(ir.package.full_version, "1:29.1+1-4");
    }

    #[test]
    fn test_parse_maintainer_multi() {
        let maintainers = parse_maintainer(
            "Steve Langasek <vorlon@debian.org>, Michael Vogt <michael.vogt@ubuntu.com>",
        );
        assert_eq!(maintainers.len(), 2);
        assert_eq!(maintainers[0].name, "Steve Langasek");
        assert_eq!(maintainers[0].email.as_deref(), Some("vorlon@debian.org"));
        assert_eq!(maintainers[1].name, "Michael Vogt");
        assert_eq!(
            maintainers[1].email.as_deref(),
            Some("michael.vogt@ubuntu.com")
        );
    }

    #[test]
    fn test_parse_maintainer_name_only() {
        let maintainers = parse_maintainer("Debian QA Group");
        assert_eq!(maintainers.len(), 1);
        assert_eq!(maintainers[0].name, "Debian QA Group");
        assert!(maintainers[0].email.is_none());
        assert_eq!(maintainers[0].role_hint.as_deref(), Some("maintainer"));
    }

    #[test]
    fn test_parse_maintainer_quoted_comma() {
        let maintainers = parse_maintainer("\"Doe, Jane\" <jane@example.org>");
        assert_eq!(maintainers.len(), 1);
        assert_eq!(maintainers[0].name, "Doe, Jane");
        assert_eq!(maintainers[0].email.as_deref(), Some("jane@example.org"));
    }

    #[test]
    fn test_normalize_multi_maintainer_package() {
        let mut fields = HashMap::new();
        fields.insert("Package".to_string(), "apt".to_string());
        fields.insert("Version".to_string(), "2.7.14".to_string());
        fields.insert("Architecture".to_string(), "amd64".to_string());
        fields.insert(
            "Maintainer".to_string(),
            "APT Development Team <deity@lists.debian.org>, John Doe <john@example.org>"
                .to_string(),
        );

        let ir = normalize_debian_package(&fields, &sample_scope(), "sha256:xyz").unwrap();
        assert_eq!(ir.maintainers.len(), 2);
        assert_eq!(ir.maintainers[0].name, "APT Development Team");
        assert_eq!(ir.maintainers[1].name, "John Doe");
    }
}
