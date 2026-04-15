use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

pub struct HackageCollector {
    client: Client,
    base_url: String,
}

#[derive(Debug, Deserialize)]
struct PreferredInfo {
    normal: Vec<String>,
}

#[derive(Debug, Default)]
struct CabalMetadata {
    name: String,
    version: String,
    synopsis: Option<String>,
    description: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
    category: Option<String>,
    maintainer: Option<String>,
    author: Option<String>,
    bug_reports: Option<String>,
    build_depends: Vec<String>,
}

impl HackageCollector {
    pub fn new(base_url: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, base_url }
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let package_names = read_hackage_seed_file(packages_file)?;
        eprintln!("Loaded {} Hackage package names from seed file", package_names.len());

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 200;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, package_names.len());
            }

            match self.fetch_package_with_retry(name, &mut base_delay_ms) {
                Ok(cabal) => {
                    total_triples += self.emit_package_triples(&mut writer, &cabal)?;
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
        let dist_uri = distro_uri("hackage");
        let rel_uri = release_uri("hackage", "hackage");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Hackage")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "hackage")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_package_with_retry(
        &self,
        name: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<CabalMetadata, String> {
        let preferred_url = format!("{}/package/{}/preferred", self.base_url, name);
        let max_attempts = 5;

        // Get preferred version
        let mut version = String::new();
        for attempt in 0..max_attempts {
            match self.client.get(&preferred_url).send() {
                Ok(response) => {
                    if response.status() == StatusCode::NOT_FOUND {
                        return Err(format!("404: {}", name));
                    }
                    let text = response.text().map_err(|e| e.to_string())?;
                    let pref: PreferredInfo = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                    if let Some(ver) = pref.normal.first() {
                        version = ver.clone();
                        break;
                    } else {
                        return Err(format!("No versions for {}", name));
                    }
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        std::thread::sleep(Duration::from_millis(1000 * 2u64.pow(attempt as u32)));
                        continue;
                    }
                    return Err(e.to_string());
                }
            }
        }

        if version.is_empty() {
            return Err(format!("Failed to get version for {}", name));
        }

        // Fetch .cabal file
        let cabal_url = format!("{}/package/{}-{}/{}.cabal", self.base_url, name, version, name);

        for attempt in 0..max_attempts {
            match self.client.get(&cabal_url).send() {
                Ok(response) => {
                    if response.status() == StatusCode::TOO_MANY_REQUESTS {
                        let retry_secs = 2u64.pow(attempt as u32);
                        eprintln!("  Rate limited, waiting {}s...", retry_secs);
                        std::thread::sleep(Duration::from_secs(retry_secs));
                        *base_delay_ms = (*base_delay_ms * 2).min(5000);
                        continue;
                    }

                    let text = response.text().map_err(|e| e.to_string())?;
                    return self.parse_cabal(&text, name, &version);
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        std::thread::sleep(Duration::from_millis(1000 * 2u64.pow(attempt as u32)));
                        continue;
                    }
                    return Err(e.to_string());
                }
            }
        }

        Err(format!("Max retries exceeded for {}", name))
    }

    fn parse_cabal(&self, cabal_text: &str, name: &str, version: &str) -> std::result::Result<CabalMetadata, String> {
        let mut cabal = CabalMetadata {
            name: name.to_string(),
            version: version.to_string(),
            ..Default::default()
        };

        let mut current_field = String::new();
        let mut current_value = String::new();

        for line in cabal_text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("--") {
                continue;
            }

            if line.starts_with(' ') || line.starts_with('\t') {
                // Continuation
                current_value.push(' ');
                current_value.push_str(trimmed);
            } else if let Some((key, value)) = line.split_once(':') {
                // Flush previous
                if !current_field.is_empty() {
                    self.set_cabal_field(&mut cabal, &current_field, &current_value);
                }
                current_field = key.trim().to_lowercase();
                current_value = value.trim().to_string();
            }
        }

        if !current_field.is_empty() {
            self.set_cabal_field(&mut cabal, &current_field, &current_value);
        }

        Ok(cabal)
    }

    fn set_cabal_field(&self, cabal: &mut CabalMetadata, key: &str, value: &str) {
        match key {
            "synopsis" => cabal.synopsis = Some(value.to_string()),
            "description" => cabal.description = Some(value.to_string()),
            "license" => cabal.license = Some(value.to_string()),
            "homepage" => cabal.homepage = Some(value.to_string()),
            "category" => cabal.category = Some(value.to_string()),
            "maintainer" => cabal.maintainer = Some(value.to_string()),
            "author" => cabal.author = Some(value.to_string()),
            "bug-reports" => cabal.bug_reports = Some(value.to_string()),
            "build-depends" => cabal.build_depends = self.parse_build_depends(value),
            _ => {}
        }
    }

    fn parse_build_depends(&self, value: &str) -> Vec<String> {
        value
            .split(',')
            .map(|s| {
                // Strip version constraints like "base >=4.12 && <5"
                s.trim()
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty() && s != "base")
            .collect()
    }

    fn emit_package_triples(&self, writer: &mut NTriplesWriter, cabal: &CabalMetadata) -> Result<usize> {
        let pkg_uri = package_uri("hackage", "hackage", "any", &cabal.name, &cabal.version);
        let identity_uri = package_identity_uri("hackage", "hackage", "any", &cabal.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{HACKAGE}HackagePackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &cabal.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &cabal.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("hackage", "hackage", &cabal.name, &cabal.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &cabal.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("hackage");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(synopsis) = &cabal.synopsis {
            writer.write_literal(&pkg_uri, &format!("{HACKAGE}synopsis"), synopsis)?;
            triples += 1;
        }
        if let Some(desc) = &cabal.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(license) = &cabal.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
        }
        if let Some(homepage) = &cabal.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(category) = &cabal.category {
            writer.write_literal(&pkg_uri, &format!("{HACKAGE}category"), category)?;
            triples += 1;
        }
        if let Some(maintainer) = &cabal.maintainer {
            writer.write_literal(&pkg_uri, &format!("{HACKAGE}maintainer"), maintainer)?;
            triples += 1;
        }

        // Dependencies
        for dep_name in &cabal.build_depends {
            let target = package_identity_uri("hackage", "hackage", "any", dep_name);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target)?;
            triples += 1;
        }

        Ok(triples)
    }
}

