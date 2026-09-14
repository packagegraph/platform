//! File-based JSON cache with TTL expiration and optional Minio S3 sync.
//!
//! Cache entries are stored as JSON files keyed by SHA-256 hash of the cache key.
//! When Minio is configured, entries are also synced to S3-compatible storage
//! for sharing across hosts.

use hmac::{Hmac, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::blocking::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

type HmacSha256 = Hmac<Sha256>;

// S3 canonical-URI encoding: percent-encode everything except unreserved
// characters, leaving '/' as the path-segment separator.
const S3_PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'%')
    .add(b'+')
    .add(b':')
    .add(b';')
    .add(b'=')
    .add(b'@')
    .add(b'[')
    .add(b']')
    .add(b'^')
    .add(b'|')
    .add(b'\\')
    .add(b'!')
    .add(b'$')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b',');

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Region for AWS Signature V4 -- derived from the endpoint host since
/// `MinioConfig` has no separate region field. Recognizes Scaleway Object
/// Storage's `s3.<region>.scw.cloud` convention (the only backend this
/// deployment targets, see deploy/quadlet/README.md); falls back to
/// "us-east-1", the default most S3-compatible services accept regardless
/// of where they actually run.
fn region_from_endpoint(endpoint: &str) -> String {
    let host = endpoint
        .split("://")
        .nth(1)
        .unwrap_or(endpoint)
        .split('/')
        .next()
        .unwrap_or("");
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() >= 2 && parts[0] == "s3" && host.ends_with(".scw.cloud") {
        parts[1].to_string()
    } else {
        "us-east-1".to_string()
    }
}

fn host_from_endpoint(endpoint: &str) -> String {
    endpoint
        .split("://")
        .nth(1)
        .unwrap_or(endpoint)
        .split('/')
        .next()
        .unwrap_or(endpoint)
        .to_string()
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| utf8_percent_encode(segment, S3_PATH_ENCODE_SET).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Sign a single-object S3 GET/PUT request per AWS Signature Version 4
/// (no query string). Returns the `Authorization`, `x-amz-date`, and
/// `x-amz-content-sha256` header values the request must carry.
fn sign_s3_request(
    config: &MinioConfig,
    method: &str,
    canonical_uri: &str,
    payload: &[u8],
) -> (String, String, String) {
    let region = region_from_endpoint(&config.endpoint);
    let host = host_from_endpoint(&config.endpoint);
    let now = chrono::Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(payload);

    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host, payload_hash, amz_date
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    let canonical_request = format!(
        "{}\n{}\n\n{}\n{}\n{}",
        method,
        encode_path(canonical_uri),
        canonical_headers,
        signed_headers,
        payload_hash
    );

    let credential_scope = format!("{}/{}/s3/aws4_request", date_stamp, region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        credential_scope,
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", config.secret_key).as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hmac_sha256(&k_signing, string_to_sign.as_bytes())
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        config.access_key, credential_scope, signed_headers, signature
    );

    (authorization, amz_date, payload_hash)
}

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

        let client = crate::enricher::http_client_builder()
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
    ///
    /// KNOWN DEFECT: the TTL is only enforced on the local tier. `read_minio`
    /// accepts any successful response without checking age, and the hit is
    /// then written back to the local file — refreshing its mtime. So when
    /// Minio is configured, an entry never expires: the local copy ages out,
    /// the remote copy resurrects it, and the clock restarts. Callers must not
    /// rely on the TTL to retire bad data; version the key instead (see
    /// `enrich_koji::KOJI_RPC_CACHE_VERSION`). Fixing this needs a stored
    /// timestamp in the stored value (as `http_cache.rs` does with
    /// `fetched_at`) or a bucket lifecycle rule, plus a decision about the
    /// simultaneous refetch across every collector that would follow.
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
        let canonical_uri = format!("/{}/{}", config.bucket, object_key);
        let (authorization, amz_date, payload_hash) =
            sign_s3_request(config, "GET", &canonical_uri, b"");

        let response = self
            .client
            .get(&url)
            .header("Authorization", authorization)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
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
        let canonical_uri = format!("/{}/{}", config.bucket, object_key);

        let body = serde_json::to_vec(value)
            .map_err(|e| Error::new(ErrorKind::Other, format!("JSON serialize: {}", e)))?;
        let (authorization, amz_date, payload_hash) =
            sign_s3_request(config, "PUT", &canonical_uri, &body);

        let response = self
            .client
            .put(&url)
            .header("Authorization", authorization)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", &payload_hash)
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
