use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

// Namespace constants (must match Python exactly)
pub const PKG: &str = "https://packagegraph.github.io/ontology/core#";
pub const SEC: &str = "https://packagegraph.github.io/ontology/security#";
pub const VCS: &str = "https://packagegraph.github.io/ontology/vcs#";
pub const DEB: &str = "https://packagegraph.github.io/ontology/debian#";
pub const RPM: &str = "https://packagegraph.github.io/ontology/rpm#";
pub const FOAF: &str = "http://xmlns.com/foaf/0.1/";
pub const PROV: &str = "http://www.w3.org/ns/prov#";
pub const SLSA: &str = "https://packagegraph.github.io/ontology/slsa#";
pub const DATA: &str = "https://packagegraph.github.io/d/";
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
pub const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

// Define encoding set: encode everything except unreserved characters
// Unreserved: A-Z a-z 0-9 - _ . ~
// This matches Python's quote(s, safe="") which encodes ALL special chars
const ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'=')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// URL-encode a URI path component, encoding all special characters.
/// Matches Python's urllib.parse.quote(component, safe="").
fn encode(component: &str) -> String {
    utf8_percent_encode(component, ENCODE_SET).to_string()
}

/// Build a version-agnostic PackageIdentity URI.
/// Used as the target of dependency links (instead of versioned URIs).
pub fn package_identity_uri(distro: &str, release: &str, arch: &str, name: &str) -> String {
    format!(
        "{DATA}pkg/{}/{}/{}/{}",
        encode(distro),
        encode(release),
        encode(arch),
        encode(name)
    )
}

/// Build a versioned BinaryPackage URI. Architecture is required.
pub fn package_uri(distro: &str, release: &str, arch: &str, name: &str, version: &str) -> String {
    format!(
        "{DATA}pkg/{}/{}/{}/{}/{}",
        encode(distro),
        encode(release),
        encode(arch),
        encode(name),
        encode(version)
    )
}

/// Build a SourcePackage URI.
pub fn source_uri(distro: &str, release: &str, name: &str, version: &str) -> String {
    format!(
        "{DATA}src/{}/{}/{}/{}",
        encode(distro),
        encode(release),
        encode(name),
        encode(version)
    )
}

/// Build a Version URI.
pub fn version_uri(distro: &str, release: &str, name: &str, version: &str) -> String {
    format!(
        "{DATA}ver/{}/{}/{}/{}",
        encode(distro),
        encode(release),
        encode(name),
        encode(version)
    )
}

/// Build a Maintainer URI from email address.
///
/// Email addresses are used as-is since @ and . are valid in URIs.
/// EXCEPTION: Does NOT percent-encode — matches Python namespaces.py:44-49.
pub fn maintainer_uri(email: &str) -> String {
    format!("{DATA}maintainer/{email}")
}

/// Build an Architecture URI.
pub fn arch_uri(name: &str) -> String {
    format!("{DATA}arch/{}", encode(name))
}

/// Build a Distribution URI.
pub fn distro_uri(name: &str) -> String {
    format!("{DATA}distro/{}", encode(name))
}

/// Build a DistributionRelease URI.
pub fn release_uri(distro: &str, codename: &str) -> String {
    format!("{DATA}release/{}/{}", encode(distro), encode(codename))
}

/// Build an UpstreamProject URI.
pub fn upstream_uri(name: &str) -> String {
    format!("{DATA}upstream/{}", encode(name))
}

/// Build a Vulnerability URI from CVE ID.
pub fn cve_uri(cve_id: &str) -> String {
    format!("{DATA}cve/{}", encode(cve_id))
}

/// Build a VCS Repository URI from repository URL.
/// Strips protocol and trailing slashes.
pub fn repo_uri(url: &str) -> String {
    let cleaned = url
        .replace("https://", "")
        .replace("http://", "")
        .trim_end_matches('/')
        .to_string();
    format!("{DATA}repo/{}", encode(&cleaned))
}

/// Build a SecurityAdvisory URI.
pub fn advisory_uri(advisory_id: &str) -> String {
    format!("{DATA}advisory/{}", encode(advisory_id))
}

/// Build a BuildActivity URI.
pub fn build_uri(distro: &str, release: &str, name: &str, version: &str) -> String {
    format!(
        "{DATA}build/{}/{}/{}/{}",
        encode(distro),
        encode(release),
        encode(name),
        encode(version)
    )
}

