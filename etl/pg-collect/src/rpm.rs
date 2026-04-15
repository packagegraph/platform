use crate::ntriples::{NTriplesWriter, bnode_id};
use crate::uris::*;
use flate2::read::GzDecoder;
use quick_xml::events::Event;
use quick_xml::Reader;
use regex::Regex;
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Result};
use std::time::Duration;

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
}

impl RpmCollector {
    pub fn new(repo_url: String, distro_name: String, release_name: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            repo_url,
            distro_name,
            release_name,
        }
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

        Self {
            client,
            repo_url,
            distro_name,
            release_name,
        }
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        // Emit distribution metadata
        self.emit_distribution_metadata(&mut writer)?;

        // Get primary metadata URL
        let primary_url = self.get_metadata_url("primary")?;
        eprintln!("Primary metadata URL: {}", primary_url);

        // Download and parse
        let packages_data = self.parse_primary_metadata(&primary_url)?;
        eprintln!("Found {} packages", packages_data.len());

        let mut total_triples = 0;
        for (idx, pkg_data) in packages_data.iter().enumerate() {
            total_triples += self.emit_package_triples(&mut writer, pkg_data)?;

            if (idx + 1) % 1000 == 0 {
                eprintln!("Processed {} packages", idx + 1);
            }
        }

        writer.flush()?;

        Ok((packages_data.len(), total_triples))
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

        let response = self.client_get_with_retry(&repomd_url, 3)?;
        let content = response
            .bytes()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

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

    fn download_and_decompress(&self, url: &str) -> Result<Vec<u8>> {
        eprintln!("Downloading {}", url);
        let response = self.client_get_with_retry(url, 3)?;

        let content = response
            .bytes()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        if url.ends_with(".gz") {
            let mut decoder = GzDecoder::new(&content[..]);
            let mut decompressed = Vec::new();
            std::io::copy(&mut decoder, &mut decompressed)?;
            Ok(decompressed)
        } else if url.ends_with(".zst") {
            let decompressed = zstd::decode_all(&content[..])
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(decompressed)
        } else {
            Ok(content.to_vec())
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

        if !self.release_name.is_empty() {
            let rel_uri = release_uri(&self.distro_name, &self.release_name);
            writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
            writer.write_literal(
                &rel_uri,
                &format!("{PKG}releaseCodename"),
                &self.release_name,
            )?;
            writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        }

        Ok(())
    }

    pub fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_data: &RpmPackageData,
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

        // Packaging repository (dist-git — derivable from distro + package name)
        let distgit_uri = fedora_distgit_uri(&self.distro_name, name);
        writer.write_triple(&identity_uri, &format!("{PKG}packagingRepository"), &distgit_uri)?;
        writer.write_triple(&distgit_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
        triples += 2;

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
            } else if let Some(r_name) = name.strip_prefix("R(").and_then(|s| s.strip_suffix(')')) {
                ("cran", r_name.to_string())
            } else {
                continue;
            };

            // Emit ecosystem and upstream name (once per ecosystem per package)
            if !emitted_ecosystem {
                writer.write_literal(pkg_uri, &format!("{PKG}upstreamEcosystem"), ecosystem)?;
                emitted_ecosystem = true;
                triples += 1;
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
        if let Some(caps) = re.captures(packager) {
            let name = caps.get(1).unwrap().as_str().trim();
            let email = caps.get(2).unwrap().as_str().trim();

            let maint_uri = maintainer_uri(email);

            writer.write_triple(&maint_uri, RDF_TYPE, &format!("{PKG}Maintainer"))?;
            writer.write_literal(&maint_uri, &format!("{FOAF}name"), name)?;
            writer.write_triple(
                &maint_uri,
                &format!("{FOAF}mbox"),
                &format!("mailto:{email}"),
            )?;
            writer.write_triple(pkg_uri, &format!("{PKG}maintainedBy"), &maint_uri)?;

            return Ok(4);
        }

        Ok(0)
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
            writer.write_bnode_literal(&dep_bnode, &format!("{PKG}dependencyType"), "runtime")?;
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
            triples += 4;
        }

        Ok(triples)
    }
}
