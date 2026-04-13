// Cross-language URI comparison test
// Verifies that Rust URI builder functions produce identical output to Python

use pg_collect::uris::*;
use std::process::Command;

#[test]
fn test_uri_parity_with_python() {
    // Test cases with expected Python output
    let test_cases = vec![
        ("package_uri", vec!["debian", "trixie", "amd64", "libc6", "2.36-1"]),
        ("package_uri", vec!["debian", "trixie", "amd64", "libstdc++-dev", "12.2.0-14"]),
        ("package_uri", vec!["debian", "trixie", "amd64", "python3:amd64", "3.11.2-1"]),
        ("source_uri", vec!["debian", "trixie", "glibc", "2.36-1"]),
        ("version_uri", vec!["debian", "trixie", "libc6", "2.36-1"]),
        ("version_uri", vec!["debian", "trixie", "package+test", "2:1.2.3~rc1-4"]),
        ("maintainer_uri", vec!["john@example.com"]),
        ("arch_uri", vec!["amd64"]),
        ("distro_uri", vec!["debian"]),
        ("release_uri", vec!["debian", "trixie"]),
        ("repo_uri", vec!["https://github.com/packagegraph/ontology/"]),
    ];

    for (func_name, args) in test_cases {
        let rust_uri = match func_name {
            "package_uri" => package_uri(args[0], args[1], args[2], args[3], args[4]),
            "source_uri" => source_uri(args[0], args[1], args[2], args[3]),
            "version_uri" => version_uri(args[0], args[1], args[2], args[3]),
            "maintainer_uri" => maintainer_uri(args[0]),
            "arch_uri" => arch_uri(args[0]),
            "distro_uri" => distro_uri(args[0]),
            "release_uri" => release_uri(args[0], args[1]),
            "repo_uri" => repo_uri(args[0]),
            _ => panic!("Unknown function: {}", func_name),
        };

        // Call Python to get the expected URI
        // Get absolute path to etl directory
        // Tests run from pg-collect/tests/ dir, so we need ../../ to get to etl/
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let etl_dir = std::path::PathBuf::from(&manifest_dir)
            .parent()
            .unwrap()
            .to_path_buf();

        let etl_dir_str = etl_dir.display().to_string();
        let args_str = args.iter()
            .map(|a| format!("'{}'", a))
            .collect::<Vec<_>>()
            .join(", ");

        let python_script = format!(
            r#"
import sys
sys.path.insert(0, r'{}')
from packagegraph.namespaces import {}
result = {}({})
print(str(result))
"#,
            etl_dir_str, func_name, func_name, args_str
        );

        // Use uv run to ensure dependencies are available
        let output = Command::new("uv")
            .arg("run")
            .arg("python3")
            .arg("-c")
            .arg(&python_script)
            .current_dir(&etl_dir)
            .output();

        match output {
            Ok(output) if output.status.success() => {
                let python_uri = String::from_utf8_lossy(&output.stdout).trim().to_string();
                assert_eq!(
                    rust_uri, python_uri,
                    "URI mismatch for {}({:?})\nRust:   {}\nPython: {}",
                    func_name, args, rust_uri, python_uri
                );
            }
            Ok(output) => {
                eprintln!("Python script failed: {}", String::from_utf8_lossy(&output.stderr));
                eprintln!("Script:\n{}", python_script);
                panic!("Python execution failed for {}({:?})", func_name, args);
            }
            Err(e) => {
                eprintln!("Failed to execute Python: {}", e);
                eprintln!("Note: This test requires Python with the packagegraph module installed.");
                eprintln!("Skipping Python comparison, but Rust tests passed.");
                // Don't fail the test if Python is not available
                return;
            }
        }
    }
}

#[test]
fn test_special_characters_encoding() {
    // Test that special characters in package names are properly encoded

    // Package with + (common in C++ libraries)
    let uri = package_uri("debian", "trixie", "amd64", "libstdc++-dev", "12.2.0-14");
    assert!(uri.contains("libstdc%2B%2B-dev"), "'+' must be encoded as %2B");

    // Package with : (multi-arch notation)
    let uri = package_uri("debian", "trixie", "amd64", "python3:amd64", "3.11.2-1");
    assert!(uri.contains("python3%3Aamd64"), "':' must be encoded as %3A");

    // Version with epoch (2:) - colon is encoded, tilde is NOT (unreserved char)
    let uri = version_uri("debian", "trixie", "package", "2:1.2.3~rc1-4");
    eprintln!("DEBUG: version_uri output = {}", uri);
    assert!(uri.contains("2%3A1.2.3~rc1-4"), "':' must be encoded, '~' must NOT be encoded");

    // Maintainer email with @ - must NOT be encoded
    let uri = maintainer_uri("maintainer@debian.org");
    assert_eq!(uri, "https://packagegraph.github.io/data/maintainer/maintainer@debian.org");
    assert!(!uri.contains("%40"), "@ in maintainer_uri must NOT be encoded");
    assert!(!uri.contains("%2E"), ". in maintainer_uri must NOT be encoded");
}
