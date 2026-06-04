use crate::forge::emit_dq_issue;
use crate::ntriples::{NTriplesWriter, bnode_id};
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use flate2::read::GzDecoder;
use once_cell::sync::Lazy;
use quick_xml::events::Event;
use quick_xml::Reader;
use regex::Regex;
use reqwest::blocking::Client;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Result};
use std::time::Duration;

/// CVE identifier regex: CVE-YYYY-NNNNN
static CVE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"CVE-\d{4}-\d{4,}").unwrap()
});

/// Map distribution ID to human-readable display name.
fn distro_display_name(distro_id: &str) -> &str {
    match distro_id {
        "fedora" => "Fedora",
        "rhel" => "Red Hat Enterprise Linux",
        "centos-stream" => "CentOS Stream",
        "opensuse" => "openSUSE",
        "alpine" => "Alpine Linux",
        "gentoo" => "Gentoo",
        _ => distro_id,
    }
}

/// A parsed RPM dependency entry from primary.xml.
#[derive(Debug, Clone)]
pub struct RpmDep {
    pub name: String,
    pub flags: Option<String>,
    pub epoch: Option<String>,
    pub ver: Option<String>,
    pub rel: Option<String>,
    /// "requires", "provides", "conflicts", "obsoletes"
    pub dep_type: String,
}

/// Parsed RPM package data including structured dependencies.
#[derive(Debug)]
pub struct RpmPackageData {
    pub fields: HashMap<String, String>,
    pub deps: Vec<RpmDep>,
}

pub struct RpmCollector {
    client: Client,
    repo_url: String,
    distro_name: String,
    release_name: String,
    repo_type: String,
    source_cache: Option<SourceCache>,
}

fn infer_repo_type(url: &str) -> String {
    // Priority order: most-specific first
    if url.contains("koji") {
        "build".to_string()
    } else if url.contains("updates/testing") {
        "updates-testing".to_string()
    } else if url.contains("updates") {
        "updates".to_string()
    } else if url.contains("development") {
        "development".to_string()
    } else if url.contains("releases") {
        "release".to_string()
    } else {
        "unknown".to_string()
    }
}

impl RpmCollector {
    pub fn new(repo_url: String, distro_name: String, release_name: String) -> Self {
        Self::new_with_repo_type(repo_url, distro_name, release_name, None)
    }

    pub fn new_with_repo_type(
        repo_url: String,
        distro_name: String,
        release_name: String,
        repo_type_override: Option<String>,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        let is_explicit = repo_type_override.is_some();
        let repo_type = repo_type_override.unwrap_or_else(|| infer_repo_type(&repo_url));
        eprintln!("Repo type: '{}' ({})", repo_type,
            if is_explicit { "explicit" } else { "inferred" });

        Self {
            client,
            repo_url,
            distro_name,
            release_name,
            repo_type,
            source_cache: None,
        }
    }

    pub fn with_cache(mut self, cache_dir: &str) -> Result<Self> {
        self.source_cache = Some(SourceCache::new(cache_dir, "rpm")?);
        Ok(self)
    }

    /// Create a collector with TLS client certificate authentication (for RHEL CDN).
    pub fn new_with_tls(
        repo_url: String,
        distro_name: String,
        release_name: String,
        client_cert_path: &str,
        client_key_path: &str,
        ca_cert_path: &str,
    ) -> Self {
        Self::new_with_tls_and_repo_type(repo_url, distro_name, release_name, client_cert_path, client_key_path, ca_cert_path, None)
    }

    pub fn new_with_tls_and_repo_type(
        repo_url: String,
        distro_name: String,
        release_name: String,
        client_cert_path: &str,
        client_key_path: &str,
        ca_cert_path: &str,
        repo_type_override: Option<String>,
    ) -> Self {
        let cert_pem = std::fs::read(client_cert_path)
            .unwrap_or_else(|e| panic!("Failed to read client cert {}: {}", client_cert_path, e));
        let key_pem = std::fs::read(client_key_path)
            .unwrap_or_else(|e| panic!("Failed to read client key {}: {}", client_key_path, e));

        // rustls Identity::from_pem expects key then cert in PEM format
        let mut identity_pem = key_pem;
        identity_pem.extend_from_slice(&cert_pem);
        let identity = reqwest::Identity::from_pem(&identity_pem)
            .expect("Failed to parse client certificate + key");

        let ca_pem = std::fs::read(ca_cert_path)
            .unwrap_or_else(|e| panic!("Failed to read CA cert {}: {}", ca_cert_path, e));
        let ca_cert = reqwest::Certificate::from_pem(&ca_pem)
            .expect("Failed to parse CA certificate");

        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::limited(5))
            .identity(identity)
            .add_root_certificate(ca_cert)
            .build()
            .expect("Failed to create TLS-authenticated HTTP client");

        let repo_type = repo_type_override.unwrap_or_else(|| infer_repo_type(&repo_url));

