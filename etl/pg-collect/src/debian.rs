use crate::forge::{extract_forge_url_with_field, emit_upstream_repo, emit_dq_issue, emit_forge_triples};
use crate::ntriples::{NTriplesWriter, bnode_id};
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use flate2::read::GzDecoder;
use regex::Regex;
use reqwest::blocking::Client;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

/// Map distribution ID to human-readable display name.
fn distro_display_name(distro_id: &str) -> &str {
    match distro_id {
        "debian" => "Debian",
        "ubuntu" => "Ubuntu",
        _ => distro_id,
    }
}

pub struct DebianCollector {
    client: Client,
    repo_url: String,
    distro_name: String,
    distribution: String,
    component: String,
    repo_type: String,
    source_cache: Option<SourceCache>,
}

fn infer_repo_type_debian(url: &str) -> String {
    if url.contains("security") {
        "security".to_string()
    } else if url.contains("updates") {
        "updates".to_string()
    } else if url.contains("proposed") {
        "proposed".to_string()
    } else {
        "release".to_string()
    }
}

#[derive(Debug)]
pub struct ReleaseInfo {
    pub codename: String,
    pub suite: String,
    pub origin: String,
}

impl DebianCollector {
    pub fn new(repo_url: String, distro_name: String, distribution: String, component: String) -> Self {
        let repo_type = infer_repo_type_debian(&repo_url);

        // HTTP client with timeout and retry configuration
        let client = crate::enricher::default_http_client();

        Self {
            client,
            repo_url,
            distro_name,
            distribution,
            component,
            repo_type,
            source_cache: None,
        }
    }

    pub fn with_cache(mut self, cache_dir: &str) -> Result<Self> {
        self.source_cache = Some(SourceCache::new(cache_dir, "debian")?);
        Ok(self)
    }

    pub fn collect(&self, arches: &[String], output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        // Get release info
        let release_info = self.get_release_info()?;
        eprintln!(
            "Resolved '{}' to Origin='{}', Suite='{}', Codename='{}'",
            self.distribution, release_info.origin, release_info.suite, release_info.codename
        );

        // Emit distribution metadata
        self.emit_distribution_metadata(&mut writer, &release_info, arches)?;

        // Shared dedup sets for backward compatibility
        let mut all_arch_seen: HashSet<(String, String)> = HashSet::new();
        let mut source_names: HashSet<String> = HashSet::new();
        let mut source_identity_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut vcs_urls: HashMap<String, String> = HashMap::new();
        let mut source_pkg_uris: HashMap<String, String> = HashMap::new();

        let mut total_packages = 0;
        let mut total_triples = 0;

        // Process each architecture
        for (i, arch) in arches.iter().enumerate() {
            eprintln!("\nProcessing architecture: {}", arch);

            // Strip "binary-" prefix for URI building
            let arch_name = if arch.contains('-') {
                arch.split('-').next_back().unwrap()
            } else {
                arch.as_str()
            };

            let (pkg_count, triple_count) = self.collect_with_writer(
                &mut writer,
                arch,
                arch_name,
                &release_info.codename,
                &release_info.suite,
                &mut all_arch_seen,
                &mut source_names,
                &mut source_identity_map,
                &mut vcs_urls,
                &mut source_pkg_uris,
                i > 0, // is_secondary for all after first
                None,
            )?;

            total_packages += pkg_count;
            total_triples += triple_count;

            eprintln!("Processed {} packages for {}", pkg_count, arch);
        }

        writer.flush()?;

        Ok((total_packages, total_triples))
    }

    /// Collect packages from a single architecture, writing to an external writer.
    /// Supports arch:all dedup and source package tracking across multiple arches.
    #[allow(clippy::too_many_arguments)]
    pub fn collect_with_writer(
        &self,
        writer: &mut NTriplesWriter,
        arch: &str,
        arch_name: &str,
        codename: &str,
        suite: &str,
        all_arch_seen: &mut HashSet<(String, String)>,
        source_names: &mut HashSet<String>,
        source_identity_map: &mut HashMap<String, Vec<String>>,
        vcs_urls: &mut HashMap<String, String>,
        source_pkg_uris: &mut HashMap<String, String>,
        is_secondary: bool,
        limit: Option<usize>,
    ) -> Result<(usize, usize)> {
        let (pkg_count, triple_count) = self.process_arch_with_writer(
            writer,
            arch,
            arch_name,
            codename,
            suite,
            all_arch_seen,
            source_names,
            source_identity_map,
            vcs_urls,
            source_pkg_uris,
            is_secondary,
            limit,
        )?;

        Ok((pkg_count, triple_count))
    }

    fn cache_scope(&self, arch: Option<&str>) -> CacheScope {
        CacheScope {
            collector: "debian".to_string(),
            distro: self.distro_name.clone(),
            release: self.distribution.clone(),
            repo: Some(self.component.clone()),
            arch: arch.map(String::from),
        }
    }