/// Build a SLSA ProvenanceAttestation URI.
pub fn attestation_uri(distro: &str, release: &str, name: &str, version: &str) -> String {
    format!(
        "{DATA}attestation/{}/{}/{}/{}",
        encode(distro),
        encode(release),
        encode(name),
        encode(version)
    )
}

/// Build a SLSA Builder URI from builder ID.
///
/// builder_id is typically a URL like https://koji.fedoraproject.org.
/// Strips protocol and trailing slashes.
pub fn builder_uri(builder_id: &str) -> String {
    let cleaned = builder_id
        .replace("https://", "")
        .replace("http://", "")
        .trim_end_matches('/')
        .to_string();
    format!("{DATA}builder/{}", encode(&cleaned))
}

/// Build a SLSA BuildEnvironment URI.
pub fn build_env_uri(distro: &str, release: &str, name: &str, version: &str) -> String {
    format!(
        "{DATA}buildenv/{}/{}/{}/{}",
        encode(distro),
        encode(release),
        encode(name),
        encode(version)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_basic() {
        assert_eq!(encode("hello"), "hello");
        assert_eq!(encode("hello-world"), "hello-world");
    }

    #[test]
    fn test_encode_special_chars() {
        // Test that special characters are encoded
        assert_eq!(encode("hello:world"), "hello%3Aworld");
        assert_eq!(encode("hello+world"), "hello%2Bworld");
        assert_eq!(encode("hello@world"), "hello%40world");
        assert_eq!(encode("hello world"), "hello%20world");
    }

    #[test]
    fn test_package_identity_uri() {
        let uri = package_identity_uri("debian", "trixie", "amd64", "libc6");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/pkg/debian/trixie/amd64/libc6"
        );
    }

    #[test]
    fn test_package_uri() {
        let uri = package_uri("debian", "trixie", "amd64", "libc6", "2.36-1");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/pkg/debian/trixie/amd64/libc6/2.36-1"
        );
    }

    #[test]
    fn test_package_uri_with_special_chars() {
        let uri = package_uri("debian", "trixie", "amd64", "libstdc++-dev", "12.2.0-14");
        assert!(uri.contains("libstdc%2B%2B-dev"));
    }

    #[test]
    fn test_maintainer_uri_no_encoding() {
        // CRITICAL: maintainer_uri must NOT encode @ and .
        let uri = maintainer_uri("john@example.com");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/maintainer/john@example.com"
        );
        assert!(!uri.contains("%40")); // @ must NOT be encoded
    }

    #[test]
    fn test_arch_uri() {
        assert_eq!(arch_uri("amd64"), "https://packagegraph.github.io/d/arch/amd64");
    }

    #[test]
    fn test_distro_uri() {
        assert_eq!(
            distro_uri("debian"),
            "https://packagegraph.github.io/d/distro/debian"
        );
    }

    #[test]
    fn test_release_uri() {
        assert_eq!(
            release_uri("debian", "trixie"),
            "https://packagegraph.github.io/d/release/debian/trixie"
        );
    }

    #[test]
    fn test_source_uri() {
        let uri = source_uri("debian", "trixie", "glibc", "2.36-1");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/src/debian/trixie/glibc/2.36-1"
        );
    }

    #[test]
    fn test_version_uri() {
        let uri = version_uri("debian", "trixie", "libc6", "2.36-1");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/ver/debian/trixie/libc6/2.36-1"
        );
    }

    #[test]
    fn test_repo_uri() {
        let uri = repo_uri("https://github.com/packagegraph/ontology/");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/repo/github.com%2Fpackagegraph%2Fontology"
        );
    }

    #[test]
    fn test_attestation_uri() {
        let uri = attestation_uri("fedora", "41", "gcc", "14.0.1-1.fc41");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/attestation/fedora/41/gcc/14.0.1-1.fc41"
        );
    }

    #[test]
    fn test_builder_uri() {
        let uri = builder_uri("https://koji.fedoraproject.org");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/builder/koji.fedoraproject.org"
        );
    }

    #[test]
    fn test_build_env_uri() {
        let uri = build_env_uri("fedora", "41", "gcc", "14.0.1-1.fc41");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/buildenv/fedora/41/gcc/14.0.1-1.fc41"
        );
    }
}
