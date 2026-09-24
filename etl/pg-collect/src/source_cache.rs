//! Binary artifact cache for upstream repository metadata.
//!
//! Stores raw compressed bytes (`.gz`, `.zst`) with HTTP validators (ETag, Last-Modified)
//! and content hashing. Supports conditional GET to avoid re-downloading unchanged content.
//!
//! This is NOT an extension of `FileCache` (cache.rs). `FileCache` is for enricher JSON
//! API responses with TTL + Minio sync. `SourceCache` is for binary artifacts with HTTP
//! validators and no TTL. They serve different purposes and must remain separate.

use crate::cached_fetch::HttpResponse;
use crate::fetch_error::FetchError;
use crate::http_transport::HttpTransport;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Counter for unique temp file names within a process (mirrors
/// `http_cache.rs`'s `TEMP_COUNTER` pattern for atomic tmp-file + rename
/// writes). Kept as its own separate static -- see this file's top-of-file
/// doc comment on why `SourceCache` and `HttpCache` stay independent types.
static MANIFEST_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Same idea for artifact bytes. Artifacts used to be written in place with
/// `fs::write`, which is not a publication: a kill part-way through left
/// truncated bytes sitting under the *old* manifest's size and digest, and
/// nothing ever re-read them to notice.
static ARTIFACT_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    transport: HttpTransport,
}

impl SourceCache {
    /// Create a new source cache.
    ///
    /// Creates the cache directory if it doesn't exist.
    pub fn new(cache_dir: &str, collector_name: &str) -> io::Result<Self> {
        Self::with_transport(cache_dir, collector_name, HttpTransport::new())
    }

