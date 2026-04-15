use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

pub struct HexCollector {
    client: Client,
    api_base: String,
}

#[derive(Debug, Deserialize)]
struct HexPackageResponse {
    name: String,
    #[serde(default)]
    releases: Vec<HexRelease>,
    meta: Option<HexMeta>,
}

#[derive(Debug, Deserialize)]
struct HexRelease {
    version: String,
    #[serde(default)]
    requirements: HashMap<String, HexRequirement>,
    checksum: Option<String>,
    #[serde(default)]
    retired: bool,
}

#[derive(Debug, Deserialize)]
struct HexRequirement {
    requirement: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HexMeta {
    description: Option<String>,
    licenses: Option<Vec<String>>,
    links: Option<HashMap<String, String>>,
}

impl HexCollector {
    pub fn new(api_base: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, api_base }
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let package_names = read_hex_seed_file(packages_file)?;
        eprintln!("Loaded {} Hex package names from seed file", package_names.len());

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 600;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, package_names.len());
            }

            match self.fetch_package_with_retry(name, &mut base_delay_ms) {
                Ok(pkg) => {
                    total_triples += self.emit_package_triples(&mut writer, &pkg)?;
                    total_packages += 1;
                }
                Err(e) => eprintln!("  Error fetching {}: {}", name, e),
            }

            std::thread::sleep(Duration::from_millis(base_delay_ms));
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("hex");
        let rel_uri = release_uri("hex", "pm");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Hex.pm")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "pm")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_package_with_retry(
        &self,
        name: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<HexPackageResponse, String> {
        let url = format!("{}/api/packages/{}", self.api_base, name);
        let max_attempts = 5;

        for attempt in 0..max_attempts {
            match self.client.get(&url).send() {
                Ok(response) => {
                    if response.status() == StatusCode::TOO_MANY_REQUESTS {
                        let retry_secs = 2u64.pow(attempt as u32);
                        eprintln!("  Rate limited on {}, waiting {}s...", name, retry_secs);
                        std::thread::sleep(Duration::from_secs(retry_secs));
                        *base_delay_ms = (*base_delay_ms * 2).min(5000);
                        continue;
                    }

                    if response.status() == StatusCode::NOT_FOUND {
                        return Err(format!("404: {}", name));
                    }

                    let text = response.text().map_err(|e| e.to_string())?;
                    return serde_json::from_str(&text).map_err(|e| e.to_string());
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        let delay = Duration::from_millis(1000 * 2u64.pow(attempt as u32));
                        eprintln!("  Network error on {}, retrying in {:?}...", name, delay);
                        std::thread::sleep(delay);
                        continue;
                    }
                    return Err(e.to_string());
                }
            }
        }

        Err(format!("Max retries exceeded for {}", name))
    }

    fn emit_package_triples(&self, writer: &mut NTriplesWriter, pkg: &HexPackageResponse) -> Result<usize> {
        // Get latest non-retired release
        let release = pkg
            .releases
            .iter()
            .find(|r| !r.retired)
            .or_else(|| pkg.releases.first())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "No releases"))?;

        let pkg_uri = package_uri("hex", "pm", "any", &pkg.name, &release.version);
        let identity_uri = package_identity_uri("hex", "pm", "any", &pkg.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{HEX}HexPackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &pkg.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("hex", "pm", &pkg.name, &release.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &release.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("hex");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(meta) = &pkg.meta {
            if let Some(desc) = &meta.description {
                writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
                triples += 1;
            }
            if let Some(licenses) = &meta.licenses {
                for license in licenses {
                    writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
                    triples += 1;
                }
            }
            if let Some(links) = &meta.links {
                if let Some(homepage) = links.get("Homepage").or_else(|| links.get("GitHub")) {
                    writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
                    triples += 1;
                }
            }
        }

        if let Some(checksum) = &release.checksum {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), checksum)?;
            triples += 1;
        }

        // Dependencies
        for (dep_name, _req) in &release.requirements {
            let target = package_identity_uri("hex", "pm", "any", dep_name);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target)?;
            triples += 1;
        }

        Ok(triples)
    }
}

pub fn read_hex_seed_file(path: &str) -> Result<Vec<String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut names = Vec::new();

    for line_result in reader.lines() {
        let line = line_result?;
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        names.push(trimmed.to_string());
    }

    names.sort();
    names.dedup();

    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_hex_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment").unwrap();
        writeln!(temp, "phoenix").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "ecto").unwrap();
        temp.flush().unwrap();

        let names = read_hex_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "ecto");
        assert_eq!(names[1], "phoenix");
    }

    #[test]
    fn test_hex_package_deserialization() {
        let json = r#"{
            "name": "phoenix",
            "releases": [{"version": "1.7.11", "requirements": {}, "checksum": "abc123"}],
            "meta": {
                "description": "Web framework",
                "licenses": ["MIT"],
                "links": {"Homepage": "https://www.phoenixframework.org"}
            }
        }"#;

        let pkg: HexPackageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(pkg.name, "phoenix");
        assert_eq!(pkg.releases[0].version, "1.7.11");
    }

    #[test]
    fn test_emit_hex_package() {
        let collector = HexCollector::new("https://hex.pm".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg = HexPackageResponse {
            name: "ecto".to_string(),
            releases: vec![HexRelease {
                version: "3.11.1".to_string(),
                requirements: HashMap::new(),
                checksum: Some("abc123".to_string()),
                retired: false,
            }],
            meta: Some(HexMeta {
                description: Some("Database wrapper".to_string()),
                licenses: Some(vec!["Apache-2.0".to_string()]),
                links: None,
            }),
        };

        let triples = collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("hex#HexPackage"));
        assert!(content.contains("\"ecto\""));
        assert!(content.contains("\"3.11.1\""));
        assert!(triples > 10);
    }
}
