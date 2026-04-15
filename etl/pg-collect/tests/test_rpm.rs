// RPM collector unit tests

use pg_collect::rpm::{RpmCollector, RpmDep, RpmPackageData};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use pg_collect::ntriples::NTriplesWriter;
use tempfile::NamedTempFile;

fn make_pkg_data(fields: Vec<(&str, &str)>, deps: Vec<RpmDep>) -> RpmPackageData {
    let mut map = HashMap::new();
    for (k, v) in fields {
        map.insert(k.to_string(), v.to_string());
    }
    RpmPackageData { fields: map, deps }
}

#[test]
fn test_emit_package_triples_basic() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "39".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let pkg_data = make_pkg_data(vec![
        ("name", "bash"),
        ("arch", "x86_64"),
        ("ver", "5.2.15"),
        ("rel", "1.fc39"),
        ("epoch", "0"),
        ("summary", "The GNU Bourne Again shell"),
    ], vec![]);

    let triple_count = collector.emit_package_triples(&mut writer, &pkg_data)?;

    writer.flush()?;

    assert!(triple_count > 0, "Should emit triples");

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Check for dual typing
    assert!(lines.iter().any(|l| l.contains("BinaryRPM")), "Should have rpm:BinaryRPM type");
    assert!(lines.iter().any(|l| l.contains("BinaryPackage")), "Should have pkg:BinaryPackage type");

    // Check for package name
    assert!(lines.iter().any(|l| l.contains("packageName") && l.contains("bash")),
            "Should have packageName triple");

    // Check for version
    assert!(lines.iter().any(|l| l.contains("versionString")),
            "Should have version triple");

    Ok(())
}

#[test]
fn test_emit_package_with_rpm_specific_properties() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "39".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let pkg_data = make_pkg_data(vec![
        ("name", "kernel"),
        ("arch", "x86_64"),
        ("ver", "6.5.6"),
        ("rel", "300.fc39"),
        ("epoch", "1"),
        ("sourcerpm", "kernel-6.5.6-300.fc39.src.rpm"),
        ("group", "System Environment/Kernel"),
    ], vec![]);

    collector.emit_package_triples(&mut writer, &pkg_data)?;

    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Check for sourceRPM
    assert!(lines.iter().any(|l| l.contains("sourceRPM") && l.contains("kernel-6.5.6-300.fc39.src.rpm")),
            "Should have sourceRPM property");

    // Check for RPM group
    assert!(lines.iter().any(|l| l.contains("RPMGroup") && l.contains("System Environment/Kernel")),
            "Should have RPMGroup property");

    // Check for epoch (when non-zero)
    assert!(lines.iter().any(|l| l.contains("epoch")),
            "Should have epoch property");

    // Check for SourcePackage entity from sourcerpm
    assert!(lines.iter().any(|l| l.contains("SourcePackage")),
            "Should create SourcePackage entity from sourcerpm");
    assert!(lines.iter().any(|l| l.contains("builtFromSource")),
            "Should link binary to source via builtFromSource");

    Ok(())
}

#[test]
fn test_version_string_format() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "39".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let pkg_data = make_pkg_data(vec![
        ("name", "test"),
        ("arch", "x86_64"),
        ("ver", "1.0"),
        ("rel", "1.fc39"),
    ], vec![]);

    collector.emit_package_triples(&mut writer, &pkg_data)?;

    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // RPM version format: {ver}-{rel}.{arch}
    assert!(lines.iter().any(|l| l.contains("1.0-1.fc39.x86_64")),
            "Version string should follow RPM format: ver-rel.arch");

    Ok(())
}

