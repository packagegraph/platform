//! Binary artifact cache for upstream repository metadata.
//!
//! Stores raw compressed bytes (`.gz`, `.zst`) with HTTP validators (ETag, Last-Modified)
//! and content hashing. Supports conditional GET to avoid re-downloading unchanged content.
//!
//! This is NOT an extension of `FileCache` (cache.rs). `FileCache` is for enricher JSON
//! API responses with TTL + Minio sync. `SourceCache` is for binary artifacts with HTTP
//! validators and no TTL. They serve different purposes and must remain separate.

use reqwest::blocking::{Client, Response};
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Scope identifier for a cached artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheScope {
    pub collector: String,
    pub distro: String,
    pub release: String,
    pub repo: Option<String>,
    pub arch: Option<String>,
}

/// Metadata for a single cached artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub logical_name: String,
    pub source_url: String,
    pub fetched_at: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
    pub path: String,
}

/// Shard-level cache manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheManifest {
    pub schema: String,
    pub collector: String,
    pub scope: CacheScope,
    pub artifacts: Vec<ArtifactMeta>,
}

/// Result of a fetch_or_reuse operation.
#[derive(Debug)]
pub enum CacheResult {
    /// Freshly downloaded content (raw bytes)
    Fresh(Vec<u8>),
    /// Content unchanged, use cached file at path
    Cached(PathBuf),
    /// Server returned 304 Not Modified, use cached file at path
    NotModified(PathBuf),
}

/// Binary artifact cache with conditional GET support.
pub struct SourceCache {
    cache_dir: PathBuf,
    collector_name: String,
    client: Client,
}

impl SourceCache {
    /// Create a new source cache.
    ///
    /// Creates the cache directory if it doesn't exist.
    pub fn new(cache_dir: &str, collector_name: &str) -> io::Result<Self> {
        let dir = Path::new(cache_dir).join(collector_name);
        fs::create_dir_all(&dir)?;

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        Ok(Self {
            cache_dir: dir,
            collector_name: collector_name.to_string(),
            client,
        })
    }