pub fn read_hackage_seed_file(path: &str) -> Result<Vec<String>> {
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
    fn test_parse_build_depends() {
        let collector = HackageCollector::new("https://hackage.haskell.org".into());
        let result = collector.parse_build_depends("base >=4.12 && <5, text, bytestring >=0.10");
        assert_eq!(result, vec!["text", "bytestring"]);
    }

    #[test]
    fn test_parse_cabal() {
        let cabal_text = "name: aeson\nversion: 2.2.1.0\nsynopsis: Fast JSON parsing\nlicense: BSD3\nhomepage: https://github.com/haskell/aeson\ncategory: Web\nmaintainer: Adam Bergmark\nbuild-depends: base >=4.12, text, bytestring\n";

        let collector = HackageCollector::new("https://hackage.haskell.org".into());
        let cabal = collector.parse_cabal(cabal_text, "aeson", "2.2.1.0").unwrap();

        assert_eq!(cabal.name, "aeson");
        assert_eq!(cabal.version, "2.2.1.0");
        assert_eq!(cabal.synopsis, Some("Fast JSON parsing".to_string()));
        assert_eq!(cabal.license, Some("BSD3".to_string()));
        assert_eq!(cabal.build_depends, vec!["text", "bytestring"]);
    }

    #[test]
    fn test_emit_hackage_package() {
        let collector = HackageCollector::new("https://hackage.haskell.org".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let cabal = CabalMetadata {
            name: "aeson".to_string(),
            version: "2.2.1.0".to_string(),
            synopsis: Some("Fast JSON parsing".to_string()),
            license: Some("BSD3".to_string()),
            category: Some("Web".to_string()),
            build_depends: vec!["text".to_string()],
            ..Default::default()
        };

        let triples = collector.emit_package_triples(&mut writer, &cabal).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("hackage#HackagePackage"));
        assert!(content.contains("\"aeson\""));
        assert!(content.contains("hackage#category"));
        assert!(triples > 10);
    }
}
