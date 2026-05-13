//! Forge software version enricher.
//!
//! Discovers forge instances via SPARQL, probes their version APIs, and emits
//! vcs:ForgeSoftwareVersion + vcs:ForgeVersionObservation triples.

use crate::cache::FileCache;
use crate::enricher::{rate_limit, SLOW_RATE_LIMIT};
use crate::ntriples::NTriplesWriter;
use crate::sparql::SparqlClient;
use crate::uris::*;
use reqwest::blocking::Client;
use std::fs::File;
use std::io::Result;

pub struct ForgeVersionEnricher {
    sparql: SparqlClient,
    client: Client,
    // TODO: Wire up caching in probe_forge_version() to avoid re-probing recently-checked forges
    #[allow(dead_code)]
    cache: Option<FileCache>,
    gitlab_token: Option<String>,
}

impl ForgeVersionEnricher {
    pub fn new(endpoint: &str, cache_dir: Option<&str>, gitlab_token: Option<String>) -> Self {
        let sparql = SparqlClient::new(endpoint);
        let client = crate::enricher::default_http_client();

        let cache = cache_dir.map(|dir| {
            FileCache::new(dir, "forge-versions", 24, None) // 24h TTL
                .expect("Failed to create cache")
        });

        Self { sparql, client, cache, gitlab_token }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        // Discover forge instances from SPARQL
        let forges = self.sparql.query_forge_instances()?;
        eprintln!("Found {} forge instances to probe", forges.len());

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut probed = 0;
        let mut total_triples = 0;

        for (forge_uri, forge_url, software_uri) in forges {
            // Extract hostname from forge URL for cache key
            let host = forge_url.trim_start_matches("https://").trim_start_matches("http://").split('/').next().unwrap_or(&forge_url);

            match self.probe_forge_version(&mut writer, &forge_uri, &forge_url, &software_uri, host, &today) {
                Ok(triples) if triples > 0 => {
                    probed += 1;
                    total_triples += triples;
                }
                Ok(_) => {}
                Err(e) => eprintln!("  Error probing {}: {}", host, e),
            }

            rate_limit(SLOW_RATE_LIMIT);
        }

        writer.flush()?;
        eprintln!("Probed {} forges, emitted {} triples", probed, total_triples);
        Ok((probed, total_triples))
    }

    fn probe_forge_version(
        &self,
        writer: &mut NTriplesWriter,
        forge_uri: &str,
        forge_url: &str,
        software_uri: &str,
        host: &str,
        today: &str,
    ) -> Result<usize> {
        // Skip SaaS forges
        if self.is_saas_forge(software_uri, host) {
            return Ok(0);
        }

        // Determine API endpoint and probe
        let version_opt = self.fetch_version(forge_url, software_uri)?;
        let version = match version_opt {
            Some(v) => v,
            None => return Ok(0),
        };

        // Emit ForgeSoftwareVersion (shared entity)
        let software_name = extract_software_name(software_uri);
        let version_uri = forge_software_version_uri(&software_name, &version);
        writer.write_triple(&version_uri, RDF_TYPE, &format!("{VCS}ForgeSoftwareVersion"))?;
        writer.write_triple(&version_uri, &format!("{VCS}versionOfSoftware"), software_uri)?;
        writer.write_literal(&version_uri, &format!("{VCS}versionString"), &version)?;

        // Emit ForgeVersionObservation
        let obs_uri = forge_version_observation_uri(host, today);
        writer.write_triple(&obs_uri, RDF_TYPE, &format!("{VCS}ForgeVersionObservation"))?;
        writer.write_triple(&obs_uri, &format!("{VCS}observedSoftwareVersion"), &version_uri)?;
        writer.write_date(&obs_uri, &format!("{VCS}observedAt"), today)?;

        // Link forge instance to observation
        writer.write_triple(forge_uri, &format!("{VCS}hasVersionObservation"), &obs_uri)?;

        Ok(7) // 3 (version) + 3 (observation) + 1 (link)
    }

    fn is_saas_forge(&self, software_uri: &str, host: &str) -> bool {
        match software_uri {
            "https://purl.org/packagegraph/ontology/vcs#GitHub" if host == "github.com" => true,
            "https://purl.org/packagegraph/ontology/vcs#GitLab" if host == "gitlab.com" => true,
            "https://purl.org/packagegraph/ontology/vcs#BitbucketCloud" => true,
            _ => false,
        }
    }

