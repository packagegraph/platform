// Debian collector unit tests
// Note: Full integration tests with mock HTTP servers are in integration.rs (Task 8)

use pg_collect::debian::DebianCollector;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use pg_collect::ntriples::NTriplesWriter;
use tempfile::NamedTempFile;

#[test]
fn test_emit_package_triples_basic() -> std::io::Result<()> {
    // Test that basic package triple emission works
    let collector = DebianCollector::new(
        "http://example.com".to_string(),
        "debian".to_string(),
        "stable".to_string(),
        "main".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let mut pkg_data = HashMap::new();
    pkg_data.insert("Package".to_string(), "testpkg".to_string());
    pkg_data.insert("Version".to_string(), "1.0-1".to_string());
    pkg_data.insert("Description".to_string(), "Test package".to_string());
    pkg_data.insert("Maintainer".to_string(), "John Doe <john@example.com>".to_string());

    let triple_count = collector.emit_package_triples(
        &mut writer,
        &pkg_data,
        "trixie",
        "stable",
        "amd64",
    )?;

    writer.flush()?;

    // Verify we emitted some triples
    assert!(triple_count > 0, "Should emit triples for package");

    // Read output and verify key triples are present
    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Check for dual typing
    let pkg_type_count = lines.iter().filter(|l| l.contains("BinaryPackage")).count();
    assert!(pkg_type_count >= 2, "Should have dual typing (pkg:BinaryPackage + deb:BinaryPackage)");

    // Check for package name
    assert!(lines.iter().any(|l| l.contains("packageName") && l.contains("testpkg")),
            "Should have packageName triple");

    // Check for version
    assert!(lines.iter().any(|l| l.contains("versionString") && l.contains("1.0-1")),
            "Should have version triple");

    // Check for maintainer
    assert!(lines.iter().any(|l| l.contains("mailto:john@example.com")),
            "Should have maintainer email");

    // Check for source package (defaults to same as binary when Source field absent)
    assert!(lines.iter().any(|l| l.contains("SourcePackage")),
            "Should have SourcePackage type");

    Ok(())
}

#[test]
fn test_emit_package_with_dependencies() -> std::io::Result<()> {
    let collector = DebianCollector::new(
        "http://example.com".to_string(),
        "debian".to_string(),
        "stable".to_string(),
        "main".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let mut pkg_data = HashMap::new();
    pkg_data.insert("Package".to_string(), "testpkg".to_string());
    pkg_data.insert("Version".to_string(), "1.0-1".to_string());
    pkg_data.insert("Depends".to_string(), "libc6 (>= 2.36), libstdc++6 (>= 12)".to_string());

    collector.emit_package_triples(
        &mut writer,
        &pkg_data,
        "trixie",
        "stable",
        "amd64",
    )?;

    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Check for dependency triples
    assert!(lines.iter().any(|l| l.contains("directlyDependsOn")),
            "Should have directlyDependsOn triple");

    assert!(lines.iter().any(|l| l.contains("Dependency")),
            "Should have Dependency type for reified node");

    assert!(lines.iter().any(|l| l.contains("VersionConstraint")),
            "Should have VersionConstraint for version-constrained dependency");

    Ok(())
}

#[test]
fn test_emit_package_with_source_field() -> std::io::Result<()> {
    let collector = DebianCollector::new(
        "http://example.com".to_string(),
        "debian".to_string(),
        "stable".to_string(),
        "main".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let mut pkg_data = HashMap::new();
    pkg_data.insert("Package".to_string(), "testpkg-bin".to_string());
    pkg_data.insert("Version".to_string(), "1.0-1".to_string());
    pkg_data.insert("Source".to_string(), "testpkg (1.0-1~deb12u1)".to_string());

    collector.emit_package_triples(
        &mut writer,
        &pkg_data,
        "trixie",
        "stable",
        "amd64",
    )?;

    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Check for source package link
    assert!(lines.iter().any(|l| l.contains("builtFromSource")),
            "Should have builtFromSource link");

    assert!(lines.iter().any(|l| l.contains("src/debian/trixie/testpkg/")),
            "Should reference correct source package URI");

    Ok(())
}

// Full integration test with mock HTTP server will be in integration.rs (Task 8)

#[test]
fn test_emit_package_with_provides() -> std::io::Result<()> {
    let collector = DebianCollector::new(
        "http://example.com".to_string(),
        "debian".to_string(),
        "stable".to_string(),
        "main".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let mut pkg_data = HashMap::new();
    pkg_data.insert("Package".to_string(), "testpkg".to_string());
    pkg_data.insert("Version".to_string(), "1.0-1".to_string());
    pkg_data.insert("Provides".to_string(), "virtual-foo, virtual-bar (= 2.0), virtual-baz".to_string());

    collector.emit_package_triples(
        &mut writer,
        &pkg_data,
        "trixie",
        "stable",
        "amd64",
    )?;

    writer.flush()?;

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Check for provides triples
    assert!(lines.iter().any(|l| l.contains("directlyProvides")),
            "Should have directlyProvides triple");

    assert!(lines.iter().any(|l| l.contains("debProvides")),
            "Should have debProvides triple");

    // Check for provided package identities
    assert!(lines.iter().any(|l| l.contains("virtual-foo")),
            "Should reference virtual-foo");
    assert!(lines.iter().any(|l| l.contains("virtual-bar")),
            "Should reference virtual-bar");
    assert!(lines.iter().any(|l| l.contains("virtual-baz")),
            "Should reference virtual-baz");

    assert!(lines.iter().any(|l| l.contains("PackageIdentity")),
            "Should have PackageIdentity type for provided packages");

    // Verify that version constraints are stripped (we should NOT see "(= 2.0)" in package names)
    assert!(!lines.iter().any(|l| l.contains("packageName") && l.contains("(=")),
            "Package names should not include version constraints");

    Ok(())
}