    /// Fetch an artifact or reuse cached version with conditional GET.
    ///
    /// Returns:
    /// - `Fresh(bytes)` if content was downloaded
    /// - `Cached(path)` if cache is valid
    /// - `NotModified(path)` if server returned 304
    pub fn fetch_or_reuse(
        &self,
        url: &str,
        scope: &CacheScope,
        logical_name: &str,
    ) -> io::Result<CacheResult> {
        let artifact_path = self.artifact_path(scope, logical_name);
        let manifest_path = self.manifest_path(scope);

        // Check if cached artifact exists with valid manifest
        if let Some(meta) = self.read_manifest(&manifest_path, logical_name)? {
            // Attempt conditional GET
            let mut req = self.client.get(url);
            if let Some(ref etag) = meta.etag {
                req = req.header(IF_NONE_MATCH, etag);
            }
            if let Some(ref lm) = meta.last_modified {
                req = req.header(IF_MODIFIED_SINCE, lm);
            }

            match req.send() {
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_MODIFIED => {
                    // 304 Not Modified — use cached version
                    return Ok(CacheResult::NotModified(artifact_path));
                }
                Ok(resp) if resp.status().is_success() => {
                    // Content changed — download and cache
                    return self.download_and_cache(resp, url, scope, logical_name);
                }
                Ok(resp) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("HTTP {}", resp.status()),
                    ));
                }
                Err(e) => {
                    // Network error — if cache exists, use it as fallback
                    if artifact_path.exists() {
                        eprintln!("Warning: network error, using cached version: {}", e);
                        return Ok(CacheResult::Cached(artifact_path));
                    }
                    return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
                }
            }
        }

        // No cache — fresh download
        let resp = self
            .client
            .get(url)
            .send()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        if !resp.status().is_success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("HTTP {}", resp.status()),
            ));
        }

        self.download_and_cache(resp, url, scope, logical_name)
    }

    fn download_and_cache(
        &self,
        resp: Response,
        url: &str,
        scope: &CacheScope,
        logical_name: &str,
    ) -> io::Result<CacheResult> {
        let etag = resp.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(String::from);
        let last_modified = resp
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let bytes = resp
            .bytes()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?
            .to_vec();

        // Compute content hash
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let sha256 = format!("{:x}", hasher.finalize());

        // Save artifact
        let artifact_path = self.artifact_path(scope, logical_name);
        fs::create_dir_all(artifact_path.parent().unwrap())?;
        fs::write(&artifact_path, &bytes)?;

        // Update manifest
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let fetched_at = format!(
            "{}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            1970 + now / 31557600,
            (now / 2629800) % 12 + 1,
            (now / 86400) % 30 + 1,
            (now / 3600) % 24,
            (now / 60) % 60,
            now % 60
        );

        let relative_path = artifact_path
            .strip_prefix(&self.cache_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();

        let meta = ArtifactMeta {
            logical_name: logical_name.to_string(),
            source_url: url.to_string(),
            fetched_at,
            etag,
            last_modified,
            sha256,
            size_bytes: bytes.len() as u64,
            path: relative_path,
        };

        self.write_manifest(scope, meta)?;

        Ok(CacheResult::Fresh(bytes))
    }

    fn artifact_path(&self, scope: &CacheScope, logical_name: &str) -> PathBuf {
        let mut path = self.cache_dir.join(&scope.distro).join(&scope.release);
        if let Some(ref repo) = scope.repo {
            path = path.join(repo);
        }
        if let Some(ref arch) = scope.arch {
            path = path.join(arch);
        }
        path.join("artifacts").join(logical_name)
    }

    fn manifest_path(&self, scope: &CacheScope) -> PathBuf {
        let mut path = self.cache_dir.join(&scope.distro).join(&scope.release);
        if let Some(ref repo) = scope.repo {
            path = path.join(repo);
        }
        if let Some(ref arch) = scope.arch {
            path = path.join(arch);
        }
        path.join("manifest.json")
    }

    fn read_manifest(
        &self,
        manifest_path: &Path,
        logical_name: &str,
    ) -> io::Result<Option<ArtifactMeta>> {
        if !manifest_path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(manifest_path)?;
        let manifest: CacheManifest = serde_json::from_str(&content)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        Ok(manifest
            .artifacts
            .into_iter()
            .find(|a| a.logical_name == logical_name))
    }

    fn write_manifest(&self, scope: &CacheScope, new_meta: ArtifactMeta) -> io::Result<()> {
        let manifest_path = self.manifest_path(scope);
        fs::create_dir_all(manifest_path.parent().unwrap())?;

        // Read existing manifest or create new
        let mut manifest = if manifest_path.exists() {
            let content = fs::read_to_string(&manifest_path)?;
            serde_json::from_str::<CacheManifest>(&content)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        } else {
            CacheManifest {
                schema: "artifact-cache/v1".to_string(),
                collector: self.collector_name.clone(),
                scope: scope.clone(),
                artifacts: Vec::new(),
            }
        };

        // Update or append artifact metadata
        if let Some(existing) = manifest
            .artifacts
            .iter_mut()
            .find(|a| a.logical_name == new_meta.logical_name)
        {
            *existing = new_meta;
        } else {
            manifest.artifacts.push(new_meta);
        }

        // Write manifest
        let content = serde_json::to_string_pretty(&manifest)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        fs::write(&manifest_path, content)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cache_new_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "rpm").unwrap();
        assert!(tmp.path().join("rpm").exists());
    }

    #[test]
    fn test_cache_miss_fresh_download() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();

        let scope = CacheScope {
            collector: "test".to_string(),
            distro: "fedora".to_string(),
            release: "43".to_string(),
            repo: Some("fedora".to_string()),
            arch: Some("x86_64".to_string()),
        };

        // Mock HTTP server
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/repomd.xml")
            .with_status(200)
            .with_header("etag", "\"abc123\"")
            .with_body("test content")
            .create();

        let url = format!("{}/repomd.xml", server.url());
        let result = cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap();

        mock.assert();

        match result {
            CacheResult::Fresh(bytes) => {
                assert_eq!(bytes, b"test content");
            }
            _ => panic!("Expected Fresh, got {:?}", result),
        }

        // Verify manifest was written
        let manifest_path = cache.manifest_path(&scope);
        assert!(manifest_path.exists());
    }

    #[test]
    fn test_cache_hit_not_modified() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();

        let scope = CacheScope {
            collector: "test".to_string(),
            distro: "fedora".to_string(),
            release: "43".to_string(),
            repo: Some("fedora".to_string()),
            arch: Some("x86_64".to_string()),
        };

        let mut server = mockito::Server::new();
        let url = format!("{}/repomd.xml", server.url());

        // First request: fresh download
        let mock1 = server
            .mock("GET", "/repomd.xml")
            .with_status(200)
            .with_header("etag", "\"abc123\"")
            .with_body("test content")
            .create();

        cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap();
        mock1.assert();

        // Second request: should send If-None-Match, get 304
        let mock2 = server
            .mock("GET", "/repomd.xml")
            .match_header("if-none-match", "\"abc123\"")
            .with_status(304)
            .create();

        let result = cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap();
        mock2.assert();

        match result {
            CacheResult::NotModified(path) => {
                assert!(path.exists());
                let content = fs::read_to_string(&path).unwrap();
                assert_eq!(content, "test content");
            }
            _ => panic!("Expected NotModified, got {:?}", result),
        }
    }

    #[test]
    fn test_content_hash_stability() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();

        let scope = CacheScope {
            collector: "test".to_string(),
            distro: "test".to_string(),
            release: "1".to_string(),
            repo: None,
            arch: None,
        };

        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/file")
            .with_status(200)
            .with_body("content")
            .create();

        let url = format!("{}/file", server.url());
        cache.fetch_or_reuse(&url, &scope, "file").unwrap();

        let manifest_path = cache.manifest_path(&scope);
        let content = fs::read_to_string(&manifest_path).unwrap();
        let manifest: CacheManifest = serde_json::from_str(&content).unwrap();

        // Verify SHA-256 hash
        let expected_hash = format!("{:x}", Sha256::digest(b"content"));
        assert_eq!(manifest.artifacts[0].sha256, expected_hash);
    }
}
