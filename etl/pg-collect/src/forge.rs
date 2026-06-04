//! Shared forge library — repository URL extraction, normalization, and validation.
//!
//! Consumed by all collectors and enrichers for consistent repository URL handling.
//! Every component that touches a URL with potential repository semantics routes
//! through this module.

use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use once_cell::sync::Lazy;
use regex::Regex;
use std::io::Result;

/// Confidence level for an extracted repository URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// Ambiguous homepage, non-forge download site, or generic git host
    Low,
    /// Homepage or URL field matching a known forge repository pattern
    Medium,
    /// Explicit forge macro, structured API field, or normalized forge archive URL
    High,
}

/// Result of extracting a forge repository URL from a raw string.
#[derive(Debug, Clone)]
pub struct ForgeExtraction {
    /// Normalized canonical repository URL (https, no .git, no fragment)
    pub repo_url: String,
    /// Confidence level of the extraction
    pub confidence: Confidence,
    /// Which input field the URL came from
    pub source_field: String,
    /// Which extractor rule matched
    pub extractor: &'static str,
}

// ─── Known forge hosts ──────────────────────────────────────────────────

/// Hosts that are known forges (owner/repo pattern).
const FORGE_HOSTS: &[&str] = &[
    "github.com",
    "codeberg.org",
    "sr.ht",
];

/// Hosts that are known GitLab instances.
const GITLAB_HOSTS: &[&str] = &[
    "gitlab.com",
    "gitlab.freedesktop.org",
    "gitlab.gnome.org",
    "gitlab.xfce.org",
    "gitlab.archlinux.org",
    "invent.kde.org",
    "salsa.debian.org", // Debian's GitLab instance
];

/// Hosts that are known Gitea/Forgejo instances.
const GITEA_HOSTS: &[&str] = &[
    "gitea.com",
    "forgejo.org",
    "notabug.org",
];

/// Regex for git-describe output: name-VERSION-COUNT-gHASH
static GIT_DESCRIBE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"-g([0-9a-f]{7,40})$").unwrap()
});

/// Regex for extracting owner/repo from forge URLs (handles dots in repo names).
static FORGE_OWNER_REPO_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^([^/]+)/([^/]+?)(?:\.git)?(?:/.*)?$").unwrap()
});

// ─── Extraction (pure, no I/O) ──────────────────────────────────────────

/// Normalize a raw URL string before extraction.
///
/// - Strip fragments (#readme, etc.)
/// - Strip query parameters
/// - Normalize protocol to https
/// - Strip trailing slashes
/// - Strip .git suffix
fn pre_normalize(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    // Strip Yocto SRC_URI parameters (;branch=master, ;protocol=https)
    let url = url.split(';').next().unwrap_or(url);

    // Strip fragment
    let url = url.split('#').next().unwrap_or(url);

    // Strip query parameters
    let url = url.split('?').next().unwrap_or(url);

    // Normalize git:// and git+https:// to https://
    let url = url
        .strip_prefix("git+https://")
        .or_else(|| url.strip_prefix("git+http://"))
        .or_else(|| url.strip_prefix("git://"))
        .map(|rest| format!("https://{}", rest))
        .unwrap_or_else(|| {
            // Normalize http:// to https://
            url.strip_prefix("http://")
                .map(|rest| format!("https://{}", rest))
                .unwrap_or_else(|| url.to_string())
        });

    // Strip trailing slashes and .git suffix
    let url = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/');

    if url.is_empty() || !url.starts_with("https://") {
        return None;
    }

    Some(url.to_string())
}

/// Extract the path portion after stripping the protocol prefix.
fn strip_protocol(url: &str) -> &str {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
}

/// Extract a canonical forge repository URL from a raw URL string.
///
/// Tries extractors in priority order and returns the first match.
/// Returns None for URLs that don't match any known forge pattern.
pub fn extract_forge_url(url: &str) -> Option<ForgeExtraction> {
    extract_forge_url_with_field(url, "url")
}

/// Like `extract_forge_url` but records the source field name.
pub fn extract_forge_url_with_field(url: &str, field: &str) -> Option<ForgeExtraction> {
    let normalized = pre_normalize(url)?;

    // Try archive URL normalization first (highest specificity)
    if let Some(repo_url) = normalize_archive_url_inner(&normalized) {
        return Some(ForgeExtraction {
            repo_url,
            confidence: Confidence::High,
            source_field: field.to_string(),
            extractor: "archive-url",
        });
    }

    // Try direct forge URL matching
    if let Some(repo_url) = normalize_direct_forge(&normalized) {
        let confidence = if is_high_confidence_host(strip_protocol(&normalized)) {
            Confidence::High
        } else {
            Confidence::Medium
        };
        return Some(ForgeExtraction {
            repo_url,
            confidence,
            source_field: field.to_string(),
            extractor: "direct-forge",
        });
    }

    // Try FTP/mirror mappings
    if let Some(repo_url) = normalize_ftp_mirror(&normalized) {
        return Some(ForgeExtraction {
            repo_url,
            confidence: Confidence::Medium,
            source_field: field.to_string(),
            extractor: "ftp-mirror-mapping",
        });
    }

    None
}

/// Try to extract upstream repo from multiple candidate fields,
/// returning the highest-confidence match.
pub fn extract_best_repo(candidates: &[(&str, &str)]) -> Option<ForgeExtraction> {
    let mut best: Option<ForgeExtraction> = None;

    for (field, url) in candidates {
        if let Some(extraction) = extract_forge_url_with_field(url, field) {
            match &best {
                None => best = Some(extraction),
                Some(current) if extraction.confidence > current.confidence => {
                    best = Some(extraction);
                }
                _ => {}
            }
        }
    }

    best
}

/// Check if a host is a high-confidence forge.
fn is_high_confidence_host(path: &str) -> bool {
    for host in FORGE_HOSTS {
        if path.starts_with(host) {
            return true;
        }
    }
    for host in GITLAB_HOSTS {
        if path.starts_with(host) {
            return true;
        }
    }
    for host in GITEA_HOSTS {
        if path.starts_with(host) {
            return true;
        }
    }
    // Debian/Fedora packaging forges
    if path.starts_with("salsa.debian.org/")
        || path.starts_with("src.fedoraproject.org/")
        || path.starts_with("pagure.io/")
    {
        return true;
    }
    false
}

