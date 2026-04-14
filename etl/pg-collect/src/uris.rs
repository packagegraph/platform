use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

// Namespace constants (must match Python exactly)
pub const PKG: &str = "https://purl.org/packagegraph/ontology/core#";
pub const SEC: &str = "https://purl.org/packagegraph/ontology/security#";
pub const VCS: &str = "https://purl.org/packagegraph/ontology/vcs#";
pub const DEB: &str = "https://purl.org/packagegraph/ontology/debian#";
pub const RPM: &str = "https://purl.org/packagegraph/ontology/rpm#";
pub const FOAF: &str = "http://xmlns.com/foaf/0.1/";
pub const PROV: &str = "http://www.w3.org/ns/prov#";
pub const SLSA: &str = "https://purl.org/packagegraph/ontology/slsa#";
pub const APK: &str = "https://purl.org/packagegraph/ontology/alpine#";
pub const BREW: &str = "https://purl.org/packagegraph/ontology/homebrew#";
pub const ARCH: &str = "https://purl.org/packagegraph/ontology/arch#";
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
        .trim_end_matches(".git")
        .to_string();
    format!("{DATA}repo/{}", encode(&cleaned))
}

/// Try to normalize a URL into a canonical forge repository URI.
/// Returns Some(repo_uri) if the URL matches a known forge pattern, None otherwise.
///
/// Recognized forges:
///   github.com, gitlab.com, codeberg.org, pagure.io,
///   src.fedoraproject.org, salsa.debian.org,
///   git.savannah.gnu.org, savannah.gnu.org, savannah.nongnu.org,
///   sourceware.org, git.kernel.org
pub fn normalize_forge_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    // Strip protocol
    let path = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    // Strip trailing slashes, .git suffix, and common subpaths
    let path = path
        .trim_end_matches('/')
        .trim_end_matches(".git");

    // GitHub: github.com/{owner}/{repo}[/tree/...][/wiki][/issues]
    if path.starts_with("github.com/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 && !parts[1].is_empty() && !parts[2].is_empty() {
            return Some(repo_uri(&format!("https://github.com/{}/{}", parts[1], parts[2])));
        }
    }

    // GitLab (any instance): gitlab.com, gitlab.freedesktop.org, etc.
    if path.contains("gitlab.") || path.starts_with("gitlab/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 {
            let host = parts[0];
            return Some(repo_uri(&format!("https://{}/{}/{}", host, parts[1], parts[2])));
        }
    }

    // Codeberg: codeberg.org/{owner}/{repo}
    if path.starts_with("codeberg.org/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 {
            return Some(repo_uri(&format!("https://codeberg.org/{}/{}", parts[1], parts[2])));
        }
    }

    // Salsa (Debian): salsa.debian.org/{team}/{repo}
    if path.starts_with("salsa.debian.org/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 {
            return Some(repo_uri(&format!("https://salsa.debian.org/{}/{}", parts[1], parts[2])));
        }
    }

    // Pagure (Fedora): pagure.io/{repo}
    if path.starts_with("pagure.io/") {
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 {
            return Some(repo_uri(&format!("https://pagure.io/{}", parts[1])));
        }
    }

    // Fedora dist-git: src.fedoraproject.org/rpms/{name}
    if path.starts_with("src.fedoraproject.org/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 {
            return Some(repo_uri(&format!("https://src.fedoraproject.org/{}/{}", parts[1], parts[2])));
        }
    }

    // Savannah (GNU): git.savannah.gnu.org/git/{project} or savannah.gnu.org/projects/{project}
    if path.starts_with("git.savannah.gnu.org/") || path.starts_with("git.savannah.nongnu.org/") {
        // git.savannah.gnu.org/git/bash.git → savannah.gnu.org/git/bash
        let host = if path.contains("nongnu") { "savannah.nongnu.org" } else { "savannah.gnu.org" };
        if let Some(rest) = path.split_once('/').map(|(_, r)| r) {
            let rest = rest.trim_start_matches("git/").trim_start_matches("cgit/");
            return Some(repo_uri(&format!("https://{}/git/{}", host, rest)));
        }
    }
    if path.starts_with("savannah.gnu.org/") || path.starts_with("savannah.nongnu.org/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 {
            let project = parts[2];
            return Some(repo_uri(&format!("https://{}/git/{}", parts[0], project)));
        }
    }

    // Sourceware: sourceware.org/git/{project}
    if path.starts_with("sourceware.org/") {
        if let Some(project) = path.strip_prefix("sourceware.org/git/") {
            return Some(repo_uri(&format!("https://sourceware.org/git/{}", project)));
        }
        // sourceware.org/{project} (e.g., sourceware.org/glibc)
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[1].contains('.') {
            return Some(repo_uri(&format!("https://sourceware.org/git/{}", parts[1])));
        }
    }

    // kernel.org: git.kernel.org/pub/scm/{path}/{repo}
    if path.starts_with("git.kernel.org/") {
        // Normalize to: git.kernel.org/{everything after pub/scm/}
        let cleaned = path.replace("/pub/scm/", "/");
        return Some(repo_uri(&format!("https://{}", cleaned)));
    }

    None
}

/// Derive a Fedora/CentOS dist-git packaging repository URI from package name.
pub fn fedora_distgit_uri(distro: &str, name: &str) -> String {
    match distro {
        "fedora" => repo_uri(&format!("https://src.fedoraproject.org/rpms/{}", name)),
        "centos-stream" => repo_uri(&format!("https://gitlab.com/redhat/centos-stream/rpms/{}", name)),
        _ => repo_uri(&format!("https://src.fedoraproject.org/rpms/{}", name)),
    }
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