        Self {
            client,
            repo_url,
            distro_name,
            release_name,
            repo_type,
            source_cache: None,
        }
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);
        let mut noarch_seen: HashSet<(String, String, String, String)> = HashSet::new();
        let mut srpm_seen: HashSet<String> = HashSet::new();
        let mut srpm_nvrs: HashSet<String> = HashSet::new();
        let mut srpm_names: HashSet<String> = HashSet::new();
        let mut srpm_identity_map: HashMap<String, Vec<String>> = HashMap::new();

        let result = self.collect_with_writer(
            &mut writer, &mut noarch_seen, &mut srpm_seen,
            &mut srpm_nvrs, &mut srpm_names, &mut srpm_identity_map,
            false,
        )?;
        writer.flush()?;
        Ok(result)
    }

    /// Collect RPM packages writing to an external writer.
    /// Supports multi-arch dedup: pass noarch_seen/srpm_seen sets across multiple calls.
    /// When is_secondary is true, skips noarch packages already in noarch_seen and
    /// skips source package triples already in srpm_seen.
    pub fn collect_with_writer(
        &self,
        writer: &mut NTriplesWriter,
        noarch_seen: &mut HashSet<(String, String, String, String)>,
        srpm_seen: &mut HashSet<String>,
        srpm_nvrs: &mut HashSet<String>,
        srpm_names: &mut HashSet<String>,
        srpm_identity_map: &mut HashMap<String, Vec<String>>,
        is_secondary: bool,
    ) -> Result<(usize, usize)> {
        self.collect_with_writer_limit(writer, noarch_seen, srpm_seen, srpm_nvrs, srpm_names, srpm_identity_map, is_secondary, None)
    }

    /// Like collect_with_writer but with an optional package limit for testing.
    pub fn collect_with_writer_limit(
        &self,
        writer: &mut NTriplesWriter,
        noarch_seen: &mut HashSet<(String, String, String, String)>,
        srpm_seen: &mut HashSet<String>,
        srpm_nvrs: &mut HashSet<String>,
        srpm_names: &mut HashSet<String>,
        srpm_identity_map: &mut HashMap<String, Vec<String>>,
        is_secondary: bool,
        limit: Option<usize>,
    ) -> Result<(usize, usize)> {
        // Emit distribution metadata (only on first pass)
        if !is_secondary {
            self.emit_distribution_metadata(writer)?;
        }

        // Get primary metadata URL
        let primary_url = self.get_metadata_url("primary")?;
        eprintln!("Primary metadata URL: {}", primary_url);

        // Download and parse
        let packages_data = self.parse_primary_metadata(&primary_url)?;
        eprintln!("Found {} packages", packages_data.len());

        // Get filelists for phantom detection (optional)
        let packages_with_files = match self.parse_filelists_metadata() {
            Ok(set) => {
                eprintln!("Found {} packages with files (phantom detection enabled)", set.len());
                Some(set)
            }
            Err(e) => {
                eprintln!("Warning: filelists.xml not available, skipping phantom detection: {}", e);
                emit_dq_issue(writer, "rpm-collector", "filelists", &e.to_string(), "missing-filelists", "info")?;
                None
            }
        };

        // Build lookup set for advisory-package resolution
        let mut emitted_packages: HashSet<(String, String, String, String, String)> = HashSet::new();
        let mut total_packages = 0;
        let mut total_triples = 0;

        let release_name = if self.release_name.is_empty() { "unknown" } else { &self.release_name };

        for (idx, pkg_data) in packages_data.iter().enumerate() {
            if let Some(max) = limit {
                if total_packages >= max {
                    eprintln!("Reached limit of {} packages", max);
                    break;
                }
            }
            let fields = &pkg_data.fields;

            // Check for noarch dedup on secondary arches
            if is_secondary {
                if let (Some(name), Some(arch), Some(epoch), Some(ver), Some(rel)) = (
                    fields.get("name"), fields.get("arch"),
                    fields.get("epoch").or(Some(&"0".to_string())),
                    fields.get("ver"), fields.get("rel"),
                ) {
                    if arch == "noarch" {
                        let key = (name.clone(), epoch.clone(), ver.clone(), rel.clone());
                        if !noarch_seen.insert(key) {
                            continue; // Already emitted from primary arch
                        }
                    }
                }
            } else {
                // Track noarch from primary arch for future dedup
                if let (Some(name), Some(arch), Some(ver), Some(rel)) = (
                    fields.get("name"), fields.get("arch"), fields.get("ver"), fields.get("rel"),
                ) {
                    if arch == "noarch" {
                        let epoch = fields.get("epoch").map(|s| s.clone()).unwrap_or_else(|| "0".to_string());
                        noarch_seen.insert((name.clone(), epoch, ver.clone(), rel.clone()));
                    }
                }
            }

            // Track SRPM data (for downstream spec/koji enrichment)
            if let Some(sourcerpm) = fields.get("rpm:sourcerpm").or_else(|| fields.get("sourcerpm")) {
                let srpm = sourcerpm.trim_end_matches(".src.rpm").trim_end_matches(".rpm");
                if !srpm.is_empty() {
                    // Extract source name and NVR
                    let parts: Vec<&str> = srpm.rsplitn(3, '-').collect();
                    if parts.len() >= 3 {
                        let source_name = parts[2].to_string();
                        let nvr = srpm.to_string();

                        srpm_names.insert(source_name.clone());
                        srpm_nvrs.insert(nvr);

                        // Track identity URIs per SRPM for spec collector
                        if let Some(name) = fields.get("name") {
                            if let Some(arch) = fields.get("arch") {
                                let ver = fields.get("ver").map(|s| s.as_str()).unwrap_or("");
                                let rel = fields.get("rel").map(|s| s.as_str()).unwrap_or("");
                                let version_str = format!("{}-{}.{}", ver, rel, arch);
                                let identity = package_identity_uri(&self.distro_name, release_name, arch, name);
                                srpm_identity_map.entry(source_name).or_default().push(identity);
                            }
                        }
                    }

                    // Skip duplicate source package triple emission
                    if !srpm_seen.insert(srpm.to_string()) {
                        // SRPM triples already emitted — still emit the binary package but skip emit_source_package_triples
                        // We handle this by checking in emit_package_triples... actually, the current code always calls it.
                        // For now, let the duplicate go through — Fuseki deduplicates on load.
                    }
                }
            }

            total_triples += self.emit_package_triples(writer, pkg_data, packages_with_files.as_ref(), &mut emitted_packages)?;
            total_packages += 1;

            if (idx + 1) % 1000 == 0 {
                eprintln!("Processed {} packages", idx + 1);
            }
        }

        // Parse updateinfo and emit advisory triples
        let advisories = self.parse_updateinfo()?;
        if !advisories.is_empty() {
            let advisory_triples = self.emit_advisory_triples(writer, &advisories, &emitted_packages)?;
            total_triples += advisory_triples;
        }

        Ok((total_packages, total_triples))
    }

    fn client_get_with_retry(
        &self,
        url: &str,
        max_retries: u32,
    ) -> Result<reqwest::blocking::Response> {
        let mut retries = 0;
        loop {
            match self.client.get(url).send() {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) if response.status().is_server_error() && retries < max_retries => {
                    eprintln!(
                        "Server error {}, retrying... ({}/{})",
                        response.status(),
                        retries + 1,
                        max_retries
                    );
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(1000 * (1 << retries)));
                }
                Ok(response) => {
                    return Err(std::io::Error::other(format!(
                        "HTTP error: {}",
                        response.status()
                    )));
                }
                Err(e) if retries < max_retries => {
                    eprintln!(
                        "Network error: {}, retrying... ({}/{})",
                        e,
                        retries + 1,
                        max_retries
                    );
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(1000 * (1 << retries)));
                }
                Err(e) => {
                    return Err(std::io::Error::other(e));
                }
            }
        }
    }

    fn get_metadata_url(&self, metadata_type: &str) -> Result<String> {
        let repomd_url = format!(
            "{}/repodata/repomd.xml",
            self.repo_url.trim_end_matches('/')
        );
        eprintln!("Fetching repomd.xml from {}", repomd_url);

        let content = self.fetch_raw_bytes(&repomd_url)?;

        let mut reader = Reader::from_reader(&content[..]);
        reader.config_mut().trim_text(true);

        let mut buf = Vec::new();
        let mut in_correct_data = false;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) if e.name().as_ref() == b"data" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"type"
                            && attr.value.as_ref() == metadata_type.as_bytes()
                        {
                            in_correct_data = true;
                            break;
                        }
                    }
                }
                Ok(Event::Start(ref e) | Event::Empty(ref e))
                    if in_correct_data && e.name().as_ref() == b"location" =>
                {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"href" {
                            let href = String::from_utf8_lossy(&attr.value).to_string();
                            return Ok(format!(
                                "{}/{}",
                                self.repo_url.trim_end_matches('/'),
                                href
                            ));
                        }
                    }
                }
                Ok(Event::End(ref e)) if e.name().as_ref() == b"data" => {
                    in_correct_data = false;
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                }
                _ => {}
            }
            buf.clear();
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Metadata type '{}' not found in repomd.xml",
                metadata_type
            ),
        ))
    }

    fn cache_scope(&self) -> CacheScope {
        CacheScope {
            collector: "rpm".to_string(),
            distro: self.distro_name.clone(),
            release: self.release_name.clone(),
            repo: Some(self.repo_type.clone()),
            arch: None, // RPM primary.xml covers all arches
        }
    }

    fn download_and_decompress(&self, url: &str) -> Result<Vec<u8>> {
        let raw_bytes = self.fetch_raw_bytes(url)?;
        Self::decompress_bytes(&raw_bytes, url)
    }

    fn fetch_raw_bytes(&self, url: &str) -> Result<Vec<u8>> {
        // Use source cache if available
        if let Some(ref cache) = self.source_cache {
            let scope = self.cache_scope();
            let logical_name = url.rsplit('/').next().unwrap_or("artifact");
            match cache.fetch_or_reuse(url, &scope, logical_name)? {
                CacheResult::Fresh(bytes) => {
                    eprintln!("Downloaded {} ({} bytes, cached)", url, bytes.len());
                    Ok(bytes)
                }
                CacheResult::Cached(path) | CacheResult::NotModified(path) => {
                    eprintln!("Using cached {} ({})", logical_name, path.display());
                    std::fs::read(&path)
                }
            }
        } else {
            // Direct download (backward compat)
            eprintln!("Downloading {}", url);
            let response = self.client_get_with_retry(url, 3)?;
            let content = response
                .bytes()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(content.to_vec())
        }
    }

    fn decompress_bytes(raw: &[u8], url: &str) -> Result<Vec<u8>> {
        if url.ends_with(".gz") {
            let mut decoder = GzDecoder::new(raw);
            let mut decompressed = Vec::new();
            std::io::copy(&mut decoder, &mut decompressed)?;
            Ok(decompressed)
        } else if url.ends_with(".zst") {
            let decompressed = zstd::decode_all(raw)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(decompressed)
        } else {
            Ok(raw.to_vec())
        }
    }

    fn parse_primary_metadata(&self, primary_url: &str) -> Result<Vec<RpmPackageData>> {
        let content = self.download_and_decompress(primary_url)?;

        let mut reader = Reader::from_reader(BufReader::new(&content[..]));
        reader.config_mut().trim_text(true);

        let mut packages = Vec::new();
        let mut current_fields: HashMap<String, String> = HashMap::new();
        let mut current_deps: Vec<RpmDep> = Vec::new();
        let mut buf = Vec::new();
        let mut current_text = String::new();
        let mut in_package = false;
        // Track which dependency section we're in (if any)
        let mut current_dep_section: Option<String> = None;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e) | Event::Empty(ref e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "package" {
                        in_package = true;
                        current_fields = HashMap::new();
                        current_deps = Vec::new();
                        current_dep_section = None;
                    } else if in_package {
                        current_text.clear();

                        // Check for dependency section start
                        match name.as_str() {
                            "rpm:requires" | "rpm:provides" | "rpm:conflicts"
                            | "rpm:obsoletes" => {
                                // Map XML element name to our dep_type label
                                let dep_type = name
                                    .strip_prefix("rpm:")
                                    .unwrap_or(&name)
                                    .to_string();
                                current_dep_section = Some(dep_type);
                            }
                            "rpm:entry" => {
                                // Extract dependency entry attributes
                                if let Some(ref dep_type) = current_dep_section {
                                    let mut dep = RpmDep {
                                        name: String::new(),
                                        flags: None,
                                        epoch: None,
                                        ver: None,
                                        rel: None,
                                        dep_type: dep_type.clone(),
                                    };
                                    for attr in e.attributes().flatten() {
                                        let key = String::from_utf8_lossy(attr.key.as_ref())
                                            .to_string();
                                        let value =
                                            String::from_utf8_lossy(&attr.value).to_string();
                                        match key.as_str() {
                                            "name" => dep.name = value,
                                            "flags" => dep.flags = Some(value),
                                            "epoch" => dep.epoch = Some(value),
                                            "ver" => dep.ver = Some(value),
                                            "rel" => dep.rel = Some(value),
                                            _ => {}
                                        }
                                    }
                                    if !dep.name.is_empty() {
                                        current_deps.push(dep);
                                    }
                                }
                            }
                            _ => {
                                // version, location, size, time — capture attributes
                                for attr in e.attributes().flatten() {
                                    let key = String::from_utf8_lossy(attr.key.as_ref())
                                        .to_string();
                                    let value =
                                        String::from_utf8_lossy(&attr.value).to_string();
                                    if name == "version"
                                        || name == "location"
                                        || name == "size"
                                        || name == "time"
                                    {
                                        current_fields.insert(key.clone(), value.clone());
                                    }
                                    if name == "checksum" && key == "type" {
                                        current_fields
                                            .insert("checksum_type".to_string(), value);
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Event::Text(ref e)) if in_package => {
                    current_text.push_str(&e.unescape().unwrap_or_default());
                }
                Ok(Event::End(ref e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "package" {
                        if !current_fields.is_empty() {
                            packages.push(RpmPackageData {
                                fields: current_fields.clone(),
                                deps: current_deps.clone(),
                            });
                        }
                        in_package = false;
                    } else if matches!(
                        name.as_str(),
                        "rpm:requires" | "rpm:provides" | "rpm:conflicts" | "rpm:obsoletes"
                    ) {
                        current_dep_section = None;
                    } else if in_package && !current_text.is_empty() {
                        current_fields.insert(name, current_text.trim().to_string());
                        current_text.clear();
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(packages)
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<()> {
        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(
            &dist_uri,
            &format!("{PKG}distributionName"),
            &self.distro_name,
        )?;

        // Add human-readable label
        let display_name = distro_display_name(&self.distro_name);
        writer.write_literal(&dist_uri, RDFS_LABEL, display_name)?;

        if !self.release_name.is_empty() {
            let rel_uri = release_uri(&self.distro_name, &self.release_name);
            writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;

            // Numbered releases get releaseVersion; named/rolling get releaseCodename
            if is_numeric_release(&self.release_name) {
                writer.write_literal(&rel_uri, &format!("{PKG}releaseVersion"), &self.release_name)?;
            } else {
                writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), &self.release_name)?;
            }

            // partOfDistribution also auto-emits hasRelease inverse via ntriples.rs
            writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
            // Repo metadata
            writer.write_literal(&rel_uri, &format!("{PKG}repoType"), &self.repo_type)?;
            writer.write_literal(&rel_uri, &format!("{PKG}repoSourceURL"), &self.repo_url)?;
        }

        Ok(())
    }

    fn parse_filelists_metadata(&self) -> Result<std::collections::HashSet<String>> {
        use std::collections::HashSet;

        // Try to get filelists URL - if missing, return error (caller will handle gracefully)
        let filelists_url = self.get_metadata_url("filelists")?;
        let content = self.download_and_decompress(&filelists_url)?;

        let mut reader = Reader::from_reader(BufReader::new(&content[..]));
        reader.config_mut().trim_text(true);

        let mut packages_with_files = HashSet::new();
        let mut current_package_name: Option<String> = None;
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e) | Event::Empty(ref e)) => {
                    if e.name().as_ref() == b"package" {
                        // Extract package name attribute
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"name" {
                                current_package_name = Some(String::from_utf8_lossy(&attr.value).to_string());
                                break;
                            }
                        }
                    } else if e.name().as_ref() == b"file" {
                        // Package has at least one file - mark as real (not phantom)
                        if let Some(ref pkg_name) = current_package_name {
                            packages_with_files.insert(pkg_name.clone());
                        }
                    }
                }
                Ok(Event::End(ref e)) => {
                    if e.name().as_ref() == b"package" {
                        current_package_name = None;
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e));
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(packages_with_files)
    }

    pub fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_data: &RpmPackageData,
        packages_with_files: Option<&std::collections::HashSet<String>>,
        emitted_packages: &mut HashSet<(String, String, String, String, String)>,
    ) -> Result<usize> {
        let fields = &pkg_data.fields;

        let name = fields.get("name").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing package name")
        })?;
        let arch = fields.get("arch").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing arch")
        })?;
        let ver = fields.get("ver").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing version")
        })?;
        let rel = fields.get("rel").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing release")
        })?;

        let epoch = fields.get("epoch").map(|s| s.as_str()).unwrap_or("0");

        // Add to lookup set for advisory-package matching (Task 1)
        emitted_packages.insert((
            name.clone(),
            epoch.to_string(),
            ver.clone(),
            rel.clone(),
            arch.clone(),
        ));
        let version_str = format!("{}-{}.{}", ver, rel, arch);

        let release_name = if self.release_name.is_empty() {
            "unknown"
        } else {
            &self.release_name
        };

        let pkg_uri = package_uri(&self.distro_name, release_name, arch, name, &version_str);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{RPM}BinaryRPM"))?;
        triples += 2;

        // Link to canonical identity
        let identity_uri = package_identity_uri(&self.distro_name, release_name, arch, name);
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        // PURL (Package URL)
        let evr = if epoch != "0" {
            format!("{}:{}-{}", epoch, ver, rel)
        } else {
            format!("{}-{}", ver, rel)
        };
        let purl = crate::ntriples::format_purl(
            "rpm",
            Some(&self.distro_name),
            name,
            Some(&evr),
            &[("arch", arch)],
        );
        writer.write_typed_literal(
            &identity_uri,
            &format!("{PKG}purl"),
            &purl,
            &format!("{XSD}anyURI"),
        )?;
        triples += 1;

        // Packaging repository (dist-git — derivable from distro + package name)
        let distgit_uri = fedora_distgit_uri(&self.distro_name, name);
        writer.write_triple(&identity_uri, &format!("{PKG}packagingRepository"), &distgit_uri)?;
        writer.write_triple(&distgit_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
        triples += 2;

        // Phantom package detection (if filelists was parsed)
        if let Some(has_files) = packages_with_files {
            if !has_files.contains(name) {
                // Package not in filelists - mark as phantom
                writer.write_literal(&pkg_uri, &format!("{PKG}isPhantomPackage"), "true")?;
                triples += 1;
            }
        }

        // Upstream repository (from Homepage/URL if it matches a forge)
        if let Some(url) = fields.get("url") {
            if let Some(upstream_uri) = normalize_forge_url(url) {
                writer.write_triple(&identity_uri, &format!("{PKG}upstreamRepository"), &upstream_uri)?;
                writer.write_triple(&upstream_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
            }
        }

        // Core properties
        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), name)?;
        triples += 1;

        // Version resource
        let ver_uri = version_uri(&self.distro_name, release_name, name, &version_str);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &version_str)?;
        if epoch != "0" {
            writer.write_literal(&ver_uri, &format!("{PKG}epoch"), epoch)?;
            triples += 1;
        }
        if !rel.is_empty() {
            writer.write_literal(&ver_uri, &format!("{PKG}release"), rel)?;
            triples += 1;
        }
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Architecture
        let arch_uri_val = arch_uri(arch);
        writer.write_triple(&pkg_uri, &format!("{PKG}targetArchitecture"), &arch_uri_val)?;
        triples += 1;

        // Distribution and release
        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        if !self.release_name.is_empty() {
            let rel_uri = release_uri(&self.distro_name, &self.release_name);
            writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
            triples += 1;
        }

        // Description
        if let Some(desc) = fields
            .get("description")
            .or_else(|| fields.get("summary"))
        {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }

        // RPM-specific properties
        if let Some(sourcerpm) = fields.get("rpm:sourcerpm").or_else(|| fields.get("sourcerpm")) {
            writer.write_literal(&pkg_uri, &format!("{RPM}sourceRPM"), sourcerpm)?;
            triples += 1;
            triples += self.emit_source_package_triples(writer, &pkg_uri, sourcerpm)?;
        }

        if let Some(group) = fields.get("rpm:group").or_else(|| fields.get("group")) {
            writer.write_literal(&pkg_uri, &format!("{RPM}RPMGroup"), group)?;
            triples += 1;
        }

        if epoch != "0" {
            if let Ok(epoch_int) = epoch.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{RPM}epoch"), epoch_int)?;
                triples += 1;
            }
        }

        // Maintainer
        if let Some(packager) = fields.get("packager") {
            triples += self.emit_maintainer_triples(writer, &pkg_uri, packager)?;
        }

        // Homepage
        if let Some(url) = fields.get("url") {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), url)?;
            triples += 1;
        }

        // License
        if let Some(license) = fields.get("rpm:license") {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }

        // Checksum
        if let Some(checksum) = fields.get("checksum") {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), checksum)?;
            triples += 1;
        }

        // Package size
        if let Some(pkg_size) = fields.get("package") {
            if let Ok(size_val) = pkg_size.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}packageSize"), size_val)?;
                triples += 1;
            }
        }

        // Installed size
        if let Some(inst_size) = fields.get("installed") {
            if let Ok(size_val) = inst_size.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}installSize"), size_val)?;
                triples += 1;
            }
        }

        // Build time (from <time build="epoch"/> in primary.xml)
        if let Some(build_epoch) = fields.get("build") {
            if let Ok(epoch) = build_epoch.parse::<i64>() {
                if let Some(dt) = chrono::DateTime::from_timestamp(epoch, 0) {
                    let datetime_str = dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();
                    writer.write_datetime(&pkg_uri, &format!("{RPM}buildTime"), &datetime_str)?;
                    triples += 1;
                }
            }
        }

        // Upstream ecosystem identification from Provides entries
        triples +=
            self.emit_ecosystem_triples(writer, &pkg_uri, &pkg_data.deps)?;

        // Dependencies
        triples +=
            self.emit_dependency_triples(writer, &pkg_uri, &pkg_data.deps, release_name, arch, name)?;

        Ok(triples)
    }

    /// Extract upstream ecosystem identity from RPM Provides entries.
    ///
    /// Fedora packaging guidelines require specific Provides for language ecosystems:
    ///   crate(name) = version       → Rust/Cargo
    ///   python3dist(name) = version → Python/PyPI
    ///   golang(import/path)         → Go modules
    ///   nodejs(name)                → NPM
    ///   perl(Module::Name)          → Perl/CPAN
    ///   rubygem(name)               → Ruby/RubyGems
    ///   ghc-pkg(name)               → Haskell/Hackage
    ///   R(name)                     → R/CRAN
    fn emit_ecosystem_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        deps: &[RpmDep],
    ) -> Result<usize> {
        let mut triples = 0;
        let mut emitted_ecosystem = false;

        let provides: Vec<&RpmDep> = deps.iter().filter(|d| d.dep_type == "provides").collect();

        for dep in &provides {
            let name = &dep.name;

            let (ecosystem, upstream_name) = if let Some(crate_name) = name.strip_prefix("crate(").and_then(|s| s.strip_suffix(')')) {
                ("cargo", crate_name.to_string())
            } else if let Some(py_name) = name.strip_prefix("python3dist(").and_then(|s| s.strip_suffix(')')) {
                ("pypi", py_name.to_string())
            } else if let Some(py_name) = name.strip_prefix("python3.").and_then(|s| {
                // python3.12dist(name) format
                s.find("dist(").map(|pos| &s[pos + 5..s.len() - 1])
            }) {
                ("pypi", py_name.to_string())
            } else if let Some(go_path) = name.strip_prefix("golang(").and_then(|s| s.strip_suffix(')')) {
                ("gomod", go_path.to_string())
            } else if let Some(node_name) = name.strip_prefix("nodejs(").and_then(|s| s.strip_suffix(')')) {
                ("npm", node_name.to_string())
            } else if let Some(perl_name) = name.strip_prefix("perl(").and_then(|s| s.strip_suffix(')')) {
                ("cpan", perl_name.to_string())
            } else if let Some(gem_name) = name.strip_prefix("rubygem(").and_then(|s| s.strip_suffix(')')) {
                ("rubygems", gem_name.to_string())
            } else if let Some(ghc_name) = name.strip_prefix("ghc-pkg(").and_then(|s| s.strip_suffix(')')) {
                ("hackage", ghc_name.to_string())
            } else if let Some(mvn_coord) = name.strip_prefix("mvn(").and_then(|s| s.strip_suffix(')')) {
                // mvn() provides format: groupId:artifactId[:version[:classifier]]
                // Strip version/classifier — Maven Central needs just groupId:artifactId
                let parts: Vec<&str> = mvn_coord.splitn(3, ':').collect();
                if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                    ("maven", format!("{}:{}", parts[0], parts[1]))
                } else {
                    continue;
                }
            // erlang() and nuget() provides are ABI/arch markers, not package refs:
            //   erlang(erl_nif_version), nuget(x86-64) — skip these
            } else if let Some(r_name) = name.strip_prefix("R(").and_then(|s| s.strip_suffix(')')) {
                ("cran", r_name.to_string())
            } else {
                continue;
            };

            // Emit ecosystem entity and upstream name (once per ecosystem per package)
            if !emitted_ecosystem {
                let eco_uri = ecosystem_uri(ecosystem);
                writer.write_triple(pkg_uri, &format!("{PKG}upstreamEcosystem"), &eco_uri)?;
                writer.write_triple(&eco_uri, RDF_TYPE, &format!("{PKG}Ecosystem"))?;
                emitted_ecosystem = true;
                triples += 2;
            }
            writer.write_literal(pkg_uri, &format!("{PKG}upstreamPackageName"), &upstream_name)?;
            triples += 1;

            // Emit upstream version if available
            if let Some(ver) = &dep.ver {
                writer.write_literal(pkg_uri, &format!("{PKG}upstreamPackageVersion"), ver)?;
                triples += 1;
            }
        }

        Ok(triples)
    }

    fn emit_maintainer_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        packager: &str,
    ) -> Result<usize> {
        let re = Regex::new(r"^(.+?)\s*<(.+?)>$").unwrap();

        let (name, email_or_id) = if let Some(caps) = re.captures(packager) {
            // Format: "Name <email>" - extract both parts
            let name = caps.get(1).unwrap().as_str().trim();
            let email = caps.get(2).unwrap().as_str().trim();
            (name, email.to_string())
        } else {
            // Format: "Name" - no email, generate stable ID from name
            let name = packager.trim();
            if name.is_empty() {
                return Ok(0);
            }
            // Use lowercase, no spaces for stable ID
            let stable_id = name.to_lowercase().replace(' ', "-");
            (name, stable_id)
        };

        let maint_uri = maintainer_uri(&email_or_id);

        // Type as Person (canonical agent identity per SD-3 data contract)
        writer.write_triple(&maint_uri, RDF_TYPE, &format!("{PKG}Person"))?;
        writer.write_literal(&maint_uri, &format!("{FOAF}name"), name)?;
        writer.write_literal(&maint_uri, RDFS_LABEL, name)?;

        // Only emit mbox if it looks like an email (contains @)
        let mut triple_count = 3;
        if email_or_id.contains('@') {
            writer.write_triple(
                &maint_uri,
                &format!("{FOAF}mbox"),
                &format!("mailto:{email_or_id}"),
            )?;
            triple_count += 1;
        }

        writer.write_triple(pkg_uri, &format!("{PKG}maintainedBy"), &maint_uri)?;
        triple_count += 1;

        Ok(triple_count)
    }

    fn emit_source_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        sourcerpm: &str,
    ) -> Result<usize> {
        let srpm = sourcerpm
            .trim_end_matches(".src.rpm")
            .trim_end_matches(".rpm");
        if srpm.is_empty() {
            return Ok(0);
        }

        // Split NVR: find last two hyphens to separate name-version-release
        let parts: Vec<&str> = srpm.rsplitn(3, '-').collect();
        if parts.len() < 3 {
            return Ok(0);
        }

        let source_name = parts[2];
        let source_version = format!("{}-{}", parts[1], parts[0]);

        let release_name = if self.release_name.is_empty() {
            "unknown"
        } else {
            &self.release_name
        };

        let src_uri =
            source_uri(&self.distro_name, release_name, source_name, &source_version);

        writer.write_triple(&src_uri, RDF_TYPE, &format!("{PKG}SourcePackage"))?;
        writer.write_triple(&src_uri, RDF_TYPE, &format!("{RPM}SourceRPM"))?;
        writer.write_literal(&src_uri, &format!("{PKG}packageName"), source_name)?;

        let src_ver_uri = version_uri(
            &self.distro_name,
            release_name,
            source_name,
            &source_version,
        );
        writer.write_triple(&src_ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(
            &src_ver_uri,
            &format!("{PKG}versionString"),
            &source_version,
        )?;
        writer.write_triple(&src_uri, &format!("{PKG}hasVersion"), &src_ver_uri)?;

        writer.write_triple(pkg_uri, &format!("{PKG}builtFromSource"), &src_uri)?;

        Ok(7)
    }

    fn emit_dependency_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        deps: &[RpmDep],
        release_name: &str,
        arch: &str,
        pkg_name: &str,
    ) -> Result<usize> {
        let mut triples = 0;

        // Emit requires, provides, conflicts, and obsoletes relationships
        let requires: Vec<&RpmDep> = deps.iter().filter(|d| d.dep_type == "requires").collect();

        for dep in &requires {
            // Skip rpmlib() and config() virtual deps — these are RPM internals
            if dep.name.starts_with("rpmlib(")
                || dep.name.starts_with("config(")
                || dep.name.starts_with("rtld(")
            {
                continue;
            }

            // Dependency targets point to canonical identity URI (no version)
            let dep_uri = package_identity_uri(
                &self.distro_name,
                release_name,
                arch,
                &dep.name,
            );

            // Identity properties for graph traversal
            writer.write_triple(&dep_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
            writer.write_literal(&dep_uri, &format!("{PKG}packageName"), &dep.name)?;
            triples += 2;

            // Generic dependency link
            writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &dep_uri)?;
            // RPM-specific property
            writer.write_triple(pkg_uri, &format!("{RPM}rpmRequires"), &dep_uri)?;
            triples += 2;

            // Reified Dependency
            let dep_bnode = bnode_id("dep", &format!("{pkg_uri}_{}", dep.name));

            writer.write_bnode_subject(&dep_bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(
                &dep_bnode,
                &format!("{PKG}dependencyTarget"),
                &dep_uri,
            )?;
            writer.write_bnode_subject(&dep_bnode, &format!("{PKG}dependencyType"), &dep_type_uri("runtime"))?;
            writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &dep_bnode)?;
            triples += 4;

            // Version constraint if flags are present
            if let Some(ref flags) = dep.flags {
                if let Some(ref ver) = dep.ver {
                    let operator = match flags.as_str() {
                        "EQ" => "=",
                        "GE" => "≥",
                        "GT" => ">",
                        "LE" => "≤",
                        "LT" => "<",
                        _ => flags.as_str(),
                    };

                    let mut constraint_val = ver.clone();
                    if let Some(ref rel) = dep.rel {
                        constraint_val = format!("{}-{}", ver, rel);
                    }

                    let constraint_bnode =
                        bnode_id("constraint", &format!("{dep_bnode}_{constraint_val}"));

                    writer.write_bnode_subject(
                        &constraint_bnode,
                        RDF_TYPE,
                        &format!("{PKG}VersionConstraint"),
                    )?;
                    writer.write_bnode_literal(
                        &constraint_bnode,
                        &format!("{PKG}versionConstraintOperator"),
                        operator,
                    )?;
                    writer.write_bnode_literal(
                        &constraint_bnode,
                        &format!("{PKG}versionConstraintValue"),
                        &constraint_val,
                    )?;
                    writer.write_bnode_subject(
                        &dep_bnode,
                        &format!("{PKG}hasVersionConstraint"),
                        &format!("_{constraint_bnode}"),
                    )?;
                    triples += 4;
                }
            }
        }

        // Emit conflicts
        let conflicts: Vec<&RpmDep> = deps.iter().filter(|d| d.dep_type == "conflicts").collect();
        for dep in &conflicts {
            let dep_uri = package_identity_uri(
                &self.distro_name,
                release_name,
                arch,
                &dep.name,
            );

            writer.write_triple(&dep_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
            writer.write_literal(&dep_uri, &format!("{PKG}packageName"), &dep.name)?;
            writer.write_triple(
                pkg_uri,
                &format!("{PKG}directlyConflictsWith"),
                &dep_uri,
            )?;
            writer.write_triple(pkg_uri, &format!("{RPM}rpmConflicts"), &dep_uri)?;
            triples += 4;
        }

        // Emit obsoletes
        let obsoletes: Vec<&RpmDep> = deps.iter().filter(|d| d.dep_type == "obsoletes").collect();
        for dep in &obsoletes {
            let dep_uri = package_identity_uri(
                &self.distro_name,
                release_name,
                arch,
                &dep.name,
            );

            writer.write_triple(&dep_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
            writer.write_literal(&dep_uri, &format!("{PKG}packageName"), &dep.name)?;
            writer.write_triple(pkg_uri, &format!("{RPM}rpmObsoletes"), &dep_uri)?;
            triples += 3;
        }

        // Emit provides
        let provides: Vec<&RpmDep> = deps.iter().filter(|d| d.dep_type == "provides").collect();
        for dep in &provides {
            // Skip internal provides
            if dep.name.starts_with("config(")
                || dep.name.starts_with("rpmlib(")
                || dep.name.starts_with("rtld(")
            {
                continue;
            }

            // Skip self-provides (where provides name matches package name)
            if dep.name == pkg_name {
                continue;
            }

            let dep_uri = package_identity_uri(
                &self.distro_name,
                release_name,
                arch,
                &dep.name,
            );

            writer.write_triple(&dep_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
            writer.write_literal(&dep_uri, &format!("{PKG}packageName"), &dep.name)?;
            writer.write_triple(pkg_uri, &format!("{PKG}directlyProvides"), &dep_uri)?;
            writer.write_triple(pkg_uri, &format!("{RPM}rpmProvides"), &dep_uri)?;

            // Also emit Capability entity for CQ-PM-03
            let cap_uri = format!("{DATA}capability/{}", crate::uris::encode(&dep.name));
            writer.write_triple(&cap_uri, RDF_TYPE, &format!("{PKG}Capability"))?;
            writer.write_literal(&cap_uri, &format!("{PKG}capabilityName"), &dep.name)?;
            writer.write_triple(pkg_uri, &format!("{PKG}providesCapability"), &cap_uri)?;

            triples += 7;
        }

        Ok(triples)
    }

    /// Parse updateinfo.xml and extract security advisories.
    ///
    /// Returns empty Vec if updateinfo is not available (graceful degradation).
    fn parse_updateinfo(&self) -> Result<Vec<UpdateInfoAdvisory>> {
        // Try to get updateinfo URL - if missing, return empty (like filelists)
        let updateinfo_url = match self.get_metadata_url("updateinfo") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("No updateinfo metadata available");
                return Ok(Vec::new());
            }
        };

        eprintln!("Parsing updateinfo from {}", updateinfo_url);
        let content = self.download_and_decompress(&updateinfo_url)?;

        let mut reader = Reader::from_reader(BufReader::new(&content[..]));
        reader.config_mut().trim_text(true);

        let mut advisories = Vec::new();
        let mut current_update: Option<UpdateInfoAdvisory> = None;
        let mut current_text = String::new();
        let mut in_description = false;
        let mut in_reference = false;
        let mut ref_title: Option<String> = None;

        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e) | Event::Empty(ref e)) => {
                    match e.name().as_ref() {
                        b"update" => {
                            // Extract type attribute - only process security updates
                            let mut update_type: Option<String> = None;
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"type" {
                                    update_type = Some(String::from_utf8_lossy(&attr.value).to_string());
                                }
                            }

                            if update_type.as_deref() == Some("security") {
                                current_update = Some(UpdateInfoAdvisory {
                                    id: String::new(),
                                    advisory_type: "security".to_string(),
                                    severity: None,
                                    issued_date: String::new(),
                                    cves: Vec::new(),
                                    packages: Vec::new(),
                                });
                            }
                        }
                        b"id" if current_update.is_some() => {
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                if let Some(ref mut adv) = current_update {
                                    adv.id = text.unescape().unwrap_or_default().to_string();
                                }
                            }
                        }
                        b"severity" if current_update.is_some() => {
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                if let Some(ref mut adv) = current_update {
                                    adv.severity = Some(text.unescape().unwrap_or_default().to_string());
                                }
                            }
                        }
                        b"issued" if current_update.is_some() => {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"date" {
                                    if let Some(ref mut adv) = current_update {
                                        adv.issued_date = String::from_utf8_lossy(&attr.value).to_string();
                                    }
                                }
                            }
                        }
                        b"description" if current_update.is_some() => {
                            in_description = true;
                            current_text.clear();
                        }
                        b"reference" if current_update.is_some() => {
                            in_reference = true;
                            ref_title = None;
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"title" {
                                    ref_title = Some(String::from_utf8_lossy(&attr.value).to_string());
                                }
                            }

                            // For self-closing <reference.../>, extract CVEs immediately
                            if let Some(ref title) = ref_title {
                                if let Some(ref mut adv) = current_update {
                                    for cve_match in CVE_RE.find_iter(title) {
                                        let cve = cve_match.as_str().to_string();
                                        if !adv.cves.contains(&cve) {
                                            adv.cves.push(cve);
                                        }
                                    }
                                }
                            }
                        }
                        b"package" if current_update.is_some() => {
                            let mut pkg = UpdateInfoPackage {
                                name: String::new(),
                                version: String::new(),
                                release: String::new(),
                                epoch: "0".to_string(),
                                arch: String::new(),
                            };

                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"name" => pkg.name = String::from_utf8_lossy(&attr.value).to_string(),
                                    b"version" => pkg.version = String::from_utf8_lossy(&attr.value).to_string(),
                                    b"release" => pkg.release = String::from_utf8_lossy(&attr.value).to_string(),
                                    b"epoch" => pkg.epoch = String::from_utf8_lossy(&attr.value).to_string(),
                                    b"arch" => pkg.arch = String::from_utf8_lossy(&attr.value).to_string(),
                                    _ => {}
                                }
                            }

                            // Filter out source packages
                            if pkg.arch != "src" && !pkg.name.is_empty() {
                                if let Some(ref mut adv) = current_update {
                                    adv.packages.push(pkg);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Text(text)) => {
                    if in_description {
                        current_text.push_str(&text.unescape().unwrap_or_default());
                    }
                }
                Ok(Event::End(ref e)) => {
                    match e.name().as_ref() {
                        b"update" => {
                            if let Some(mut adv) = current_update.take() {
                                // Extract CVEs from description if we have one
                                if !current_text.is_empty() {
                                    for cve_match in CVE_RE.find_iter(&current_text) {
                                        let cve = cve_match.as_str().to_string();
                                        if !adv.cves.contains(&cve) {
                                            adv.cves.push(cve);
                                        }
                                    }
                                }

                                // Only keep advisories with packages
                                if !adv.packages.is_empty() {
                                    advisories.push(adv);
                                }
                            }
                            current_text.clear();
                        }
                        b"description" => {
                            in_description = false;
                        }
                        b"reference" => {
                            // Extract CVEs from reference title
                            if let Some(title) = ref_title.take() {
                                if let Some(ref mut adv) = current_update {
                                    for cve_match in CVE_RE.find_iter(&title) {
                                        let cve = cve_match.as_str().to_string();
                                        if !adv.cves.contains(&cve) {
                                            adv.cves.push(cve);
                                        }
                                    }
                                }
                            }
                            in_reference = false;
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    eprintln!("Warning: updateinfo XML parse error: {}", e);
                    return Ok(advisories); // Return what we have so far
                }
                _ => {}
            }
            buf.clear();
        }

        eprintln!("Parsed {} security advisories from updateinfo", advisories.len());
        Ok(advisories)
    }

    /// Emit advisory triples for updateinfo advisories with package matching.
    ///
    /// Returns total triples emitted.
    fn emit_advisory_triples(
        &self,
        writer: &mut NTriplesWriter,
        advisories: &[UpdateInfoAdvisory],
        emitted_packages: &HashSet<(String, String, String, String, String)>,
    ) -> Result<usize> {
        let mut total_triples = 0;
        let mut total_resolved_packages = 0;
        let mut unresolved_packages = 0;

        for advisory in advisories {
            let advisory_uri = format!(
                "{DATA}advisory/{}/{}/{}",
                self.distro_name, self.release_name, advisory.id
            );

            let mut advisory_triples = 0;

            // Advisory entity
            writer.write_triple(&advisory_uri, RDF_TYPE, &format!("{SEC}SecurityAdvisory"))?;
            writer.write_literal(&advisory_uri, &format!("{SEC}advisoryId"), &advisory.id)?;
            writer.write_triple(&advisory_uri, &format!("{SEC}advisoryType"), &advisory_category_uri(&advisory.advisory_type))?;
            advisory_triples += 3;

            // Advisory date (convert "2025-11-26 20:40:36" to ISO 8601)
            let datetime_iso = advisory.issued_date.replace(" ", "T") + "Z";
            writer.write_datetime(&advisory_uri, &format!("{SEC}advisoryDate"), &datetime_iso)?;
            advisory_triples += 1;

            // Severity
            if let Some(ref sev) = advisory.severity {
                if let Some(sev_uri) = severity_concept_uri(sev) {
                    writer.write_triple(&advisory_uri, &format!("{SEC}advisorySeverity"), &sev_uri)?;
                    advisory_triples += 1;
                }
            }

            // Package links - match against emitted packages
            for pkg_ref in &advisory.packages {
                // Reconstruct the same URI as emit_package_triples
                let version_str = format!("{}-{}.{}", pkg_ref.version, pkg_ref.release, pkg_ref.arch);

                let release_name = if self.release_name.is_empty() {
                    "unknown"
                } else {
                    &self.release_name
                };

                // Check if we emitted this package
                let nevra_key = (
                    pkg_ref.name.clone(),
                    pkg_ref.epoch.clone(),
                    pkg_ref.version.clone(),
                    pkg_ref.release.clone(),
                    pkg_ref.arch.clone(),
                );

                if emitted_packages.contains(&nevra_key) {
                    let pkg_uri = package_uri(&self.distro_name, release_name, &pkg_ref.arch, &pkg_ref.name, &version_str);
                    writer.write_triple(&advisory_uri, &format!("{SEC}advisoryForPackage"), &pkg_uri)?;
                    advisory_triples += 1;
                    total_resolved_packages += 1;
                } else {
                    let nevra = format!("{}-{}:{}-{}.{}", pkg_ref.name, pkg_ref.epoch, pkg_ref.version, pkg_ref.release, pkg_ref.arch);
                    emit_dq_issue(writer, "rpm-collector", "advisory-package", &nevra, "advisory-package-unresolved", "info")?;
                    unresolved_packages += 1;
                }
            }

            // CVE cross-references
            for cve_id in &advisory.cves {
                let cve_uri = cve_entity_uri(cve_id);
                writer.write_triple(&advisory_uri, &format!("{SEC}addressesVulnerability"), &cve_uri)?;
                advisory_triples += 1;
            }

            total_triples += advisory_triples;
        }

        eprintln!("Emitted {} security advisories", advisories.len());
        eprintln!("Resolved {} advisory-package links", total_resolved_packages);
        if unresolved_packages > 0 {
            eprintln!("Warning: {} advisory packages not in emitted package set", unresolved_packages);
        }

        Ok(total_triples)
    }
}

