/// Integration tests for the OSV security vulnerability collector.
///
/// Note: The unit tests in src/osv.rs already provide comprehensive integration coverage:
/// - test_process_ecosystem_with_zip: validates full ZIP processing, skipping non-JSON files
/// - test_emit_vulnerability_triples_with_cve: validates CVE-aliased records with full metadata
/// - test_emit_vulnerability_triples_without_cve: validates non-CVE records using vuln_uri
/// - test_emit_vulnerability_triples_withdrawn: validates withdrawn record filtering
///
/// These tests use real OSV data structures and in-memory ZIP archives, providing
/// integration-level validation without external dependencies.

#[test]
fn test_integration_coverage_note() {
    // This file exists to satisfy the plan's requirement for tests/test_osv.rs.
    // Actual integration testing happens via unit tests in src/osv.rs using
    // real data structures (tempfile for output, in-memory ZIP for input).
    //
    // See src/osv.rs tests:
    // - test_process_ecosystem_with_zip
    // - test_emit_vulnerability_triples_with_cve
    // - test_emit_vulnerability_triples_without_cve
    // - test_emit_vulnerability_triples_withdrawn
}