/// Normalize a direct forge URL to canonical form.
///
/// Handles all known forge patterns: GitHub, GitLab instances, Codeberg,
/// Pagure, Fedora dist-git, Savannah, Sourceware, kernel.org, Gitea/Forgejo.
fn normalize_direct_forge(url: &str) -> Option<String> {
    let path = strip_protocol(url);

    // GitHub: github.com/{owner}/{repo}[/tree/...][/wiki][/issues]
    if path.starts_with("github.com/") {
        let rest = path.strip_prefix("github.com/")?;
        let caps = FORGE_OWNER_REPO_RE.captures(rest)?;
        let owner = caps.get(1)?.as_str();
        let repo = caps.get(2)?.as_str();
        if !owner.is_empty() && !repo.is_empty() {
            return Some(format!("https://github.com/{}/{}", owner, repo));
        }
    }

    // GitLab instances: gitlab.com, gitlab.freedesktop.org, etc.
    for host in GITLAB_HOSTS {
        if path.starts_with(&format!("{}/", host)) {
            let rest = path.strip_prefix(&format!("{}/", host))?;
            // GitLab repos can have nested groups: gitlab.com/group/subgroup/repo
            // Strip known subpaths: /-/tree, /-/blob, /-/archive, /-/commits, etc.
            let rest = rest.split("/-/").next().unwrap_or(rest);
            let rest = rest.trim_end_matches('/');
            if !rest.is_empty() && rest.contains('/') {
                return Some(format!("https://{}/{}", host, rest));
            }
        }
    }

    // Codeberg: codeberg.org/{owner}/{repo}
    if path.starts_with("codeberg.org/") {
        let rest = path.strip_prefix("codeberg.org/")?;
        let caps = FORGE_OWNER_REPO_RE.captures(rest)?;
        let owner = caps.get(1)?.as_str();
        let repo = caps.get(2)?.as_str();
        return Some(format!("https://codeberg.org/{}/{}", owner, repo));
    }

    // Gitea/Forgejo instances
    for host in GITEA_HOSTS {
        if path.starts_with(&format!("{}/", host)) {
            let rest = path.strip_prefix(&format!("{}/", host))?;
            let caps = FORGE_OWNER_REPO_RE.captures(rest)?;
            let owner = caps.get(1)?.as_str();
            let repo = caps.get(2)?.as_str();
            return Some(format!("https://{}/{}/{}", host, owner, repo));
        }
    }

    // Sourcehut: sr.ht/~{user}/{repo}
    if path.starts_with("sr.ht/~") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 {
            return Some(format!("https://sr.ht/{}/{}", parts[1], parts[2]));
        }
    }

    // Salsa (Debian): salsa.debian.org/{team}/{repo}
    if path.starts_with("salsa.debian.org/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 {
            return Some(format!("https://salsa.debian.org/{}/{}", parts[1], parts[2]));
        }
    }

    // Pagure (Fedora): pagure.io/{repo}
    if path.starts_with("pagure.io/") {
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[1].is_empty() {
            return Some(format!("https://pagure.io/{}", parts[1]));
        }
    }

    // Fedora dist-git: src.fedoraproject.org/rpms/{name}
    if path.starts_with("src.fedoraproject.org/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 {
            return Some(format!("https://src.fedoraproject.org/{}/{}", parts[1], parts[2]));
        }
    }

    // Savannah (GNU/non-GNU): git.savannah.gnu.org, savannah.gnu.org
    if path.starts_with("git.savannah.gnu.org/") || path.starts_with("git.savannah.nongnu.org/") {
        let host = if path.contains("nongnu") { "savannah.nongnu.org" } else { "savannah.gnu.org" };
        if let Some(rest) = path.split_once('/').map(|(_, r)| r) {
            let rest = rest.trim_start_matches("git/").trim_start_matches("cgit/");
            if !rest.is_empty() {
                return Some(format!("https://{}/git/{}", host, rest));
            }
        }
    }
    if path.starts_with("savannah.gnu.org/") || path.starts_with("savannah.nongnu.org/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 {
            return Some(format!("https://{}/git/{}", parts[0], parts[2]));
        }
    }

    // Sourceware: sourceware.org/git/{project}
    if path.starts_with("sourceware.org/") {
        if let Some(project) = path.strip_prefix("sourceware.org/git/") {
            return Some(format!("https://sourceware.org/git/{}", project));
        }
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[1].contains('.') && !parts[1].is_empty() {
            return Some(format!("https://sourceware.org/git/{}", parts[1]));
        }
    }

    // kernel.org: git.kernel.org/pub/scm/{path}/{repo}
    if path.starts_with("git.kernel.org/") {
        let cleaned = path.replace("/pub/scm/", "/");
        return Some(format!("https://{}", cleaned));
    }

    // Generic git.* hosts (git.openssl.org, git.ffmpeg.org, etc.)
    if path.starts_with("git.") && path.contains('/') {
        // Only match if it looks like a repo path (host/path with at least one segment)
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[1].is_empty() {
            return Some(format!("https://{}", path));
        }
    }

    None
}

/// Normalize an archive/tarball URL back to the repository root.
///
/// Handles GitHub, GitLab, and generic forge archive patterns.
pub fn normalize_archive_url(url: &str) -> Option<String> {
    let normalized = pre_normalize(url)?;
    normalize_archive_url_inner(&normalized)
}

fn normalize_archive_url_inner(url: &str) -> Option<String> {
    let path = strip_protocol(url);

    // GitHub archive patterns:
    // github.com/{owner}/{repo}/archive/refs/tags/...
    // github.com/{owner}/{repo}/archive/v1.0.tar.gz
    // github.com/{owner}/{repo}/releases/download/...
    // github.com/{owner}/{repo}/tarball/...
    if path.starts_with("github.com/") {
        let parts: Vec<&str> = path.splitn(5, '/').collect();
        if parts.len() >= 4 {
            let subpath = parts[3];
            if subpath == "archive" || subpath == "releases" || subpath == "tarball" || subpath == "zipball" {
                let owner = parts[1];
                let repo = parts[2].trim_end_matches(".git");
                return Some(format!("https://github.com/{}/{}", owner, repo));
            }
        }
    }

    // GitLab archive patterns:
    // gitlab.*/.../-/archive/...
    for host in GITLAB_HOSTS {
        if path.starts_with(&format!("{}/", host)) {
            if let Some(idx) = path.find("/-/archive") {
                let repo_path = &path[..idx];
                return Some(format!("https://{}", repo_path));
            }
        }
    }

    // Generic: any forge URL with /archive/, /releases/, /downloads/ subpath
    // Only match if the host is a known forge
    if is_high_confidence_host(path) {
        for marker in &["/archive/", "/releases/", "/tarball/", "/zipball/"] {
            if let Some(idx) = path.find(marker) {
                let repo_path = &path[..idx];
                return Some(format!("https://{}", repo_path));
            }
        }
    }

    None
}

/// Normalize FTP/mirror URLs to their forge equivalents.
fn normalize_ftp_mirror(url: &str) -> Option<String> {
    let path = strip_protocol(url);

    // GNU FTP → Savannah: ftp.gnu.org/gnu/{project}/... → savannah.gnu.org/git/{project}
    if path.starts_with("ftp.gnu.org/gnu/") || path.starts_with("ftp.gnu.org/pub/gnu/") {
        let rest = path
            .strip_prefix("ftp.gnu.org/pub/gnu/")
            .or_else(|| path.strip_prefix("ftp.gnu.org/gnu/"))?;
        let project = rest.split('/').next()?;
        if !project.is_empty() {
            return Some(format!("https://savannah.gnu.org/git/{}", project));
        }
    }

    None
}

/// Normalize a repository URL to canonical form.
///
/// This is the successor to `normalize_forge_url()` in `uris.rs`.
/// Strips .git, fragments, trailing slashes, normalizes protocol.
/// Returns None if the URL is empty or doesn't match a known forge.
pub fn normalize_repo_url(url: &str) -> Option<String> {
    extract_forge_url(url).map(|e| e.repo_url)
}