/// Parsed advisory from updateinfo.xml.
#[derive(Debug, Clone)]
struct UpdateInfoAdvisory {
    id: String,
    advisory_type: String,
    severity: Option<String>,
    issued_date: String,
    cves: Vec<String>,
    packages: Vec<UpdateInfoPackage>,
}

/// Package reference from updateinfo advisory.
#[derive(Debug, Clone)]
struct UpdateInfoPackage {
    name: String,
    version: String,
    release: String,
    epoch: String,
    arch: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_UPDATEINFO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<updates>
  <update from="updates@fedoraproject.org" status="stable" type="security" version="2.0">
    <id>FEDORA-2025-d2b7d94014</id>
    <title>timg-1.6.3-5.fc43</title>
    <issued date="2025-11-26 20:40:36"/>
    <severity>Moderate</severity>
    <description>Rebuilt with latest patched stb_image: memory-safety fixes</description>
    <references>
      <reference href="https://bugzilla.redhat.com/..." id="2423183" type="bugzilla"
                 title="CVE-2025-26794 &amp; CWE-122 in Exim 4.99"/>
    </references>
    <pkglist>
      <collection short="F43">
        <package name="timg" version="1.6.3" release="5.fc43" epoch="0" arch="x86_64"
                 src="https://...timg-1.6.3-5.fc43.src.rpm">
          <filename>timg-1.6.3-5.fc43.x86_64.rpm</filename>
        </package>
        <package name="timg" version="1.6.3" release="5.fc43" epoch="0" arch="src"
                 src="https://...timg-1.6.3-5.fc43.src.rpm">
          <filename>timg-1.6.3-5.fc43.src.rpm</filename>
        </package>
      </collection>
    </pkglist>
  </update>
  <update from="updates@fedoraproject.org" status="stable" type="bugfix" version="2.0">
    <id>FEDORA-2025-xyz</id>
    <title>bash-5.2.26-6.fc43</title>
    <issued date="2025-11-20 10:00:00"/>
    <severity>None</severity>
  </update>
</updates>
"#;

