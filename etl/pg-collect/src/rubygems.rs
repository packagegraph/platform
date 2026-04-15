use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

pub struct RubyGemsCollector {
    client: Client,
    api_base: String,
}

#[derive(Debug, Deserialize)]
struct GemDoc {
    name: String,
    version: String,
    info: Option<String>,
    licenses: Option<Vec<String>>,
    homepage_uri: Option<String>,
    source_code_uri: Option<String>,
    project_uri: Option<String>,
    gem_uri: Option<String>,
    sha: Option<String>,
    authors: Option<String>,
    downloads: Option<u64>,
    platform: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GemsVersionsResponse {
    #[serde(default)]
    versions: Vec<GemVersion>,
}

#[derive(Debug, Deserialize)]
struct GemVersion {
    number: String,
    #[serde(default)]
    prerelease: bool,
    yanked: Option<bool>,
    sha: Option<String>,
    #[serde(default)]
    dependencies: GemDependencies,
}

#[derive(Debug, Deserialize, Default)]
struct GemDependencies {
    #[serde(default)]
    runtime: Vec<GemDependency>,
    #[serde(default)]
    development: Vec<GemDependency>,
}

#[derive(Debug, Deserialize)]
struct GemDependency {
    name: String,
    requirements: String,
}

impl RubyGemsCollector {
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

        let package_names = read_seed_file(packages_file)?;
        eprintln!("Loaded {} gem names from seed file", package_names.len());

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 100;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, package_names.len());
            }

            match self.fetch_gem_with_retry(name, &mut base_delay_ms) {
                Ok(gem) => {
                    total_triples += self.emit_gem_triples(&mut writer, &gem)?;
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
        let dist_uri = distro_uri("rubygems");
        let rel_uri = release_uri("rubygems", "org");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "RubyGems.org")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "org")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_gem_with_retry(
        &self,
        name: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<GemDoc, String> {
        let url = format!("{}/api/v1/gems/{}.json", self.api_base, name);
        let max_attempts = 5;

        for attempt in 0..max_attempts {
            match self.client.get(&url).send() {
                Ok(response) => {
                    if response.status() == StatusCode::TOO_MANY_REQUESTS {
                        let retry_after_secs = response
                            .headers()
                            .get("retry-after")
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or_else(|| 2u64.pow(attempt as u32));

                        let delay_ms = retry_after_secs * 1000;
                        eprintln!("  Rate limited on {}, waiting {}s...", name, retry_after_secs);
                        std::thread::sleep(Duration::from_millis(delay_ms));
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

    fn emit_gem_triples(&self, writer: &mut NTriplesWriter, gem: &GemDoc) -> Result<usize> {
        let pkg_uri = package_uri("rubygems", "org", "any", &gem.name, &gem.version);
        let identity_uri = package_identity_uri("rubygems", "org", "any", &gem.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{GEMS}Gem"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &gem.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &gem.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("rubygems", "org", &gem.name, &gem.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &gem.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("rubygems");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(info) = &gem.info {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), info)?;
            triples += 1;
        }
        if let Some(homepage) = &gem.homepage_uri {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(source_code_uri) = &gem.source_code_uri {
            writer.write_literal(&pkg_uri, &format!("{GEMS}sourceCodeUri"), source_code_uri)?;
            triples += 1;
        }
        if let Some(project_uri) = &gem.project_uri {
            writer.write_literal(&pkg_uri, &format!("{GEMS}projectUri"), project_uri)?;
            triples += 1;
        }
        if let Some(gem_uri) = &gem.gem_uri {
            writer.write_literal(&pkg_uri, &format!("{GEMS}gemUri"), gem_uri)?;
            triples += 1;
        }
        if let Some(authors) = &gem.authors {
            writer.write_literal(&pkg_uri, &format!("{GEMS}authors"), authors)?;
            triples += 1;
        }
        if let Some(platform) = &gem.platform {
            writer.write_literal(&pkg_uri, &format!("{GEMS}platform"), platform)?;
            triples += 1;
        }
        if let Some(sha) = &gem.sha {
            writer.write_literal(&pkg_uri, &format!("{GEMS}sha256"), sha)?;
            triples += 1;
        }
        if let Some(downloads) = gem.downloads {
            writer.write_integer(&pkg_uri, &format!("{GEMS}downloads"), downloads as i64)?;
            triples += 1;
        }
        if let Some(licenses) = &gem.licenses {
            for license in licenses {
                writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
                triples += 1;
            }
        }

        Ok(triples)
    }
}

/// Read gem names from a seed file (one per line).
pub fn read_seed_file(path: &str) -> Result<Vec<String>> {
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
    fn test_read_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment line").unwrap();
        writeln!(temp, "rails").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "nokogiri").unwrap();
        writeln!(temp, "rails").unwrap();
        temp.flush().unwrap();

        let names = read_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "nokogiri");
        assert_eq!(names[1], "rails");
    }

    #[test]
    fn test_gem_doc_deserialization() {
        let json = r#"{
            "name": "rails",
            "version": "7.1.3",
            "info": "Full-stack web framework",
            "licenses": ["MIT"],
            "homepage_uri": "https://rubyonrails.org",
            "source_code_uri": "https://github.com/rails/rails",
            "project_uri": "https://rubyonrails.org",
            "gem_uri": "https://rubygems.org/gems/rails-7.1.3.gem",
            "sha": "abc123...",
            "authors": "DHH",
            "downloads": 424242,
            "platform": "ruby"
        }"#;

        let gem: GemDoc = serde_json::from_str(json).unwrap();
        assert_eq!(gem.name, "rails");
        assert_eq!(gem.version, "7.1.3");
        assert_eq!(gem.platform.unwrap(), "ruby");
    }

    #[test]
    fn test_emit_gem_dual_typing() {
        let collector = RubyGemsCollector::new("https://rubygems.org".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let gem = GemDoc {
            name: "nokogiri".into(),
            version: "1.15.5".into(),
            info: Some("HTML/XML parser".into()),
            licenses: Some(vec!["MIT".into()]),
            homepage_uri: Some("https://nokogiri.org".into()),
            source_code_uri: Some("https://github.com/sparklemotion/nokogiri".into()),
            project_uri: Some("https://nokogiri.org".into()),
            gem_uri: Some("https://rubygems.org/gems/nokogiri-1.15.5.gem".into()),
            sha: Some("abc123".into()),
            authors: Some("Mike Dalessio".into()),
            downloads: Some(1000000),
            platform: Some("ruby".into()),
        };

        let triples = collector.emit_gem_triples(&mut writer, &gem).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("rubygems#Gem"));
        assert!(content.contains("\"nokogiri\""));
        assert!(content.contains("\"1.15.5\""));
        assert!(content.contains("rubygems#platform"));
        assert!(content.contains("rubygems#sha256"));
        assert!(triples > 15);
    }
}