/// Extract owner and repo name from a forge URL.
///
/// Replaces `github_owner_repo()` in `enricher.rs` with a generalized version
/// that handles GitHub, GitLab, Codeberg, and other forges.
pub fn extract_owner_repo(url: &str) -> Option<(String, String)> {
    let normalized = pre_normalize(url)?;
    let path = strip_protocol(&normalized);

    // GitHub
    if path.starts_with("github.com/") {
        let rest = path.strip_prefix("github.com/")?;
        let caps = FORGE_OWNER_REPO_RE.captures(rest)?;
        return Some((
            caps.get(1)?.as_str().to_string(),
            caps.get(2)?.as_str().to_string(),
        ));
    }

    // GitLab instances
    for host in GITLAB_HOSTS {
        if path.starts_with(&format!("{}/", host)) {
            let rest = path.strip_prefix(&format!("{}/", host))?;
            let rest = rest.split("/-/").next().unwrap_or(rest);
            // For GitLab, "owner" might be a group/subgroup, "repo" is the last segment
            let parts: Vec<&str> = rest.rsplitn(2, '/').collect();
            if parts.len() == 2 {
                return Some((parts[1].to_string(), parts[0].to_string()));
            }
        }
    }

    // Codeberg and other simple forges
    for host in FORGE_HOSTS.iter().chain(GITEA_HOSTS.iter()) {
        if *host == "sr.ht" { continue; } // sr.ht uses ~user prefix
        if path.starts_with(&format!("{}/", host)) {
            let rest = path.strip_prefix(&format!("{}/", host))?;
            let caps = FORGE_OWNER_REPO_RE.captures(rest)?;
            return Some((
                caps.get(1)?.as_str().to_string(),
                caps.get(2)?.as_str().to_string(),
            ));
        }
    }

    None
}

/// Extract a git commit hash from a git-describe string.
///
/// The `g` prefix in git-describe output means "git commit":
/// `glibc-2.43.9000-253-gd4c66edeef` → `d4c66edeef`
pub fn extract_git_describe_hash(desc: &str) -> Option<String> {
    let caps = GIT_DESCRIBE_RE.captures(desc)?;
    Some(caps.get(1)?.as_str().to_string())
}

// ─── Validation (I/O — HTTP HEAD requests) ──────────────────────────────

/// Semantic interpretation of HTTP status codes for forge URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    /// 200 OK — URL is alive and points to a valid resource
    Alive,
    /// 301/308 — Permanently moved; canonical_url is the new location.
    /// Common for GitHub org/repo renames.
    Moved,
    /// 302/307 — Temporary redirect. Use original URL for identity.
    TemporaryRedirect,
    /// 404 — Resource does not exist. Negative cache.
    NotFound,
    /// 403 — Access denied. Repo may be private. Negative cache.
    AccessDenied,
    /// 429 — Rate limited. Retry later. Do NOT cache.
    RateLimited,
    /// 5xx — Server error. Transient. Do NOT cache.
    ServerError,
    /// Connection refused, DNS failure, timeout.
    NetworkError,
}

/// Result of validating a URL via HTTP HEAD request.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Original URL that was validated
    pub original_url: String,
    /// HTTP status code
    pub status: u16,
    /// Canonical URL after following redirects (if 301/302)
    pub canonical_url: Option<String>,
    /// Number of redirects followed
    pub redirect_count: u32,
    /// Last-Modified header value
    pub last_modified: Option<String>,
    /// ETag header value
    pub etag: Option<String>,
    /// Timestamp of validation (ISO 8601)
    pub validated_at: String,
    /// Semantic interpretation
    pub status_class: StatusClass,
}

impl StatusClass {
    /// Whether this status should be cached (permanent or negative).
    pub fn is_cacheable(&self) -> bool {
        matches!(self, StatusClass::Alive | StatusClass::Moved | StatusClass::NotFound | StatusClass::AccessDenied)
    }

    /// Whether this status indicates the URL should be retried later.
    pub fn is_transient(&self) -> bool {
        matches!(self, StatusClass::RateLimited | StatusClass::ServerError | StatusClass::NetworkError)
    }

    /// Whether a valid upstreamRepository edge should be emitted.
    pub fn should_emit_repo(&self) -> bool {
        matches!(self, StatusClass::Alive | StatusClass::Moved | StatusClass::TemporaryRedirect)
    }

    /// DQ issue type for this status, if applicable.
    pub fn dq_issue_type(&self) -> Option<&'static str> {
        match self {
            StatusClass::NotFound => Some("dead-repository"),
            StatusClass::AccessDenied => Some("private-repository"),
            StatusClass::Moved => Some("repository-redirect"),
            StatusClass::NetworkError => Some("unreachable-url"),
            _ => None,
        }
    }

    /// DQ severity for this status.
    pub fn dq_severity(&self) -> &'static str {
        match self {
            StatusClass::NotFound => "error",
            StatusClass::AccessDenied => "warning",
            StatusClass::Moved => "info",
            StatusClass::NetworkError => "warning",
            _ => "info",
        }
    }
}

fn classify_status(code: u16) -> StatusClass {
    match code {
        200 | 204 => StatusClass::Alive,
        301 | 308 => StatusClass::Moved,
        302 | 303 | 307 => StatusClass::TemporaryRedirect,
        404 | 410 => StatusClass::NotFound,
        401 | 403 => StatusClass::AccessDenied,
        429 => StatusClass::RateLimited,
        500..=599 => StatusClass::ServerError,
        _ => StatusClass::NetworkError,
    }
}

/// Validate a URL via HTTP HEAD request.
///
/// Does NOT follow redirects automatically — instead captures each redirect
/// manually to record the chain and count. Stops after `max_redirects` hops.
///
/// Rate limiting is the caller's responsibility.
pub fn validate_url(
    client: &reqwest::blocking::Client,
    url: &str,
    max_redirects: u32,
) -> ValidationResult {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut current_url = url.to_string();
    let mut redirect_count = 0;

    loop {
        let result = client
            .head(&current_url)
            .header("User-Agent", "pg-collect/0.1.0 (forge-validator)")
            .send();

        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                let status_class = classify_status(status);

                // Extract headers before consuming response
                let last_modified = response.headers().get("last-modified")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let etag = response.headers().get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let location = response.headers().get("location")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                match status_class {
                    StatusClass::Moved | StatusClass::TemporaryRedirect if redirect_count < max_redirects => {
                        if let Some(ref loc) = location {
                            redirect_count += 1;
                            // Handle relative redirects
                            let next_url = if loc.starts_with("http") {
                                loc.clone()
                            } else {
                                // Relative redirect — resolve against current URL
                                format!("{}/{}", current_url.rsplit_once('/').map(|(base, _)| base).unwrap_or(&current_url), loc.trim_start_matches('/'))
                            };
                            current_url = next_url;
                            continue;
                        }
                        // No Location header on redirect — treat as the final status
                        return ValidationResult {
                            original_url: url.to_string(),
                            status,
                            canonical_url: None,
                            redirect_count,
                            last_modified,
                            etag,
                            validated_at: now,
                            status_class,
                        };
                    }
                    _ => {
                        let canonical = if redirect_count > 0 && current_url != url {
                            Some(current_url)
                        } else {
                            None
                        };
                        return ValidationResult {
                            original_url: url.to_string(),
                            status,
                            canonical_url: canonical,
                            redirect_count,
                            last_modified,
                            etag,
                            validated_at: now,
                            status_class,
                        };
                    }
                }
            }
            Err(_) => {
                return ValidationResult {
                    original_url: url.to_string(),
                    status: 0,
                    canonical_url: None,
                    redirect_count,
                    last_modified: None,
                    etag: None,
                    validated_at: now,
                    status_class: StatusClass::NetworkError,
                };
            }
        }
    }
}

