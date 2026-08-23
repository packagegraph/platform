//! CLI integration tests for the Maven collector.
//!
//! These test that the binary accepts the expected CLI flags without
//! needing a running Fuseki instance.

use std::process::Command;

fn pg_collect_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pg-collect"))
}

#[test]
fn test_maven_cli_accepts_cache_dir() {
    // --help should list the --cache-dir option
    let output = pg_collect_bin()
        .args(["maven", "--help"])
        .output()
        .expect("failed to execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--cache-dir"),
        "Maven --help should mention --cache-dir, got:\n{}",
        stdout
    );
}

#[test]
fn test_maven_cli_accepts_cache_refresh() {
    let output = pg_collect_bin()
        .args(["maven", "--help"])
        .output()
        .expect("failed to execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--cache-refresh"),
        "Maven --help should mention --cache-refresh, got:\n{}",
        stdout
    );
}

#[test]
fn test_maven_cli_rejects_non_central_repo_base() {
    // Provide a non-Central --repo-base and required flags
    // Should fail with "Only Maven Central is supported"
    let output = pg_collect_bin()
        .args([
            "maven",
            "--repo-base",
            "https://nexus.example.com/maven",
            "--packages-file",
            "/dev/null",
            "-o",
            "/dev/null",
        ])
        .output()
        .expect("failed to execute");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "Should fail with non-Central repo-base"
    );
    assert!(
        stderr.contains("Only Maven Central is supported"),
        "Error should mention Central restriction, got:\n{}",
        stderr
    );
}