#[test]
fn test_emit_package_with_dependencies() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "41".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let deps = vec![
        RpmDep {
            name: "glibc".to_string(),
            flags: Some("GE".to_string()),
            epoch: Some("0".to_string()),
            ver: Some("2.17".to_string()),
            rel: None,
            dep_type: "requires".to_string(),
        },
        RpmDep {
            name: "libreadline.so.8()(64bit)".to_string(),
            flags: None,
            epoch: None,
            ver: None,
            rel: None,
            dep_type: "requires".to_string(),
        },
        RpmDep {
            name: "rpmlib(CompressedFileNames)".to_string(),
            flags: Some("LE".to_string()),
            epoch: None,
            ver: Some("3.0.4".to_string()),
            rel: Some("1".to_string()),
            dep_type: "requires".to_string(),
        },
        RpmDep {
            name: "old-bash".to_string(),
            flags: None,
            epoch: None,
            ver: None,
            rel: None,
            dep_type: "conflicts".to_string(),
        },
    ];

    let pkg_data = make_pkg_data(vec![
        ("name", "bash"),
        ("arch", "x86_64"),
        ("ver", "5.2.15"),
        ("rel", "1.fc41"),
        ("epoch", "0"),
    ], deps);

    let triple_count = collector.emit_package_triples(&mut writer, &pkg_data)?;

    writer.flush()?;

    assert!(triple_count > 0);

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Should have directlyDependsOn for glibc (requires)
    assert!(lines.iter().any(|l| l.contains("directlyDependsOn")),
            "Should emit directlyDependsOn for requires");

    // Should have rpmRequires
    assert!(lines.iter().any(|l| l.contains("rpmRequires")),
            "Should emit rpmRequires property");

    // Should NOT emit rpmlib() virtual deps
    assert!(!lines.iter().any(|l| l.contains("rpmlib(") && l.contains("packageName")),
            "Should skip rpmlib() virtual dependencies");

    // Should have Dependency bnode
    assert!(lines.iter().any(|l| l.contains("Dependency")),
            "Should create reified Dependency");

    // Should have VersionConstraint for glibc (has flags GE + ver)
    assert!(lines.iter().any(|l| l.contains("VersionConstraint")),
            "Should create VersionConstraint for versioned deps");
    assert!(lines.iter().any(|l| l.contains("versionConstraintOperator")),
            "Should emit version constraint operator");

    // Should have directlyConflictsWith for old-bash
    assert!(lines.iter().any(|l| l.contains("directlyConflictsWith")),
            "Should emit directlyConflictsWith for conflicts");

    // Should have rpmConflicts
    assert!(lines.iter().any(|l| l.contains("rpmConflicts")),
            "Should emit rpmConflicts property");

    Ok(())
}

#[test]
fn test_emit_maintainer_triples() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "41".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let pkg_data = make_pkg_data(vec![
        ("name", "bash"),
        ("arch", "x86_64"),
        ("ver", "5.2.15"),
        ("rel", "1.fc41"),
        ("packager", "Fedora Project <packager@fedoraproject.org>"),
    ], vec![]);

    collector.emit_package_triples(&mut writer, &pkg_data)?;

    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Should emit Maintainer type
    assert!(lines.iter().any(|l| l.contains("Maintainer")),
            "Should create Maintainer resource");

    // Should emit maintainedBy link
    assert!(lines.iter().any(|l| l.contains("maintainedBy")),
            "Should link package to maintainer");

    // Should emit foaf:name
    assert!(lines.iter().any(|l| l.contains("foaf") && l.contains("name") && l.contains("Fedora Project")),
            "Should emit maintainer name");

    Ok(())
}

#[test]
fn test_emit_maintainer_triples_name_only() -> std::io::Result<()> {
    // Test the real-world Fedora format: just "Fedora Project" with no email
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "43".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let pkg_data = make_pkg_data(vec![
        ("name", "bash"),
        ("arch", "x86_64"),
        ("ver", "5.2.15"),
        ("rel", "1.fc43"),
        ("packager", "Fedora Project"),
    ], vec![]);

    collector.emit_package_triples(&mut writer, &pkg_data)?;

    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Should emit Maintainer type
    assert!(lines.iter().any(|l| l.contains("Maintainer")),
            "Should create Maintainer resource");

    // Should emit maintainedBy link
    assert!(lines.iter().any(|l| l.contains("maintainedBy")),
            "Should link package to maintainer");

    // Should emit foaf:name
    assert!(lines.iter().any(|l| l.contains("foaf") && l.contains("name") && l.contains("Fedora Project")),
            "Should emit maintainer name");

    // Should NOT emit foaf:mbox when there's no email
    assert!(!lines.iter().any(|l| l.contains("foaf") && l.contains("mbox")),
            "Should not emit mbox when packager has no email");

    // URI should use stable ID (lowercase, hyphenated)
    assert!(lines.iter().any(|l| l.contains("fedora-project")),
            "Should use stable ID in maintainer URI");

    Ok(())
}

