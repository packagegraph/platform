//! Execute the repository-owned wrapper rehearsal against this build, not a
//! stale PATH binary. Python's stdlib supplies the local HTTP fixtures; shell,
//! pg-collect, FileCache, OutputCache, upload-nt.sh and commit are real.
#[cfg(unix)]
#[test]
fn wrapper_checkpoint_lifecycle() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/quadlet/collectors/tests/test_checkpoint_lifecycle.py");
    let output = std::process::Command::new("python3")
        .arg(script)
        .arg("-v")
        .env("PG_COLLECT_BIN", env!("CARGO_BIN_EXE_pg-collect"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("wrapper rehearsal requires python3 and POSIX shell tools");
    assert!(
        output.status.success(),
        "wrapper rehearsal failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
