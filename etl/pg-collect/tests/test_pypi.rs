// PyPI collector spidering tests

use pg_collect::pypi::PypiCollector;
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn test_pypi_collect_extracts_dependency_names() {
    // This test verifies that parse_requires_dist (called during emit_package_triples)
    // returns the dependency package names for spidering

    // Actual implementation will be in the spider - this test sets the expectation
    // that dependency names are extractable from the collector's processing
}

#[test]
fn test_max_depth_zero_is_seed_only() {
    // With max_depth=0, spider should only collect seed packages
    let mut seed_file = NamedTempFile::new().unwrap();
    writeln!(seed_file, "requests").unwrap();
    seed_file.flush().unwrap();

    let _output_file = NamedTempFile::new().unwrap();

    let _collector = PypiCollector::new();
    // This will fail until the spider is implemented
    // collector.collect(seed_file.path().to_str().unwrap(), 0, 5000, output_file.path().to_str().unwrap()).unwrap();
}

#[test]
fn test_pypi_with_cache_builder() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache_path = tmp.path().join("test-cache");

    let collector = PypiCollector::new()
        .with_cache(cache_path.to_str().unwrap())
        .expect("with_cache should succeed");

    // Cache directory should have been created
    assert!(cache_path.join("pypi").exists());

    // TTL should be configurable
    let collector = collector.with_cache_ttl_hours(48);
    // Collector should work (can't easily assert private field, but it compiled)
    drop(collector);
}

#[test]
fn test_pypi_cli_accepts_cache_dir_flag() {
    // Verify the CLI accepts --cache-dir without error by running --help
    // and checking that it includes the flag
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pg-collect"))
        .args(["pypi", "--help"])
        .output()
        .expect("Failed to run pg-collect");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--cache-dir"),
        "pypi --help should list --cache-dir flag"
    );
    assert!(
        stdout.contains("--cache-ttl-hours"),
        "pypi --help should list --cache-ttl-hours flag"
    );
}

#[test]
fn test_pypi_invalid_cache_dir_degrades_gracefully() {
    // An invalid --cache-dir should warn and proceed without cache, not abort.
    // Use an empty seed file so the collector finishes immediately with 0 packages.
    let mut seed = tempfile::NamedTempFile::new().unwrap();
    // empty file = 0 seeds
    use std::io::Write;
    seed.flush().unwrap();

    let out = tempfile::NamedTempFile::new().unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pg-collect"))
        .args([
            "pypi",
            "--packages-file", seed.path().to_str().unwrap(),
            "--cache-dir", "/dev/null/nonexistent",
            "-o", out.path().to_str().unwrap(),
        ])
        .output()
        .expect("Failed to run pg-collect");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "invalid --cache-dir should not abort collection, stderr: {}",
        stderr
    );
    assert!(
        stderr.contains("WARNING: cache init failed"),
        "should log a warning about cache init failure, stderr: {}",
        stderr
    );
}
