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
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// Counter for unique temp file names within a process (mirrors
/// `http_cache.rs`'s `TEMP_COUNTER` pattern for atomic tmp-file + rename
/// writes). Kept as its own separate static -- see this file's top-of-file
/// doc comment on why `SourceCache` and `HttpCache` stay independent types.
static MANIFEST_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let client = crate::enricher::http_client_builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        Self::with_client(cache_dir, collector_name, client)
    }

    /// Create a source cache that fetches through a caller-supplied client
    /// rather than building a plain one. Needed by any collector whose
    /// requests require more than the default client -- e.g. RpmCollector's
    /// TLS client-cert auth against the RHEL CDN, which `new` silently
    /// dropped: `fetch_or_reuse` always downloaded through `self.client`
    /// here, so plugging in `--cache-dir` on a TLS-authenticated collector
    /// made every request hit cdn.redhat.com without the client cert,
    /// failing outright (confirmed live 2026-09-11).
    pub fn with_client(cache_dir: &str, collector_name: &str, client: Client) -> io::Result<Self> {
        let dir = Path::new(cache_dir).join(collector_name);
        fs::create_dir_all(&dir)?;

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
        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
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
        let manifest = match self.load_manifest(manifest_path)? {
            Some(m) => m,
            None => return Ok(None),
        };

        Ok(manifest
            .artifacts
            .into_iter()
            .find(|a| a.logical_name == logical_name))
    }

    /// Load and parse a manifest file, self-healing on corruption.
    ///
    /// Returns `Ok(None)` if the file doesn't exist OR fails to parse (a
    /// truncated file from a SIGTERM/OOM mid-write, disk corruption,
    /// etc.). In the parse-failure case, the corrupt file is deleted
    /// (best-effort) so it can't keep failing every subsequent call --
    /// this is what caused ~24,000 spec fetches to hard-fail from one
    /// truncated shared manifest.json (2026-09-11 fedora-43 incident).
    /// Genuine IO errors (permission denied, etc.) still propagate.
    fn load_manifest(&self, manifest_path: &Path) -> io::Result<Option<CacheManifest>> {
        if !manifest_path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(manifest_path)?;
        match serde_json::from_str::<CacheManifest>(&content) {
            Ok(manifest) => Ok(Some(manifest)),
            Err(e) => {
                eprintln!(
                    "Warning: evicting unparseable cache manifest {}: {}",
                    manifest_path.display(),
                    e
                );
                let _ = fs::remove_file(manifest_path);
                Ok(None)
            }
        }
    }

    fn write_manifest(&self, scope: &CacheScope, new_meta: ArtifactMeta) -> io::Result<()> {
        let manifest_path = self.manifest_path(scope);
        fs::create_dir_all(manifest_path.parent().unwrap())?;

        let mut manifest = self
            .load_manifest(&manifest_path)?
            .unwrap_or_else(|| CacheManifest {
                schema: "artifact-cache/v1".to_string(),
                collector: self.collector_name.clone(),
                scope: scope.clone(),
                artifacts: Vec::new(),
            });

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

        self.write_manifest_atomic(&manifest_path, &manifest)
    }

    /// Serialize and write a manifest atomically via temp file + rename.
    ///
    /// Mirrors `http_cache.rs`'s `write_envelope_atomic`: write to a
    /// uniquely-named `.tmp` file in the same directory, `sync_all()` it,
    /// drop the handle, then `fs::rename` over the target. Rename within
    /// the same directory is atomic on POSIX filesystems, so a process
    /// kill (SIGTERM via `systemctl stop`, OOM) can never leave a
    /// truncated/corrupt `manifest.json` -- readers see either the old
    /// complete file or the new complete file, never a partial one.
    fn write_manifest_atomic(
        &self,
        manifest_path: &Path,
        manifest: &CacheManifest,
    ) -> io::Result<()> {
        let parent = manifest_path.parent().unwrap();
        fs::create_dir_all(parent)?;

        let counter = MANIFEST_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp_path = parent.join(format!("manifest.json.{}.{}.tmp", pid, counter));

        let content = serde_json::to_string_pretty(manifest)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);

        fs::rename(&tmp_path, manifest_path)?;
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

    #[test]
    fn test_corrupt_manifest_treated_as_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();

        let scope = CacheScope {
            collector: "test".to_string(),
            distro: "fedora".to_string(),
            release: "43".to_string(),
            repo: Some("fedora".to_string()),
            arch: Some("x86_64".to_string()),
        };

        // Simulate a SIGTERM/OOM mid-write: manifest.json exists but is
        // truncated garbage, exactly like the live fedora-43 incident.
        let manifest_path = cache.manifest_path(&scope);
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        fs::write(&manifest_path, b"{\"schema\": \"artifact-cache/v1\", \"artifacts\": [ { \"logical_nam")
            .unwrap();

        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/repomd.xml")
            .with_status(200)
            .with_header("etag", "\"abc123\"")
            .with_body("test content")
            .create();

        let url = format!("{}/repomd.xml", server.url());

        // Must NOT hard-fail: corrupt manifest => cache miss => fresh download.
        let result = cache
            .fetch_or_reuse(&url, &scope, "repomd.xml")
            .expect("corrupt manifest must be treated as a cache miss, not an error");

        mock.assert();
        match result {
            CacheResult::Fresh(bytes) => assert_eq!(bytes, b"test content"),
            _ => panic!("Expected Fresh, got {:?}", result),
        }

        // Manifest must now be valid JSON reflecting the fresh write.
        let content = fs::read_to_string(&manifest_path).unwrap();
        let manifest: CacheManifest = serde_json::from_str(&content)
            .expect("manifest must be valid JSON after self-healing write");
        assert_eq!(manifest.artifacts.len(), 1);
        assert_eq!(manifest.artifacts[0].logical_name, "repomd.xml");
    }

    #[test]
    fn test_write_recovers_from_corrupt_existing_manifest() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();

        let scope = CacheScope {
            collector: "test".to_string(),
            distro: "fedora".to_string(),
            release: "43".to_string(),
            repo: None,
            arch: None,
        };

        // Corrupt manifest already on disk before any fetch happens (e.g.
        // left behind by a killed prior run for a *different* artifact).
        let manifest_path = cache.manifest_path(&scope);
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        fs::write(&manifest_path, b"not even close to json").unwrap();

        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/primary.xml")
            .with_status(200)
            .with_body("primary content")
            .create();
        let url = format!("{}/primary.xml", server.url());

        cache
            .fetch_or_reuse(&url, &scope, "primary.xml")
            .expect("write against a corrupt existing manifest must succeed");
        mock.assert();

        // The write must have produced a fresh, valid, single-entry manifest
        // rather than propagating the old corruption or erroring out.
        let content = fs::read_to_string(&manifest_path).unwrap();
        let manifest: CacheManifest = serde_json::from_str(&content).unwrap();
        assert_eq!(manifest.artifacts.len(), 1);
        assert_eq!(manifest.artifacts[0].logical_name, "primary.xml");
    }

    #[test]
    fn test_stray_tmp_file_does_not_affect_read_or_write() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();

        let scope = CacheScope {
            collector: "test".to_string(),
            distro: "fedora".to_string(),
            release: "43".to_string(),
            repo: None,
            arch: None,
        };

        let mut server = mockito::Server::new();
        let url = format!("{}/repomd.xml", server.url());

        // First write: establishes a valid manifest.json.
        let mock1 = server
            .mock("GET", "/repomd.xml")
            .with_status(200)
            .with_header("etag", "\"abc123\"")
            .with_body("first content")
            .create();
        cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap();
        mock1.assert();

        // Simulate a crashed write: a stray .tmp file left in the same
        // directory as manifest.json (process killed after File::create
        // but before fs::rename).
        let manifest_path = cache.manifest_path(&scope);
        let stray_tmp = manifest_path
            .parent()
            .unwrap()
            .join("manifest.json.999999.0.tmp");
        fs::write(&stray_tmp, b"garbage from a crashed write").unwrap();

        // A subsequent read (via 304) must still see the real manifest,
        // untouched by the stray tmp file.
        let mock2 = server
            .mock("GET", "/repomd.xml")
            .match_header("if-none-match", "\"abc123\"")
            .with_status(304)
            .create();
        let result = cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap();
        mock2.assert();
        assert!(matches!(result, CacheResult::NotModified(_)));

        // A subsequent write must also succeed cleanly, ignoring the stray tmp.
        let mock3 = server
            .mock("GET", "/repomd.xml")
            .with_status(200)
            .with_header("etag", "\"def456\"")
            .with_body("second content")
            .create();
        cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap();
        mock3.assert();

        let content = fs::read_to_string(&manifest_path).unwrap();
        let manifest: CacheManifest = serde_json::from_str(&content).unwrap();
        assert_eq!(manifest.artifacts.len(), 1);
        assert_eq!(
            manifest.artifacts[0].sha256,
            format!("{:x}", Sha256::digest(b"second content"))
        );

        // Stray tmp file is inert leftover cruft, not touched by our code
        // path (only the target manifest.json is managed) — confirm it's
        // simply ignored rather than corrupting anything.
        assert!(stray_tmp.exists());
    }

    #[test]
    fn test_fetch_or_reuse_returns_err_only_for_real_io_errors() {
        // Guard against a regression where corruption-tolerance
        // accidentally swallows genuine IO errors (e.g. permission denied)
        // too. Uses a manifest path that is a directory (not a file) to
        // force a real fs::read_to_string error distinct from a parse error.
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();

        let scope = CacheScope {
            collector: "test".to_string(),
            distro: "fedora".to_string(),
            release: "43".to_string(),
            repo: None,
            arch: None,
        };

        let manifest_path = cache.manifest_path(&scope);
        fs::create_dir_all(&manifest_path).unwrap(); // manifest.json is a directory, not a file

        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/repomd.xml")
            .with_status(200)
            .with_body("content")
            .expect(0) // the read error must occur before the HTTP call is ever attempted
            .create();
        let url = format!("{}/repomd.xml", server.url());

        // Real IO errors (not "file has malformed JSON") must still surface.
        let result = cache.fetch_or_reuse(&url, &scope, "repomd.xml");
        assert!(
            result.is_err(),
            "a genuine IO error (path is a directory) must not be silently swallowed"
        );
        mock.assert();
    }
}
