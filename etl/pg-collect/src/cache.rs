//! File-based JSON cache with TTL expiration and optional Minio S3 sync.
//!
//! Cache entries are stored as JSON files keyed by SHA-256 hash of the cache key.
//! When Minio is configured, entries are also synced to S3-compatible storage
//! for sharing across hosts.

use reqwest::blocking::Client;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Minio S3-compatible storage configuration.
#[derive(Debug, Clone)]
pub struct MinioConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}

/// File-based cache with TTL and optional Minio S3 sync.
pub struct FileCache {
    cache_dir: PathBuf,
    enricher_name: String,
    ttl: Duration,
    minio: Option<MinioConfig>,
    client: Client,
}

impl FileCache {
    /// Create a new file cache.
    ///
    /// - `cache_dir`: Base cache directory
    /// - `enricher_name`: Subdirectory name for this enricher's cache
    /// - `ttl_hours`: Time-to-live in hours for cache entries
    /// - `minio`: Optional Minio S3 configuration for remote sync
    pub fn new(
        cache_dir: &str,
        enricher_name: &str,
        ttl_hours: u64,
        minio: Option<MinioConfig>,
    ) -> Result<Self> {
        let dir = Path::new(cache_dir).join(enricher_name);
        fs::create_dir_all(&dir)?;

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                Error::new(
                    ErrorKind::Other,
                    format!("Failed to create HTTP client: {}", e),
                )
            })?;

        Ok(Self {
            cache_dir: dir,
            enricher_name: enricher_name.to_string(),
            ttl: Duration::from_secs(ttl_hours * 3600),
            minio,
            client,
        })
    }

    /// Get a cached value by key. Returns None if not cached or expired.
    ///
    /// Check order: local file → Minio (if configured).
    pub fn get(&self, key: &str) -> Option<Value> {
        let file_path = self.key_path(key);

        // Check local cache
        if let Some(value) = self.read_local(&file_path) {
            return Some(value);
        }

        // Fall back to Minio
        if let Some(ref minio) = self.minio {
            if let Some(value) = self.read_minio(minio, key) {
                // Write to local cache for next time
                let _ = self.write_local(&file_path, &value);
                return Some(value);
            }
        }

        None
    }

    /// Store a value in the cache.
    ///
    /// Writes to local file and uploads to Minio (if configured).
    pub fn put(&self, key: &str, value: &Value) {
        let file_path = self.key_path(key);

        if let Err(e) = self.write_local(&file_path, value) {
            eprintln!("Warning: cache write failed for {}: {}", key, e);
        }

        // Upload to Minio (best-effort)
        if let Some(ref minio) = self.minio {
            if let Err(e) = self.write_minio(minio, key, value) {
                eprintln!("Warning: Minio sync failed for {}: {}", key, e);
            }
        }
    }

    fn key_path(&self, key: &str) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());
        self.cache_dir.join(format!("{}.json", hash))
    }

    fn read_local(&self, path: &Path) -> Option<Value> {
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata.modified().ok()?;
        let age = SystemTime::now().duration_since(modified).ok()?;

        if age > self.ttl {
            return None; // Expired
        }

        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn write_local(&self, path: &Path, value: &Value) -> Result<()> {
        let content = serde_json::to_string(value)
            .map_err(|e| Error::new(ErrorKind::Other, format!("JSON serialize: {}", e)))?;
        fs::write(path, content)
    }

    fn minio_key(&self, key: &str) -> String {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());
        format!("cache/{}/{}.json", self.enricher_name, hash)
    }

    fn read_minio(&self, config: &MinioConfig, key: &str) -> Option<Value> {
        let object_key = self.minio_key(key);
        let url = format!("{}/{}/{}", config.endpoint, config.bucket, object_key);

        let response = self
            .client
            .get(&url)
            .basic_auth(&config.access_key, Some(&config.secret_key))
            .send()
            .ok()?;

        if !response.status().is_success() {
            return None;
        }

        response.json().ok()
    }

    fn write_minio(&self, config: &MinioConfig, key: &str, value: &Value) -> Result<()> {
        let object_key = self.minio_key(key);
        let url = format!("{}/{}/{}", config.endpoint, config.bucket, object_key);

        let body = serde_json::to_vec(value)
            .map_err(|e| Error::new(ErrorKind::Other, format!("JSON serialize: {}", e)))?;

        let response = self
            .client
            .put(&url)
            .basic_auth(&config.access_key, Some(&config.secret_key))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .map_err(|e| Error::new(ErrorKind::Other, format!("Minio PUT: {}", e)))?;

        if !response.status().is_success() {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Minio PUT failed: {}", response.status()),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cache_put_and_get() {
        let tmp = TempDir::new().unwrap();
        let cache = FileCache::new(tmp.path().to_str().unwrap(), "test", 24, None).unwrap();

        let value = serde_json::json!({"stars": 42, "language": "Rust"});
        cache.put("repo:openssl/openssl", &value);

        let result = cache.get("repo:openssl/openssl");
        assert!(result.is_some(), "Should find cached value");
        assert_eq!(result.unwrap()["stars"], 42);
    }

    #[test]
    fn test_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = FileCache::new(tmp.path().to_str().unwrap(), "test", 24, None).unwrap();

        let result = cache.get("nonexistent");
        assert!(result.is_none(), "Should return None for missing key");
    }

    #[test]
    fn test_cache_expired() {
        let tmp = TempDir::new().unwrap();
        // TTL of 0 hours = everything is expired
        let cache = FileCache::new(tmp.path().to_str().unwrap(), "test", 0, None).unwrap();

        let value = serde_json::json!({"data": "old"});
        cache.put("key", &value);

        // Wait a tiny bit so the file modification time is in the past
        std::thread::sleep(Duration::from_millis(10));

        let result = cache.get("key");
        assert!(result.is_none(), "Should return None for expired entry");
    }

    #[test]
    fn test_cache_different_keys() {
        let tmp = TempDir::new().unwrap();
        let cache = FileCache::new(tmp.path().to_str().unwrap(), "test", 24, None).unwrap();

        cache.put("key1", &serde_json::json!({"id": 1}));
        cache.put("key2", &serde_json::json!({"id": 2}));

        assert_eq!(cache.get("key1").unwrap()["id"], 1);
        assert_eq!(cache.get("key2").unwrap()["id"], 2);
    }

    #[test]
    fn test_cache_directory_created() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("deep").join("path");
        let cache = FileCache::new(nested.to_str().unwrap(), "myenricher", 24, None).unwrap();

        assert!(
            nested.join("myenricher").exists(),
            "Should create cache subdirectory"
        );

        cache.put("test", &serde_json::json!({}));
        assert!(cache.get("test").is_some());
    }

    #[test]
    fn test_minio_fallback_on_local_miss() {
        let tmp = TempDir::new().unwrap();

        // Create a mock Minio server
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"from": "minio"}"#)
            .create();

        let minio_config = MinioConfig {
            endpoint: server.url(),
            bucket: "test-bucket".to_string(),
            access_key: "minioadmin".to_string(),
            secret_key: "minioadmin".to_string(),
        };

        let cache =
            FileCache::new(tmp.path().to_str().unwrap(), "test", 24, Some(minio_config)).unwrap();

        let result = cache.get("remote-key");
        mock.assert();
        assert!(result.is_some(), "Should fall back to Minio");
        assert_eq!(result.unwrap()["from"], "minio");

        // Should now be cached locally
        let local = cache.get("remote-key");
        assert!(
            local.is_some(),
            "Should be cached locally after Minio fetch"
        );
    }
}