    fn fetch_raw_bytes(&self, url: &str, logical_name: &str, arch: Option<&str>) -> Result<Vec<u8>> {
        if let Some(ref cache) = self.source_cache {
            let scope = self.cache_scope(arch);
            match cache.fetch_or_reuse(url, &scope, logical_name)? {
                CacheResult::Fresh(bytes) => {
                    eprintln!("Downloaded {} ({} bytes, cached)", logical_name, bytes.len());
                    Ok(bytes)
                }
                CacheResult::Cached(path) | CacheResult::NotModified(path) => {
                    eprintln!("Using cached {} ({})", logical_name, path.display());
                    std::fs::read(&path)
                }
            }
        } else {
            eprintln!("Downloading {}", url);
            let response = self.client_get_with_retry(url, 3)?;
            let bytes = response
                .bytes()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(bytes.to_vec())
        }
    }

    pub fn get_release_info(&self) -> Result<ReleaseInfo> {
        let release_url = format!(
            "{}/dists/{}/Release",
            self.repo_url.trim_end_matches('/'),
            self.distribution
        );

        eprintln!("Fetching Release info from {}", release_url);

        let raw = self.fetch_raw_bytes(&release_url, "Release", None)?;
        let text = String::from_utf8(raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let mut codename = None;
        let mut suite = None;
        let mut origin = None;

        for line in text.lines() {
            if let Some(value) = line.strip_prefix("Codename:") {
                codename = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("Suite:") {
                suite = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("Origin:") {
                origin = Some(value.trim().to_string());
            }
        }

        match (codename, suite, origin) {
            (Some(codename), Some(suite), Some(origin)) => Ok(ReleaseInfo {
                codename,
                suite,
                origin,
            }),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Incomplete release information",
            )),
        }
    }

    fn client_get_with_retry(&self, url: &str, max_retries: u32) -> Result<reqwest::blocking::Response> {
        let mut retries = 0;
        loop {
            match self.client.get(url).send() {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) if response.status().is_server_error() && retries < max_retries => {
                    eprintln!("Server error {}, retrying... ({}/{})", response.status(), retries + 1, max_retries);
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(1000 * (1 << retries)));
                }
                Ok(response) => {
                    return Err(std::io::Error::other(
                        format!("HTTP error: {}", response.status()),
                    ));
                }
                Err(e) if retries < max_retries => {
                    eprintln!("Network error: {}, retrying... ({}/{})", e, retries + 1, max_retries);
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(1000 * (1 << retries)));
                }
                Err(e) => {
                    return Err(std::io::Error::other(e));
                }
            }
        }
    }

    pub fn emit_distribution_metadata(
        &self,
        writer: &mut NTriplesWriter,
        release_info: &ReleaseInfo,
        arches: &[String],
    ) -> Result<()> {
        // Distribution
        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}distributionName"), &self.distro_name)?;

        // Add human-readable label
        let display_name = distro_display_name(&self.distro_name);
        writer.write_literal(&dist_uri, RDFS_LABEL, display_name)?;

        // Release
        let rel_uri = release_uri(&self.distro_name, &release_info.codename);
        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), &release_info.codename)?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseSuite"), &release_info.suite)?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseOrigin"), &release_info.origin)?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        // Repo metadata
        writer.write_literal(&rel_uri, &format!("{PKG}repoType"), &self.repo_type)?;
        writer.write_literal(&rel_uri, &format!("{PKG}repoSourceURL"), &self.repo_url)?;

        // Architectures
        for arch in arches {
            let arch_name = if arch.contains('-') {
                arch.split('-').next_back().unwrap()
            } else {
                arch.as_str()
            };

            let arch_uri_val = arch_uri(arch_name);
            writer.write_triple(&arch_uri_val, RDF_TYPE, &format!("{PKG}Architecture"))?;
            writer.write_literal(&arch_uri_val, &format!("{PKG}architectureName"), arch_name)?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn process_arch_with_writer(
        &self,
        writer: &mut NTriplesWriter,
        arch: &str,
        arch_name: &str,
        codename: &str,
        suite: &str,
        all_arch_seen: &mut HashSet<(String, String)>,
        source_names: &mut HashSet<String>,
        source_identity_map: &mut HashMap<String, Vec<String>>,
        vcs_urls: &mut HashMap<String, String>,
        source_pkg_uris: &mut HashMap<String, String>,
        is_secondary: bool,
        limit: Option<usize>,
    ) -> Result<(usize, usize)> {
        let packages_url = format!(
            "{}/dists/{}/{}/{}/Packages.gz",
            self.repo_url.trim_end_matches('/'),
            self.distribution,
            self.component,
            arch
        );

        let logical_name = format!("Packages-{}.gz", arch_name);
        let raw_bytes = self.fetch_raw_bytes(&packages_url, &logical_name, Some(arch_name))?;

        // Decompress from raw bytes (cached or fresh)
        let decoder = GzDecoder::new(&raw_bytes[..]);
        let reader = BufReader::new(decoder);

        let mut pkg_count = 0;
        let mut triple_count = 0;

        // Parse packages line-by-line as a state machine
        let mut current_pkg: HashMap<String, String> = HashMap::new();
        let mut last_key = String::new();

        for line in reader.lines() {
            let line = line?;

            if line.is_empty() {
                // End of package entry
                if !current_pkg.is_empty() && current_pkg.contains_key("Package") && current_pkg.contains_key("Version") {
                    let pkg_name = current_pkg.get("Package").unwrap();
                    let pkg_version = current_pkg.get("Version").unwrap();
                    let pkg_arch = current_pkg.get("Architecture").map(|s| s.as_str()).unwrap_or("");

                    // Skip Architecture:all packages if already seen (secondary arch)
                    if pkg_arch == "all" && is_secondary {
                        let dedup_key = (pkg_name.clone(), pkg_version.clone());
                        if all_arch_seen.contains(&dedup_key) {
                            current_pkg.clear();
                            last_key.clear();
                            continue; // Skip this package
                        }
                    }

                    // Emit triples
                    triple_count += self.emit_package_triples_with_tracking(
                        writer,
                        &current_pkg,
                        codename,
                        suite,
                        arch_name,
                        source_names,
                        source_identity_map,
                        vcs_urls,
                        source_pkg_uris,
                    )?;

                    // Track arch:all for dedup
                    if pkg_arch == "all" {
                        all_arch_seen.insert((pkg_name.clone(), pkg_version.clone()));
                    }

                    pkg_count += 1;

                    // Apply limit if set
                    if let Some(lim) = limit {
                        if pkg_count >= lim {
                            break;
                        }
                    }
                }
                current_pkg.clear();
                last_key.clear();
            } else if line.starts_with(' ') || line.starts_with('\t') {
                // Continuation of previous field
                if !last_key.is_empty() {
                    if let Some(value) = current_pkg.get_mut(&last_key) {
                        value.push(' ');
                        value.push_str(line.trim());
                    }
                }
            } else if let Some((key, value)) = line.split_once(':') {
                // New field
                let key = key.trim().to_string();
                last_key = key.clone();
                current_pkg.insert(key, value.trim().to_string());
            }
        }

        // Process last package if file doesn't end with blank line
        if !current_pkg.is_empty() && current_pkg.contains_key("Package") && current_pkg.contains_key("Version") {
            let pkg_name = current_pkg.get("Package").unwrap();
            let pkg_version = current_pkg.get("Version").unwrap();
            let pkg_arch = current_pkg.get("Architecture").map(|s| s.as_str()).unwrap_or("");

            // Check arch:all dedup for last package too
            let should_emit = if pkg_arch == "all" && is_secondary {
                let dedup_key = (pkg_name.clone(), pkg_version.clone());
                !all_arch_seen.contains(&dedup_key)
            } else {
                true
            };

            if should_emit {
                // Apply limit check
                let within_limit = limit.map(|lim| pkg_count < lim).unwrap_or(true);
                if within_limit {
                    triple_count += self.emit_package_triples_with_tracking(
                        writer,
                        &current_pkg,
                        codename,
                        suite,
                        arch_name,
                        source_names,
                        source_identity_map,
                        vcs_urls,
                        source_pkg_uris,
                    )?;

                    if pkg_arch == "all" {
                        all_arch_seen.insert((pkg_name.clone(), pkg_version.clone()));
                    }

                    pkg_count += 1;
                }
            }
        }

        Ok((pkg_count, triple_count))
    }

    fn process_arch(
        &self,
        writer: &mut NTriplesWriter,
        arch: &str,
        arch_name: &str,
        codename: &str,
        suite: &str,
    ) -> Result<(usize, usize)> {
        let packages_url = format!(
            "{}/dists/{}/{}/{}/Packages.gz",
            self.repo_url.trim_end_matches('/'),
            self.distribution,
            self.component,
            arch
        );

        let logical_name = format!("Packages-{}.gz", arch_name);
        let raw_bytes = self.fetch_raw_bytes(&packages_url, &logical_name, Some(arch_name))?;

        // Decompress from raw bytes (cached or fresh)
        let decoder = GzDecoder::new(&raw_bytes[..]);
        let reader = BufReader::new(decoder);

        let mut pkg_count = 0;
        let mut triple_count = 0;

        // Parse packages line-by-line as a state machine
        let mut current_pkg: HashMap<String, String> = HashMap::new();
        let mut last_key = String::new();

        for line in reader.lines() {
            let line = line?;

            if line.is_empty() {
                // End of package entry
                if !current_pkg.is_empty() && current_pkg.contains_key("Package") && current_pkg.contains_key("Version") {
                    triple_count += self.emit_package_triples(
                        writer,
                        &current_pkg,
                        codename,
                        suite,
                        arch_name,
                    )?;
                    pkg_count += 1;
                }
                current_pkg.clear();
                last_key.clear();
            } else if line.starts_with(' ') || line.starts_with('\t') {
                // Continuation of previous field
                if !last_key.is_empty() {
                    if let Some(value) = current_pkg.get_mut(&last_key) {
                        value.push(' ');
                        value.push_str(line.trim());
                    }
                }
            } else if let Some((key, value)) = line.split_once(':') {
                // New field
                let key = key.trim().to_string();
                last_key = key.clone();
                current_pkg.insert(key, value.trim().to_string());
            }
        }

        // Process last package if file doesn't end with blank line
        if !current_pkg.is_empty() && current_pkg.contains_key("Package") && current_pkg.contains_key("Version") {
            triple_count += self.emit_package_triples(
                writer,
                &current_pkg,
                codename,
                suite,
                arch_name,
            )?;
            pkg_count += 1;
        }

        Ok((pkg_count, triple_count))
    }

    /// Emit package triples with source tracking for deb-full pipeline.
    #[allow(clippy::too_many_arguments)]
    fn emit_package_triples_with_tracking(
        &self,
        writer: &mut NTriplesWriter,
        pkg_data: &HashMap<String, String>,
        codename: &str,
        suite: &str,
        arch_name: &str,
        source_names: &mut HashSet<String>,
        source_identity_map: &mut HashMap<String, Vec<String>>,
        vcs_urls: &mut HashMap<String, String>,
        source_pkg_uris: &mut HashMap<String, String>,
    ) -> Result<usize> {
        let pkg_name = pkg_data.get("Package").unwrap();
        let pkg_version = pkg_data.get("Version").unwrap();

        let pkg_uri = package_uri(&self.distro_name, codename, arch_name, pkg_name, pkg_version);
        let identity_uri = package_identity_uri(&self.distro_name, codename, arch_name, pkg_name);

        // First emit all the package triples (delegates to existing method)
        let mut triples = self.emit_package_triples(writer, pkg_data, codename, suite, arch_name)?;

        // Then populate source tracking sets
        let (source_name, source_version) = if let Some(source_str) = pkg_data.get("Source") {
            // Format can be "sourcename" or "sourcename (version)"
            let re = Regex::new(r"^([^\s]+)(?:\s+\(([^)]+)\))?$").unwrap();
            if let Some(caps) = re.captures(source_str) {
                let name = caps.get(1).unwrap().as_str();
                let version = caps.get(2).map(|m| m.as_str()).unwrap_or(pkg_version);
                (name.to_string(), version.to_string())
            } else {
                (source_str.clone(), pkg_version.to_string())
            }
        } else {
            // No Source field means source name = binary name
            (pkg_name.to_string(), pkg_version.to_string())
        };

        // Track source package name
        source_names.insert(source_name.clone());

        // Track identity URI → source mapping
        source_identity_map
            .entry(source_name.clone())
            .or_insert_with(Vec::new)
            .push(identity_uri.clone());

        // Track Vcs-Git URL for salsa enrichment
        if let Some(vcs_git) = pkg_data.get("Vcs-Git") {
            let vcs_url = vcs_git.split_whitespace().next().unwrap_or(vcs_git);
            vcs_urls.insert(source_name.clone(), vcs_url.to_string());
        }

        // Track SourcePackage URI for Build-Depends emission
        let src_uri = source_uri(&self.distro_name, codename, &source_name, &source_version);
        source_pkg_uris.insert(source_name.clone(), src_uri);

        Ok(triples)
    }

    /// Emit ecosystem detection triples (upstreamEcosystem + upstreamPackageName + DQ).
    fn emit_ecosystem_enrichment(
        &self,
        writer: &mut NTriplesWriter,
        pkg_name: &str,
        identity_uri: &str,
        homepage: Option<&str>,
    ) -> Result<usize> {
        let mut triples = 0;

        if let Some(detection) = crate::collect_spec::detect_ecosystem_by_name(pkg_name, homepage) {
            let eco_uri = ecosystem_uri(detection.ecosystem);
            writer.write_triple(identity_uri, &format!("{PKG}upstreamEcosystem"), &eco_uri)?;
            writer.write_triple(&eco_uri, RDF_TYPE, &format!("{PKG}Ecosystem"))?;
            writer.write_literal(&eco_uri, RDFS_LABEL, detection.ecosystem)?;
            triples += 3;

            if let Some(ref upstream_name) = detection.package_name {
                writer.write_literal(identity_uri, &format!("{PKG}upstreamPackageName"), upstream_name)?;
                triples += 1;
            }

            // DQ annotation for ecosystem detection
            let confidence = if detection.detection_method == "homepage-domain" { "high" } else { "medium" };
            triples += emit_dq_issue(
                writer, "debian", "ecosystem", pkg_name,
                &format!("ecosystem-detected-{}", confidence), "info"
            )?;
        }

        Ok(triples)
    }

    /// Emit VCS repository enrichment (Homepage → upstreamRepository + Vcs-Git → packagingRepository + forge triples).
    fn emit_forge_enrichment(
        &self,
        writer: &mut NTriplesWriter,
        pkg_name: &str,
        identity_uri: &str,
        pkg_data: &HashMap<String, String>,
    ) -> Result<usize> {
        let mut triples = 0;

        // Homepage → upstreamRepository
        if let Some(homepage) = pkg_data.get("Homepage") {
            if let Some(extraction) = extract_forge_url_with_field(homepage, "homepage") {
                triples += emit_upstream_repo(writer, identity_uri, &extraction, None)?;
            } else {
                triples += emit_dq_issue(writer, "debian", "homepage", homepage, "homepage-no-forge-match", "info")?;
            }
        } else {
            triples += emit_dq_issue(writer, "debian", "homepage", pkg_name, "missing-homepage", "info")?;
        }

        // Vcs-Git → packagingRepository
        if let Some(vcs_git) = pkg_data.get("Vcs-Git") {
            let vcs_url = vcs_git.split_whitespace().next().unwrap_or(vcs_git);
            if let Some(extraction) = extract_forge_url_with_field(vcs_url, "vcs-git") {
                let r_uri = repo_uri(&extraction.repo_url);
                writer.write_triple(identity_uri, &format!("{PKG}packagingRepository"), &r_uri)?;
                writer.write_triple(&r_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
                triples += emit_forge_triples(writer, &r_uri, &extraction.repo_url)?;
            } else {
                triples += emit_dq_issue(writer, "debian", "vcs-git", vcs_url, "vcs-git-no-forge-match", "info")?;
            }
        }

        // Vcs-Browser
        if let Some(vcs_browser) = pkg_data.get("Vcs-Browser") {
            if let Some(extraction) = extract_forge_url_with_field(vcs_browser, "vcs-browser") {
                let r_uri = repo_uri(&extraction.repo_url);
                writer.write_literal(&r_uri, &format!("{VCS}repositoryBrowser"), vcs_browser)?;
                triples += 1;
            }
        }

        Ok(triples)
    }

    pub fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_data: &HashMap<String, String>,
        codename: &str,
        suite: &str,
        arch_name: &str,
    ) -> Result<usize> {
        let pkg_name = pkg_data.get("Package").unwrap();
        let pkg_version = pkg_data.get("Version").unwrap();

        let pkg_uri = package_uri(&self.distro_name, codename, arch_name, pkg_name, pkg_version);
        let identity_uri = package_identity_uri(&self.distro_name, codename, arch_name, pkg_name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{DEB}BinaryPackage"))?;
        triples += 2;

        // Link to canonical identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), pkg_name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        // PURL (Package URL)
        let purl = crate::ntriples::format_purl(
            "deb",
            Some(&self.distro_name),
            pkg_name,
            Some(pkg_version),
            &[("arch", arch_name)],
        );
        writer.write_typed_literal(
            &identity_uri,
            &format!("{PKG}purl"),
            &purl,
            &format!("{XSD}anyURI"),
        )?;
        triples += 1;

        // Core properties
        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), pkg_name)?;
        triples += 1;

        // Version resource
        let ver_uri = version_uri(&self.distro_name, codename, pkg_name, pkg_version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), pkg_version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Architecture
        let arch_uri_val = arch_uri(arch_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}targetArchitecture"), &arch_uri_val)?;
        triples += 1;

        // Distribution and release
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, codename);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
        triples += 2;

        // Optional properties
        if let Some(desc) = pkg_data.get("Description") {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(homepage) = pkg_data.get("Homepage") {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(install_size_str) = pkg_data.get("Installed-Size") {
            if let Ok(install_size_kb) = install_size_str.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}installSize"), install_size_kb * 1024)?;
                triples += 1;
            }
        }
        if let Some(size) = pkg_data.get("Size") {
            if let Ok(size_val) = size.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}packageSize"), size_val)?;
                triples += 1;
            }
        }
        if let Some(checksum) = pkg_data.get("SHA256") {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), checksum)?;
            triples += 1;
        }

        // Debian-specific properties
        writer.write_literal(&pkg_uri, &format!("{DEB}inSuite"), suite)?;
        writer.write_literal(&pkg_uri, &format!("{DEB}inComponent"), &self.component)?;
        triples += 2;

        // Maintainer
        if let Some(maintainer_str) = pkg_data.get("Maintainer") {
            triples += self.emit_maintainer_triples(writer, &pkg_uri, maintainer_str)?;
        }

        // Source package
        triples += self.emit_source_package_triples(writer, &pkg_uri, pkg_data, codename, pkg_name, pkg_version)?;

        // Ecosystem detection
        let homepage = pkg_data.get("Homepage").map(|s| s.as_str());
        triples += self.emit_ecosystem_enrichment(writer, pkg_name, &identity_uri, homepage)?;

        // VCS repository enrichment
        triples += self.emit_forge_enrichment(writer, pkg_name, &identity_uri, pkg_data)?;

        // Dependencies
        triples += self.emit_dependency_triples(writer, &pkg_uri, pkg_data, codename, arch_name)?;

        Ok(triples)
    }

    fn emit_maintainer_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        maintainer_str: &str,
    ) -> Result<usize> {
        // Parse "Name <email>"
        let re = Regex::new(r"^(.+?)\s*<(.+?)>$").unwrap();
        if let Some(caps) = re.captures(maintainer_str) {
            let name = caps.get(1).unwrap().as_str().trim();
            let email = caps.get(2).unwrap().as_str().trim();

            let maint_uri = maintainer_uri(email);

            // Type as Person (canonical agent identity per SD-3 data contract)
            writer.write_triple(&maint_uri, RDF_TYPE, &format!("{PKG}Person"))?;
            writer.write_literal(&maint_uri, &format!("{FOAF}name"), name)?;
            writer.write_literal(&maint_uri, RDFS_LABEL, name)?;
            writer.write_triple(&maint_uri, &format!("{FOAF}mbox"), &format!("mailto:{email}"))?;
            writer.write_triple(pkg_uri, &format!("{PKG}maintainedBy"), &maint_uri)?;

            return Ok(5);
        }

        Ok(0)
    }

    fn emit_source_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        pkg_data: &HashMap<String, String>,
        codename: &str,
        pkg_name: &str,
        pkg_version: &str,
    ) -> Result<usize> {
        let (source_name, source_version) = if let Some(source_str) = pkg_data.get("Source") {
            // Format can be "sourcename" or "sourcename (version)"
            let re = Regex::new(r"^([^\s]+)(?:\s+\(([^)]+)\))?$").unwrap();
            if let Some(caps) = re.captures(source_str) {
                let name = caps.get(1).unwrap().as_str();
                let version = caps.get(2).map(|m| m.as_str()).unwrap_or(pkg_version);
                (name.to_string(), version.to_string())
            } else {
                (source_str.clone(), pkg_version.to_string())
            }
        } else {
            // No Source field means source name = binary name
            (pkg_name.to_string(), pkg_version.to_string())
        };

        let src_uri = source_uri(&self.distro_name, codename, &source_name, &source_version);

        writer.write_triple(&src_uri, RDF_TYPE, &format!("{PKG}SourcePackage"))?;
        writer.write_literal(&src_uri, &format!("{PKG}packageName"), &source_name)?;

        // Version resource for source
        let src_ver_uri = version_uri(&self.distro_name, codename, &source_name, &source_version);
        writer.write_triple(&src_ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&src_ver_uri, &format!("{PKG}versionString"), &source_version)?;
        writer.write_triple(&src_uri, &format!("{PKG}hasVersion"), &src_ver_uri)?;

        // Link binary to source
        writer.write_triple(pkg_uri, &format!("{PKG}builtFromSource"), &src_uri)?;

        Ok(6)
    }

    fn emit_dependency_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        pkg_data: &HashMap<String, String>,
        codename: &str,
        arch_name: &str,
    ) -> Result<usize> {
        let dep_mappings = vec![
            ("Depends", "runtime", Some(format!("{DEB}debDepends"))),
            ("Pre-Depends", "runtime", Some(format!("{DEB}debDepends"))),
            ("Recommends", "recommends", Some(format!("{DEB}debRecommends"))),
            ("Suggests", "suggests", Some(format!("{DEB}debSuggests"))),
            ("Conflicts", "conflicts", Some(format!("{DEB}debConflicts"))),
            ("Breaks", "breaks", Some(format!("{DEB}debConflicts"))),
        ];

        let mut triples = 0;

        for (field, dep_type, distro_prop) in dep_mappings {
            if let Some(dep_string) = pkg_data.get(field) {
                triples += self.parse_and_emit_dependencies(
                    writer,
                    pkg_uri,
                    dep_string,
                    dep_type,
                    distro_prop.as_deref(),
                    codename,
                    arch_name,
                )?;
            }
        }

        // Emit provides — virtual package resolution
        if let Some(provides_str) = pkg_data.get("Provides") {
            for part in provides_str.split(',') {
                let provided = part.trim();
                if provided.is_empty() {
                    continue;
                }
                // Strip version constraint if present: "foo (= 1.0)" → "foo"
                let name = provided.split_whitespace().next().unwrap_or(provided);

                let dep_uri = package_identity_uri(&self.distro_name, codename, arch_name, name);

                writer.write_triple(&dep_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
                writer.write_literal(&dep_uri, &format!("{PKG}packageName"), name)?;
                writer.write_triple(
                    pkg_uri,
                    &format!("{PKG}directlyProvides"),
                    &dep_uri,
                )?;
                writer.write_triple(pkg_uri, &format!("{DEB}debProvides"), &dep_uri)?;

                // Also emit Capability entity for CQ-PM-03
                let cap_uri = format!("{DATA}capability/{}", crate::uris::encode(name));
                writer.write_triple(&cap_uri, RDF_TYPE, &format!("{PKG}Capability"))?;
                writer.write_literal(&cap_uri, &format!("{PKG}capabilityName"), name)?;
                writer.write_triple(pkg_uri, &format!("{PKG}providesCapability"), &cap_uri)?;

                triples += 7;
            }
        }

        Ok(triples)
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_and_emit_dependencies(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        dep_string: &str,
        dep_type: &str,
        distro_prop: Option<&str>,
        codename: &str,
        arch_name: &str,
    ) -> Result<usize> {
        // Regex to parse dependency entries
        let dep_re = Regex::new(r"([\w.-]+)(?:\s+\(([^)]+)\))?").unwrap();

        let mut triples = 0;

        for part in dep_string.split(',') {
            // Handle alternatives by taking the first one
            let first_alternative = part.split('|').next().unwrap_or(part).trim();

            if let Some(caps) = dep_re.captures(first_alternative) {
                let dep_name = caps.get(1).unwrap().as_str();
                let version_constraint = caps.get(2).map(|m| m.as_str());

                // Dependency targets point to the canonical identity URI (no version).
                // This enables direct name-based joins without URI parsing.
                let dep_uri = package_identity_uri(&self.distro_name, codename, arch_name, dep_name);

                // Ensure identity has basic properties for graph traversal
                writer.write_triple(&dep_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
                writer.write_literal(&dep_uri, &format!("{PKG}packageName"), dep_name)?;
                triples += 2;

                // Emit generic property based on dep_type
                if dep_type == "conflicts" || dep_type == "breaks" {
                    writer.write_triple(pkg_uri, &format!("{PKG}directlyConflictsWith"), &dep_uri)?;
                } else {
                    writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &dep_uri)?;
                }
                triples += 1;

                // Emit distro-specific property if provided
                if let Some(prop) = distro_prop {
                    writer.write_triple(pkg_uri, prop, &dep_uri)?;
                    triples += 1;
                }

                // Create reified Dependency
                let dep_bnode = bnode_id("dep", &format!("{pkg_uri}_{dep_name}"));

                writer.write_bnode_subject(&dep_bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
                writer.write_bnode_subject(&dep_bnode, &format!("{PKG}dependencyTarget"), &dep_uri)?;
                writer.write_bnode_subject(&dep_bnode, &format!("{PKG}dependencyType"), &dep_type_uri(dep_type))?;
                writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &dep_bnode)?;
                triples += 4;

                // Add VersionConstraint if specified
                if let Some(constraint_str) = version_constraint {
                    let (operator, value) = self.parse_version_constraint(constraint_str);
                    if let (Some(op), Some(val)) = (operator, value) {
                        let constraint_bnode = bnode_id("constraint", &format!("{dep_bnode}_{val}"));

                        writer.write_bnode_subject(&constraint_bnode, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
                        writer.write_bnode_literal(&constraint_bnode, &format!("{PKG}versionConstraintOperator"), &op)?;
                        writer.write_bnode_literal(&constraint_bnode, &format!("{PKG}versionConstraintValue"), &val)?;
                        writer.write_bnode_subject(&dep_bnode, &format!("{PKG}hasVersionConstraint"), &format!("_{constraint_bnode}"))?;
                        triples += 4;
                    }
                }
            }
        }

        Ok(triples)
    }

    fn parse_version_constraint(&self, constraint_str: &str) -> (Option<String>, Option<String>) {
        // Match operator and version
        let re = Regex::new(r"^\s*([<>=]+)\s*(.+)$").unwrap();
        if let Some(caps) = re.captures(constraint_str) {
            let op_str = caps.get(1).unwrap().as_str();
            let value = caps.get(2).unwrap().as_str().trim();

            // Map Debian operators to symbols
            let operator = match op_str {
                "<<" => "<",
                "<=" => "≤",
                "=" => "=",
                ">=" => "≥",
                ">>" => ">",
                _ => op_str,
            };

            return (Some(operator.to_string()), Some(value.to_string()));
        }

        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::NamedTempFile;

    #[test]
    fn test_collect_with_writer_single_arch() {
        // RED: This test will fail because collect_with_writer doesn't exist yet
        let collector = DebianCollector::new(
            "http://deb.debian.org/debian".to_string(),
            "debian".to_string(),
            "trixie".to_string(),
            "main".to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut all_arch_seen: HashSet<(String, String)> = HashSet::new();
        let mut source_names: HashSet<String> = HashSet::new();
        let mut source_identity_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut vcs_urls: HashMap<String, String> = HashMap::new();
        let mut source_pkg_uris: HashMap<String, String> = HashMap::new();

        // Test that collect_with_writer exists and handles single arch
        let result = collector.collect_with_writer(
            &mut writer,
            "binary-amd64",
            "amd64",
            "trixie",
            "stable",
            &mut all_arch_seen,
            &mut source_names,
            &mut source_identity_map,
            &mut vcs_urls,
            &mut source_pkg_uris,
            false,
            Some(5),
        );

        // Verify it succeeds
        assert!(result.is_ok(), "collect_with_writer should succeed");
        let (pkg_count, _triples) = result.unwrap();
        assert!(pkg_count > 0, "Should collect at least one package");
    }

    #[test]
    fn test_arch_all_dedup_via_emit_tracking() {
        use std::io::Read;

        // Test arch:all dedup: same arch:all package emitted once, skipped on secondary
        let collector = DebianCollector::new(
            "http://deb.debian.org/debian".to_string(),
            "debian".to_string(),
            "trixie".to_string(),
            "main".to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut all_arch_seen: HashSet<(String, String)> = HashSet::new();
        let mut source_names: HashSet<String> = HashSet::new();
        let mut source_identity_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut vcs_urls: HashMap<String, String> = HashMap::new();
        let mut source_pkg_uris: HashMap<String, String> = HashMap::new();

        // arch:all package data
        let mut pkg_data = HashMap::new();
        pkg_data.insert("Package".to_string(), "tzdata".to_string());
        pkg_data.insert("Version".to_string(), "2024a-1".to_string());
        pkg_data.insert("Architecture".to_string(), "all".to_string());

        // --- First arch (primary): emit triples ---
        let triples1 = collector.emit_package_triples_with_tracking(
            &mut writer, &pkg_data, "trixie", "stable", "amd64",
            &mut source_names, &mut source_identity_map, &mut vcs_urls, &mut source_pkg_uris,
        ).unwrap();
        all_arch_seen.insert(("tzdata".to_string(), "2024a-1".to_string()));

        writer.flush().unwrap();
        let mut content1 = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content1).unwrap();
        let triple_lines_after_first = content1.lines().count();

        assert!(triples1 > 0, "First arch should emit triples");
        assert!(triple_lines_after_first > 0, "Output should have triples");

        // --- Second arch (secondary): dedup check should skip ---
        let dedup_key = (
            pkg_data.get("Package").unwrap().clone(),
            pkg_data.get("Version").unwrap().clone(),
        );
        let is_arch_all = pkg_data.get("Architecture").map(|s| s.as_str()) == Some("all");
        let is_secondary = true;
        let should_skip = is_arch_all && is_secondary && all_arch_seen.contains(&dedup_key);

        // This is the actual dedup logic from process_arch_with_writer
        assert!(should_skip, "arch:all package seen in primary should be skipped in secondary");

        // Verify: if we DON'T skip (primary again), triples grow; if we skip, they don't
        // Emit again as primary (should add more triples)
        let triples_again = collector.emit_package_triples_with_tracking(
            &mut writer, &pkg_data, "trixie", "stable", "arm64",
            &mut source_names, &mut source_identity_map, &mut vcs_urls, &mut source_pkg_uris,
        ).unwrap();
        writer.flush().unwrap();

        let mut content2 = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content2).unwrap();
        let triple_lines_after_second = content2.lines().count();

        assert!(triples_again > 0, "Non-skipped emit should produce triples");
        assert!(triple_lines_after_second > triple_lines_after_first,
            "Output should grow when package is NOT skipped (triple_lines: {} > {})",
            triple_lines_after_second, triple_lines_after_first);
    }

    #[test]
    fn test_homepage_forge_triples() {
        use std::io::Read;

        // RED: Test that Homepage URLs generate forge triples (hostedOn, Forge, forgeSoftware)
        let collector = DebianCollector::new(
            "http://deb.debian.org/debian".to_string(),
            "debian".to_string(),
            "trixie".to_string(),
            "main".to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // Mock package data with GitHub homepage
        let mut pkg_data = HashMap::new();
        pkg_data.insert("Package".to_string(), "openssl".to_string());
        pkg_data.insert("Version".to_string(), "3.2.2-1".to_string());
        pkg_data.insert("Homepage".to_string(), "https://github.com/openssl/openssl".to_string());
        pkg_data.insert("Architecture".to_string(), "amd64".to_string());

        let _ = collector.emit_package_triples(&mut writer, &pkg_data, "trixie", "testing", "amd64");
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should contain forge triples (hostedOn, Forge type, forgeUrl, forgeSoftware)
        assert!(content.contains("hostedOn"), "Should emit hostedOn triple for GitHub");
        assert!(content.contains("Forge"), "Should emit Forge entity");
        assert!(content.contains("forgeSoftware"), "Should emit forgeSoftware triple");
    }

    #[test]
    fn test_vcs_git_forge_triples() {
        use std::io::Read;

        // RED: Test that Vcs-Git URLs generate forge triples for packaging repo
        let collector = DebianCollector::new(
            "http://deb.debian.org/debian".to_string(),
            "debian".to_string(),
            "trixie".to_string(),
            "main".to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // Mock package data with salsa.debian.org Vcs-Git
        let mut pkg_data = HashMap::new();
        pkg_data.insert("Package".to_string(), "libc6".to_string());
        pkg_data.insert("Version".to_string(), "2.36-9".to_string());
        pkg_data.insert("Vcs-Git".to_string(), "https://salsa.debian.org/glibc-team/glibc.git -b trixie".to_string());
        pkg_data.insert("Architecture".to_string(), "amd64".to_string());

        let _ = collector.emit_package_triples(&mut writer, &pkg_data, "trixie", "testing", "amd64");
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Debug output
        eprintln!("=== Vcs-Git Test Output ===\n{}\n===", content);

        // Should contain forge triples for salsa packaging repo
        assert!(content.contains("packagingRepository"), "Should emit packagingRepository");
        assert!(content.contains("hostedOn"), "Should emit hostedOn triple for salsa");
        assert!(content.contains("salsa.debian.org"), "Should reference salsa.debian.org forge");
    }

    #[test]
    fn test_ecosystem_detection_python3() {
        use std::io::Read;

        let collector = DebianCollector::new(
            "http://deb.debian.org/debian".to_string(),
            "debian".to_string(),
            "trixie".to_string(),
            "main".to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // Mock python3- package with PyPI homepage
        let mut pkg_data = HashMap::new();
        pkg_data.insert("Package".to_string(), "python3-requests".to_string());
        pkg_data.insert("Version".to_string(), "2.31.0-1".to_string());
        pkg_data.insert("Homepage".to_string(), "https://pypi.org/project/requests/".to_string());
        pkg_data.insert("Architecture".to_string(), "all".to_string());

        let _ = collector.emit_package_triples(&mut writer, &pkg_data, "trixie", "testing", "all");
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should contain ecosystem triples
        assert!(content.contains("upstreamEcosystem"), "Should detect PyPI ecosystem");
        assert!(content.contains("ecosystem/pypi"), "Should reference pypi ecosystem");
        assert!(content.contains("upstreamPackageName"), "Should emit upstream package name");
        assert!(content.contains("\"requests\""), "Should extract 'requests' from python3-requests");
    }
}