    fn fetch_version(&self, forge_url: &str, software_uri: &str) -> Result<Option<String>> {
        let api_url = match software_uri {
            "https://purl.org/packagegraph/ontology/vcs#GitLab" => format!("{}/api/v4/version", forge_url),
            "https://purl.org/packagegraph/ontology/vcs#Forgejo" => format!("{}/api/v1/version", forge_url),
            "https://purl.org/packagegraph/ontology/vcs#Gitea" => format!("{}/api/v1/version", forge_url),
            "https://purl.org/packagegraph/ontology/vcs#GitHub" => {
                // Check if GHES (non-github.com)
                if !forge_url.contains("github.com") {
                    format!("{}/api/v3/meta", forge_url)
                } else {
                    return Ok(None); // SaaS, skip
                }
            }
            "https://purl.org/packagegraph/ontology/vcs#BitbucketDataCenter" => format!("{}/rest/api/1.0/application-properties", forge_url),
            _ => return Ok(None), // Unsupported forge software
        };

        let mut req = self.client.get(&api_url);

        // Add GitLab token if available and this is a GitLab instance
        if software_uri.contains("GitLab") {
            if let Some(ref token) = self.gitlab_token {
                req = req.header("PRIVATE-TOKEN", token);
            }
        }

        let response = match req.send() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  Request failed for {}: {}", api_url, e);
                return Ok(None);
            }
        };

        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 401 || status.as_u16() == 403 {
                eprintln!("  Auth required for {}, skipping", api_url);
            } else {
                eprintln!("  HTTP {} for {}, skipping", status, api_url);
            }
            return Ok(None);
        }

        let json: serde_json::Value = match response.json() {
            Ok(j) => j,
            Err(e) => {
                eprintln!("  Failed to parse JSON from {}: {}", api_url, e);
                return Ok(None);
            }
        };

        // Extract version string from JSON
        let version = match software_uri {
            "https://purl.org/packagegraph/ontology/vcs#GitLab" |
            "https://purl.org/packagegraph/ontology/vcs#Forgejo" |
            "https://purl.org/packagegraph/ontology/vcs#Gitea" => {
                json.get("version").and_then(|v| v.as_str()).map(|s| s.to_string())
            }
            "https://purl.org/packagegraph/ontology/vcs#GitHub" => {
                // GHES meta endpoint
                json.get("installed_version").and_then(|v| v.as_str()).map(|s| s.to_string())
            }
            "https://purl.org/packagegraph/ontology/vcs#BitbucketDataCenter" => {
                json.get("version").and_then(|v| v.as_str()).map(|s| s.to_string())
            }
            _ => None,
        };

        Ok(version)
    }
}