    /// Create a source cache that fetches through a caller-supplied client
    /// rather than building a plain one. Needed by any collector whose
    /// requests require more than the default client -- e.g. RpmCollector's
    /// TLS client-cert auth against the RHEL CDN, which `new` silently
    /// dropped: `fetch_or_reuse` always downloaded through `self.client`
    /// here, so plugging in `--cache-dir` on a TLS-authenticated collector
    /// made every request hit cdn.redhat.com without the client cert,
    /// failing outright (confirmed live 2026-09-11).
    pub fn with_transport(
        cache_dir: &str,
        collector_name: &str,
        transport: HttpTransport,
    ) -> io::Result<Self> {
        let dir = Path::new(cache_dir).join(collector_name);
        fs::create_dir_all(&dir)?;

        Ok(Self {
            cache_dir: dir,
            collector_name: collector_name.to_string(),
            transport,
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
            // Verify the BYTES before trusting the metadata that describes
            // them. Both reuse paths below -- a 304 and the stale-on-network-
            // error fallback -- hand the caller this file without reading it,
            // so an entry whose manifest survived while its artifact did not
            // was silently served as if it were good. That happens: the
            // artifact and the manifest are two separate writes, so an
            // interrupted publication leaves new bytes under an old digest,
            // and a truncated or half-written file is exactly the shape a
            // kill during `fs::write` produced.
            let intact = match Self::verify_artifact(&artifact_path, &meta) {
                Ok(()) => true,
                Err(why) => {
                    eprintln!(
                        "Warning: cached artifact {} failed verification ({}) — refetching",
                        artifact_path.display(),
                        why
                    );
                    false
                }
            };

            // Validators describe bytes we no longer have, so sending them
            // would invite a 304 and with it the corrupt copy. Ask
            // unconditionally instead.
            let mut headers: Vec<(&str, &str)> = Vec::new();
            let mut etag: Option<&str> = None;
            if intact {
                if let Some(ref lm) = meta.last_modified {
                    headers.push(("If-Modified-Since", lm));
                }
                etag = meta.etag.as_deref();
            }

            match self.transport.get_with(url, &headers, etag) {
                Ok(resp) if resp.status == 304 => {
                    if !intact {
                        // We sent no validators, so this is the server being
                        // wrong. Refusing is the only safe answer: the only
                        // bytes we could return are the ones we just rejected.
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{}: server returned 304 to an unconditional request while \
                                 the cached artifact is invalid; refusing to serve it",
                                url
                            ),
                        ));
                    }
                    // 304 Not Modified — use the verified cached version
                    return Ok(CacheResult::NotModified(artifact_path));
                }
                Ok(resp) => {
                    // Content changed — download and cache
                    return self.download_and_cache(resp, url, scope, logical_name);
                }
                Err(FetchError::Transport { source, .. }) => {
                    // Network error, after the transport exhausted its
                    // retries — if a VERIFIED cached copy exists, prefer it to
                    // failing the run. Unverified, an error is the better
                    // outcome: a collector that silently parses truncated
                    // repodata publishes a plausible, wrong graph, and the
                    // completeness gates downstream only catch gross loss.
                    if intact {
                        eprintln!("Warning: network error, using cached version: {}", source);
                        return Ok(CacheResult::Cached(artifact_path));
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!(
                            "{}: cached artifact is invalid and cannot be refetched: {}",
                            url, source
                        ),
                    ));
                }
                Err(e) => {
                    return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
                }
            }
        }

        // No cache — fresh download
        let resp = self
            .transport
            .get(url, None)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        self.download_and_cache(resp, url, scope, logical_name)
    }

    /// Like [`fetch_or_reuse`](Self::fetch_or_reuse) but always yields the
    /// on-disk artifact path, so callers can stream large artifacts instead of
    /// holding them in memory.
    ///
    /// Safe for every branch: `download_and_cache` writes the artifact before
    /// returning `Fresh`, so a path exists in all three cases.
    pub fn fetch_or_reuse_to_path(
        &self,
        url: &str,
        scope: &CacheScope,
        logical_name: &str,
    ) -> io::Result<PathBuf> {
        match self.fetch_or_reuse(url, scope, logical_name)? {
            // Drop the in-memory copy immediately; it is already on disk.
            CacheResult::Fresh(_) => Ok(self.artifact_path(scope, logical_name)),
            CacheResult::Cached(path) | CacheResult::NotModified(path) => Ok(path),
        }
    }

    /// Does the artifact on disk still match what the manifest says?
    ///
    /// Existence, then length, then digest -- cheapest discriminator first,
    /// so the common truncation case costs a `stat` rather than a full read.
    /// The digest is streamed; some of these artifacts are tens of megabytes
    /// and there is no reason to hold one in memory just to hash it.
    fn verify_artifact(path: &Path, meta: &ArtifactMeta) -> Result<(), String> {
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(e) => return Err(format!("cannot open: {}", e)),
        };
        let len = match file.metadata() {
            Ok(m) => m.len(),
            Err(e) => return Err(format!("cannot stat: {}", e)),
        };
        if len != meta.size_bytes {
            return Err(format!("size {} != manifest {}", len, meta.size_bytes));
        }

        let mut reader = io::BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buf[..n]),
                Err(e) => return Err(format!("read failed: {}", e)),
            }
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != meta.sha256 {
            return Err(format!("sha256 {} != manifest {}", actual, meta.sha256));
        }
        Ok(())
    }

    /// Publish artifact bytes: same-directory temp file, fsync, rename.
    ///
    /// `fs::write` truncates the target and then streams into it, so a kill
    /// mid-write left a short file described by the previous manifest entry's
    /// size and digest -- corrupt bytes wearing valid metadata. A rename
    /// within one directory is atomic on POSIX, so a reader sees either the
    /// whole old artifact or the whole new one.
    ///
    /// The artifact and its manifest entry are still two writes, and this does
    /// not make them one. What it guarantees is that the failure is always
    /// *detectable*: interrupted between them, the new bytes sit under the old
    /// digest, and `verify_artifact` rejects them on the next run.
    fn write_artifact_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "artifact has no parent"))?;
        fs::create_dir_all(parent)?;

        let counter = ARTIFACT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let stem = path.file_name().unwrap().to_string_lossy();
        let tmp_path = parent.join(format!(".{}.{}.{}.tmp", stem, std::process::id(), counter));

        let mut file = fs::File::create(&tmp_path)?;
        if let Err(e) = file
            .write_all(bytes)
            .and_then(|_| file.sync_all())
        {
            drop(file);
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        drop(file);

        if let Err(e) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok(())
    }

    /// RFC3339 in UTC, to the second.
    ///
    /// Split out from the caller so the calendar conversion is testable at
    /// month and year boundaries. It used to be open-coded arithmetic --
    /// `1970 + secs / 31557600` for the year, `(secs / 2629800) % 12 + 1` for
    /// the month, `(secs / 86400) % 30 + 1` for the day -- using an average
    /// month and a 30-day month, so the day drifted within every month, the
    /// month drifted within every year, and both could report values that are
    /// not valid dates at all.
    fn format_fetched_at(when: DateTime<Utc>) -> String {
        when.to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    fn download_and_cache(
        &self,
        resp: HttpResponse,
        url: &str,
        scope: &CacheScope,
        logical_name: &str,
    ) -> io::Result<CacheResult> {
        let etag = resp.etag;
        let last_modified = resp.last_modified;
        let bytes = resp.bytes;

        // Compute content hash
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let sha256 = format!("{:x}", hasher.finalize());

        // Publish the artifact, then describe it. This order matters: the
        // manifest is the claim, the bytes are the evidence, and a manifest
        // entry must never point at bytes that were never fully written.
        let artifact_path = self.artifact_path(scope, logical_name);
        Self::write_artifact_atomic(&artifact_path, &bytes)?;

        let fetched_at = Self::format_fetched_at(Utc::now());

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

    // ---- #74: verify cached bytes before reusing them --------------------

    /// Seed the cache with one good artifact and return its scope + url.
    fn seed(cache: &SourceCache, server: &mut mockito::ServerGuard, body: &str) -> (CacheScope, String) {
        let scope = CacheScope {
            collector: "test".to_string(),
            distro: "fedora".to_string(),
            release: "43".to_string(),
            repo: None,
            arch: None,
        };
        let url = format!("{}/repomd.xml", server.url());
        let mock = server
            .mock("GET", "/repomd.xml")
            .with_status(200)
            .with_header("etag", "\"v1\"")
            .with_body(body)
            .create();
        cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap();
        mock.assert();
        (scope, url)
    }

    #[test]
    fn a_verified_cache_still_falls_back_to_stale_bytes_when_the_network_dies() {
        // The tolerance this cache is built around must survive the
        // verification added alongside it.
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();
        let mut server = mockito::Server::new();
        let (scope, _url) = seed(&cache, &mut server, "good content");

        // Point at a port nothing is listening on: a transport error after
        // the transport has exhausted its own retries.
        let dead = "http://127.0.0.1:1/repomd.xml";
        match cache.fetch_or_reuse(dead, &scope, "repomd.xml") {
            Ok(CacheResult::Cached(path)) => {
                assert_eq!(fs::read_to_string(&path).unwrap(), "good content");
            }
            other => panic!("expected Cached fallback, got {:?}", other),
        }
    }

    #[test]
    fn a_truncated_artifact_plus_a_network_failure_is_an_error_not_bad_bytes() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();
        let mut server = mockito::Server::new();
        let (scope, _url) = seed(&cache, &mut server, "good content");

        // Exactly what a kill during the old in-place `fs::write` produced:
        // a short file still described by the manifest's size and digest.
        let artifact = cache.artifact_path(&scope, "repomd.xml");
        fs::write(&artifact, "good con").unwrap();

        let dead = "http://127.0.0.1:1/repomd.xml";
        let result = cache.fetch_or_reuse(dead, &scope, "repomd.xml");
        assert!(
            result.is_err(),
            "unverified cached bytes must not be served as a stale fallback; got {:?}",
            result
        );
    }

    #[test]
    fn a_corrupt_artifact_forces_an_unconditional_fetch_rather_than_a_304() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();
        let mut server = mockito::Server::new();
        let (scope, url) = seed(&cache, &mut server, "good content");

        // Same length, different bytes: only the digest can catch this one.
        let artifact = cache.artifact_path(&scope, "repomd.xml");
        assert_eq!("good content".len(), "BAD! content".len());
        fs::write(&artifact, "BAD! content").unwrap();

        // Sending the validator would invite a 304 and with it the bad copy.
        let conditional = server
            .mock("GET", "/repomd.xml")
            .match_header("if-none-match", "\"v1\"")
            .with_status(304)
            .expect(0)
            .create();
        let unconditional = server
            .mock("GET", "/repomd.xml")
            .match_header("if-none-match", mockito::Matcher::Missing)
            .with_status(200)
            .with_header("etag", "\"v2\"")
            .with_body("good content")
            .create();

        match cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap() {
            CacheResult::Fresh(bytes) => assert_eq!(bytes, b"good content"),
            other => panic!("expected a fresh download, got {:?}", other),
        }
        conditional.assert();
        unconditional.assert();
        assert_eq!(fs::read_to_string(&artifact).unwrap(), "good content");
    }

    #[test]
    fn a_304_to_an_unconditional_request_over_a_bad_artifact_is_refused() {
        // A misbehaving server cannot talk us into serving bytes we already
        // rejected -- they are the only ones we have.
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();
        let mut server = mockito::Server::new();
        let (scope, url) = seed(&cache, &mut server, "good content");

        let artifact = cache.artifact_path(&scope, "repomd.xml");
        fs::write(&artifact, "truncated").unwrap();

        let _rude = server
            .mock("GET", "/repomd.xml")
            .with_status(304)
            .create();

        let result = cache.fetch_or_reuse(&url, &scope, "repomd.xml");
        assert!(result.is_err(), "expected a refusal, got {:?}", result);
    }

    #[test]
    fn a_missing_artifact_under_a_live_manifest_is_refetched() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();
        let mut server = mockito::Server::new();
        let (scope, url) = seed(&cache, &mut server, "good content");

        fs::remove_file(cache.artifact_path(&scope, "repomd.xml")).unwrap();

        let refetch = server
            .mock("GET", "/repomd.xml")
            .match_header("if-none-match", mockito::Matcher::Missing)
            .with_status(200)
            .with_body("good content")
            .create();
        match cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap() {
            CacheResult::Fresh(bytes) => assert_eq!(bytes, b"good content"),
            other => panic!("expected a fresh download, got {:?}", other),
        }
        refetch.assert();
    }

    // ---- #74: publication is atomic and its failures are detectable ------

    #[test]
    fn an_interrupted_publication_is_detected_rather_than_trusted() {
        // Publishing the bytes and recording them are two writes. Interrupted
        // between them, the artifact is whole but the manifest describes the
        // previous one -- which must read as a miss, not a hit.
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();
        let mut server = mockito::Server::new();
        let (scope, url) = seed(&cache, &mut server, "generation one");

        let artifact = cache.artifact_path(&scope, "repomd.xml");
        fs::write(&artifact, "generation two, never recorded").unwrap();

        let refetch = server
            .mock("GET", "/repomd.xml")
            .match_header("if-none-match", mockito::Matcher::Missing)
            .with_status(200)
            .with_body("generation three")
            .create();
        match cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap() {
            CacheResult::Fresh(bytes) => assert_eq!(bytes, b"generation three"),
            other => panic!("expected a fresh download, got {:?}", other),
        }
        refetch.assert();
    }

    #[test]
    fn publishing_an_artifact_leaves_no_temp_file_behind() {
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();
        let mut server = mockito::Server::new();
        let (scope, _url) = seed(&cache, &mut server, "content");

        let dir = cache.artifact_path(&scope, "repomd.xml").parent().unwrap().to_path_buf();
        let strays: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files survived publication: {:?}", strays);
    }

    #[test]
    fn a_replaced_artifact_is_verified_against_its_new_digest() {
        // Guards the obvious way to break verification: checking the bytes
        // against a stale manifest entry that was never updated.
        let tmp = TempDir::new().unwrap();
        let cache = SourceCache::new(tmp.path().to_str().unwrap(), "test").unwrap();
        let mut server = mockito::Server::new();
        let (scope, url) = seed(&cache, &mut server, "first");

        let replace = server
            .mock("GET", "/repomd.xml")
            .with_status(200)
            .with_header("etag", "\"v2\"")
            .with_body("second body, longer")
            .create();
        cache.fetch_or_reuse(&url, &scope, "repomd.xml").unwrap();
        replace.assert();

        let meta = cache
            .read_manifest(&cache.manifest_path(&scope), "repomd.xml")
            .unwrap()
            .expect("manifest entry");
        assert_eq!(meta.size_bytes, "second body, longer".len() as u64);
        SourceCache::verify_artifact(&cache.artifact_path(&scope, "repomd.xml"), &meta)
            .expect("the freshly published artifact must verify against its own entry");
    }

    #[test]
    fn a_failed_publication_leaves_the_previous_artifact_intact() {
        // The difference between `fs::write` and temp+rename only shows up
        // when the write fails: in place, the destination is already
        // truncated by then. Force the failure by making the artifact
        // directory unwritable, so even creating the temp file fails.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("artifacts");
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("repomd.xml");
        fs::write(&dest, "the previous generation").unwrap();

        let mut perms = fs::metadata(&dir).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&dir, perms).unwrap();

        // Running as root defeats the mechanism entirely. Fail loudly rather
        // than reporting a pass that proved nothing.
        let root_can_still_write = fs::File::create(dir.join(".root-probe")).is_ok();
        if root_can_still_write {
            let mut perms = fs::metadata(&dir).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(&dir, perms).unwrap();
            panic!("this test is meaningless when the writer can ignore permissions (root?)");
        }

        let result = SourceCache::write_artifact_atomic(&dest, b"a new generation");
        assert!(result.is_err(), "expected the publication to fail");

        let mut perms = fs::metadata(&dir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&dir, perms).unwrap();

        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            "the previous generation",
            "a failed publication truncated or replaced the previous artifact"
        );
    }

    // ---- #74: UTC timestamps -------------------------------------------

    #[test]
    fn fetched_at_is_a_real_utc_calendar_date() {
        use chrono::TimeZone;

        // The previous open-coded arithmetic used an average month
        // (2629800s) and a 30-day month, so it drifted. Each `was` below is
        // what it actually produced for that instant.
        let cases = [
            // (epoch seconds, expected, what the old arithmetic said)
            (1790251200i64, "2026-09-24T12:00:00Z", "2026-09-21T12:00:00Z"),
            (1798761600, "2027-01-01T00:00:00Z", "2026-12-30T00:00:00Z"),
            (1798761599, "2026-12-31T23:59:59Z", "2026-12-29T23:59:59Z"),
            (1835438400, "2028-02-29T12:00:00Z", "2028-02-04T12:00:00Z"),
        ];
        for (epoch, expected, was) in cases {
            let when = Utc.timestamp_opt(epoch, 0).unwrap();
            let got = SourceCache::format_fetched_at(when);
            assert_eq!(got, expected, "epoch {} (old arithmetic said {})", epoch, was);
            assert_ne!(got, was, "epoch {} must no longer reproduce the old value", epoch);
        }
    }

    #[test]
    fn fetched_at_round_trips_as_rfc3339() {
        let recorded = SourceCache::format_fetched_at(Utc::now());
        let parsed = DateTime::parse_from_rfc3339(&recorded)
            .expect("fetched_at must parse as RFC3339");
        assert_eq!(parsed.timezone().local_minus_utc(), 0, "must be UTC: {}", recorded);
        assert!(recorded.ends_with('Z'), "must use Z, not +00:00: {}", recorded);
    }

}
