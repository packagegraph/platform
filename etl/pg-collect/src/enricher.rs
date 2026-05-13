//! Shared enricher infrastructure.
//!
//! All enrichers follow the same pattern:
//! 1. Query Fuseki SPARQL for packages/repos to process
//! 2. Call external HTTP APIs for enrichment data
//! 3. Stream N-Triples to output file via NTriplesWriter
//!
//! This module provides shared utilities used across enrichers.

use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::blocking::Client;
use std::time::Duration;

/// Regex for extracting owner/repo from GitHub URLs.
///
/// Matches: https://github.com/{owner}/{repo}[.git][/...]
/// Repo names may contain dots (e.g., docopt.cpp, vue.js, socket.io).
static GITHUB_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?(?:/.*)?$").unwrap()
});

/// Extract (owner, repo) from a GitHub URL.
///
/// Strips URL fragments (#readme, etc.) before parsing.
/// Returns None if the URL is not a recognized GitHub repository URL.
///
/// # Examples
/// ```ignore
/// assert_eq!(github_owner_repo("https://github.com/openssl/openssl"), Some(("openssl", "openssl")));
/// assert_eq!(github_owner_repo("https://github.com/docopt/docopt.cpp"), Some(("docopt", "docopt.cpp")));
/// assert_eq!(github_owner_repo("https://example.com"), None);
/// ```
pub fn github_owner_repo(url: &str) -> Option<(String, String)> {
    // Strip URL fragment before parsing
    let url = url.split('#').next().unwrap_or(url);
    let caps = GITHUB_RE.captures(url)?;
    Some((
        caps.get(1)?.as_str().to_string(),
        caps.get(2)?.as_str().to_string(),
    ))
}

/// Sleep for rate limiting between API calls.
pub fn rate_limit(duration: Duration) {
    std::thread::sleep(duration);
}

/// Default rate limit for most APIs (200ms between calls).
pub const DEFAULT_RATE_LIMIT: Duration = Duration::from_millis(200);

/// Rate limit for rate-sensitive APIs like Repology (1s between calls).
pub const SLOW_RATE_LIMIT: Duration = Duration::from_secs(1);

/// Shared HTTP client with consistent User-Agent, timeout, and redirect policy.
pub fn default_http_client() -> Client {
    Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("pg-collect/1.0 (PackageGraph; https://packagegraph.github.io)")
        .build()
        .expect("Failed to create HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_github_owner_repo_standard() {
        let result = github_owner_repo("https://github.com/openssl/openssl");
        assert_eq!(result, Some(("openssl".to_string(), "openssl".to_string())));
    }

    #[test]
    fn test_github_owner_repo_with_git_suffix() {
        let result = github_owner_repo("https://github.com/curl/curl.git");
        assert_eq!(result, Some(("curl".to_string(), "curl".to_string())));
    }

    #[test]
    fn test_github_owner_repo_with_trailing_slash() {
        let result = github_owner_repo("https://github.com/systemd/systemd/");
        assert_eq!(result, Some(("systemd".to_string(), "systemd".to_string())));
    }

    #[test]
    fn test_github_owner_repo_with_subpath() {
        let result = github_owner_repo("https://github.com/torvalds/linux/tree/master");
        assert_eq!(result, Some(("torvalds".to_string(), "linux".to_string())));
    }

    #[test]
    fn test_github_owner_repo_http() {
        let result = github_owner_repo("http://github.com/foo/bar");
        assert_eq!(result, Some(("foo".to_string(), "bar".to_string())));
    }

    #[test]
    fn test_github_owner_repo_non_github() {
        assert_eq!(github_owner_repo("https://gitlab.com/foo/bar"), None);
        assert_eq!(github_owner_repo("https://example.com"), None);
        assert_eq!(github_owner_repo("not a url"), None);
    }

    #[test]
    fn test_github_owner_repo_edge_cases() {
        // Just github.com with no path
        assert_eq!(github_owner_repo("https://github.com/"), None);
        // Only owner, no repo
        assert_eq!(github_owner_repo("https://github.com/owner"), None);
    }

    #[test]
    fn test_github_owner_repo_with_fragment() {
        let result = github_owner_repo("https://github.com/openssl/openssl#readme");
        assert_eq!(result, Some(("openssl".to_string(), "openssl".to_string())));
    }

    #[test]
    fn test_github_owner_repo_with_dotted_name() {
        // Repos with dots in the name (docopt.cpp, vue.js, socket.io)
        let result = github_owner_repo("https://github.com/docopt/docopt.cpp");
        assert_eq!(result, Some(("docopt".to_string(), "docopt.cpp".to_string())));

        let result = github_owner_repo("https://github.com/vuejs/vue.js");
        assert_eq!(result, Some(("vuejs".to_string(), "vue.js".to_string())));
    }

    #[test]
    fn test_github_owner_repo_dotted_with_git_suffix() {
        // Repo with dot in name AND .git suffix
        let result = github_owner_repo("https://github.com/docopt/docopt.cpp.git");
        assert_eq!(result, Some(("docopt".to_string(), "docopt.cpp".to_string())));
    }

    #[test]
    fn test_github_owner_repo_dotted_with_fragment() {
        let result = github_owner_repo("https://github.com/docopt/docopt.cpp#readme");
        assert_eq!(result, Some(("docopt".to_string(), "docopt.cpp".to_string())));
    }
}
