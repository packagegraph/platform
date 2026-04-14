use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use flate2::read::GzDecoder;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Result};
use std::time::Duration;

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

    fn get_metadata_url(&self, metadata_type: &str) -> Result<String> {
        let repomd_url = format!("{}/repodata/repomd.xml", self.repo_url.trim_end_matches('/'));
        eprintln!("Fetching repomd.xml from {}", repomd_url);

        let response = self.client_get_with_retry(&repomd_url, 3)?;
        let content = response.bytes().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let mut reader = Reader::from_reader(&content[..]);
        reader.config_mut().trim_text(true);

        let mut buf = Vec::new();
        let mut in_correct_data = false;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) if e.name().as_ref() == b"data" => {
                    // Check if this is the correct metadata type
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"type" && attr.value.as_ref() == metadata_type.as_bytes() {
                            in_correct_data = true;
                            break;
                        }
                    }
                }
                Ok(Event::Start(ref e) | Event::Empty(ref e)) if in_correct_data && e.name().as_ref() == b"location" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"href" {
                            let href = String::from_utf8_lossy(&attr.value).to_string();
                            return Ok(format!("{}/{}", self.repo_url.trim_end_matches('/'), href));
                        }
                    }
                }
                Ok(Event::End(ref e)) if e.name().as_ref() == b"data" => {
                    in_correct_data = false;
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                _ => {}
            }
            buf.clear();
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Metadata type '{}' not found in repomd.xml", metadata_type),
        ))
    }

    fn download_and_decompress(&self, url: &str) -> Result<Vec<u8>> {
        eprintln!("Downloading {}", url);
        let response = self.client_get_with_retry(url, 3)?;

        let content = response.bytes().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

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

    fn parse_primary_metadata(&self, primary_url: &str) -> Result<Vec<HashMap<String, String>>> {
        let content = self.download_and_decompress(primary_url)?;

        let mut reader = Reader::from_reader(BufReader::new(&content[..]));
        reader.config_mut().trim_text(true);

        let mut packages = Vec::new();
        let mut current_pkg: HashMap<String, String> = HashMap::new();
        let mut buf = Vec::new();
        let mut current_text = String::new();
        let mut in_package = false;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e) | Event::Empty(ref e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "package" {
                        in_package = true;
                        current_pkg = HashMap::new();
                    } else if in_package {
                        current_text.clear();
                        // version, location, and other self-closing elements
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let value = String::from_utf8_lossy(&attr.value).to_string();
                            if name == "version" || name == "location" || name == "size" || name == "time" {
                                current_pkg.insert(key, value);
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
                        if !current_pkg.is_empty() {
                            packages.push(current_pkg.clone());
                        }
                        in_package = false;
                    } else if in_package && !current_text.is_empty() {
                        current_pkg.insert(name, current_text.trim().to_string());
                        current_text.clear();
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                _ => {}
            }
            buf.clear();
        }

        Ok(packages)
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<()> {
        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}distributionName"), &self.distro_name)?;

        if !self.release_name.is_empty() {
            let rel_uri = release_uri(&self.distro_name, &self.release_name);
            writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
            writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), &self.release_name)?;
            writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        }

        Ok(())
    }

    pub fn emit_package_triples(&self, writer: &mut NTriplesWriter, pkg_data: &HashMap<String, String>) -> Result<usize> {
        let name = pkg_data.get("name").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing package name")
        })?;
        let arch = pkg_data.get("arch").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing arch")
        })?;
        let ver = pkg_data.get("ver").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing version")
        })?;
        let rel = pkg_data.get("rel").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing release")
        })?;

        let epoch = pkg_data.get("epoch").map(|s| s.as_str()).unwrap_or("0");
        let version_str = format!("{}-{}.{}", ver, rel, arch);

        let release_name = if self.release_name.is_empty() { "unknown" } else { &self.release_name };

        let pkg_uri = package_uri(&self.distro_name, release_name, arch, name, &version_str);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{RPM}BinaryRPM"))?;
        triples += 2;

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
        if let Some(desc) = pkg_data.get("description").or_else(|| pkg_data.get("summary")) {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }

        // RPM-specific properties
        if let Some(sourcerpm) = pkg_data.get("sourcerpm") {
            writer.write_literal(&pkg_uri, &format!("{RPM}sourceRPM"), sourcerpm)?;
            triples += 1;
        }

        if let Some(group) = pkg_data.get("group") {
            writer.write_literal(&pkg_uri, &format!("{RPM}RPMGroup"), group)?;
            triples += 1;
        }

        if epoch != "0" {
            if let Ok(epoch_int) = epoch.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{RPM}epoch"), epoch_int)?;
                triples += 1;
            }
        }

        Ok(triples)
    }
}