/// Validate a URL and return the canonical form.
///
/// Returns the redirect target for 301s, the original for 200s, or None for errors.
pub fn resolve_canonical(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Option<String> {
    let result = validate_url(client, url, 10);
    match result.status_class {
        StatusClass::Alive => Some(result.original_url),
        StatusClass::Moved => result.canonical_url.or(Some(result.original_url)),
        StatusClass::TemporaryRedirect => Some(result.original_url),
        _ => None,
    }
}

// ─── DQ Emission ────────────────────────────────────────────────────────

/// Emit a `dq:DataQualityIssue` for a URL that failed extraction or validation.
///
/// Returns the number of triples emitted (7).
pub fn emit_dq_issue(
    writer: &mut NTriplesWriter,
    detector: &str,
    field: &str,
    raw_value: &str,
    issue_type: &str,
    severity: &str,
) -> Result<usize> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(raw_value, &mut hasher);
    let url_hash = format!("{:x}", std::hash::Hasher::finish(&hasher));

    let issue_uri = dq_issue_uri(detector, field, &url_hash[..12]);
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    writer.write_triple(&issue_uri, RDF_TYPE, &format!("{DQ}DataQualityIssue"))?;
    writer.write_literal(&issue_uri, &format!("{DQ}issueType"), issue_type)?;
    writer.write_literal(&issue_uri, &format!("{DQ}rawValue"), raw_value)?;
    writer.write_literal(&issue_uri, &format!("{DQ}detectedBy"), detector)?;
    writer.write_literal(&issue_uri, &format!("{DQ}field"), field)?;
    writer.write_literal(&issue_uri, &format!("{DQ}severity"), severity)?;
    writer.write_literal(&issue_uri, &format!("{DQ}detectedAt"), &now)?;

    Ok(7)
}

/// Emit a `dq:DataQualityIssue` from a validation result.
///
/// Automatically selects issue_type and severity based on StatusClass.
/// For redirects, also emits the canonical URL in the raw value.
/// Returns 0 if the status doesn't warrant a DQ issue (e.g., Alive).
pub fn emit_validation_dq(
    writer: &mut NTriplesWriter,
    detector: &str,
    validation: &ValidationResult,
) -> Result<usize> {
    let issue_type = match validation.status_class.dq_issue_type() {
        Some(t) => t,
        None => return Ok(0),
    };

    let raw_value = if let Some(ref canonical) = validation.canonical_url {
        format!("{} -> {}", validation.original_url, canonical)
    } else {
        validation.original_url.clone()
    };

    emit_dq_issue(writer, detector, "repository-url", &raw_value, issue_type,
        validation.status_class.dq_severity())
}

// ─── Forge Software Mapping (ontology v0.8.0) ──────────────────────────

/// Map a forge hostname to the ontology's vcs:ForgeSoftware individual URI.
///
/// Returns the full URI for the forge software product, or None for hosts
/// where no individual is defined in the ontology (e.g., Pagure).
pub fn detect_forge_software(host: &str) -> Option<&'static str> {
    let host = host.trim_end_matches('/');
    // GitHub
    if host == "github.com" {
        return Some("https://purl.org/packagegraph/ontology/vcs#GitHub");
    }
    // GitLab instances
    if GITLAB_HOSTS.iter().any(|h| host == *h) {
        return Some("https://purl.org/packagegraph/ontology/vcs#GitLab");
    }
    // Forgejo/Gitea instances (Codeberg runs Forgejo)
    if GITEA_HOSTS.iter().any(|h| host == *h) || host == "codeberg.org" {
        return Some("https://purl.org/packagegraph/ontology/vcs#Forgejo");
    }
    // SourceHut
    if host == "sr.ht" || host.ends_with(".sr.ht") {
        return Some("https://purl.org/packagegraph/ontology/vcs#SourceHut");
    }
    // Bitbucket — Cloud (SaaS) vs Data Center (self-hosted), split in ontology v0.8.0
    if host == "bitbucket.org" {
        return Some("https://purl.org/packagegraph/ontology/vcs#BitbucketCloud");
    }
    if host.starts_with("bitbucket.") && host != "bitbucket.org" {
        return Some("https://purl.org/packagegraph/ontology/vcs#BitbucketDataCenter");
    }
    // GNU Savannah
    if host == "savannah.gnu.org" || host == "savannah.nongnu.org"
        || host == "git.savannah.gnu.org" || host == "git.savannah.nongnu.org"
    {
        return Some("https://purl.org/packagegraph/ontology/vcs#Savannah");
    }
    // cgit instances (kernel.org, sourceware.org)
    if host == "git.kernel.org" || host == "sourceware.org" {
        return Some("https://purl.org/packagegraph/ontology/vcs#cgit");
    }
    None
}

/// Extract the forge host from a canonical repository URL.
fn forge_host_from_url(repo_url: &str) -> Option<String> {
    let stripped = repo_url
        .strip_prefix("https://")
        .or_else(|| repo_url.strip_prefix("http://"))?;
    let host = stripped.split('/').next()?;
    if host.contains('.') { Some(host.to_string()) } else { None }
}

// ─── RDF Emission ───────────────────────────────────────────────────────

/// Emit forge instance triples for a repository URL.
///
/// Emits (when forge software is recognized):
///   ?repo vcs:hostedOn ?forge .
///   ?forge a vcs:Forge .
///   ?forge vcs:forgeUrl "https://github.com"^^xsd:anyURI .
///   ?forge vcs:forgeSoftware vcs:GitHub .
///
/// Forge triples are idempotent — the same forge URI is emitted for every
/// repo on the same host, so duplicates across packages are harmless in
/// N-Triples (Fuseki deduplicates on load).
///
/// Returns the number of triples emitted (0 or 4).
pub fn emit_forge_triples(
    writer: &mut NTriplesWriter,
    repo_uri: &str,
    repo_url: &str,
) -> Result<usize> {
    let host = match forge_host_from_url(repo_url) {
        Some(h) => h,
        None => return Ok(0),
    };
    let software = match detect_forge_software(&host) {
        Some(s) => s,
        None => return Ok(0),
    };
    let f_uri = forge_uri(&host);

    writer.write_triple(repo_uri, &format!("{VCS}hostedOn"), &f_uri)?;
    writer.write_triple(&f_uri, RDF_TYPE, &format!("{VCS}Forge"))?;
    writer.write_literal(&f_uri, &format!("{VCS}forgeUrl"), &format!("https://{host}"))?;
    writer.write_triple(&f_uri, &format!("{VCS}forgeSoftware"), software)?;

    Ok(4)
}

