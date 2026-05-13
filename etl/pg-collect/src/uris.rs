use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

// Namespace constants (must match Python exactly)
pub const PKG: &str = "https://purl.org/packagegraph/ontology/core#";
pub const SEC: &str = "https://purl.org/packagegraph/ontology/security#";
pub const VCS: &str = "https://purl.org/packagegraph/ontology/vcs#";
pub const DEB: &str = "https://purl.org/packagegraph/ontology/deb#";
pub const RPM: &str = "https://purl.org/packagegraph/ontology/rpm#";
pub const FOAF: &str = "http://xmlns.com/foaf/0.1/";
pub const PROV: &str = "http://www.w3.org/ns/prov#";
pub const SLSA: &str = "https://purl.org/packagegraph/ontology/slsa#";
pub const APK: &str = "https://purl.org/packagegraph/ontology/apk#";
pub const BREW: &str = "https://purl.org/packagegraph/ontology/homebrew#";
pub const ARCH: &str = "https://purl.org/packagegraph/ontology/pacman#";
pub const NPM: &str = "https://purl.org/packagegraph/ontology/npm#";
pub const PYPI: &str = "https://purl.org/packagegraph/ontology/pypi#";
pub const CARGO: &str = "https://purl.org/packagegraph/ontology/cargo#";
pub const GOMOD: &str = "https://purl.org/packagegraph/ontology/gomod#";
pub const CONDA: &str = "https://purl.org/packagegraph/ontology/conda#";
pub const FLATPAK: &str = "https://purl.org/packagegraph/ontology/flatpak#";
pub const SNAP: &str = "https://purl.org/packagegraph/ontology/snap#";
pub const GENTOO: &str = "https://purl.org/packagegraph/ontology/portage#";
pub const VOID: &str = "https://purl.org/packagegraph/ontology/xbps#";
pub const GEMS: &str = "https://purl.org/packagegraph/ontology/rubygems#";
pub const MAVEN: &str = "https://purl.org/packagegraph/ontology/maven#";
pub const CPAN: &str = "https://purl.org/packagegraph/ontology/cpan#";
pub const CRAN: &str = "https://purl.org/packagegraph/ontology/cran#";
pub const HACKAGE: &str = "https://purl.org/packagegraph/ontology/hackage#";
pub const NUGET: &str = "https://purl.org/packagegraph/ontology/nuget#";
pub const HEX: &str = "https://purl.org/packagegraph/ontology/hex#";
pub const FREEBSD: &str = "https://purl.org/packagegraph/ontology/bsdpkg#";
pub const NIX: &str = "https://purl.org/packagegraph/ontology/nix#";
pub const CHOCO: &str = "https://purl.org/packagegraph/ontology/chocolatey#";
pub const BUILDROOT: &str = "https://purl.org/packagegraph/ontology/buildroot#";
pub const OPENWRT: &str = "https://purl.org/packagegraph/ontology/opkg#";
pub const YOCTO: &str = "https://purl.org/packagegraph/ontology/bitbake#";
pub const MET: &str = "https://purl.org/packagegraph/ontology/metrics#";
pub const DQ: &str = "https://purl.org/packagegraph/ontology/dq#";
pub const ATT: &str = "https://purl.org/packagegraph/ontology/attestation#";
pub const DATA: &str = "https://packagegraph.github.io/d/";
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
pub const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
pub const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
pub const SPDX: &str = "https://spdx.org/licenses/";
pub const CWE_BASE: &str = "https://cwe.mitre.org/data/definitions/";

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
    .add(b'<')
    .add(b'=')
    .add(b'>')
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
pub fn encode(component: &str) -> String {
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

/// Normalize obfuscated email addresses.
///
/// Package maintainers frequently obfuscate emails in changelogs and metadata
/// to avoid spam scraping:
///   - `"pnemade AT redhat DOT com"` → `"pnemade@redhat.com"`
///   - `"bradbell at seanet dot com"` → `"bradbell@seanet.com"`
///   - `"kkeithle at redhat-com"` → `""` (can't reliably reconstruct)
///
/// Returns empty string if the input can't be normalized to a valid `user@domain.tld` address.
pub fn normalize_email(email: &str) -> String {
    let email = email.trim();

    // Already a normal email
    if email.contains('@') && !email.contains(' ') {
        return email.to_string();
    }

    // Try common obfuscation patterns (case-sensitive to avoid false matches)
    let normalized = email
        .replace(" AT ", "@")
        .replace(" at ", "@")
        .replace(" DOT ", ".")
        .replace(" dot ", ".");

    // Verify result looks like user@domain.tld
    if normalized.contains('@') && normalized.contains('.') && !normalized.contains(' ') {
        normalized
    } else {
        String::new()
    }
}

/// Build a Person URI from an email address, normalizing obfuscation first.
///
/// Returns None if the email can't be normalized.
pub fn person_uri_from_email(email: &str) -> Option<String> {
    let normalized = normalize_email(email);
    if normalized.is_empty() {
        return None;
    }
    Some(format!("{DATA}person/{}",
        normalized.replace('@', "-at-").replace('.', "-")))
}

/// Build a GitHub Person URI from GitHub login.
pub fn github_person_uri(login: &str) -> String {
    format!("{DATA}person/github/{login}")
}

/// Build a GitHub ContributorAccount URI from GitHub login.
pub fn github_account_uri(login: &str) -> String {
    format!("{DATA}account/github/{login}")
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

/// Build a Vulnerability URI from OSV ID (for non-CVE vulnerabilities).
/// Used when an OSV record has no CVE alias (e.g., GHSA-*, RUSTSEC-*, PYSEC-*).
pub fn vuln_uri(osv_id: &str) -> String {
    format!("{DATA}vuln/{}", encode(osv_id))
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

/// Build a vcs:Forge instance URI from the forge hostname.
///
/// Example: "github.com" → "https://packagegraph.github.io/d/forge/github.com"
pub fn forge_uri(host: &str) -> String {
    format!("{DATA}forge/{}", encode(host))
}

/// Build a vcs:ForgeSoftwareVersion URI.
///
/// Example: ("gitlab", "17.1.0") → "https://packagegraph.github.io/d/forge-version/gitlab/17.1.0"
pub fn forge_software_version_uri(software: &str, version: &str) -> String {
    format!("{DATA}forge-version/{}/{}", encode(software), encode(version))
}

/// Build a vcs:ForgeVersionObservation URI.
///
/// Example: ("gitlab.gnome.org", "2026-04-26") → "https://packagegraph.github.io/d/forge-obs/gitlab.gnome.org/2026-04-26"
pub fn forge_version_observation_uri(host: &str, date: &str) -> String {
    format!("{DATA}forge-obs/{}/{}", encode(host), encode(date))
}

/// Build an att:DigitalSignature URI.
///
/// Example: ("npm", "sigstore", "2.0.0") → ".../d/signature/npm/sigstore/2.0.0"
pub fn signature_uri(ecosystem: &str, name: &str, version: &str) -> String {
    format!(
        "{DATA}signature/{}/{}/{}",
        encode(ecosystem),
        encode(name),
        encode(version)
    )
}

/// Build an att:TransparencyLogEntry URI from the log index.
///
/// Example: ("rekor", 12345) → ".../d/tlog/rekor/12345"
pub fn tlog_entry_uri(log_name: &str, log_index: i64) -> String {
    format!("{DATA}tlog/{}/{}", encode(log_name), log_index)
}

/// Build an SPDX license URI from a license identifier.
/// Example: "MIT" -> "https://spdx.org/licenses/MIT"
/// Compound expressions like "BSD-3-Clause OR GPL-2.0-or-later" are percent-encoded.
pub fn spdx_license_uri(license_id: &str) -> String {
    format!("{SPDX}{}", encode(license_id))
}

/// Build an Ecosystem entity URI.
/// Example: "cargo" -> "https://packagegraph.github.io/d/ecosystem/cargo"
pub fn ecosystem_uri(name: &str) -> String {
    format!("{DATA}ecosystem/{}", encode(name))
}

/// Build a DataQualityIssue URI from a deterministic key.
///
/// The key should uniquely identify the issue (e.g., hash of detector + field + raw value).
/// Example: "enrich-github/homepage/abc123" -> "https://packagegraph.github.io/d/dq/enrich-github/homepage/abc123"
pub fn dq_issue_uri(detector: &str, field: &str, value_hash: &str) -> String {
    format!("{DATA}dq/{}/{}/{}", encode(detector), encode(field), value_hash)
}

/// Check if a release name is numeric (version number) vs a codename.
///
/// Numeric: "43", "44", "9", "10", "3.20", "14", "44-beta"
/// Codename: "trixie", "rawhide", "tumbleweed", "arch", "noble"
pub fn is_numeric_release(name: &str) -> bool {
    // Strip common prefixes and suffixes
    let base = name.strip_prefix('v').unwrap_or(name);
    let base = base.split('-').next().unwrap_or(base);
    // Check if base starts with a digit (covers "43", "3.20", "10", "v3.20")
    base.chars().next().map_or(false, |c| c.is_ascii_digit())
}

/// Map a collector's dependency-type string to the canonical property URI.
///
/// The ontology v0.7.0 properties-as-taxonomy pattern requires `dependencyType`
/// to be an IRI (ObjectProperty) pointing to a core dependency property, not a
/// string literal. The SHACL constraint is:
///   sh:in (pkg:dependsOn pkg:buildDependsOn pkg:recommends pkg:suggests
///          pkg:enhances pkg:supplements pkg:checkRequires pkg:preDepends pkg:conflicts)
pub fn dep_type_uri(dep_type: &str) -> String {
    let prop = match dep_type {
        // Runtime / general
        "depends" | "runtime" | "run" | "compile" => "dependsOn",
        // Build-time
        "build" | "build_depends" | "makedepends" | "host" | "dev_depends" => "buildDependsOn",
        // Recommendations
        "recommends" | "recommended" => "recommends",
        // Suggestions / optional
        "suggests" | "optdepends" | "optional_depends" | "optional" => "suggests",
        // Enhancements
        "enhances" => "enhances",
        // Supplements
        "supplements" => "supplements",
        // Check / test
        "checkdepends" | "check_requires" | "test" => "checkRequires",
        // Pre-dependencies
        "pre_depends" | "pre-depends" => "preDepends",
        // Conflicts
        "conflicts" => "conflicts",
        // Peer dependencies (NPM) — closest semantic match is runtime
        "peer_depends" => "dependsOn",
        // Maven scopes
        "provided" => "buildDependsOn",
        "system" => "dependsOn",
        "import" => "dependsOn",
        // Fallback: treat unknown as runtime dependency
        _ => "dependsOn",
    };
    format!("{PKG}{prop}")
}

/// Build a CVSSScore entity URI.
/// Example: "CVE-2024-1234", "v3.1" -> "https://packagegraph.github.io/d/cvss/CVE-2024-1234/v3.1"
pub fn cvss_score_uri(vuln_id: &str, version: &str) -> String {
    format!("{DATA}cvss/{}/{}", encode(vuln_id), encode(version))
}

/// Build a CWE URI from a CWE identifier.
/// Handles both "CWE-79" and "79" formats.
/// Example: "CWE-79" -> "https://cwe.mitre.org/data/definitions/79"
pub fn cwe_uri(cwe_id: &str) -> String {
    let num = cwe_id.strip_prefix("CWE-").unwrap_or(cwe_id);
    format!("{CWE_BASE}{num}")
}

/// Build a CVE entity URI for shared CVE identification across graphs.
/// Example: "CVE-2024-1234" -> "https://packagegraph.github.io/d/cve/CVE-2024-1234"
pub fn cve_entity_uri(cve_id: &str) -> String {
    format!("{DATA}cve/{cve_id}")
}

/// Map a severity string to a SKOS concept URI from sec:SeverityScheme.
///
/// Handles multiple naming conventions (RHSA, Debian, CVSS-derived, Bodhi):
/// - "critical" / "CRITICAL" / "urgent" / "URGENT" → sec:sev-critical
/// - "important" / "high" / "HIGH" → sec:sev-important
/// - "moderate" / "medium" / "MEDIUM" → sec:sev-moderate
/// - "low" / "LOW" → sec:sev-low
/// - "none" / "NONE" → sec:sev-none
///
/// Returns None for unrecognized values (e.g., Debian's "unimportant", Bodhi's "unspecified").
pub fn severity_concept_uri(severity: &str) -> Option<String> {
    let concept = match severity.to_ascii_lowercase().as_str() {
        "critical" | "urgent" => "sev-critical",
        "important" | "high" => "sev-important",
        "moderate" | "medium" => "sev-moderate",
        "low" => "sev-low",
        "none" => "sev-none",
        _ => return None,
    };
    Some(format!("{SEC}{concept}"))
}

/// Map an advisory category string to a SKOS concept URI from sec:AdvisoryCategoryScheme.
///
/// - "security" → sec:cat-security
/// - "bugfix" → sec:cat-bugfix
/// - "enhancement" → sec:cat-enhancement
pub fn advisory_category_uri(category: &str) -> String {
    let concept = match category.to_ascii_lowercase().as_str() {
        "security" => "cat-security",
        "bugfix" => "cat-bugfix",
        "enhancement" => "cat-enhancement",
        _ => "cat-security", // default: security advisories
    };
    format!("{SEC}{concept}")
}

/// Map an event type string to a SKOS concept URI from sec:EventTypeScheme.
///
/// - "introduced" → sec:event-introduced
/// - "fixed" → sec:event-fixed
/// - "last_affected" → sec:event-last-affected
pub fn event_type_uri(event_type: &str) -> String {
    let concept = match event_type {
        "introduced" => "event-introduced",
        "fixed" => "event-fixed",
        "last_affected" => "event-last-affected",
        _ => "event-introduced", // fallback
    };
    format!("{SEC}{concept}")
}

/// Map a range type string to a SKOS concept URI from sec:RangeTypeScheme.
///
/// - "SEMVER" → sec:range-semver
/// - "ECOSYSTEM" → sec:range-ecosystem
/// - "GIT" → sec:range-git
pub fn range_type_uri(range_type: &str) -> String {
    let concept = match range_type {
        "SEMVER" => "range-semver",
        "ECOSYSTEM" => "range-ecosystem",
        "GIT" => "range-git",
        _ => "range-ecosystem", // fallback
    };
    format!("{SEC}{concept}")
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

    #[test]
    fn test_vuln_uri() {
        let uri = vuln_uri("GHSA-2g4f-4pwh-qvx6");
        assert_eq!(
            uri,
            "https://packagegraph.github.io/d/vuln/GHSA-2g4f-4pwh-qvx6"
        );

        // Test with special characters
        let uri2 = vuln_uri("RUSTSEC-2024-001");
        assert_eq!(
            uri2,
            "https://packagegraph.github.io/d/vuln/RUSTSEC-2024-001"
        );
    }

    #[test]
    fn test_spdx_license_uri() {
        assert_eq!(spdx_license_uri("MIT"), "https://spdx.org/licenses/MIT");
        assert_eq!(spdx_license_uri("GPL-2.0-only"), "https://spdx.org/licenses/GPL-2.0-only");
        assert_eq!(spdx_license_uri("Apache-2.0"), "https://spdx.org/licenses/Apache-2.0");
    }

    #[test]
    fn test_spdx_license_uri_compound_expression() {
        // Alpine packages use compound SPDX expressions with spaces
        let uri = spdx_license_uri("BSD-3-Clause OR GPL-2.0-or-later");
        assert_eq!(uri, "https://spdx.org/licenses/BSD-3-Clause%20OR%20GPL-2.0-or-later");
        assert!(!uri.contains(' '), "URI must not contain raw spaces");
    }

    #[test]
    fn test_cwe_uri() {
        assert_eq!(cwe_uri("CWE-79"), "https://cwe.mitre.org/data/definitions/79");
        assert_eq!(cwe_uri("79"), "https://cwe.mitre.org/data/definitions/79");
        assert_eq!(cwe_uri("CWE-200"), "https://cwe.mitre.org/data/definitions/200");
    }

    #[test]
    fn test_cve_entity_uri() {
        assert_eq!(cve_entity_uri("CVE-2024-1234"), "https://packagegraph.github.io/d/cve/CVE-2024-1234");
    }

    #[test]
    fn test_ecosystem_uri() {
        assert_eq!(ecosystem_uri("cargo"), "https://packagegraph.github.io/d/ecosystem/cargo");
        assert_eq!(ecosystem_uri("pypi"), "https://packagegraph.github.io/d/ecosystem/pypi");
    }

    #[test]
    fn test_dep_type_uri_runtime() {
        let pkg = "https://purl.org/packagegraph/ontology/core#";
        assert_eq!(dep_type_uri("depends"), format!("{pkg}dependsOn"));
        assert_eq!(dep_type_uri("runtime"), format!("{pkg}dependsOn"));
        assert_eq!(dep_type_uri("run"), format!("{pkg}dependsOn"));
        assert_eq!(dep_type_uri("compile"), format!("{pkg}dependsOn"));
    }

    #[test]
    fn test_dep_type_uri_build() {
        let pkg = "https://purl.org/packagegraph/ontology/core#";
        assert_eq!(dep_type_uri("build"), format!("{pkg}buildDependsOn"));
        assert_eq!(dep_type_uri("makedepends"), format!("{pkg}buildDependsOn"));
        assert_eq!(dep_type_uri("build_depends"), format!("{pkg}buildDependsOn"));
        assert_eq!(dep_type_uri("host"), format!("{pkg}buildDependsOn"));
    }

    #[test]
    fn test_dep_type_uri_optional() {
        let pkg = "https://purl.org/packagegraph/ontology/core#";
        assert_eq!(dep_type_uri("suggests"), format!("{pkg}suggests"));
        assert_eq!(dep_type_uri("optdepends"), format!("{pkg}suggests"));
        assert_eq!(dep_type_uri("optional_depends"), format!("{pkg}suggests"));
        assert_eq!(dep_type_uri("recommends"), format!("{pkg}recommends"));
    }

    #[test]
    fn test_dep_type_uri_check() {
        let pkg = "https://purl.org/packagegraph/ontology/core#";
        assert_eq!(dep_type_uri("checkdepends"), format!("{pkg}checkRequires"));
        assert_eq!(dep_type_uri("test"), format!("{pkg}checkRequires"));
    }

    #[test]
    fn test_dep_type_uri_fallback() {
        let pkg = "https://purl.org/packagegraph/ontology/core#";
        assert_eq!(dep_type_uri("unknown_type"), format!("{pkg}dependsOn"));
    }

    #[test]
    fn test_cvss_score_uri() {
        assert_eq!(
            cvss_score_uri("CVE-2024-1234", "v3.1"),
            "https://packagegraph.github.io/d/cvss/CVE-2024-1234/v3.1"
        );
    }

    #[test]
    fn test_severity_concept_uri() {
        let sec = "https://purl.org/packagegraph/ontology/security#";
        assert_eq!(severity_concept_uri("critical"), Some(format!("{sec}sev-critical")));
        assert_eq!(severity_concept_uri("CRITICAL"), Some(format!("{sec}sev-critical")));
        assert_eq!(severity_concept_uri("important"), Some(format!("{sec}sev-important")));
        assert_eq!(severity_concept_uri("high"), Some(format!("{sec}sev-important")));
        assert_eq!(severity_concept_uri("HIGH"), Some(format!("{sec}sev-important")));
        assert_eq!(severity_concept_uri("moderate"), Some(format!("{sec}sev-moderate")));
        assert_eq!(severity_concept_uri("medium"), Some(format!("{sec}sev-moderate")));
        assert_eq!(severity_concept_uri("MEDIUM"), Some(format!("{sec}sev-moderate")));
        assert_eq!(severity_concept_uri("low"), Some(format!("{sec}sev-low")));
        assert_eq!(severity_concept_uri("LOW"), Some(format!("{sec}sev-low")));
        assert_eq!(severity_concept_uri("none"), Some(format!("{sec}sev-none")));
        assert_eq!(severity_concept_uri("urgent"), Some(format!("{sec}sev-critical")));
        assert_eq!(severity_concept_uri("URGENT"), Some(format!("{sec}sev-critical")));
        assert_eq!(severity_concept_uri("unimportant"), None);
        assert_eq!(severity_concept_uri("unknown"), None);
    }

    #[test]
    fn test_advisory_category_uri() {
        let sec = "https://purl.org/packagegraph/ontology/security#";
        assert_eq!(advisory_category_uri("security"), format!("{sec}cat-security"));
        assert_eq!(advisory_category_uri("bugfix"), format!("{sec}cat-bugfix"));
        assert_eq!(advisory_category_uri("enhancement"), format!("{sec}cat-enhancement"));
    }

    #[test]
    fn test_event_type_uri() {
        let sec = "https://purl.org/packagegraph/ontology/security#";
        assert_eq!(event_type_uri("introduced"), format!("{sec}event-introduced"));
        assert_eq!(event_type_uri("fixed"), format!("{sec}event-fixed"));
        assert_eq!(event_type_uri("last_affected"), format!("{sec}event-last-affected"));
    }

    #[test]
    fn test_range_type_uri() {
        let sec = "https://purl.org/packagegraph/ontology/security#";
        assert_eq!(range_type_uri("SEMVER"), format!("{sec}range-semver"));
        assert_eq!(range_type_uri("ECOSYSTEM"), format!("{sec}range-ecosystem"));
        assert_eq!(range_type_uri("GIT"), format!("{sec}range-git"));
    }

    #[test]
    fn test_forge_software_version_uri() {
        assert_eq!(
            forge_software_version_uri("gitlab", "17.1.0"),
            "https://packagegraph.github.io/d/forge-version/gitlab/17.1.0"
        );
        assert_eq!(
            forge_software_version_uri("forgejo", "9.0.0"),
            "https://packagegraph.github.io/d/forge-version/forgejo/9.0.0"
        );
    }

    #[test]
    fn test_forge_version_observation_uri() {
        assert_eq!(
            forge_version_observation_uri("gitlab.gnome.org", "2026-04-26"),
            "https://packagegraph.github.io/d/forge-obs/gitlab.gnome.org/2026-04-26"
        );
        assert_eq!(
            forge_version_observation_uri("codeberg.org", "2026-04-26"),
            "https://packagegraph.github.io/d/forge-obs/codeberg.org/2026-04-26"
        );
    }

    #[test]
    fn test_normalize_email_normal() {
        assert_eq!(normalize_email("user@example.com"), "user@example.com");
        assert_eq!(normalize_email("  user@example.com  "), "user@example.com");
    }

    #[test]
    fn test_normalize_email_obfuscated_caps() {
        assert_eq!(normalize_email("pnemade AT redhat DOT com"), "pnemade@redhat.com");
    }

    #[test]
    fn test_normalize_email_obfuscated_lower() {
        assert_eq!(normalize_email("bradbell at seanet dot com"), "bradbell@seanet.com");
    }

    #[test]
    fn test_normalize_email_unrecoverable() {
        // Can't reconstruct — missing domain separator
        assert_eq!(normalize_email("kkeithle at redhat-com"), "");
        // No email-like structure at all
        assert_eq!(normalize_email("someperson"), "");
        assert_eq!(normalize_email(""), "");
    }

    #[test]
    fn test_person_uri_from_email_normal() {
        let uri = person_uri_from_email("user@example.com").unwrap();
        assert_eq!(uri, "https://packagegraph.github.io/d/person/user-at-example-com");
    }

    #[test]
    fn test_person_uri_from_email_obfuscated() {
        let uri = person_uri_from_email("pnemade AT redhat DOT com").unwrap();
        assert_eq!(uri, "https://packagegraph.github.io/d/person/pnemade-at-redhat-com");
    }

    #[test]
    fn test_person_uri_from_email_invalid() {
        assert!(person_uri_from_email("kkeithle at redhat-com").is_none());
        assert!(person_uri_from_email("not-an-email").is_none());
    }
}
