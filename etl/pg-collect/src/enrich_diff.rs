use crate::cache::FileCache;
use crate::enricher::{github_owner_repo, rate_limit};
use crate::ntriples::NTriplesWriter;
use crate::sparql::SparqlClient;
use crate::uris::*;
use reqwest::blocking::Client;
use serde::Deserialize;
use percent_encoding::percent_decode_str;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct DiffEnricher {
    sparql: SparqlClient,
    client: Client,
    cache: Option<FileCache>,
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubTag {
    name: String,
    commit: GitHubTagCommit,
}

#[derive(Debug, Deserialize)]
struct GitHubTagCommit {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GitHubCompare {
    #[allow(dead_code)]
    total_commits: usize,
    files: Option<Vec<GitHubCompareFile>>,
    #[serde(default)]
    #[allow(dead_code)]
    ahead_by: usize,
}

#[derive(Debug, Deserialize)]
struct GitHubCompareFile {
    additions: usize,
    deletions: usize,
}

impl DiffEnricher {
    pub fn new(
        endpoint: &str,
        github_token: Option<String>,
        cache_dir: Option<&str>,
    ) -> Self {
        let sparql = SparqlClient::new(endpoint);
        let client = crate::enricher::default_http_client();

        let cache = cache_dir.map(|dir| {
            FileCache::new(dir, "diff", 168, None)
                .expect("Failed to create cache")
        });

        Self {
            sparql,
            client,
            cache,
            token: github_token,
        }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        let repos = self.query_maven_repos()?;
        eprintln!(
            "Found {} unique Maven repos with upstreamRepository",
            repos.len()
        );

        let mut total_repos = 0;
        let mut total_triples = 0;

        for (repo_uri, clone_url) in &repos {
            let normalized = clone_url
                .replace("git://github.com/", "https://github.com/")
                .replace("ssh://git@github.com/", "https://github.com/");
            let (owner, repo_name) = match github_owner_repo(&normalized)
                .or_else(|| Self::extract_github_from_repo_uri(repo_uri))
            {
                Some(pair) => pair,
                None => continue,
            };

            total_repos += 1;
            if total_repos % 10 == 0 {
                eprintln!(
                    "Progress: {}/{} repos processed",
                    total_repos,
                    repos.len()
                );
            }

            match self.process_repo(&mut writer, repo_uri, &owner, &repo_name) {
                Ok(triples) => total_triples += triples,
                Err(e) => eprintln!("  Error processing {}/{}: {}", owner, repo_name, e),
            }
        }

        writer.flush()?;
        Ok((total_repos, total_triples))
    }

    fn query_maven_repos(&self) -> Result<Vec<(String, String)>> {
        let sparql = "\
            PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
            PREFIX vcs: <https://purl.org/packagegraph/ontology/vcs#>\n\
            PREFIX maven: <https://purl.org/packagegraph/ontology/maven#>\n\
            SELECT DISTINCT ?repo ?cloneUrl WHERE {\n\
              ?identity a pkg:PackageIdentity ;\n\
                        pkg:upstreamRepository ?repo .\n\
              ?pkg pkg:isVersionOf ?identity ;\n\
                   a maven:MavenArtifact .\n\
              ?repo vcs:cloneUrl ?cloneUrl .\n\
              FILTER(CONTAINS(STR(?cloneUrl), \"github.com\") || CONTAINS(STR(?repo), \"github.com\"))\n\
            }";

        let bindings = self
            .sparql
            .query(sparql)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let repos: Vec<(String, String)> = bindings
            .into_iter()
            .filter_map(|b| {
                Some((b.get("repo")?.clone(), b.get("cloneUrl")?.clone()))
            })
            .collect();

        // Deduplicate by repo URI
        let mut seen = HashSet::new();
        let repos = repos
            .into_iter()
            .filter(|(repo, _)| seen.insert(repo.clone()))
            .collect();

        Ok(repos)
    }

    fn query_versions_for_repo(&self, repo_uri: &str) -> Result<HashMap<String, Vec<String>>> {
        let sparql = format!("\
            PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
            SELECT ?versionString ?versionUri WHERE {{\n\
              ?identity pkg:upstreamRepository <{repo_uri}> .\n\
              ?pkg pkg:isVersionOf ?identity ;\n\
                   pkg:hasVersion ?versionUri .\n\
              ?versionUri pkg:versionString ?versionString .\n\
            }}");

        let bindings = self.sparql.query(&sparql)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let mut versions: HashMap<String, Vec<String>> = HashMap::new();
        for b in bindings {
            if let (Some(vs), Some(vu)) = (b.get("versionString"), b.get("versionUri")) {
                versions.entry(vs.clone()).or_default().push(vu.clone());
            }
        }
        Ok(versions)
    }

    fn process_repo(
        &self,
        writer: &mut NTriplesWriter,
        repo_uri: &str,
        owner: &str,
        repo_name: &str,
    ) -> Result<usize> {
        let tags = self.fetch_tags(owner, repo_name)?;
        if tags.len() < 2 {
            return Ok(0);
        }

        let versions = self.query_versions_for_repo(repo_uri)?;
        let mut triples = 0;

        // Process consecutive tag pairs
        for window in tags.windows(2) {
            let older = &window[0];
            let newer = &window[1];

            // Build URIs
            let older_release_uri = format!(
                "{DATA}release/github/{}/{}/{}",
                owner, repo_name, older.name
            );
            let newer_release_uri = format!(
                "{DATA}release/github/{}/{}/{}",
                owner, repo_name, newer.name
            );
            let older_commit_uri = format!("{DATA}commit/{}", &older.commit.sha);
            let newer_commit_uri = format!("{DATA}commit/{}", &newer.commit.sha);

            // Newer release
            writer.write_triple(&newer_release_uri, RDF_TYPE, &format!("{VCS}Release"))?;
            writer.write_triple(&newer_release_uri, RDF_TYPE, &format!("{VCS}Tag"))?;
            writer.write_literal(
                &newer_release_uri,
                &format!("{VCS}tagName"),
                &newer.name,
            )?;
            writer.write_triple(
                &newer_release_uri,
                &format!("{VCS}pointsTo"),
                &newer_commit_uri,
            )?;
            writer.write_triple(
                &newer_release_uri,
                &format!("{VCS}previousRelease"),
                &older_release_uri,
            )?;
            triples += 5;

            let newer_ver = newer.name.strip_prefix('v').unwrap_or(&newer.name);
            if let Some(ver_uris) = versions.get(newer_ver) {
                for ver_uri in ver_uris {
                    writer.write_triple(
                        &newer_release_uri,
                        &format!("{VCS}correspondingPackageVersion"),
                        ver_uri,
                    )?;
                    triples += 1;
                }
            }

            // Older release
            writer.write_triple(&older_release_uri, RDF_TYPE, &format!("{VCS}Release"))?;
            writer.write_triple(&older_release_uri, RDF_TYPE, &format!("{VCS}Tag"))?;
            writer.write_literal(
                &older_release_uri,
                &format!("{VCS}tagName"),
                &older.name,
            )?;
            writer.write_triple(
                &older_release_uri,
                &format!("{VCS}pointsTo"),
                &older_commit_uri,
            )?;
            triples += 4;

            let older_ver = older.name.strip_prefix('v').unwrap_or(&older.name);
            if let Some(ver_uris) = versions.get(older_ver) {
                for ver_uri in ver_uris {
                    writer.write_triple(
                        &older_release_uri,
                        &format!("{VCS}correspondingPackageVersion"),
                        ver_uri,
                    )?;
                    triples += 1;
                }
            }

            // Commit entities
            writer.write_triple(&older_commit_uri, RDF_TYPE, &format!("{VCS}Commit"))?;
            writer.write_literal(
                &older_commit_uri,
                &format!("{VCS}commitHash"),
                &older.commit.sha,
            )?;
            writer.write_triple(&newer_commit_uri, RDF_TYPE, &format!("{VCS}Commit"))?;
            writer.write_literal(
                &newer_commit_uri,
                &format!("{VCS}commitHash"),
                &newer.commit.sha,
            )?;
            triples += 4;

            // Fetch compare stats
            match self.fetch_compare(owner, repo_name, &older.name, &newer.name) {
                Ok(compare) => {
                    let diff_uri = format!(
                        "{DATA}diff/github/{}/{}/{}...{}",
                        owner, repo_name, older.name, newer.name
                    );
                    let compare_url = format!(
                        "https://github.com/{}/{}/compare/{}...{}",
                        owner, repo_name, older.name, newer.name
                    );

                    writer.write_triple(&diff_uri, RDF_TYPE, &format!("{VCS}Diff"))?;
                    writer.write_triple(
                        &diff_uri,
                        &format!("{VCS}diffFrom"),
                        &older_commit_uri,
                    )?;
                    writer.write_triple(
                        &diff_uri,
                        &format!("{VCS}diffTo"),
                        &newer_commit_uri,
                    )?;
                    writer.write_typed_literal(
                        &diff_uri,
                        &format!("{VCS}diffUrl"),
                        &compare_url,
                        "http://www.w3.org/2001/XMLSchema#anyURI",
                    )?;
                    triples += 4;

                    let (additions, deletions) = compare
                        .files
                        .as_ref()
                        .map(|files| {
                            files
                                .iter()
                                .fold((0usize, 0usize), |(a, d), f| {
                                    (a + f.additions, d + f.deletions)
                                })
                        })
                        .unwrap_or((0, 0));
                    let files_changed =
                        compare.files.as_ref().map(|f| f.len()).unwrap_or(0);

                    writer.write_typed_literal(
                        &diff_uri,
                        &format!("{VCS}linesAdded"),
                        &additions.to_string(),
                        "http://www.w3.org/2001/XMLSchema#int",
                    )?;
                    writer.write_typed_literal(
                        &diff_uri,
                        &format!("{VCS}linesDeleted"),
                        &deletions.to_string(),
                        "http://www.w3.org/2001/XMLSchema#int",
                    )?;
                    writer.write_typed_literal(
                        &diff_uri,
                        &format!("{VCS}filesChanged"),
                        &files_changed.to_string(),
                        "http://www.w3.org/2001/XMLSchema#int",
                    )?;
                    triples += 3;

                    // Link release to diff
                    writer.write_triple(
                        &newer_release_uri,
                        &format!("{VCS}hasDiff"),
                        &diff_uri,
                    )?;
                    triples += 1;
                }
                Err(e) => {
                    eprintln!(
                        "  Compare failed for {}...{}: {}",
                        older.name, newer.name, e
                    );
                }
            }

            rate_limit(Duration::from_millis(500));
        }

        Ok(triples)
    }

    fn fetch_tags(&self, owner: &str, repo: &str) -> Result<Vec<GitHubTag>> {
        let cache_key = format!("tags-{}-{}", owner, repo);

        if let Some(cached) = self.cached_get::<Vec<GitHubTag>>(&cache_key) {
            return Ok(cached);
        }

        let url = format!(
            "https://api.github.com/repos/{}/{}/tags?per_page=30",
            owner, repo
        );
        match self.api_get(&url) {
            Ok(val) => {
                self.cache_put(&cache_key, &val);
                let tags: Vec<GitHubTag> = serde_json::from_value(val).unwrap_or_default();
                Ok(tags)
            }
            Err(e) => {
                eprintln!("  Failed to fetch tags for {}/{}: {}", owner, repo, e);
                Ok(Vec::new())
            }
        }
    }

    fn fetch_compare(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
        head: &str,
    ) -> std::result::Result<GitHubCompare, String> {
        let cache_key = format!("compare-{}-{}-{}...{}", owner, repo, base, head);

        if let Some(val) = self.cache.as_ref().and_then(|c| c.get(&cache_key)) {
            return serde_json::from_value(val).map_err(|e| e.to_string());
        }

        let url = format!(
            "https://api.github.com/repos/{}/{}/compare/{}...{}",
            owner, repo, base, head
        );
        let val = self.api_get(&url)?;
        self.cache_put(&cache_key, &val);
        serde_json::from_value(val).map_err(|e| e.to_string())
    }

    fn api_get(
        &self,
        url: &str,
    ) -> std::result::Result<serde_json::Value, String> {
        let mut req = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json");

        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        let resp = req.send().map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}: {}", resp.status(), url));
        }

        resp.json().map_err(|e| e.to_string())
    }

    fn extract_github_from_repo_uri(repo_uri: &str) -> Option<(String, String)> {
        let decoded = percent_decode_str(repo_uri).decode_utf8().ok()?;
        let path = decoded
            .strip_prefix("https://packagegraph.github.io/d/repo/")?;
        if !path.starts_with("github.com/") {
            return None;
        }
        let gh_path = path.strip_prefix("github.com/")?;
        let parts: Vec<&str> = gh_path.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            Some((parts[0].to_string(), parts[1].to_string()))
        } else {
            None
        }
    }

    fn cached_get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        let val = self.cache.as_ref()?.get(key)?;
        serde_json::from_value(val).ok()
    }

    fn cache_put(&self, key: &str, data: &serde_json::Value) {
        if let Some(ref cache) = self.cache {
            cache.put(key, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_github_owner_repo_for_diff() {
        // Verify the shared github_owner_repo helper works for our use case
        assert_eq!(
            github_owner_repo("https://github.com/spring-projects/spring-framework.git"),
            Some((
                "spring-projects".to_string(),
                "spring-framework".to_string()
            ))
        );
        assert_eq!(
            github_owner_repo("https://github.com/apache/struts"),
            Some(("apache".to_string(), "struts".to_string()))
        );
        assert_eq!(github_owner_repo("https://gitlab.com/foo/bar"), None);
    }

    #[test]
    fn test_extract_github_from_repo_uri() {
        assert_eq!(
            DiffEnricher::extract_github_from_repo_uri(
                "https://packagegraph.github.io/d/repo/github.com%2Fspring-projects%2Fspring-framework"
            ),
            Some(("spring-projects".to_string(), "spring-framework".to_string()))
        );
        assert_eq!(
            DiffEnricher::extract_github_from_repo_uri(
                "https://packagegraph.github.io/d/repo/gitlab.com%2Ffoo%2Fbar"
            ),
            None
        );
    }

    #[test]
    fn test_diff_enricher_creation() {
        let enricher = DiffEnricher::new("http://localhost:3030/test", None, None);
        assert!(enricher.cache.is_none());
        assert!(enricher.token.is_none());
    }

    #[test]
    fn test_diff_enricher_with_token() {
        let enricher = DiffEnricher::new(
            "http://localhost:3030/test",
            Some("ghp_test123".to_string()),
            None,
        );
        assert_eq!(enricher.token, Some("ghp_test123".to_string()));
    }
}