/// Emit the standard upstream repository edge from a ForgeExtraction.
///
/// Emits:
///   ?identity pkg:upstreamRepository ?repo .
///   ?repo a vcs:Repository .
///   ?repo vcs:repositoryURL ?canonicalUrl .
///   + forge triples (vcs:hostedOn, vcs:Forge, vcs:forgeSoftware) when recognized
///
/// If a ValidationResult shows a redirect, uses the canonical URL.
pub fn emit_upstream_repo(
    writer: &mut NTriplesWriter,
    identity_uri: &str,
    extraction: &ForgeExtraction,
    validation: Option<&ValidationResult>,
) -> Result<usize> {
    // Use canonical URL from redirect resolution if available
    let repo_url = validation
        .and_then(|v| v.canonical_url.as_deref())
        .unwrap_or(&extraction.repo_url);

    let r_uri = repo_uri(repo_url);

    writer.write_triple(identity_uri, &format!("{PKG}upstreamRepository"), &r_uri)?;
    writer.write_triple(&r_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
    writer.write_literal(&r_uri, &format!("{VCS}repositoryURL"), repo_url)?;
    let mut triples = 3;

    // Emit forge instance triples (v0.8.0)
    triples += emit_forge_triples(writer, &r_uri, repo_url)?;

    Ok(triples)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── pre_normalize ──────────────────────────────────────────────

    #[test]
    fn test_pre_normalize_strips_fragment() {
        let result = pre_normalize("https://github.com/openssl/openssl#readme");
        assert_eq!(result, Some("https://github.com/openssl/openssl".to_string()));
    }

    #[test]
    fn test_pre_normalize_strips_git_suffix() {
        let result = pre_normalize("https://github.com/openssl/openssl.git");
        assert_eq!(result, Some("https://github.com/openssl/openssl".to_string()));
    }

    #[test]
    fn test_pre_normalize_git_protocol() {
        let result = pre_normalize("git://github.com/openssl/openssl.git");
        assert_eq!(result, Some("https://github.com/openssl/openssl".to_string()));
    }

    #[test]
    fn test_pre_normalize_git_plus_https() {
        let result = pre_normalize("git+https://github.com/foo/bar.git");
        assert_eq!(result, Some("https://github.com/foo/bar".to_string()));
    }

    #[test]
    fn test_pre_normalize_yocto_params() {
        let result = pre_normalize("git://github.com/openembedded/meta-oe.git;branch=master;protocol=https");
        assert_eq!(result, Some("https://github.com/openembedded/meta-oe".to_string()));
    }

    #[test]
    fn test_pre_normalize_http_to_https() {
        let result = pre_normalize("http://github.com/foo/bar");
        assert_eq!(result, Some("https://github.com/foo/bar".to_string()));
    }

    #[test]
    fn test_pre_normalize_trailing_slash() {
        let result = pre_normalize("https://github.com/foo/bar/");
        assert_eq!(result, Some("https://github.com/foo/bar".to_string()));
    }

    #[test]
    fn test_pre_normalize_empty() {
        assert_eq!(pre_normalize(""), None);
        assert_eq!(pre_normalize("   "), None);
    }

    // ─── extract_forge_url ──────────────────────────────────────────

    #[test]
    fn test_extract_github_direct() {
        let result = extract_forge_url("https://github.com/openssl/openssl").unwrap();
        assert_eq!(result.repo_url, "https://github.com/openssl/openssl");
        assert_eq!(result.confidence, Confidence::High);
        assert_eq!(result.extractor, "direct-forge");
    }

    #[test]
    fn test_extract_github_with_subpath() {
        let result = extract_forge_url("https://github.com/torvalds/linux/tree/master").unwrap();
        assert_eq!(result.repo_url, "https://github.com/torvalds/linux");
    }

    #[test]
    fn test_extract_github_dotted_repo() {
        let result = extract_forge_url("https://github.com/docopt/docopt.cpp").unwrap();
        assert_eq!(result.repo_url, "https://github.com/docopt/docopt.cpp");
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn test_extract_github_fragment() {
        let result = extract_forge_url("https://github.com/openssl/openssl#readme").unwrap();
        assert_eq!(result.repo_url, "https://github.com/openssl/openssl");
    }

    #[test]
    fn test_extract_github_git_suffix() {
        let result = extract_forge_url("https://github.com/curl/curl.git").unwrap();
        assert_eq!(result.repo_url, "https://github.com/curl/curl");
    }

    #[test]
    fn test_extract_github_git_protocol() {
        let result = extract_forge_url("git://github.com/openssl/openssl.git").unwrap();
        assert_eq!(result.repo_url, "https://github.com/openssl/openssl");
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn test_extract_github_org_only_rejected() {
        // github.com/org without a repo should not match
        assert!(extract_forge_url("https://github.com/tesseract-ocr").is_none());
    }

    #[test]
    fn test_extract_gitlab_freedesktop() {
        let result = extract_forge_url("https://gitlab.freedesktop.org/xorg/lib/libx11").unwrap();
        assert_eq!(result.repo_url, "https://gitlab.freedesktop.org/xorg/lib/libx11");
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn test_extract_gitlab_with_subpath() {
        let result = extract_forge_url("https://gitlab.gnome.org/GNOME/glib/-/tree/main").unwrap();
        assert_eq!(result.repo_url, "https://gitlab.gnome.org/GNOME/glib");
    }

    #[test]
    fn test_extract_codeberg() {
        let result = extract_forge_url("https://codeberg.org/Freeyourgadget/Gadgetbridge").unwrap();
        assert_eq!(result.repo_url, "https://codeberg.org/Freeyourgadget/Gadgetbridge");
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn test_extract_salsa_debian() {
        let result = extract_forge_url("https://salsa.debian.org/debian/openssl").unwrap();
        assert_eq!(result.repo_url, "https://salsa.debian.org/debian/openssl");
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn test_extract_pagure() {
        let result = extract_forge_url("https://pagure.io/python-rpm-generators").unwrap();
        assert_eq!(result.repo_url, "https://pagure.io/python-rpm-generators");
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn test_extract_fedora_distgit() {
        let result = extract_forge_url("https://src.fedoraproject.org/rpms/openssl").unwrap();
        assert_eq!(result.repo_url, "https://src.fedoraproject.org/rpms/openssl");
    }

    #[test]
    fn test_extract_savannah_gnu() {
        let result = extract_forge_url("git://git.savannah.gnu.org/git/bash.git").unwrap();
        assert_eq!(result.repo_url, "https://savannah.gnu.org/git/bash");
    }

    #[test]
    fn test_extract_sourceware() {
        let result = extract_forge_url("https://sourceware.org/git/glibc").unwrap();
        assert_eq!(result.repo_url, "https://sourceware.org/git/glibc");
    }

    #[test]
    fn test_extract_kernel_org() {
        let result = extract_forge_url("https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux").unwrap();
        assert_eq!(result.repo_url, "https://git.kernel.org/linux/kernel/git/torvalds/linux");
    }

    #[test]
    fn test_extract_generic_git_host() {
        let result = extract_forge_url("https://git.openssl.org/openssl").unwrap();
        assert_eq!(result.repo_url, "https://git.openssl.org/openssl");
        assert_eq!(result.confidence, Confidence::Medium);
    }

    #[test]
    fn test_extract_non_forge_returns_none() {
        assert!(extract_forge_url("https://www.openssl.org/").is_none());
        assert!(extract_forge_url("https://example.com/downloads/foo.tar.gz").is_none());
        assert!(extract_forge_url("not a url").is_none());
    }

    // ─── normalize_archive_url ──────────────────────────────────────

    #[test]
    fn test_archive_github_refs_tags() {
        let result = normalize_archive_url("https://github.com/openssl/openssl/archive/refs/tags/openssl-3.2.2.tar.gz").unwrap();
        assert_eq!(result, "https://github.com/openssl/openssl");
    }

    #[test]
    fn test_archive_github_releases_download() {
        let result = normalize_archive_url("https://github.com/x/y/releases/download/v1.0/y-1.0.tar.gz").unwrap();
        assert_eq!(result, "https://github.com/x/y");
    }

    #[test]
    fn test_archive_github_tarball() {
        let result = normalize_archive_url("https://github.com/x/y/tarball/v1.0").unwrap();
        assert_eq!(result, "https://github.com/x/y");
    }

    #[test]
    fn test_archive_gitlab() {
        let result = normalize_archive_url("https://gitlab.freedesktop.org/xorg/lib/libx11/-/archive/xorg/libX11-1.8.10/libx11.tar.gz").unwrap();
        assert_eq!(result, "https://gitlab.freedesktop.org/xorg/lib/libx11");
    }

    #[test]
    fn test_archive_non_forge_returns_none() {
        assert!(normalize_archive_url("https://example.com/archive/foo.tar.gz").is_none());
    }

    // ─── FTP mirror mapping ─────────────────────────────────────────

    #[test]
    fn test_ftp_gnu_to_savannah() {
        let result = extract_forge_url("https://ftp.gnu.org/gnu/glibc/glibc-2.43.tar.xz").unwrap();
        assert_eq!(result.repo_url, "https://savannah.gnu.org/git/glibc");
        assert_eq!(result.confidence, Confidence::Medium);
        assert_eq!(result.extractor, "ftp-mirror-mapping");
    }

    #[test]
    fn test_ftp_gnu_pub_path() {
        let result = extract_forge_url("https://ftp.gnu.org/pub/gnu/bash/bash-5.2.tar.gz").unwrap();
        assert_eq!(result.repo_url, "https://savannah.gnu.org/git/bash");
    }

    // ─── extract_best_repo ──────────────────────────────────────────

    #[test]
    fn test_best_repo_prefers_high_confidence() {
        let candidates = [
            ("homepage", "https://www.openssl.org/"),
            ("forgeurl", "https://github.com/openssl/openssl"),
        ];
        let result = extract_best_repo(&candidates).unwrap();
        assert_eq!(result.repo_url, "https://github.com/openssl/openssl");
        assert_eq!(result.source_field, "forgeurl");
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn test_best_repo_falls_back_to_medium() {
        let candidates = [
            ("homepage", "https://www.openssl.org/"),
            ("URL", "https://git.openssl.org/openssl"),
        ];
        let result = extract_best_repo(&candidates).unwrap();
        assert_eq!(result.confidence, Confidence::Medium);
    }

    #[test]
    fn test_best_repo_returns_none_when_no_forges() {
        let candidates = [
            ("homepage", "https://www.openssl.org/"),
            ("URL", "https://example.com"),
        ];
        assert!(extract_best_repo(&candidates).is_none());
    }

    // ─── extract_owner_repo ─────────────────────────────────────────

    #[test]
    fn test_owner_repo_github() {
        let (owner, repo) = extract_owner_repo("https://github.com/openssl/openssl").unwrap();
        assert_eq!(owner, "openssl");
        assert_eq!(repo, "openssl");
    }

    #[test]
    fn test_owner_repo_github_dotted() {
        let (owner, repo) = extract_owner_repo("https://github.com/docopt/docopt.cpp").unwrap();
        assert_eq!(owner, "docopt");
        assert_eq!(repo, "docopt.cpp");
    }

    #[test]
    fn test_owner_repo_github_fragment() {
        let (owner, repo) = extract_owner_repo("https://github.com/foo/bar#readme").unwrap();
        assert_eq!(owner, "foo");
        assert_eq!(repo, "bar");
    }

    #[test]
    fn test_owner_repo_github_git_proto() {
        let (owner, repo) = extract_owner_repo("git://github.com/foo/bar.git").unwrap();
        assert_eq!(owner, "foo");
        assert_eq!(repo, "bar");
    }

    #[test]
    fn test_owner_repo_gitlab() {
        let (owner, repo) = extract_owner_repo("https://gitlab.gnome.org/GNOME/glib").unwrap();
        assert_eq!(owner, "GNOME");
        assert_eq!(repo, "glib");
    }

    #[test]
    fn test_owner_repo_gitlab_nested() {
        let (owner, repo) = extract_owner_repo("https://gitlab.freedesktop.org/xorg/lib/libx11").unwrap();
        assert_eq!(owner, "xorg/lib");
        assert_eq!(repo, "libx11");
    }

    #[test]
    fn test_owner_repo_codeberg() {
        let (owner, repo) = extract_owner_repo("https://codeberg.org/Freeyourgadget/Gadgetbridge").unwrap();
        assert_eq!(owner, "Freeyourgadget");
        assert_eq!(repo, "Gadgetbridge");
    }

    #[test]
    fn test_owner_repo_non_forge_returns_none() {
        assert!(extract_owner_repo("https://www.openssl.org/").is_none());
    }

    // ─── extract_git_describe_hash ──────────────────────────────────

    #[test]
    fn test_git_describe_hash() {
        let hash = extract_git_describe_hash("glibc-2.43.9000-253-gd4c66edeef").unwrap();
        assert_eq!(hash, "d4c66edeef");
    }

    #[test]
    fn test_git_describe_hash_long() {
        let hash = extract_git_describe_hash("v1.0.0-10-gabcdef1234567890").unwrap();
        assert_eq!(hash, "abcdef1234567890");
    }

    #[test]
    fn test_git_describe_no_hash() {
        assert!(extract_git_describe_hash("glibc-2.43").is_none());
        assert!(extract_git_describe_hash("v1.0.0").is_none());
    }

    // ─── Yocto SRC_URI patterns ─────────────────────────────────────

    #[test]
    fn test_yocto_git_src_uri() {
        let result = extract_forge_url("git://github.com/openembedded/meta-openembedded.git;branch=master").unwrap();
        assert_eq!(result.repo_url, "https://github.com/openembedded/meta-openembedded");
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn test_yocto_git_src_uri_with_protocol() {
        let result = extract_forge_url("git://github.com/foo/bar.git;branch=main;protocol=https").unwrap();
        assert_eq!(result.repo_url, "https://github.com/foo/bar");
    }

    // ─── OpenWRT git source ─────────────────────────────────────────

    #[test]
    fn test_openwrt_git_source() {
        let result = extract_forge_url("git://git.openwrt.org/project/ubus.git").unwrap();
        assert_eq!(result.repo_url, "https://git.openwrt.org/project/ubus");
    }

    // ─── NPM/Cargo structured fields ────────────────────────────────

    #[test]
    fn test_cargo_repository_field() {
        let result = extract_forge_url("https://github.com/serde-rs/serde").unwrap();
        assert_eq!(result.repo_url, "https://github.com/serde-rs/serde");
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn test_npm_git_plus_https() {
        let result = extract_forge_url("git+https://github.com/expressjs/express.git").unwrap();
        assert_eq!(result.repo_url, "https://github.com/expressjs/express");
    }

    // ─── StatusClass ────────────────────────────────────────────────

    #[test]
    fn test_classify_status() {
        assert_eq!(classify_status(200), StatusClass::Alive);
        assert_eq!(classify_status(301), StatusClass::Moved);
        assert_eq!(classify_status(308), StatusClass::Moved);
        assert_eq!(classify_status(302), StatusClass::TemporaryRedirect);
        assert_eq!(classify_status(307), StatusClass::TemporaryRedirect);
        assert_eq!(classify_status(404), StatusClass::NotFound);
        assert_eq!(classify_status(410), StatusClass::NotFound);
        assert_eq!(classify_status(403), StatusClass::AccessDenied);
        assert_eq!(classify_status(429), StatusClass::RateLimited);
        assert_eq!(classify_status(500), StatusClass::ServerError);
        assert_eq!(classify_status(503), StatusClass::ServerError);
    }

    #[test]
    fn test_status_class_cacheable() {
        assert!(StatusClass::Alive.is_cacheable());
        assert!(StatusClass::Moved.is_cacheable());
        assert!(StatusClass::NotFound.is_cacheable());
        assert!(StatusClass::AccessDenied.is_cacheable());
        assert!(!StatusClass::RateLimited.is_cacheable());
        assert!(!StatusClass::ServerError.is_cacheable());
        assert!(!StatusClass::NetworkError.is_cacheable());
    }

    #[test]
    fn test_status_class_should_emit() {
        assert!(StatusClass::Alive.should_emit_repo());
        assert!(StatusClass::Moved.should_emit_repo());
        assert!(StatusClass::TemporaryRedirect.should_emit_repo());
        assert!(!StatusClass::NotFound.should_emit_repo());
        assert!(!StatusClass::AccessDenied.should_emit_repo());
        assert!(!StatusClass::RateLimited.should_emit_repo());
    }

    #[test]
    fn test_status_class_dq_issue_type() {
        assert_eq!(StatusClass::NotFound.dq_issue_type(), Some("dead-repository"));
        assert_eq!(StatusClass::AccessDenied.dq_issue_type(), Some("private-repository"));
        assert_eq!(StatusClass::Moved.dq_issue_type(), Some("repository-redirect"));
        assert_eq!(StatusClass::Alive.dq_issue_type(), None);
    }

    // ─── Validation with mockito ────────────────────────────────────

    #[test]
    fn test_validate_url_200() {
        let mut server = mockito::Server::new();
        let mock = server.mock("HEAD", "/openssl/openssl")
            .with_status(200)
            .with_header("last-modified", "Thu, 01 Jan 2026 00:00:00 GMT")
            .with_header("etag", "\"abc123\"")
            .create();

        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build().unwrap();

        let url = format!("{}/openssl/openssl", server.url());
        let result = validate_url(&client, &url, 10);

        mock.assert();
        assert_eq!(result.status, 200);
        assert_eq!(result.status_class, StatusClass::Alive);
        assert_eq!(result.redirect_count, 0);
        assert!(result.canonical_url.is_none());
        assert_eq!(result.last_modified.as_deref(), Some("Thu, 01 Jan 2026 00:00:00 GMT"));
        assert_eq!(result.etag.as_deref(), Some("\"abc123\""));
    }

    #[test]
    fn test_validate_url_301_redirect() {
        let mut server = mockito::Server::new();

        // First request returns 301 → /new/repo
        let mock1 = server.mock("HEAD", "/old/repo")
            .with_status(301)
            .with_header("location", &format!("{}/new/repo", server.url()))
            .create();

        // Redirect target returns 200
        let mock2 = server.mock("HEAD", "/new/repo")
            .with_status(200)
            .create();

        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build().unwrap();

        let url = format!("{}/old/repo", server.url());
        let result = validate_url(&client, &url, 10);

        mock1.assert();
        mock2.assert();
        assert_eq!(result.status, 200);
        assert_eq!(result.status_class, StatusClass::Alive);
        assert_eq!(result.redirect_count, 1);
        assert!(result.canonical_url.is_some());
        assert!(result.canonical_url.unwrap().contains("/new/repo"));
    }

    #[test]
    fn test_validate_url_404() {
        let mut server = mockito::Server::new();
        let mock = server.mock("HEAD", "/deleted/repo")
            .with_status(404)
            .create();

        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build().unwrap();

        let url = format!("{}/deleted/repo", server.url());
        let result = validate_url(&client, &url, 10);

        mock.assert();
        assert_eq!(result.status, 404);
        assert_eq!(result.status_class, StatusClass::NotFound);
        assert!(result.status_class.is_cacheable());
        assert!(!result.status_class.should_emit_repo());
    }

    #[test]
    fn test_validate_url_max_redirects() {
        let mut server = mockito::Server::new();

        // Create an infinite redirect loop
        let url = format!("{}/loop", server.url());
        let _mock = server.mock("HEAD", "/loop")
            .with_status(301)
            .with_header("location", &url)
            .expect_at_least(3)
            .create();

        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build().unwrap();

        let result = validate_url(&client, &url, 3);

        // Should stop after max_redirects
        assert_eq!(result.redirect_count, 3);
        assert_eq!(result.status, 301);
        assert_eq!(result.status_class, StatusClass::Moved);
    }

    #[test]
    fn test_resolve_canonical_200() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("HEAD", "/repo")
            .with_status(200)
            .create();

        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build().unwrap();

        let url = format!("{}/repo", server.url());
        let canonical = resolve_canonical(&client, &url);
        assert_eq!(canonical, Some(url));
    }

    // ─── DQ emission ──────────────────────────────────────────────

    #[test]
    fn test_emit_dq_issue() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp.reopen().unwrap());

        let count = emit_dq_issue(&mut writer, "collect-rpm", "homepage",
            "https://example.com/broken", "no-forge-url-extractable", "info").unwrap();
        writer.flush().unwrap();

        assert_eq!(count, 7);
        let mut content = String::new();
        std::io::Read::read_to_string(&mut temp.reopen().unwrap(), &mut content).unwrap();
        assert!(content.contains("dq#DataQualityIssue"));
        assert!(content.contains("\"no-forge-url-extractable\""));
        assert!(content.contains("\"collect-rpm\""));
    }

    #[test]
    fn test_emit_validation_dq_404() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp.reopen().unwrap());

        let validation = ValidationResult {
            original_url: "https://github.com/deleted/repo".to_string(),
            status: 404,
            canonical_url: None,
            redirect_count: 0,
            last_modified: None,
            etag: None,
            validated_at: "2026-04-26T00:00:00Z".to_string(),
            status_class: StatusClass::NotFound,
        };

        let count = emit_validation_dq(&mut writer, "enrich-github", &validation).unwrap();
        writer.flush().unwrap();

        assert_eq!(count, 7);
        let mut content = String::new();
        std::io::Read::read_to_string(&mut temp.reopen().unwrap(), &mut content).unwrap();
        assert!(content.contains("\"dead-repository\""));
        assert!(content.contains("\"error\""));
    }

    #[test]
    fn test_emit_validation_dq_alive_returns_zero() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp.reopen().unwrap());

        let validation = ValidationResult {
            original_url: "https://github.com/alive/repo".to_string(),
            status: 200,
            canonical_url: None,
            redirect_count: 0,
            last_modified: None,
            etag: None,
            validated_at: "2026-04-26T00:00:00Z".to_string(),
            status_class: StatusClass::Alive,
        };

        let count = emit_validation_dq(&mut writer, "enrich-github", &validation).unwrap();
        assert_eq!(count, 0); // No DQ issue for alive URLs
    }

    #[test]
    fn test_emit_validation_dq_redirect_includes_canonical() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp.reopen().unwrap());

        let validation = ValidationResult {
            original_url: "https://github.com/old/repo".to_string(),
            status: 301,
            canonical_url: Some("https://github.com/new/repo".to_string()),
            redirect_count: 1,
            last_modified: None,
            etag: None,
            validated_at: "2026-04-26T00:00:00Z".to_string(),
            status_class: StatusClass::Moved,
        };

        let count = emit_validation_dq(&mut writer, "collect-rpm", &validation).unwrap();
        writer.flush().unwrap();

        assert_eq!(count, 7);
        let mut content = String::new();
        std::io::Read::read_to_string(&mut temp.reopen().unwrap(), &mut content).unwrap();
        assert!(content.contains("\"repository-redirect\""));
        assert!(content.contains("\"info\""));
        assert!(content.contains("old/repo"));
        assert!(content.contains("new/repo"));
    }

    // ─── RDF emission ───────────────────────────────────────────────

    #[test]
    fn test_emit_upstream_repo_basic() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp.reopen().unwrap());

        let extraction = ForgeExtraction {
            repo_url: "https://github.com/openssl/openssl".to_string(),
            confidence: Confidence::High,
            source_field: "forgeurl".to_string(),
            extractor: "direct-forge",
        };

        let count = emit_upstream_repo(&mut writer,
            "https://packagegraph.github.io/d/pkgid/rpm/fedora/openssl",
            &extraction, None).unwrap();
        writer.flush().unwrap();

        assert_eq!(count, 7); // 3 repo + 4 forge
        let mut content = String::new();
        std::io::Read::read_to_string(&mut temp.reopen().unwrap(), &mut content).unwrap();
        assert!(content.contains("core#upstreamRepository"));
        assert!(content.contains("vcs#Repository"));
        assert!(content.contains("vcs#repositoryURL"));
        // No PackageRelationship — upstream repo links use pkg:upstreamRepository directly
        assert!(!content.contains("PackageRelationship"), "Upstream repo links should NOT use PackageRelationship");
        // Forge triples (v0.8.0)
        assert!(content.contains("vcs#hostedOn"), "Should link repo to forge");
        assert!(content.contains("vcs#Forge"), "Should type forge instance");
        assert!(content.contains("vcs#forgeSoftware"), "Should link to software");
        assert!(content.contains("vcs#GitHub"), "Should identify GitHub software");
    }

    #[test]
    fn test_emit_upstream_repo_uses_canonical_from_redirect() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp.reopen().unwrap());

        let extraction = ForgeExtraction {
            repo_url: "https://github.com/old/repo".to_string(),
            confidence: Confidence::High,
            source_field: "homepage".to_string(),
            extractor: "direct-forge",
        };

        let validation = ValidationResult {
            original_url: "https://github.com/old/repo".to_string(),
            status: 200,
            canonical_url: Some("https://github.com/new/repo".to_string()),
            redirect_count: 1,
            last_modified: None,
            etag: None,
            validated_at: "2026-04-26T00:00:00Z".to_string(),
            status_class: StatusClass::Alive,
        };

        emit_upstream_repo(&mut writer,
            "https://packagegraph.github.io/d/pkgid/rpm/fedora/test",
            &extraction, Some(&validation)).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        std::io::Read::read_to_string(&mut temp.reopen().unwrap(), &mut content).unwrap();
        // Should use the canonical URL from the redirect, not the original
        assert!(content.contains("new/repo"), "Should use canonical URL from redirect");
    }

    #[test]
    fn test_resolve_canonical_404_returns_none() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("HEAD", "/gone")
            .with_status(404)
            .create();

        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build().unwrap();

        let url = format!("{}/gone", server.url());
        assert!(resolve_canonical(&client, &url).is_none());
    }

    // ─── detect_forge_software ─────────────────────────────────────────

    #[test]
    fn test_detect_forge_software_github() {
        assert_eq!(detect_forge_software("github.com"),
            Some("https://purl.org/packagegraph/ontology/vcs#GitHub"));
    }

    #[test]
    fn test_detect_forge_software_gitlab_instances() {
        for host in &["gitlab.com", "gitlab.freedesktop.org", "gitlab.gnome.org", "invent.kde.org"] {
            assert_eq!(detect_forge_software(host),
                Some("https://purl.org/packagegraph/ontology/vcs#GitLab"),
                "Failed for {}", host);
        }
    }

    #[test]
    fn test_detect_forge_software_codeberg_is_forgejo() {
        // v0.8.0: Codeberg is a Forge instance running Forgejo, not a software product
        assert_eq!(detect_forge_software("codeberg.org"),
            Some("https://purl.org/packagegraph/ontology/vcs#Forgejo"));
    }

    #[test]
    fn test_detect_forge_software_gitea_forgejo() {
        for host in &["gitea.com", "forgejo.org", "notabug.org"] {
            assert_eq!(detect_forge_software(host),
                Some("https://purl.org/packagegraph/ontology/vcs#Forgejo"),
                "Failed for {}", host);
        }
    }

    #[test]
    fn test_detect_forge_software_sourcehut() {
        assert_eq!(detect_forge_software("sr.ht"),
            Some("https://purl.org/packagegraph/ontology/vcs#SourceHut"));
    }

    #[test]
    fn test_detect_forge_software_cgit() {
        assert_eq!(detect_forge_software("git.kernel.org"),
            Some("https://purl.org/packagegraph/ontology/vcs#cgit"));
        assert_eq!(detect_forge_software("sourceware.org"),
            Some("https://purl.org/packagegraph/ontology/vcs#cgit"));
    }

    #[test]
    fn test_detect_forge_software_unknown_returns_none() {
        assert_eq!(detect_forge_software("pagure.io"), None);
        assert_eq!(detect_forge_software("example.com"), None);
    }
}
