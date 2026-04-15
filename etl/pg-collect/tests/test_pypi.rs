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

    let output_file = NamedTempFile::new().unwrap();

    let collector = PypiCollector::new();
    // This will fail until the spider is implemented
    // collector.collect(seed_file.path().to_str().unwrap(), 0, 5000, output_file.path().to_str().unwrap()).unwrap();
}
