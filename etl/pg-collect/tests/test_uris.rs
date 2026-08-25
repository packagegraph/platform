// Cross-language URI comparison test — REMOVED after Python deletion.
// Python is gone, so URI parity is now enforced structurally (single Rust implementation)
// rather than via cross-language testing. The special_characters_encoding test below
// verifies the Rust implementation is correct.

use pg_collect::uris::*;

#[test]
fn test_special_characters_encoding() {
    // Test that special characters in package names are properly encoded

    // Package with + (common in C++ libraries)
    let uri = package_uri("debian", "trixie", "amd64", "libstdc++-dev", "12.2.0-14");
    assert!(
        uri.contains("libstdc%2B%2B-dev"),
        "'+' must be encoded as %2B"
    );

    // Package with : (multi-arch notation)
    let uri = package_uri("debian", "trixie", "amd64", "python3:amd64", "3.11.2-1");
    assert!(
        uri.contains("python3%3Aamd64"),
        "':' must be encoded as %3A"
    );

    // Version with epoch (2:) - colon is encoded, tilde is NOT (unreserved char)
    let uri = version_uri("debian", "trixie", "package", "2:1.2.3~rc1-4");
    eprintln!("DEBUG: version_uri output = {}", uri);
    assert!(
        uri.contains("2%3A1.2.3~rc1-4"),
        "':' must be encoded, '~' must NOT be encoded"
    );

    // Maintainer email with @ - must NOT be encoded
    let uri = maintainer_uri("maintainer@debian.org");
    assert_eq!(
        uri,
        "https://packagegraph.github.io/d/maintainer/maintainer@debian.org"
    );
    assert!(
        !uri.contains("%40"),
        "@ in maintainer_uri must NOT be encoded"
    );
    assert!(
        !uri.contains("%2E"),
        ". in maintainer_uri must NOT be encoded"
    );
}
