// RPM collector unit tests

use pg_collect::rpm::RpmCollector;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use pg_collect::ntriples::NTriplesWriter;
use tempfile::NamedTempFile;

#[test]
fn test_emit_package_triples_basic() -> std::io::Result<()> {
    let collector = RpmCollector::new(
        "http://example.com".to_string(),
        "fedora".to_string(),
        "39".to_string(),
    );

    let temp_file = NamedTempFile::new()?;
    let mut writer = NTriplesWriter::new(temp_file.reopen()?);

    let mut pkg_data = HashMap::new();
    pkg_data.insert("name".to_string(), "bash".to_string());
    pkg_data.insert("arch".to_string(), "x86_64".to_string());
    pkg_data.insert("ver".to_string(), "5.2.15".to_string());
    pkg_data.insert("rel".to_string(), "1.fc39".to_string());
    pkg_data.insert("epoch".to_string(), "0".to_string());
    pkg_data.insert("summary".to_string(), "The GNU Bourne Again shell".to_string());

    let triple_count = collector.emit_package_triples(&mut writer, &pkg_data)?;

    writer.flush()?;

    assert!(triple_count > 0, "Should emit triples");

    let output_file = temp_file.reopen()?;
    let reader = BufReader::new(output_file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    // Check for dual typing
    let rpm_type_count = lines.iter().filter(|l| l.contains("BinaryRPM")).count();
    assert!(rpm_type_count >= 1, "Should have rpm:BinaryRPM type");

    let pkg_type_count = lines.iter().filter(|l| l.contains("BinaryPackage")).count();
    assert!(pkg_type_count >= 1, "Should have pkg:BinaryPackage type");

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

    let mut pkg_data = HashMap::new();
    pkg_data.insert("name".to_string(), "kernel".to_string());
    pkg_data.insert("arch".to_string(), "x86_64".to_string());
    pkg_data.insert("ver".to_string(), "6.5.6".to_string());
    pkg_data.insert("rel".to_string(), "300.fc39".to_string());
    pkg_data.insert("epoch".to_string(), "1".to_string());
    pkg_data.insert("sourcerpm".to_string(), "kernel-6.5.6-300.fc39.src.rpm".to_string());
    pkg_data.insert("group".to_string(), "System Environment/Kernel".to_string());

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

    let mut pkg_data = HashMap::new();
    pkg_data.insert("name".to_string(), "test".to_string());
    pkg_data.insert("arch".to_string(), "x86_64".to_string());
    pkg_data.insert("ver".to_string(), "1.0".to_string());
    pkg_data.insert("rel".to_string(), "1.fc39".to_string());

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

// Note: Dependency parsing (requires, provides, conflicts) is noted in plan but not in current Python implementation.
// It's in the DoD but will need to be added. For now, tests verify the core functionality works.