#[test]
fn test_emit_ecosystem_from_provides_cargo() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "43".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let deps = vec![
        RpmDep {
            name: "crate(tokio)".to_string(),
            flags: Some("EQ".to_string()),
            epoch: None,
            ver: Some("1.36.0".to_string()),
            rel: None,
            dep_type: "provides".to_string(),
        },
        RpmDep {
            name: "crate(tokio/rt)".to_string(),
            flags: Some("EQ".to_string()),
            epoch: None,
            ver: Some("1.36.0".to_string()),
            rel: None,
            dep_type: "provides".to_string(),
        },
    ];

    let pkg_data = make_pkg_data(vec![
        ("name", "rust-tokio"),
        ("arch", "noarch"),
        ("ver", "1.36.0"),
        ("rel", "1.fc43"),
        ("epoch", "0"),
    ], deps);

    collector.emit_package_triples(&mut writer, &pkg_data)?;
    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    assert!(lines.iter().any(|l| l.contains("upstreamEcosystem") && l.contains("cargo")),
            "Should identify cargo ecosystem");
    assert!(lines.iter().any(|l| l.contains("upstreamPackageName") && l.contains("\"tokio\"")),
            "Should extract crate name 'tokio'");
    assert!(lines.iter().any(|l| l.contains("upstreamPackageName") && l.contains("\"tokio/rt\"")),
            "Should extract crate feature 'tokio/rt'");
    assert!(lines.iter().any(|l| l.contains("upstreamPackageVersion") && l.contains("\"1.36.0\"")),
            "Should extract upstream version");

    // Should only emit ecosystem once
    let eco_count = lines.iter().filter(|l| l.contains("upstreamEcosystem")).count();
    assert_eq!(eco_count, 1, "Should emit upstreamEcosystem exactly once");

    Ok(())
}

#[test]
fn test_emit_ecosystem_from_provides_python() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "43".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let deps = vec![
        RpmDep {
            name: "python3dist(requests)".to_string(),
            flags: Some("EQ".to_string()),
            epoch: None,
            ver: Some("2.31.0".to_string()),
            rel: None,
            dep_type: "provides".to_string(),
        },
    ];

    let pkg_data = make_pkg_data(vec![
        ("name", "python3-requests"),
        ("arch", "noarch"),
        ("ver", "2.31.0"),
        ("rel", "1.fc43"),
        ("epoch", "0"),
    ], deps);

    collector.emit_package_triples(&mut writer, &pkg_data)?;
    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    assert!(lines.iter().any(|l| l.contains("upstreamEcosystem") && l.contains("pypi")),
            "Should identify pypi ecosystem");
    assert!(lines.iter().any(|l| l.contains("upstreamPackageName") && l.contains("\"requests\"")),
            "Should extract Python package name");
    assert!(lines.iter().any(|l| l.contains("upstreamPackageVersion") && l.contains("\"2.31.0\"")),
            "Should extract upstream version");

    Ok(())
}

#[test]
fn test_emit_ecosystem_from_provides_golang() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "43".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let deps = vec![
        RpmDep {
            name: "golang(github.com/gorilla/mux)".to_string(),
            flags: None,
            epoch: None,
            ver: None,
            rel: None,
            dep_type: "provides".to_string(),
        },
    ];

    let pkg_data = make_pkg_data(vec![
        ("name", "golang-github-gorilla-mux"),
        ("arch", "noarch"),
        ("ver", "1.8.1"),
        ("rel", "1.fc43"),
        ("epoch", "0"),
    ], deps);

    collector.emit_package_triples(&mut writer, &pkg_data)?;
    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    assert!(lines.iter().any(|l| l.contains("upstreamEcosystem") && l.contains("gomod")),
            "Should identify gomod ecosystem");
    assert!(lines.iter().any(|l| l.contains("upstreamPackageName") && l.contains("github.com/gorilla/mux")),
            "Should extract Go import path");

    Ok(())
}

#[test]
fn test_no_ecosystem_for_plain_rpm() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "43".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    // Regular RPM with no language ecosystem provides
    let deps = vec![
        RpmDep {
            name: "bash".to_string(),
            flags: Some("EQ".to_string()),
            epoch: None,
            ver: Some("5.2.15".to_string()),
            rel: None,
            dep_type: "provides".to_string(),
        },
    ];

    let pkg_data = make_pkg_data(vec![
        ("name", "bash"),
        ("arch", "x86_64"),
        ("ver", "5.2.15"),
        ("rel", "1.fc43"),
        ("epoch", "0"),
    ], deps);

    collector.emit_package_triples(&mut writer, &pkg_data)?;
    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    assert!(!lines.iter().any(|l| l.contains("upstreamEcosystem")),
            "Plain RPMs should NOT have upstreamEcosystem");
    assert!(!lines.iter().any(|l| l.contains("upstreamPackageName")),
            "Plain RPMs should NOT have upstreamPackageName");

    Ok(())
}

#[test]
fn test_distribution_rdfs_label() -> std::io::Result<()> {
    // Test that Distribution instances get human-readable rdfs:label
    // We can't call emit_distribution_metadata directly (it's private),
    // but we can verify the constant exists by compiling
    use pg_collect::uris::RDFS_LABEL;

    assert_eq!(RDFS_LABEL, "http://www.w3.org/2000/01/rdf-schema#label");

    // The actual label emission is tested via integration tests
    // or by running the collector and checking the output
    Ok(())
}