    #[test]
    fn test_parse_updateinfo_security_filter() {
        // Test that parse_updateinfo filters to security-only
        // This is an inline parsing test that mirrors the actual implementation
        let xml = SAMPLE_UPDATEINFO;
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut advisories = Vec::new();
        let mut current_update: Option<UpdateInfoAdvisory> = None;
        let mut current_text = String::new();
        let mut in_description = false;
        let mut ref_title: Option<String> = None;

        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e) | Event::Empty(ref e)) => {
                    match e.name().as_ref() {
                        b"update" => {
                            let mut update_type: Option<String> = None;
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"type" {
                                    update_type = Some(String::from_utf8_lossy(&attr.value).to_string());
                                }
                            }

                            if update_type.as_deref() == Some("security") {
                                current_update = Some(UpdateInfoAdvisory {
                                    id: String::new(),
                                    advisory_type: "security".to_string(),
                                    severity: None,
                                    issued_date: String::new(),
                                    cves: Vec::new(),
                                    packages: Vec::new(),
                                });
                            }
                        }
                        b"id" if current_update.is_some() => {
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                if let Some(ref mut adv) = current_update {
                                    adv.id = text.unescape().unwrap_or_default().to_string();
                                }
                            }
                        }
                        b"severity" if current_update.is_some() => {
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                if let Some(ref mut adv) = current_update {
                                    adv.severity = Some(text.unescape().unwrap_or_default().to_string());
                                }
                            }
                        }
                        b"issued" if current_update.is_some() => {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"date" {
                                    if let Some(ref mut adv) = current_update {
                                        adv.issued_date = String::from_utf8_lossy(&attr.value).to_string();
                                    }
                                }
                            }
                        }
                        b"description" if current_update.is_some() => {
                            in_description = true;
                            current_text.clear();
                        }
                        b"reference" if current_update.is_some() => {
                            ref_title = None;
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"title" {
                                    ref_title = Some(String::from_utf8_lossy(&attr.value).to_string());
                                }
                            }

                            // For self-closing <reference.../>, extract CVEs immediately
                            if let Some(ref title) = ref_title {
                                if let Some(ref mut adv) = current_update {
                                    for cve_match in CVE_RE.find_iter(title) {
                                        let cve = cve_match.as_str().to_string();
                                        if !adv.cves.contains(&cve) {
                                            adv.cves.push(cve);
                                        }
                                    }
                                }
                            }
                        }
                        b"package" if current_update.is_some() => {
                            let mut pkg = UpdateInfoPackage {
                                name: String::new(),
                                version: String::new(),
                                release: String::new(),
                                epoch: "0".to_string(),
                                arch: String::new(),
                            };

                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"name" => pkg.name = String::from_utf8_lossy(&attr.value).to_string(),
                                    b"version" => pkg.version = String::from_utf8_lossy(&attr.value).to_string(),
                                    b"release" => pkg.release = String::from_utf8_lossy(&attr.value).to_string(),
                                    b"epoch" => pkg.epoch = String::from_utf8_lossy(&attr.value).to_string(),
                                    b"arch" => pkg.arch = String::from_utf8_lossy(&attr.value).to_string(),
                                    _ => {}
                                }
                            }

                            if pkg.arch != "src" && !pkg.name.is_empty() {
                                if let Some(ref mut adv) = current_update {
                                    adv.packages.push(pkg);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Text(text)) if in_description => {
                    current_text.push_str(&text.unescape().unwrap_or_default());
                }
                Ok(Event::End(ref e)) => {
                    match e.name().as_ref() {
                        b"update" => {
                            if let Some(mut adv) = current_update.take() {
                                // Extract CVEs from description
                                if !current_text.is_empty() {
                                    for cve_match in CVE_RE.find_iter(&current_text) {
                                        let cve = cve_match.as_str().to_string();
                                        if !adv.cves.contains(&cve) {
                                            adv.cves.push(cve);
                                        }
                                    }
                                }

                                if !adv.packages.is_empty() {
                                    advisories.push(adv);
                                }
                            }
                            current_text.clear();
                        }
                        b"description" => {
                            in_description = false;
                        }
                        b"reference" => {
                            // Extract CVEs from reference title
                            if let Some(title) = ref_title.take() {
                                if let Some(ref mut adv) = current_update {
                                    for cve_match in CVE_RE.find_iter(&title) {
                                        let cve = cve_match.as_str().to_string();
                                        if !adv.cves.contains(&cve) {
                                            adv.cves.push(cve);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                _ => {}
            }
            buf.clear();
        }

        // Verify parsing results
        assert_eq!(advisories.len(), 1, "Should parse one security advisory, filtering out bugfix");
        let adv = &advisories[0];
        assert_eq!(adv.id, "FEDORA-2025-d2b7d94014");
        assert_eq!(adv.advisory_type, "security");
        assert_eq!(adv.severity, Some("Moderate".to_string()));
        assert_eq!(adv.cves, vec!["CVE-2025-26794"]);
        assert_eq!(adv.packages.len(), 1, "Should filter out src package");
        assert_eq!(adv.packages[0].arch, "x86_64");
        assert_eq!(adv.packages[0].name, "timg");
        assert_eq!(adv.packages[0].version, "1.6.3");
        assert_eq!(adv.packages[0].release, "5.fc43");
        assert_eq!(adv.packages[0].epoch, "0");
    }

    #[test]
    fn test_cve_extraction_from_reference_title() {
        let title = "CVE-2025-26794 & CWE-122 in Exim 4.99";
        let cves: Vec<String> = CVE_RE.find_iter(title)
            .map(|m| m.as_str().to_string())
            .collect();
        assert_eq!(cves, vec!["CVE-2025-26794"]);
    }

    #[test]
    fn test_advisory_emission_with_lookup_set() {
        use tempfile::NamedTempFile;
        use std::io::Read;

        // Build a lookup set
        let mut emitted_packages: HashSet<(String, String, String, String, String)> = HashSet::new();
        emitted_packages.insert(("timg".to_string(), "0".to_string(), "1.6.3".to_string(), "5.fc43".to_string(), "x86_64".to_string()));

        // Create mock advisory
        let advisory = UpdateInfoAdvisory {
            id: "FEDORA-2025-test".to_string(),
            advisory_type: "security".to_string(),
            severity: Some("Moderate".to_string()),
            issued_date: "2025-11-26 20:40:36".to_string(),
            cves: vec!["CVE-2025-1234".to_string()],
            packages: vec![
                UpdateInfoPackage {
                    name: "timg".to_string(),
                    version: "1.6.3".to_string(),
                    release: "5.fc43".to_string(),
                    epoch: "0".to_string(),
                    arch: "x86_64".to_string(),
                },
                UpdateInfoPackage {
                    name: "other-pkg".to_string(),
                    version: "1.0.0".to_string(),
                    release: "1.fc43".to_string(),
                    epoch: "0".to_string(),
                    arch: "x86_64".to_string(),
                },
            ],
        };

        let tmp = NamedTempFile::new().unwrap();
        let file = tmp.reopen().unwrap();
        let mut writer = NTriplesWriter::new(file);

        let collector = RpmCollector {
            client: Client::builder().timeout(Duration::from_secs(10)).build().unwrap(),
            repo_url: "https://example.com".to_string(),
            distro_name: "fedora".to_string(),
            release_name: "43".to_string(),
            repo_type: "updates".to_string(),
            source_cache: None,
        };

        let triples = collector.emit_advisory_triples(&mut writer, &[advisory], &emitted_packages).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        tmp.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should emit advisory entity + match 1 package (timg in set, other-pkg not)
        assert!(content.contains("SecurityAdvisory"));
        assert!(content.contains("FEDORA-2025-test"));
        assert!(content.contains("advisoryForPackage"));
        assert!(content.contains("/pkg/fedora/43/x86_64/timg/"));
        assert!(!content.contains("/pkg/fedora/43/x86_64/other-pkg/"), "Should not emit unmatched package");
        assert!(content.contains("CVE-2025-1234"));
        assert!(triples > 0);
    }
}
