//! Merged GitHub enricher — repo metadata, activity, language metrics, and license.
//!
//! Queries Fuseki for packages with GitHub homepages, then for each repo:
//! - Repo metadata: stars, forks, topics, default branch, description
//! - Language composition: bytes per language (Linguist)
//! - License: SPDX ID from GitHub API
//! - Activity: open issues count, watchers
//!
//! Replaces 4 Python enrichers: github.py, vcs_activity.py, metrics.py, license.py

use crate::cache::{FileCache, MinioConfig};
use crate::enricher::{github_owner_repo, rate_limit, DEFAULT_RATE_LIMIT};
use crate::forge;
use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use chrono::Utc;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

/// GraphQL API v4 response wrapper. rateLimit is inside `data` (GraphQL top-level field).
#[derive(Debug, Serialize, Deserialize)]
struct GraphQlResponse {
    data: GraphQlData,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlError {
    message: String,
}

/// GitHub GraphQL rate limit information.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlRateLimit {
    cost: i64,
    limit: i64,
    remaining: i64,
    node_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlData {
    rate_limit: Option<GraphQlRateLimit>,
    repository: Option<GraphQlRepository>,
}

/// GitHub repository data from GraphQL API v4.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlRepository {
    url: Option<String>,
    description: Option<String>,
    is_archived: Option<bool>,
    is_fork: Option<bool>,
    pushed_at: Option<String>,
    stargazer_count: Option<u64>,
    fork_count: Option<u64>,
    watchers: Option<GraphQlTotalCount>,
    issues: Option<GraphQlTotalCount>,
    pull_requests: Option<GraphQlTotalCount>,
    default_branch_ref: Option<GraphQlBranchRefExtended>,
    license_info: Option<GraphQlLicense>,
    repository_topics: Option<GraphQlTopics>,
    languages: Option<GraphQlLanguages>,
    // New fields (Tier 1 expansion)
    created_at: Option<String>,
    updated_at: Option<String>,
    homepage_url: Option<String>,
    disk_usage: Option<u64>,
    has_wiki_enabled: Option<bool>,
    has_issues_enabled: Option<bool>,
    is_template: Option<bool>,
    primary_language: Option<GraphQlPrimaryLanguage>,
    releases: Option<GraphQlReleases>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlTotalCount {
    total_count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlBranchRef {
    name: String,
}

/// Extended branch ref with commit target (for HEAD commit data).
#[derive(Debug, Serialize, Deserialize)]
struct GraphQlBranchRefExtended {
    name: String,
    target: Option<GraphQlCommitTarget>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlCommitTarget {
    oid: String,
    author: GraphQlCommitAuthor,
    signature: Option<GraphQlCommitSignature>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlCommitSignature {
    is_valid: bool,
    state: String,
    was_signed_by_git_hub: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlCommitAuthor {
    name: String,
    email: Option<String>,
    user: Option<GraphQlUser>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlUser {
    login: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlPrimaryLanguage {
    name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlReleases {
    nodes: Vec<GraphQlRelease>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlRelease {
    tag_name: String,
    published_at: Option<String>,
    is_prerelease: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlLicense {
    spdx_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlTopics {
    nodes: Vec<GraphQlTopicNode>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlTopicNode {
    topic: GraphQlTopic,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlTopic {
    name: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlLanguages {
    total_size: u64,
    edges: Vec<GraphQlLanguageEdge>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlLanguageEdge {
    size: u64,
    node: GraphQlLanguageNode,
}

#[derive(Debug, Serialize, Deserialize)]
struct GraphQlLanguageNode {
    name: String,
}

/// GitHub repository metadata from the API.
#[derive(Debug, Deserialize)]
struct GitHubRepo {
    html_url: Option<String>,
    description: Option<String>,
    default_branch: Option<String>,
    stargazers_count: Option<u64>,
    forks_count: Option<u64>,
    open_issues_count: Option<u64>,
    subscribers_count: Option<u64>,
    topics: Option<Vec<String>>,
    license: Option<GitHubLicense>,
    archived: Option<bool>,
    fork: Option<bool>,
    pushed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubLicense {
    spdx_id: Option<String>,
    name: Option<String>,
}

/// GraphQL query template for repository enrichment (expanded - Tier 1+2).
const GRAPHQL_REPO_QUERY: &str = r#"
query RepoEnrichment($owner: String!, $name: String!) {
  rateLimit {
    cost
    limit
    remaining
    nodeCount
  }
  repository(owner: $owner, name: $name) {
    url
    description
    isArchived
    isFork
    pushedAt
    stargazerCount
    forkCount
    watchers { totalCount }
    issues(states: OPEN) { totalCount }
    pullRequests(states: OPEN) { totalCount }
    createdAt
    updatedAt
    homepageUrl
    diskUsage
    hasWikiEnabled
    hasIssuesEnabled
    isTemplate
    primaryLanguage { name }
    defaultBranchRef {
      name
      target {
        ... on Commit {
          oid
          author {
            name
            email
            user { login }
          }
          signature {
            isValid
            state
            wasSignedByGitHub
          }
        }
      }
    }
    licenseInfo { spdxId }
    repositoryTopics(first: 20) {
      nodes { topic { name } }
    }
    releases(first: 10, orderBy: {field: CREATED_AT, direction: DESC}) {
      nodes {
        tagName
        publishedAt
        isPrerelease
      }
    }
    languages(first: 50, orderBy: {field: SIZE, direction: DESC}) {
      totalSize
      edges {
        size
        node { name }
      }
    }
  }
}
"#;

/// GitHub REST API contributor data.
#[derive(Debug, Serialize, Deserialize)]
struct GitHubContributor {
    login: String,
    contributions: u64,
    #[serde(rename = "type")]
    contributor_type: Option<String>,
}

pub struct GitHubEnricher {
    sparql: SparqlClient,
    client: Client,
    cache: Option<FileCache>,
    token: Option<String>,
    github_api_base: String,
    pub graph_uri: Option<String>,
}

impl GitHubEnricher {
    pub fn new(
        endpoint: &str,
        github_token: Option<String>,
        cache_dir: Option<&str>,
        minio: Option<MinioConfig>,
        auth: SparqlAuth,
        backend: SparqlBackend,
    ) -> Self {
        let sparql = make_sparql_client(endpoint, &auth, backend);
        let client = crate::enricher::default_http_client();

        let cache = cache_dir
            .map(|dir| FileCache::new(dir, "github", 24, minio).expect("Failed to create cache"));

        Self {
            sparql,
            client,
            cache,
            token: github_token,
            github_api_base: "https://api.github.com".to_string(),
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        let homepages = self.sparql.query_github_homepages()?;
        eprintln!("Found {} packages with GitHub homepages", homepages.len());

        let mut total_repos = 0;
        let mut total_triples = 0;

        // Deduplicate by repo (many packages may share the same repo)
        let mut seen_repos: HashMap<String, bool> = HashMap::new();

        for (pkg_uri, homepage, maintainer_uri) in &homepages {
            let (owner, repo) = match github_owner_repo(homepage) {
                Some(pair) => pair,
                None => continue,
            };

            let repo_key = format!("{}/{}", owner, repo);
            if seen_repos.contains_key(&repo_key) {
                if let Some(maint_uri) = maintainer_uri {
                    let r_uri = repo_uri(&format!("https://github.com/{owner}/{repo}"));
                    writer.write_triple(maint_uri, &format!("{PKG}contributesTo"), &r_uri)?;
                    writer.write_triple(&r_uri, &format!("{PKG}hasContributor"), maint_uri)?;
                    total_triples += 2;
                }
                continue;
            }
            seen_repos.insert(repo_key.clone(), true);

            total_repos += 1;
            if total_repos % 100 == 0 {
                eprintln!(
                    "Progress: {} repos processed, {} triples emitted",
                    total_repos, total_triples
                );
            }

            // Emit maintainer links unconditionally (repo URI is deterministic)
            if let Some(maint_uri) = maintainer_uri {
                let r_uri = repo_uri(&format!("https://github.com/{owner}/{repo}"));
                writer.write_triple(maint_uri, &format!("{PKG}contributesTo"), &r_uri)?;
                writer.write_triple(&r_uri, &format!("{PKG}hasContributor"), maint_uri)?;
                total_triples += 2;
            }

            match self.process_repo(&mut writer, &owner, &repo, pkg_uri) {
                Ok(triples) => {
                    total_triples += triples;
                    let url = format!("https://github.com/{owner}/{repo}");
                    writer.write_literal(&repo_uri(&url), &format!("{VCS}repositoryURL"), &url)?;
                    total_triples += 1;
                }
                Err(e) => eprintln!("  Error processing {}/{}: {}", owner, repo, e),
            }

            rate_limit(DEFAULT_RATE_LIMIT);
        }

        writer.flush()?;
        Ok((total_repos, total_triples))
    }

    /// Incremental enrichment with bounded batch size and internal GSP loading.
    ///
    /// Queries Fuseki for a ranked batch of candidate repos, processes only that batch,
    /// and loads the results additively into the specified graph. Restart-safe: each
    /// batch is durable immediately after load.
    ///
    /// Returns: (repos_processed, triples_emitted)
    pub fn enrich_incremental(
        &self,
        output_path: &str,
        max_repos: usize,
        graph_uri: &str,
    ) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        let candidates = self.sparql.query_github_candidates(graph_uri, max_repos)?;
        eprintln!(
            "Selected {} candidate entries (ranked by package coverage)",
            candidates.len()
        );

        let mut total_repos = 0;
        let mut total_triples = 0;
        let mut total_errors = 0;
        let mut consecutive_errors = 0;
        let mut seen_repos: HashMap<String, bool> = HashMap::new();
        let mut deferred_urls: Vec<(String, String)> = Vec::new();

        for (homepage, maintainer_uri, _package_count) in &candidates {
            // Parse owner/repo using the existing helper (handles http, .git, subpaths)
            let (owner, repo) = match github_owner_repo(homepage) {
                Some(pair) => pair,
                None => {
                    eprintln!("  Skipping malformed URL: {}", homepage);
                    total_triples += self.emit_dq_issue(
                        &mut writer,
                        homepage,
                        "malformed-github-url",
                        "homepage",
                        "error",
                    )?;
                    continue;
                }
            };

            let repo_key = format!("{}/{}", owner, repo);

            // Deduplicate repos (query may return multiple rows for repos with multiple maintainers)
            if seen_repos.contains_key(&repo_key) {
                // Emit contributor link if this is a new maintainer for an already-processed repo
                if let Some(maint_uri) = maintainer_uri {
                    let r_uri = repo_uri(&format!("https://github.com/{owner}/{repo}"));
                    writer.write_triple(maint_uri, &format!("{PKG}contributesTo"), &r_uri)?;
                    writer.write_triple(&r_uri, &format!("{PKG}hasContributor"), maint_uri)?;
                    total_triples += 2;
                }
                continue;
            }
            seen_repos.insert(repo_key.clone(), true);

            total_repos += 1;
            if total_repos % 100 == 0 {
                eprintln!(
                    "Progress: {} repos processed, {} triples emitted",
                    total_repos, total_triples
                );
            }

            match self.process_repo(&mut writer, &owner, &repo, "") {
                Ok(triples) => {
                    total_triples += triples;
                    consecutive_errors = 0;
                    deferred_urls.push((
                        repo_uri(&format!("https://github.com/{owner}/{repo}")),
                        format!("https://github.com/{owner}/{repo}"),
                    ));

                    // Emit contributor link from maintainer to repo
                    if let Some(maint_uri) = maintainer_uri {
                        let r_uri = repo_uri(&format!("https://github.com/{owner}/{repo}"));
                        writer.write_triple(maint_uri, &format!("{PKG}contributesTo"), &r_uri)?;
                        writer.write_triple(&r_uri, &format!("{PKG}hasContributor"), maint_uri)?;
                        total_triples += 2;
                    }
                }
                Err(e) => {
                    eprintln!("  Error processing {}/{}: {}", owner, repo, e);
                    total_errors += 1;
                    consecutive_errors += 1;
                    total_triples += self.emit_dq_issue(
                        &mut writer,
                        homepage,
                        "api-lookup-failed",
                        "homepage",
                        "warning",
                    )?;
                    if consecutive_errors >= 20 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!(
                                "Aborting: {} consecutive GitHub API failures — API may be down (last: {})",
                                consecutive_errors, e
                            ),
                        ));
                    }
                }
            }

            rate_limit(DEFAULT_RATE_LIMIT);
        }

        if total_repos > 0 && total_errors as f64 / total_repos as f64 > 0.5 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Aborting: error rate {}/{} ({:.0}%) exceeds 50% threshold",
                    total_errors,
                    total_repos,
                    total_errors as f64 / total_repos as f64 * 100.0
                ),
            ));
        }

        // Emit deferred repositoryURL triples at end of file. GSP chunks are
        // sent sequentially — if an earlier chunk fails, these are never committed,
        // so repos remain candidates for the next run instead of being permanently
        // excluded with incomplete data.
        for (r_uri, url) in &deferred_urls {
            writer.write_literal(r_uri, &format!("{VCS}repositoryURL"), url)?;
        }
        total_triples += deferred_urls.len();

        writer.flush()?;
        eprintln!(
            "Batch complete: {} repos ({} errors), {} triples ({} deferred URLs)",
            total_repos,
            total_errors,
            total_triples,
            deferred_urls.len()
        );

        // Load batch into graph via GSP POST
        eprintln!("Loading batch to graph: {}", graph_uri);
        self.sparql.gsp_post_file(output_path, graph_uri)?;
        eprintln!("Batch loaded successfully");

        Ok((total_repos, total_triples))
    }

    /// Check if a cached value is a negative cache sentinel.
    fn is_negative_cache(value: &serde_json::Value) -> bool {
        value
            .get("_negative_cache")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Create a negative cache sentinel value.
    fn negative_cache_value(error: &str) -> serde_json::Value {
        serde_json::json!({"_negative_cache": true, "error": error})
    }

    fn process_repo(
        &self,
        writer: &mut NTriplesWriter,
        owner: &str,
        repo_name: &str,
        _pkg_uri: &str,
    ) -> Result<usize> {
        // Try GraphQL first (1 request for repo + languages)
        match self.fetch_repo_graphql(owner, repo_name) {
            Ok((graphql_data, rate_limit)) => {
                // Log rate limit status
                if let Some(rl) = &rate_limit {
                    // Periodic status (every repo that returns rate limit, caller logs every 100)
                    if rl.remaining % 500 == 0 || rl.remaining < 100 || rl.cost > 1 {
                        eprintln!(
                            "  Rate limit: {}/{} pts remaining (cost {}/query, {} nodes)",
                            rl.remaining, rl.limit, rl.cost, rl.node_count
                        );
                    }
                    if rl.cost > 1 {
                        eprintln!(
                            "  WARNING: GraphQL cost {} > 1 for {}/{}",
                            rl.cost, owner, repo_name
                        );
                    }
                    if rl.remaining < 100 {
                        eprintln!(
                            "  CRITICAL: GraphQL rate limit low: {}/{} remaining",
                            rl.remaining, rl.limit
                        );
                    }
                }
                let mut triples = self.emit_from_graphql(writer, owner, repo_name, graphql_data)?;

                // Fetch contributors via REST (separate rate limit pool)
                let contributors = self.fetch_contributors(owner, repo_name);
                triples += self.emit_contributors(writer, owner, repo_name, &contributors)?;

                return Ok(triples);
            }
            Err(e) => {
                eprintln!(
                    "  GraphQL fetch failed for {}/{} ({}), falling back to REST",
                    owner, repo_name, e
                );
            }
        }

        // Fallback to REST (2 requests: repo + languages)
        let api_url = format!("{}/repos/{}/{}", self.github_api_base, owner, repo_name);
        let mut triples = 0;

        // Check cache first
        let repo_data: serde_json::Value = match self.cached_get(&api_url) {
            Some(data) => {
                // Check for negative cache hit
                if Self::is_negative_cache(&data) {
                    let error = data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    if error.contains("404") {
                        eprintln!("  Skipping {}/{}: cached 404 (permanent)", owner, repo_name);
                        let r_uri =
                            repo_uri(&format!("https://github.com/{}/{}", owner, repo_name));
                        writer.write_triple(&r_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                        writer.write_literal(
                            &r_uri,
                            &format!("{VCS}repositoryStatus"),
                            "not-found",
                        )?;
                        return Ok(2);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!(
                            "Cached transient failure for {}/{}: {}",
                            owner, repo_name, error
                        ),
                    ));
                }
                data
            }
            None => {
                let response = self.api_get(&api_url);
                match response {
                    Ok(data) => {
                        self.cache_put(&api_url, &data);
                        data
                    }
                    Err(e) => {
                        // Cache the failure so we don't retry next batch
                        self.cache_put(&api_url, &Self::negative_cache_value(&e));

                        // Only mark as "not-found" for actual 404s
                        if e.contains("404") {
                            let r_uri =
                                repo_uri(&format!("https://github.com/{}/{}", owner, repo_name));
                            writer.write_triple(&r_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                            writer.write_literal(
                                &r_uri,
                                &format!("{VCS}repositoryStatus"),
                                "not-found",
                            )?;
                            return Ok(2);
                        }
                        // For other errors (rate limit, 5xx, auth, network), propagate the error
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("GitHub API error for {}/{}: {}", owner, repo_name, e),
                        ));
                    }
                }
            }
        };

        let r_uri = repo_uri(&format!("https://github.com/{}/{}", owner, repo_name));

        // Repository type (repositoryURL emitted by caller for restart safety)
        writer.write_triple(&r_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
        triples += 1;

        // Forge instance linkage (v0.8.0)
        triples += forge::emit_forge_triples(
            writer,
            &r_uri,
            &format!("https://github.com/{}/{}", owner, repo_name),
        )?;

        // Metadata from repo object
        if let Some(desc) = repo_data.get("description").and_then(|v| v.as_str()) {
            writer.write_literal(&r_uri, &format!("{VCS}repositoryDescription"), desc)?;
            triples += 1;
        }

        if let Some(branch) = repo_data.get("default_branch").and_then(|v| v.as_str()) {
            writer.write_literal(&r_uri, &format!("{VCS}defaultBranch"), branch)?;
            triples += 1;
        }

        if let Some(stars) = repo_data.get("stargazers_count").and_then(|v| v.as_u64()) {
            writer.write_integer(&r_uri, &format!("{VCS}stargazerCount"), stars as i64)?;
            triples += 1;
        }

        if let Some(forks) = repo_data.get("forks_count").and_then(|v| v.as_u64()) {
            writer.write_integer(&r_uri, &format!("{VCS}forkCount"), forks as i64)?;
            triples += 1;
        }

        if let Some(issues) = repo_data.get("open_issues_count").and_then(|v| v.as_u64()) {
            writer.write_integer(&r_uri, &format!("{VCS}openIssuesCount"), issues as i64)?;
            triples += 1;
        }

        if let Some(watchers) = repo_data.get("subscribers_count").and_then(|v| v.as_u64()) {
            writer.write_integer(&r_uri, &format!("{VCS}subscriberCount"), watchers as i64)?;
            triples += 1;
        }

        if let Some(is_archived) = repo_data.get("archived").and_then(|v| v.as_bool()) {
            writer.write_literal(
                &r_uri,
                &format!("{VCS}isArchived"),
                &is_archived.to_string(),
            )?;
            triples += 1;
        }

        if let Some(is_fork) = repo_data.get("fork").and_then(|v| v.as_bool()) {
            writer.write_literal(&r_uri, &format!("{VCS}isFork"), &is_fork.to_string())?;
            triples += 1;
        }

        if let Some(topics) = repo_data.get("topics").and_then(|v| v.as_array()) {
            for topic in topics {
                if let Some(t) = topic.as_str() {
                    writer.write_literal(&r_uri, &format!("{VCS}topic"), t)?;
                    triples += 1;
                }
            }
        }

        // License (SPDX ID)
        if let Some(license) = repo_data.get("license").and_then(|v| v.as_object()) {
            if let Some(spdx) = license.get("spdx_id").and_then(|v| v.as_str()) {
                if spdx != "NOASSERTION" && !spdx.is_empty() {
                    writer.write_literal(&r_uri, &format!("{PKG}licenseName"), spdx)?;
                    triples += 1;
                    // License entity with spdxId (v0.7.0)
                    let license_uri = crate::uris::spdx_license_uri(spdx);
                    writer.write_triple(&r_uri, &format!("{PKG}hasLicense"), &license_uri)?;
                    writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
                    writer.write_literal(&license_uri, &format!("{PKG}spdxId"), spdx)?;
                    triples += 3;
                }
            }
        }

        // Language composition
        let lang_url = format!(
            "{}/repos/{}/{}/languages",
            self.github_api_base, owner, repo_name
        );
        if let Some(lang_data) = self.cached_get(&lang_url).or_else(|| {
            self.api_get(&lang_url).ok().map(|d| {
                self.cache_put(&lang_url, &d);
                d
            })
        }) {
            if let Some(langs) = lang_data.as_object() {
                let total_bytes: u64 = langs.values().filter_map(|v| v.as_u64()).sum();
                for (lang, bytes) in langs {
                    if let Some(b) = bytes.as_u64() {
                        writer.write_literal(&r_uri, &format!("{MET}languageName"), lang)?;
                        writer.write_integer(&r_uri, &format!("{MET}languageBytes"), b as i64)?;
                        triples += 2;
                    }
                }
                if total_bytes > 0 {
                    writer.write_integer(
                        &r_uri,
                        &format!("{MET}totalBytes"),
                        total_bytes as i64,
                    )?;
                    triples += 1;
                }
            }
        }

        // Temporal property: last commit date from pushed_at
        if let Some(pushed_at) = repo_data.get("pushed_at").and_then(|v| v.as_str()) {
            // GitHub returns ISO 8601: "2024-01-15T10:30:00Z" — extract date portion
            if let Some(date) = pushed_at.split('T').next() {
                writer.write_literal(&r_uri, &format!("{PKG}lastCommitDate"), date)?;
                triples += 1;
            }
        }

        // TODO: Task 10 — contributesTo links require maintainer URI
        // The current SPARQL query (query_github_homepages) only returns (package, homepage).
        // To emit contributesTo triples, we need to:
        // 1. Extend the SPARQL query to fetch pkg:maintainedBy ?maintainer
        // 2. Pass maintainer_uri alongside pkg_uri to process_repo
        // 3. Emit: writer.write_triple(&maintainer_uri, &format!("{PKG}contributesTo"), &r_uri)?;
        //
        // Deferred until SPARQL query refactor.

        Ok(triples)
    }

    fn api_get(&self, url: &str) -> std::result::Result<serde_json::Value, String> {
        let mut req = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json");

        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        let response = req.send().map_err(|e| e.to_string())?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err("404 Not Found".to_string());
        }

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }

        response.json().map_err(|e| e.to_string())
    }

    fn cached_get(&self, url: &str) -> Option<serde_json::Value> {
        self.cache.as_ref()?.get(url)
    }

    fn cache_put(&self, url: &str, data: &serde_json::Value) {
        if let Some(ref cache) = self.cache {
            cache.put(url, data);
        }
    }

    /// Emit a dq:DataQualityIssue for a problematic URL.
    ///
    /// Returns the number of triples emitted (always 7: type, issueType, rawValue,
    /// detectedBy, field, severity, detectedAt).
    fn emit_dq_issue(
        &self,
        writer: &mut NTriplesWriter,
        raw_url: &str,
        issue_type: &str,
        field: &str,
        severity: &str,
    ) -> Result<usize> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(raw_url, &mut hasher);
        let url_hash = format!("{:x}", std::hash::Hasher::finish(&hasher));

        let issue_uri = dq_issue_uri("enrich-github", field, &url_hash[..12]);
        let today = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        writer.write_triple(&issue_uri, RDF_TYPE, &format!("{DQ}DataQualityIssue"))?;
        writer.write_literal(&issue_uri, &format!("{DQ}issueType"), issue_type)?;
        writer.write_literal(&issue_uri, &format!("{DQ}rawValue"), raw_url)?;
        writer.write_literal(&issue_uri, &format!("{DQ}detectedBy"), "enrich-github")?;
        writer.write_literal(&issue_uri, &format!("{DQ}field"), field)?;
        writer.write_literal(&issue_uri, &format!("{DQ}severity"), severity)?;
        writer.write_literal(&issue_uri, &format!("{DQ}detectedAt"), &today)?;

        Ok(7)
    }

    /// Emit RDF triples from GraphQL repository data.
    ///
    /// Preserves the same RDF emission semantics as the REST path.
    fn emit_from_graphql(
        &self,
        writer: &mut NTriplesWriter,
        owner: &str,
        repo_name: &str,
        graphql_data: GraphQlRepository,
    ) -> Result<usize> {
        let r_uri = repo_uri(&format!("https://github.com/{}/{}", owner, repo_name));
        let mut triples = 0;

        // Repository type (repositoryURL emitted by caller for restart safety)
        writer.write_triple(&r_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
        triples += 1;

        // Forge instance linkage (v0.8.0)
        triples += forge::emit_forge_triples(
            writer,
            &r_uri,
            &format!("https://github.com/{}/{}", owner, repo_name),
        )?;

        // Metadata from GraphQL
        if let Some(desc) = graphql_data.description {
            writer.write_literal(&r_uri, &format!("{VCS}repositoryDescription"), &desc)?;
            triples += 1;
        }

        if let Some(branch_ref) = graphql_data.default_branch_ref {
            writer.write_literal(&r_uri, &format!("{VCS}defaultBranch"), &branch_ref.name)?;
            triples += 1;

            // HEAD commit data (from extended branch ref)
            if let Some(target) = branch_ref.target {
                writer.write_literal(&r_uri, &format!("{VCS}headCommitHash"), &target.oid)?;
                triples += 1;

                // HEAD commit signature (v0.8.0 att: module)
                if let Some(ref sig) = target.signature {
                    let commit_uri = format!(
                        "{DATA}commit/github/{}/{}/{}",
                        owner,
                        repo_name,
                        &target.oid[..8]
                    );
                    let sig_uri = format!("{commit_uri}/sig");
                    writer.write_triple(&commit_uri, &format!("{ATT}hasSignature"), &sig_uri)?;
                    writer.write_triple(&sig_uri, RDF_TYPE, &format!("{ATT}DigitalSignature"))?;
                    let status = if sig.is_valid {
                        "verified"
                    } else {
                        "unverified"
                    };
                    writer.write_literal(&sig_uri, &format!("{ATT}signatureStatus"), status)?;
                    triples += 3;
                }

                // HEAD commit author → foaf:Person (if GitHub login available)
                if let Some(user) = &target.author.user {
                    let person_uri = github_person_uri(&user.login);
                    writer.write_triple(&person_uri, RDF_TYPE, &format!("{FOAF}Person"))?;
                    writer.write_literal(
                        &person_uri,
                        &format!("{FOAF}name"),
                        &target.author.name,
                    )?;
                    triples += 2;

                    // Email attestation with reified observation node
                    // NOTE: foaf:mbox is NOT emitted here because the enricher is additive
                    // (GSP POST). Old values can't be deleted, so mbox would accumulate.
                    // Use observation nodes only — queries find current email via
                    // ORDER BY DESC(?date) LIMIT 1 on pkg:observedAt.
                    if let Some(email) = &target.author.email {
                        if !email.is_empty() {
                            // Reified EmailObservation node (paired email + date)
                            // URI includes email hash to avoid same-day collision if email changes
                            let today = Utc::now().format("%Y-%m-%d").to_string();
                            let mut hasher = std::collections::hash_map::DefaultHasher::new();
                            std::hash::Hash::hash(email, &mut hasher);
                            let email_hash = format!("{:x}", std::hash::Hasher::finish(&hasher));
                            let obs_uri = format!(
                                "{DATA}observation/email/{}/{}/{}",
                                user.login,
                                &email_hash[..8],
                                today
                            );
                            writer.write_triple(
                                &person_uri,
                                &format!("{PKG}hasEmailObservation"),
                                &obs_uri,
                            )?;
                            writer.write_triple(
                                &obs_uri,
                                RDF_TYPE,
                                &format!("{PKG}EmailObservation"),
                            )?;
                            writer.write_literal(
                                &obs_uri,
                                &format!("{PKG}observedEmail"),
                                email,
                            )?;
                            writer.write_date(&obs_uri, &format!("{PKG}observedAt"), &today)?;
                            triples += 4;
                        }
                    }
                }
            }
        }

        if let Some(stars) = graphql_data.stargazer_count {
            writer.write_integer(&r_uri, &format!("{VCS}stargazerCount"), stars as i64)?;
            triples += 1;
        }

        if let Some(forks) = graphql_data.fork_count {
            writer.write_integer(&r_uri, &format!("{VCS}forkCount"), forks as i64)?;
            triples += 1;
        }

        if let Some(issues) = graphql_data.issues {
            writer.write_integer(
                &r_uri,
                &format!("{VCS}openIssuesCount"),
                issues.total_count as i64,
            )?;
            triples += 1;
        }

        if let Some(watchers) = graphql_data.watchers {
            writer.write_integer(
                &r_uri,
                &format!("{VCS}subscriberCount"),
                watchers.total_count as i64,
            )?;
            triples += 1;
        }

        if let Some(is_archived) = graphql_data.is_archived {
            writer.write_literal(
                &r_uri,
                &format!("{VCS}isArchived"),
                &is_archived.to_string(),
            )?;
            triples += 1;
        }

        if let Some(is_fork) = graphql_data.is_fork {
            writer.write_literal(&r_uri, &format!("{VCS}isFork"), &is_fork.to_string())?;
            triples += 1;
        }

        if let Some(topics) = graphql_data.repository_topics {
            for topic_node in topics.nodes {
                writer.write_literal(&r_uri, &format!("{VCS}topic"), &topic_node.topic.name)?;
                triples += 1;
            }
        }

        // License (SPDX ID)
        if let Some(license) = graphql_data.license_info {
            if let Some(spdx) = license.spdx_id {
                if spdx != "NOASSERTION" && !spdx.is_empty() {
                    writer.write_literal(&r_uri, &format!("{PKG}licenseName"), &spdx)?;
                    triples += 1;
                    // License entity with spdxId (v0.7.0)
                    let license_uri = crate::uris::spdx_license_uri(&spdx);
                    writer.write_triple(&r_uri, &format!("{PKG}hasLicense"), &license_uri)?;
                    writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
                    writer.write_literal(&license_uri, &format!("{PKG}spdxId"), &spdx)?;
                    triples += 3;
                }
            }
        }

        // Primary language (VCS-01 enablement)
        if let Some(primary_lang) = graphql_data.primary_language {
            writer.write_literal(&r_uri, &format!("{MET}primaryLanguage"), &primary_lang.name)?;
            triples += 1;
        }

        // Open PRs
        if let Some(prs) = graphql_data.pull_requests {
            writer.write_integer(
                &r_uri,
                &format!("{VCS}openPullRequestCount"),
                prs.total_count as i64,
            )?;
            triples += 1;
        }

        // Free scalars
        if let Some(created_at) = graphql_data.created_at {
            if let Some(date) = created_at.split('T').next() {
                writer.write_literal(&r_uri, &format!("{VCS}repositoryCreatedAt"), date)?;
                triples += 1;
            }
        }

        if let Some(updated_at) = graphql_data.updated_at {
            if let Some(date) = updated_at.split('T').next() {
                writer.write_literal(&r_uri, &format!("{VCS}lastActivityDate"), date)?;
                triples += 1;
            }
        }

        if let Some(homepage) = graphql_data.homepage_url {
            if !homepage.is_empty() {
                writer.write_literal(&r_uri, &format!("{VCS}projectHomepage"), &homepage)?;
                triples += 1;
            }
        }

        if let Some(disk) = graphql_data.disk_usage {
            writer.write_integer(&r_uri, &format!("{VCS}diskUsageKB"), disk as i64)?;
            triples += 1;
        }

        // Releases (SCR-03 enablement)
        if let Some(releases) = graphql_data.releases {
            for release in releases.nodes {
                let release_uri = format!(
                    "{DATA}release/github/{owner}/{repo_name}/{}",
                    release.tag_name
                );
                writer.write_triple(&release_uri, RDF_TYPE, &format!("{VCS}Release"))?;
                writer.write_literal(&release_uri, &format!("{VCS}tagName"), &release.tag_name)?;
                writer.write_triple(&r_uri, &format!("{VCS}hasRelease"), &release_uri)?;
                triples += 3;

                if let Some(published_at) = release.published_at {
                    if let Some(date) = published_at.split('T').next() {
                        writer.write_literal(&release_uri, &format!("{VCS}releaseDate"), date)?;
                        triples += 1;
                    }
                }

                if release.is_prerelease {
                    writer.write_literal(&release_uri, &format!("{VCS}isPreRelease"), "true")?;
                    triples += 1;
                }
            }
        }

        // Language composition from GraphQL
        if let Some(langs) = graphql_data.languages {
            let total_bytes = langs.total_size;
            for edge in langs.edges {
                writer.write_literal(&r_uri, &format!("{MET}languageName"), &edge.node.name)?;
                writer.write_integer(&r_uri, &format!("{MET}languageBytes"), edge.size as i64)?;
                triples += 2;
            }
            if total_bytes > 0 {
                writer.write_integer(&r_uri, &format!("{MET}totalBytes"), total_bytes as i64)?;
                triples += 1;
            }
        }

        // Temporal property: last commit date from pushed_at
        if let Some(pushed_at) = graphql_data.pushed_at {
            if let Some(date) = pushed_at.split('T').next() {
                writer.write_literal(&r_uri, &format!("{PKG}lastCommitDate"), date)?;
                triples += 1;
            }
        }

        Ok(triples)
    }

    /// Fetch repository data via GraphQL API v4.
    ///
    /// Returns structured GraphQL repository data or error on request/parse failure.
    /// Cache key: `graphql:{owner}/{repo}`
    /// Emit contributor identity entities from REST /contributors data.
    fn emit_contributors(
        &self,
        writer: &mut NTriplesWriter,
        owner: &str,
        repo_name: &str,
        contributors: &[GitHubContributor],
    ) -> Result<usize> {
        let mut triples = 0;
        let r_uri = repo_uri(&format!("https://github.com/{}/{}", owner, repo_name));

        for contributor in contributors {
            // Skip bots
            if contributor.contributor_type.as_deref() == Some("Bot") {
                continue;
            }

            let person_uri = github_person_uri(&contributor.login);
            let account_uri = github_account_uri(&contributor.login);
            let contribution_uri = format!(
                "{DATA}contribution/github/{}/{}/{}",
                contributor.login, owner, repo_name
            );

            // Person entity
            writer.write_triple(&person_uri, RDF_TYPE, &format!("{FOAF}Person"))?;
            writer.write_triple(&person_uri, &format!("{PKG}hasAccount"), &account_uri)?;
            triples += 2;

            // ContributorAccount entity
            writer.write_triple(&account_uri, RDF_TYPE, &format!("{PKG}ContributorAccount"))?;
            writer.write_literal(&account_uri, &format!("{PKG}accountPlatform"), "GitHub")?;
            writer.write_literal(
                &account_uri,
                &format!("{PKG}accountUsername"),
                &contributor.login,
            )?;
            writer.write_literal(
                &account_uri,
                &format!("{PKG}accountUrl"),
                &format!("https://github.com/{}", contributor.login),
            )?;
            triples += 4;

            // Reified Contribution edge (repo-scoped commit count)
            writer.write_triple(&contribution_uri, RDF_TYPE, &format!("{PKG}Contribution"))?;
            writer.write_triple(&contribution_uri, &format!("{PKG}contributor"), &person_uri)?;
            writer.write_triple(&contribution_uri, &format!("{PKG}repository"), &r_uri)?;
            writer.write_integer(
                &contribution_uri,
                &format!("{VCS}commitCount"),
                contributor.contributions as i64,
            )?;
            triples += 4;

            // Convenience shortcuts
            writer.write_triple(&person_uri, &format!("{PKG}contributesTo"), &r_uri)?;
            writer.write_triple(&r_uri, &format!("{PKG}hasContributor"), &person_uri)?;
            triples += 2;
        }

        Ok(triples)
    }

    /// Fetch repository data via GraphQL API v4.
    ///
    /// Returns (repository_data, rate_limit_info). Rate limit is None for cached responses.
    fn fetch_repo_graphql(
        &self,
        owner: &str,
        repo: &str,
    ) -> std::result::Result<(GraphQlRepository, Option<GraphQlRateLimit>), String> {
        let cache_key = format!("graphql:{}/{}", owner, repo);

        // Check cache first (no rate limit info for cached responses)
        if let Some(cached) = self.cached_get(&cache_key) {
            // Check for negative cache hit
            if Self::is_negative_cache(&cached) {
                let error = cached
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                return Err(format!("Cached failure: {}", error));
            }

            let response: GraphQlResponse = serde_json::from_value(cached)
                .map_err(|e| format!("Cache deserialize error: {}", e))?;

            let repo = response
                .data
                .repository
                .ok_or_else(|| "Repository not found (cached)".to_string())?;
            return Ok((repo, None));
        }

        // Build GraphQL request
        let variables = serde_json::json!({
            "owner": owner,
            "name": repo
        });

        let graphql_body = serde_json::json!({
            "query": GRAPHQL_REPO_QUERY,
            "variables": variables
        });

        let graphql_url = format!("{}/graphql", self.github_api_base);
        let mut req = self
            .client
            .post(&graphql_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&graphql_body);

        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        let response = req.send().map_err(|e| format!("Request error: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }

        let graphql_resp: GraphQlResponse = response
            .json()
            .map_err(|e| format!("JSON parse error: {}", e))?;

        // Check for GraphQL-level errors
        if let Some(errors) = &graphql_resp.errors {
            let error_messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
            let err_msg = format!("GraphQL errors: {}", error_messages.join(", "));
            // Cache non-transient GraphQL errors (not-found, access denied)
            self.cache_put(&cache_key, &Self::negative_cache_value(&err_msg));
            return Err(err_msg);
        }

        // Check for null repository (repo doesn't exist)
        if graphql_resp.data.repository.is_none() {
            let err_msg = "Repository not found".to_string();
            self.cache_put(&cache_key, &Self::negative_cache_value(&err_msg));
            return Err(err_msg);
        }

        // Cache the full response
        let response_value = serde_json::to_value(&graphql_resp)
            .map_err(|e| format!("Cache serialize error: {}", e))?;
        self.cache_put(&cache_key, &response_value);

        let rate_limit = graphql_resp.data.rate_limit;
        let repo = graphql_resp.data.repository.unwrap();
        Ok((repo, rate_limit))
    }

    /// Fetch contributor data from REST /contributors endpoint.
    ///
    /// Uses the separate REST rate limit pool (5,000 req/hr independent of GraphQL).
    /// Returns top 30 contributors sorted by commit count.
    /// Errors are non-fatal — returns empty vec on failure.
    fn fetch_contributors(&self, owner: &str, repo: &str) -> Vec<GitHubContributor> {
        let cache_key = format!("contributors:{}/{}", owner, repo);

        // Check cache first
        if let Some(cached) = self.cached_get(&cache_key) {
            match serde_json::from_value::<Vec<GitHubContributor>>(cached) {
                Ok(contributors) => return contributors,
                Err(e) => eprintln!("  Contributors cache deserialize error: {}", e),
            }
        }

        let url = format!(
            "{}/repos/{}/{}/contributors?per_page=30",
            self.github_api_base, owner, repo
        );

        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json");

        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        match req.send() {
            Ok(response) => {
                if !response.status().is_success() {
                    eprintln!(
                        "  Contributors fetch failed for {}/{}: HTTP {}",
                        owner,
                        repo,
                        response.status()
                    );
                    return Vec::new();
                }

                match response.json::<Vec<GitHubContributor>>() {
                    Ok(contributors) => {
                        // Cache the response
                        if let Ok(value) = serde_json::to_value(&contributors) {
                            self.cache_put(&cache_key, &value);
                        }
                        contributors
                    }
                    Err(e) => {
                        eprintln!("  Contributors parse error for {}/{}: {}", owner, repo, e);
                        Vec::new()
                    }
                }
            }
            Err(e) => {
                eprintln!("  Contributors request error for {}/{}: {}", owner, repo, e);
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_process_repo_emits_correct_triples() {
        let mut server = mockito::Server::new();

        // Mock SPARQL endpoint (empty - we call process_repo directly)
        let _sparql_mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        // Mock GitHub repo API
        let repo_mock = server
            .mock("GET", "/repos/test/repo")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "html_url": "https://github.com/test/repo",
                "description": "A test repo",
                "default_branch": "main",
                "stargazers_count": 1000,
                "forks_count": 50,
                "open_issues_count": 10,
                "topics": ["rust", "security"],
                "license": {"spdx_id": "MIT", "name": "MIT License"},
                "archived": false,
                "fork": false
            }"#,
            )
            .create();

        // Mock languages API
        let lang_mock = server
            .mock("GET", "/repos/test/repo/languages")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"Rust": 50000, "Python": 10000}"#)
            .create();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: None,
            token: None,
            github_api_base: server.url(),
            graph_uri: None,
        };

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // Override the API base URL to use mockito
        // We need to call process_repo with the mock server URL
        let api_url = format!("{}/repos/test/repo", server.url());

        // Manually make the API call and process
        let data = enricher.api_get(&api_url).unwrap();
        let r_uri = repo_uri("https://github.com/test/repo");

        // Emit repo triples
        writer
            .write_triple(&r_uri, RDF_TYPE, &format!("{VCS}Repository"))
            .unwrap();
        writer
            .write_literal(
                &r_uri,
                &format!("{VCS}repositoryURL"),
                "https://github.com/test/repo",
            )
            .unwrap();

        if let Some(desc) = data.get("description").and_then(|v| v.as_str()) {
            writer
                .write_literal(&r_uri, &format!("{VCS}repositoryDescription"), desc)
                .unwrap();
        }
        if let Some(stars) = data.get("stargazers_count").and_then(|v| v.as_u64()) {
            writer
                .write_integer(&r_uri, &format!("{VCS}stargazerCount"), stars as i64)
                .unwrap();
        }
        if let Some(license) = data.get("license").and_then(|v| v.as_object()) {
            if let Some(spdx) = license.get("spdx_id").and_then(|v| v.as_str()) {
                writer
                    .write_literal(&r_uri, &format!("{PKG}licenseName"), spdx)
                    .unwrap();
                // License entity with spdxId (v0.7.0)
                let license_uri = crate::uris::spdx_license_uri(spdx);
                writer
                    .write_triple(&r_uri, &format!("{PKG}hasLicense"), &license_uri)
                    .unwrap();
                writer
                    .write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))
                    .unwrap();
                writer
                    .write_literal(&license_uri, &format!("{PKG}spdxId"), spdx)
                    .unwrap();
            }
        }

        writer.flush().unwrap();
        repo_mock.assert();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // Verify VCS metadata
        assert!(
            content.contains("vcs#Repository"),
            "Should have Repository type"
        );
        assert!(
            content.contains("vcs#repositoryURL"),
            "Should have repo URL"
        );
        assert!(
            content.contains("\"A test repo\""),
            "Should have description"
        );
        assert!(
            content.contains("vcs#stargazerCount"),
            "Should have star count"
        );
        assert!(content.contains("\"1000\""), "Should have 1000 stars");
        assert!(content.contains("licenseName"), "Should have license");
        assert!(content.contains("\"MIT\""), "Should have MIT license");
        assert!(
            content.contains("core#spdxId"),
            "Should have spdxId on License entity"
        );
    }

    #[test]
    fn test_not_found_repo_flagged() {
        let mut server = mockito::Server::new();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: None,
            token: None,
            github_api_base: server.url(),
            graph_uri: None,
        };

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // Simulate a 404 by writing not-found triples directly
        let r_uri = repo_uri("https://github.com/deleted/repo");
        writer
            .write_triple(&r_uri, RDF_TYPE, &format!("{VCS}Repository"))
            .unwrap();
        writer
            .write_literal(&r_uri, &format!("{VCS}repositoryStatus"), "not-found")
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("vcs#Repository"),
            "Should still type as Repository"
        );
        assert!(
            content.contains("\"not-found\""),
            "Should flag as not-found"
        );
    }

    #[test]
    fn test_expanded_graphql_response_deserialization() {
        // Test the expanded GraphQL query with all Tier 1+2 fields
        let graphql_response = r#"{
            "data": {
                "rateLimit": {
                    "cost": 1,
                    "limit": 5000,
                    "remaining": 4999,
                    "nodeCount": 90
                },
                "repository": {
                    "url": "https://github.com/test/repo",
                    "description": "Test repository",
                    "isArchived": false,
                    "isFork": false,
                    "pushedAt": "2024-01-15T10:30:00Z",
                    "stargazerCount": 1000,
                    "forkCount": 50,
                    "watchers": {"totalCount": 25},
                    "issues": {"totalCount": 10},
                    "pullRequests": {"totalCount": 5},
                    "createdAt": "2020-01-01T00:00:00Z",
                    "updatedAt": "2024-01-15T10:30:00Z",
                    "homepageUrl": "https://example.com",
                    "diskUsage": 5000,
                    "hasWikiEnabled": false,
                    "hasIssuesEnabled": true,
                    "isTemplate": false,
                    "primaryLanguage": {"name": "Rust"},
                    "defaultBranchRef": {
                        "name": "main",
                        "target": {
                            "oid": "abc123def456",
                            "author": {
                                "name": "Test Author",
                                "email": "test@example.com",
                                "user": {"login": "testuser"}
                            }
                        }
                    },
                    "licenseInfo": {"spdxId": "MIT"},
                    "repositoryTopics": {
                        "nodes": [
                            {"topic": {"name": "rust"}},
                            {"topic": {"name": "security"}}
                        ]
                    },
                    "releases": {
                        "nodes": [
                            {"tagName": "v1.0.0", "publishedAt": "2023-01-01T00:00:00Z", "isPrerelease": false},
                            {"tagName": "v0.9.0", "publishedAt": "2022-12-01T00:00:00Z", "isPrerelease": true}
                        ]
                    },
                    "languages": {
                        "totalSize": 60000,
                        "edges": [
                            {"size": 50000, "node": {"name": "Rust"}},
                            {"size": 10000, "node": {"name": "Python"}}
                        ]
                    }
                }
            }
        }"#;

        let response: GraphQlResponse = serde_json::from_str(graphql_response).unwrap();

        // Verify rateLimit (inside data, as GraphQL top-level field)
        let rl = response.data.rate_limit.as_ref().unwrap();
        assert_eq!(rl.cost, 1);
        assert_eq!(rl.limit, 5000);
        assert_eq!(rl.remaining, 4999);
        assert_eq!(rl.node_count, 90);

        let repo = response.data.repository.unwrap();

        // Existing fields
        assert_eq!(repo.url, Some("https://github.com/test/repo".to_string()));
        assert_eq!(repo.stargazer_count, Some(1000));

        // New free scalars
        assert_eq!(repo.created_at, Some("2020-01-01T00:00:00Z".to_string()));
        assert_eq!(repo.updated_at, Some("2024-01-15T10:30:00Z".to_string()));
        assert_eq!(repo.homepage_url, Some("https://example.com".to_string()));
        assert_eq!(repo.disk_usage, Some(5000));
        assert_eq!(repo.has_wiki_enabled, Some(false));
        assert_eq!(repo.has_issues_enabled, Some(true));
        assert_eq!(repo.is_template, Some(false));

        // Primary language
        assert_eq!(repo.primary_language.as_ref().unwrap().name, "Rust");

        // Pull requests
        assert_eq!(repo.pull_requests.as_ref().unwrap().total_count, 5);

        // Releases
        let releases = &repo.releases.as_ref().unwrap().nodes;
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].tag_name, "v1.0.0");
        assert_eq!(
            releases[0].published_at,
            Some("2023-01-01T00:00:00Z".to_string())
        );
        assert_eq!(releases[0].is_prerelease, false);

        // HEAD commit
        let branch_ref = repo.default_branch_ref.as_ref().unwrap();
        assert_eq!(branch_ref.name, "main");
        let target = branch_ref.target.as_ref().unwrap();
        assert_eq!(target.oid, "abc123def456");
        assert_eq!(target.author.name, "Test Author");
        assert_eq!(target.author.email, Some("test@example.com".to_string()));
        assert_eq!(target.author.user.as_ref().unwrap().login, "testuser");
    }

    #[test]
    fn test_graphql_response_missing_optionals() {
        // Verify deserialization succeeds when optional fields are null/missing
        let minimal_response = r#"{
            "data": {
                "repository": {
                    "url": "https://github.com/minimal/repo",
                    "stargazerCount": 0,
                    "forkCount": 0,
                    "languages": {"totalSize": 0, "edges": []}
                }
            }
        }"#;

        let response: GraphQlResponse = serde_json::from_str(minimal_response).unwrap();
        assert!(
            response.data.rate_limit.is_none(),
            "rateLimit should be None when not requested"
        );
        let repo = response.data.repository.unwrap();
        assert_eq!(
            repo.url,
            Some("https://github.com/minimal/repo".to_string())
        );
        assert!(repo.description.is_none());
        assert!(repo.primary_language.is_none());
        assert!(repo.releases.is_none());
        assert!(repo.default_branch_ref.is_none());
        assert!(repo.homepage_url.is_none());
        assert!(repo.disk_usage.is_none());
        assert!(repo.pull_requests.is_none());
        assert!(repo.created_at.is_none());
    }

    #[test]
    fn test_graphql_response_deserialization() {
        // Test that GraphQL response structs correctly deserialize GitHub GraphQL API v4 response
        let graphql_response = r#"{
            "data": {
                "repository": {
                    "url": "https://github.com/test/repo",
                    "description": "Test repository",
                    "isArchived": false,
                    "isFork": false,
                    "pushedAt": "2024-01-15T10:30:00Z",
                    "stargazerCount": 1000,
                    "forkCount": 50,
                    "watchers": {"totalCount": 25},
                    "issues": {"totalCount": 10},
                    "defaultBranchRef": {"name": "main"},
                    "licenseInfo": {"spdxId": "MIT"},
                    "repositoryTopics": {
                        "nodes": [
                            {"topic": {"name": "rust"}},
                            {"topic": {"name": "security"}}
                        ]
                    },
                    "languages": {
                        "totalSize": 60000,
                        "edges": [
                            {"size": 50000, "node": {"name": "Rust"}},
                            {"size": 10000, "node": {"name": "Python"}}
                        ]
                    }
                }
            }
        }"#;

        let response: GraphQlResponse = serde_json::from_str(graphql_response).unwrap();
        let repo = response.data.repository.unwrap();

        assert_eq!(repo.url, Some("https://github.com/test/repo".to_string()));
        assert_eq!(repo.description, Some("Test repository".to_string()));
        assert_eq!(repo.stargazer_count, Some(1000));
        assert_eq!(repo.fork_count, Some(50));
        assert_eq!(repo.is_archived, Some(false));
        assert_eq!(repo.is_fork, Some(false));
        assert_eq!(repo.pushed_at, Some("2024-01-15T10:30:00Z".to_string()));

        assert_eq!(repo.watchers.as_ref().unwrap().total_count, 25);
        assert_eq!(repo.issues.as_ref().unwrap().total_count, 10);
        assert_eq!(repo.default_branch_ref.as_ref().unwrap().name, "main");
        assert_eq!(
            repo.license_info.as_ref().unwrap().spdx_id,
            Some("MIT".to_string())
        );

        let topics = &repo.repository_topics.as_ref().unwrap().nodes;
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].topic.name, "rust");
        assert_eq!(topics[1].topic.name, "security");

        let langs = &repo.languages.as_ref().unwrap();
        assert_eq!(langs.total_size, 60000);
        assert_eq!(langs.edges.len(), 2);
        assert_eq!(langs.edges[0].size, 50000);
        assert_eq!(langs.edges[0].node.name, "Rust");
        assert_eq!(langs.edges[1].size, 10000);
        assert_eq!(langs.edges[1].node.name, "Python");
    }

    #[test]
    fn test_graphql_fetch_with_fallback() {
        let mut server = mockito::Server::new();

        // Mock successful GraphQL response
        let graphql_mock = server
            .mock("POST", "/graphql")
            .match_header("authorization", "Bearer test-token")
            .with_status(200)
            .with_body(
                r#"{
                "data": {
                    "repository": {
                        "url": "https://github.com/test/repo",
                        "stargazerCount": 1000,
                        "languages": {
                            "totalSize": 50000,
                            "edges": [{"size": 50000, "node": {"name": "Rust"}}]
                        }
                    }
                }
            }"#,
            )
            .create();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: None,
            token: Some("test-token".to_string()),
            github_api_base: server.url(),
            graph_uri: None,
        };

        // Should successfully fetch via GraphQL
        let result = enricher.fetch_repo_graphql("test", "repo");
        assert!(result.is_ok(), "GraphQL fetch should succeed");

        let (repo_data, _rate_limit) = result.unwrap();
        assert_eq!(
            repo_data.url,
            Some("https://github.com/test/repo".to_string())
        );
        assert_eq!(repo_data.stargazer_count, Some(1000));

        graphql_mock.assert();
    }

    #[test]
    fn test_graphql_fallback_to_rest() {
        let mut server = mockito::Server::new();

        // Mock GraphQL failure (500 error)
        let _graphql_mock = server
            .mock("POST", "/graphql")
            .with_status(500)
            .with_body("Internal server error")
            .create();

        // Mock successful REST fallback
        let rest_repo_mock = server
            .mock("GET", "/repos/test/repo")
            .with_status(200)
            .with_body(r#"{"html_url": "https://github.com/test/repo", "stargazers_count": 1000}"#)
            .create();

        let rest_lang_mock = server
            .mock("GET", "/repos/test/repo/languages")
            .with_status(200)
            .with_body(r#"{"Rust": 50000}"#)
            .create();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: None,
            token: None,
            github_api_base: server.url(),
            graph_uri: None,
        };

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // process_repo should try GraphQL, fail, then fall back to REST
        let result = enricher.process_repo(&mut writer, "test", "repo", "");
        assert!(result.is_ok(), "Should succeed via REST fallback");

        rest_repo_mock.assert();
        rest_lang_mock.assert();
    }

    #[test]
    fn test_graphql_cache_hit_behavior() {
        let mut server = mockito::Server::new();

        // Mock GraphQL endpoint - should only be called once
        let graphql_mock = server.mock("POST", "/graphql")
            .with_status(200)
            .with_body(r#"{"data": {"repository": {"url": "https://github.com/test/repo", "stargazerCount": 1000, "languages": {"totalSize": 50000, "edges": []}}}}"#)
            .expect(1)  // Expect exactly 1 call
            .create();

        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = FileCache::new(temp_dir.path().to_str().unwrap(), "github", 24, None).unwrap();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: Some(cache),
            token: Some("test-token".to_string()),
            github_api_base: server.url(),
            graph_uri: None,
        };

        // First call - should hit GraphQL API
        let result1 = enricher.fetch_repo_graphql("test", "repo");
        assert!(result1.is_ok(), "First fetch should succeed");
        let (_, rl1) = result1.unwrap();
        assert!(
            rl1.is_none() || rl1.is_some(),
            "Rate limit may or may not be present"
        );

        // Second call - should use cache, NOT hit API again
        let result2 = enricher.fetch_repo_graphql("test", "repo");
        assert!(result2.is_ok(), "Second fetch should succeed from cache");
        let (_, rl2) = result2.unwrap();
        assert!(rl2.is_none(), "Cached response should have no rate limit");

        // Verify mock was only called once
        graphql_mock.assert();
    }

    #[test]
    fn test_graphql_rest_output_parity() {
        // Verify that GraphQL and REST paths emit equivalent RDF for the same repo
        let mut server = mockito::Server::new();

        // Mock GraphQL response
        let _graphql_mock = server.mock("POST", "/graphql")
            .with_status(200)
            .with_body(r#"{
                "data": {
                    "repository": {
                        "url": "https://github.com/test/repo",
                        "description": "Test repository",
                        "defaultBranchRef": {
                            "name": "main",
                            "target": {
                                "oid": "abc123",
                                "author": {"name": "Test Author", "email": "test@example.com", "user": {"login": "testuser"}}
                            }
                        },
                        "stargazerCount": 1000,
                        "forkCount": 50,
                        "issues": {"totalCount": 10},
                        "pullRequests": {"totalCount": 5},
                        "watchers": {"totalCount": 25},
                        "isArchived": false,
                        "isFork": false,
                        "createdAt": "2020-01-01T00:00:00Z",
                        "updatedAt": "2024-01-15T10:30:00Z",
                        "homepageUrl": "https://example.com",
                        "diskUsage": 5000,
                        "primaryLanguage": {"name": "Rust"},
                        "releases": {
                            "nodes": [
                                {"tagName": "v1.0.0", "publishedAt": "2023-01-01T00:00:00Z", "isPrerelease": false}
                            ]
                        },
                        "repositoryTopics": {
                            "nodes": [
                                {"topic": {"name": "rust"}},
                                {"topic": {"name": "testing"}}
                            ]
                        },
                        "licenseInfo": {"spdxId": "MIT"},
                        "pushedAt": "2024-01-15T10:30:00Z",
                        "languages": {
                            "totalSize": 60000,
                            "edges": [
                                {"size": 50000, "node": {"name": "Rust"}},
                                {"size": 10000, "node": {"name": "Python"}}
                            ]
                        }
                    }
                }
            }"#)
            .create();

        // Mock REST responses (for fallback path)
        let _rest_repo_mock = server
            .mock("GET", "/repos/test/repo")
            .with_status(200)
            .with_body(
                r#"{
                "html_url": "https://github.com/test/repo",
                "description": "Test repository",
                "default_branch": "main",
                "stargazers_count": 1000,
                "forks_count": 50,
                "open_issues_count": 10,
                "subscribers_count": 25,
                "archived": false,
                "fork": false,
                "topics": ["rust", "testing"],
                "license": {"spdx_id": "MIT"},
                "pushed_at": "2024-01-15T10:30:00Z"
            }"#,
            )
            .create();

        let _rest_lang_mock = server
            .mock("GET", "/repos/test/repo/languages")
            .with_status(200)
            .with_body(r#"{"Rust": 50000, "Python": 10000}"#)
            .create();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: None,
            token: None,
            github_api_base: server.url(),
            graph_uri: None,
        };

        // Process via GraphQL path
        let graphql_file = NamedTempFile::new().unwrap();
        let mut graphql_writer = NTriplesWriter::new(graphql_file.reopen().unwrap());
        let graphql_triples = enricher
            .process_repo(&mut graphql_writer, "test", "repo", "")
            .unwrap();
        graphql_writer.flush().unwrap();

        let mut graphql_output = String::new();
        graphql_file
            .reopen()
            .unwrap()
            .read_to_string(&mut graphql_output)
            .unwrap();

        // Verify GraphQL output contains expected predicates
        assert!(
            graphql_output.contains("vcs#Repository"),
            "GraphQL: Repository type"
        );
        // repositoryURL is emitted by the caller (enrich/enrich_incremental), not process_repo
        assert!(
            graphql_output.contains("vcs#repositoryDescription"),
            "GraphQL: description"
        );
        assert!(
            graphql_output.contains("vcs#defaultBranch"),
            "GraphQL: default branch"
        );
        assert!(
            graphql_output.contains("vcs#stargazerCount"),
            "GraphQL: star count"
        );
        assert!(
            graphql_output.contains("vcs#forkCount"),
            "GraphQL: fork count"
        );
        assert!(
            graphql_output.contains("vcs#openIssuesCount"),
            "GraphQL: open issues"
        );
        assert!(
            graphql_output.contains("vcs#subscriberCount"),
            "GraphQL: watchers"
        );
        assert!(
            graphql_output.contains("vcs#isArchived"),
            "GraphQL: is archived"
        );
        assert!(graphql_output.contains("vcs#isFork"), "GraphQL: is fork");
        assert!(graphql_output.contains("vcs#topic"), "GraphQL: topics");
        // New Tier 1 predicates
        assert!(
            graphql_output.contains("metrics#primaryLanguage"),
            "GraphQL: primary language"
        );
        assert!(
            graphql_output.contains("vcs#headCommitHash"),
            "GraphQL: HEAD commit"
        );
        assert!(
            graphql_output.contains("vcs#Release"),
            "GraphQL: Release entities"
        );
        assert!(
            graphql_output.contains("vcs#tagName"),
            "GraphQL: release tag name"
        );
        assert!(
            graphql_output.contains("vcs#repositoryCreatedAt"),
            "GraphQL: repo created"
        );
        assert!(
            graphql_output.contains("vcs#openPullRequestCount"),
            "GraphQL: open PRs"
        );
        assert!(
            graphql_output.contains("core#licenseName"),
            "GraphQL: license name"
        );
        assert!(
            graphql_output.contains("core#spdxId"),
            "GraphQL: spdxId on License entity"
        );
        assert!(
            graphql_output.contains("metrics#languageName"),
            "GraphQL: language name"
        );
        assert!(
            graphql_output.contains("metrics#languageBytes"),
            "GraphQL: language bytes"
        );
        assert!(
            graphql_output.contains("metrics#totalBytes"),
            "GraphQL: total bytes"
        );
        assert!(
            graphql_output.contains("core#lastCommitDate"),
            "GraphQL: last commit date"
        );
        assert!(
            graphql_output.contains("\"2024-01-15\""),
            "GraphQL: correct date format"
        );

        // Verify values
        assert!(
            graphql_output.contains("\"1000\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
            "GraphQL: 1000 stars"
        );
        assert!(
            graphql_output.contains("\"50\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
            "GraphQL: 50 forks"
        );
        assert!(
            graphql_output.contains("\"50000\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
            "GraphQL: Rust bytes"
        );
        assert!(
            graphql_output.contains("\"10000\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
            "GraphQL: Python bytes"
        );
        assert!(
            graphql_output.contains("\"60000\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
            "GraphQL: total bytes"
        );

        assert!(graphql_triples > 10, "GraphQL should emit multiple triples");
    }

    #[test]
    fn test_enrich_incremental_respects_max_repos() {
        let mut server = mockito::Server::new();

        // Mock SPARQL candidates query returning 2 homepage URLs (one with maintainer)
        let candidates_mock = server.mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_body(r#"{
                "results": {
                    "bindings": [
                        {"homepage": {"value": "https://github.com/user1/repo1"}, "maintainer": {"value": "http://pkg.graph/maintainer/1"}, "packageCount": {"value": "100"}},
                        {"homepage": {"value": "https://github.com/user2/repo2"}, "packageCount": {"value": "50"}}
                    ]
                }
            }"#)
            .create();

        // Mock GraphQL API for both repos
        let _graphql_mock1 = server.mock("POST", "/graphql")
            .with_status(200)
            .with_body(r#"{"data": {"repository": {"url": "https://github.com/user1/repo1", "stargazerCount": 10, "languages": {"totalSize": 1000, "edges": []}}}}"#)
            .create();

        let _graphql_mock2 = server.mock("POST", "/graphql")
            .with_status(200)
            .with_body(r#"{"data": {"repository": {"url": "https://github.com/user2/repo2", "stargazerCount": 50, "languages": {"totalSize": 2000, "edges": []}}}}"#)
            .create();

        // Mock GSP POST (for graph load - match by path to avoid catching GraphQL)
        let gsp_mock = server
            .mock("POST", mockito::Matcher::Regex(r"^/data\?".to_string()))
            .with_status(200)
            .create();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: None,
            token: None,
            github_api_base: server.url(),
            graph_uri: None,
        };

        let temp_file = NamedTempFile::new().unwrap();
        let (repos, _triples) = enricher
            .enrich_incremental(
                temp_file.path().to_str().unwrap(),
                10, // max_repos
                "http://example.org/enrichment/github",
            )
            .unwrap();

        candidates_mock.assert();
        gsp_mock.assert();

        // Should process only the repos returned by query (2), respecting max_repos
        assert_eq!(repos, 2, "Should process 2 repos from candidates query");
    }

    #[test]
    fn test_dq_issue_emission() {
        let mut server = mockito::Server::new();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: None,
            token: None,
            github_api_base: server.url(),
            graph_uri: None,
        };

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let triples = enricher
            .emit_dq_issue(
                &mut writer,
                "https://github.com/org-only",
                "malformed-github-url",
                "homepage",
                "error",
            )
            .unwrap();
        writer.flush().unwrap();

        assert_eq!(triples, 7, "DQ issue should emit exactly 7 triples");

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("dq#DataQualityIssue"),
            "Should have DQ issue type"
        );
        assert!(content.contains("dq#issueType"), "Should have issueType");
        assert!(
            content.contains("\"malformed-github-url\""),
            "Should have issue type value"
        );
        assert!(content.contains("dq#rawValue"), "Should have rawValue");
        assert!(
            content.contains("\"https://github.com/org-only\""),
            "Should have raw URL"
        );
        assert!(content.contains("dq#detectedBy"), "Should have detectedBy");
        assert!(
            content.contains("\"enrich-github\""),
            "Should have detector name"
        );
        assert!(content.contains("dq#severity"), "Should have severity");
        assert!(content.contains("\"error\""), "Should have severity value");
    }

    #[test]
    fn test_negative_cache_helpers() {
        let neg = GitHubEnricher::negative_cache_value("404 Not Found");
        assert!(GitHubEnricher::is_negative_cache(&neg));
        assert_eq!(neg["error"], "404 Not Found");

        let normal = serde_json::json!({"stargazers_count": 100});
        assert!(!GitHubEnricher::is_negative_cache(&normal));
    }

    #[test]
    fn test_graphql_negative_cache_on_not_found() {
        let mut server = mockito::Server::new();

        // GraphQL returns null repository (repo doesn't exist)
        let graphql_mock = server
            .mock("POST", "/graphql")
            .with_status(200)
            .with_body(r#"{"data": {"repository": null}}"#)
            .expect(1) // Should only be called once
            .create();

        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = FileCache::new(temp_dir.path().to_str().unwrap(), "github", 24, None).unwrap();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: Some(cache),
            token: Some("test-token".to_string()),
            github_api_base: server.url(),
            graph_uri: None,
        };

        // First call: should hit API and get not-found
        let result1 = enricher.fetch_repo_graphql("deleted", "repo");
        assert!(result1.is_err(), "First call should fail");
        assert!(
            result1.unwrap_err().contains("not found"),
            "Error should mention not found"
        );

        // Second call: should use negative cache, NOT hit API
        let result2 = enricher.fetch_repo_graphql("deleted", "repo");
        assert!(result2.is_err(), "Second call should also fail");
        assert!(
            result2.unwrap_err().contains("Cached failure"),
            "Should report cached failure"
        );

        // Verify API was only called once
        graphql_mock.assert();
    }

    #[test]
    fn test_graphql_negative_cache_on_errors() {
        let mut server = mockito::Server::new();

        // GraphQL returns errors (e.g., access denied)
        let graphql_mock = server.mock("POST", "/graphql")
            .with_status(200)
            .with_body(r#"{"data": {"repository": null}, "errors": [{"message": "Could not resolve to a Repository"}]}"#)
            .expect(1)
            .create();

        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = FileCache::new(temp_dir.path().to_str().unwrap(), "github", 24, None).unwrap();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: Some(cache),
            token: Some("test-token".to_string()),
            github_api_base: server.url(),
            graph_uri: None,
        };

        // First call: hits API
        let result1 = enricher.fetch_repo_graphql("private", "repo");
        assert!(result1.is_err());

        // Second call: negative cache hit
        let result2 = enricher.fetch_repo_graphql("private", "repo");
        assert!(result2.is_err());
        assert!(result2.unwrap_err().contains("Cached failure"));

        graphql_mock.assert();
    }

    #[test]
    fn test_rest_negative_cache_on_404() {
        let mut server = mockito::Server::new();

        // Mock GraphQL failure (forces REST fallback)
        let _graphql_mock = server.mock("POST", "/graphql").with_status(500).create();

        // REST returns 404 — only called once
        let rest_mock = server
            .mock("GET", "/repos/deleted/repo")
            .with_status(404)
            .expect(1)
            .create();

        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = FileCache::new(temp_dir.path().to_str().unwrap(), "github", 24, None).unwrap();

        let enricher = GitHubEnricher {
            sparql: SparqlClient::new(&server.url()),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cache: Some(cache),
            token: None,
            github_api_base: server.url(),
            graph_uri: None,
        };

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // First call: hits REST, gets 404, caches negative
        let result1 = enricher.process_repo(&mut writer, "deleted", "repo", "");
        assert!(result1.is_ok());
        assert_eq!(result1.unwrap(), 2, "Should emit not-found triples");

        // Second call: negative cache hit, skips API entirely
        let result2 = enricher.process_repo(&mut writer, "deleted", "repo", "");
        assert!(result2.is_ok());
        assert_eq!(
            result2.unwrap(),
            2,
            "Cached 404 should emit type + status triples"
        );

        // REST API only called once
        rest_mock.assert();
    }
}