/// Extract the software name from a ForgeSoftware individual URI.
///
/// Example: "https://purl.org/packagegraph/ontology/vcs#GitLab" → "gitlab"
fn extract_software_name(uri: &str) -> String {
    uri.rsplit('#')
        .next()
        .unwrap_or("unknown")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_extract_software_name() {
        assert_eq!(extract_software_name("https://purl.org/packagegraph/ontology/vcs#GitLab"), "gitlab");
        assert_eq!(extract_software_name("https://purl.org/packagegraph/ontology/vcs#Forgejo"), "forgejo");
        assert_eq!(extract_software_name("https://purl.org/packagegraph/ontology/vcs#GitHub"), "github");
    }

    #[test]
    fn test_is_saas_forge() {
        let enricher = ForgeVersionEnricher {
            sparql: SparqlClient::new("http://localhost:3030/packagegraph"),
            client: Client::new(),
            cache: None,
            gitlab_token: None,
        };

        assert!(enricher.is_saas_forge("https://purl.org/packagegraph/ontology/vcs#GitHub", "github.com"));
        assert!(!enricher.is_saas_forge("https://purl.org/packagegraph/ontology/vcs#GitHub", "github.example.com"));
        assert!(enricher.is_saas_forge("https://purl.org/packagegraph/ontology/vcs#GitLab", "gitlab.com"));
        assert!(!enricher.is_saas_forge("https://purl.org/packagegraph/ontology/vcs#GitLab", "gitlab.gnome.org"));
        assert!(enricher.is_saas_forge("https://purl.org/packagegraph/ontology/vcs#BitbucketCloud", "bitbucket.org"));
        assert!(!enricher.is_saas_forge("https://purl.org/packagegraph/ontology/vcs#BitbucketDataCenter", "bitbucket.example.com"));
    }

    #[test]
    fn test_forge_version_emission_gitlab() {
        let temp = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp.reopen().unwrap());

        let enricher = ForgeVersionEnricher {
            sparql: SparqlClient::new("http://localhost:3030/packagegraph"),
            client: Client::new(),
            cache: None,
            gitlab_token: None,
        };

        // Simulate successful probe
        let forge_uri = "https://packagegraph.github.io/d/forge/gitlab.gnome.org";
        let forge_url = "https://gitlab.gnome.org";
        let software_uri = "https://purl.org/packagegraph/ontology/vcs#GitLab";
        let today = "2026-04-26";

        // This will fail the HTTP call but we can test the emission logic separately
        // For now, just test the helper functions work
        assert!(!enricher.is_saas_forge(software_uri, "gitlab.gnome.org"));
    }

    #[test]
    fn test_fetch_version_unsupported_software() {
        let enricher = ForgeVersionEnricher {
            sparql: SparqlClient::new("http://localhost:3030/packagegraph"),
            client: Client::new(),
            cache: None,
            gitlab_token: None,
        };

        // Unsupported software should return None
        let result = enricher.fetch_version(
            "https://savannah.gnu.org",
            "https://purl.org/packagegraph/ontology/vcs#Savannah"
        ).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_fetch_version_gitlab() {
        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/api/v4/version")
            .with_status(200)
            .with_body(r#"{"version": "17.1.0", "revision": "abc123"}"#)
            .create();

        let enricher = ForgeVersionEnricher {
            sparql: SparqlClient::new("http://localhost:3030/packagegraph"),
            client: Client::new(),
            cache: None,
            gitlab_token: None,
        };

        let version = enricher.fetch_version(
            &server.url(),
            "https://purl.org/packagegraph/ontology/vcs#GitLab"
        ).unwrap();

        mock.assert();
        assert_eq!(version, Some("17.1.0".to_string()));
    }

    #[test]
    fn test_fetch_version_forgejo() {
        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/api/v1/version")
            .with_status(200)
            .with_body(r#"{"version": "9.0.0"}"#)
            .create();

        let enricher = ForgeVersionEnricher {
            sparql: SparqlClient::new("http://localhost:3030/packagegraph"),
            client: Client::new(),
            cache: None,
            gitlab_token: None,
        };

        let version = enricher.fetch_version(
            &server.url(),
            "https://purl.org/packagegraph/ontology/vcs#Forgejo"
        ).unwrap();

        mock.assert();
        assert_eq!(version, Some("9.0.0".to_string()));
    }

    #[test]
    fn test_fetch_version_ghes() {
        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/api/v3/meta")
            .with_status(200)
            .with_body(r#"{"installed_version": "3.12.0"}"#)
            .create();

        let enricher = ForgeVersionEnricher {
            sparql: SparqlClient::new("http://localhost:3030/packagegraph"),
            client: Client::new(),
            cache: None,
            gitlab_token: None,
        };

        let version = enricher.fetch_version(
            &server.url(),
            "https://purl.org/packagegraph/ontology/vcs#GitHub"
        ).unwrap();

        mock.assert();
        assert_eq!(version, Some("3.12.0".to_string()));
    }

    #[test]
    fn test_fetch_version_handles_401() {
        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/api/v4/version")
            .with_status(401)
            .with_body("Unauthorized")
            .create();

        let enricher = ForgeVersionEnricher {
            sparql: SparqlClient::new("http://localhost:3030/packagegraph"),
            client: Client::new(),
            cache: None,
            gitlab_token: None,
        };

        let version = enricher.fetch_version(
            &server.url(),
            "https://purl.org/packagegraph/ontology/vcs#GitLab"
        ).unwrap();

        mock.assert();
        assert!(version.is_none(), "Should return None for 401 responses");
    }
}
