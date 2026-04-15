use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

pub struct CpanCollector {
    client: Client,
    api_base: String,
}

#[derive(Debug, Deserialize)]
struct MetaCpanRelease {
    distribution: String,
    version: String,
    #[serde(rename = "abstract")]
    abstract_text: Option<String>,
    author: String,
    status: Option<String>,
    license: Option<Vec<String>>,
    #[serde(default)]
    dependency: Vec<MetaCpanDependency>,
    resources: Option<MetaCpanResources>,
}

#[derive(Debug, Deserialize)]
struct MetaCpanDependency {
    module: String,
    phase: String,
    relationship: String,
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MetaCpanResources {
    homepage: Option<String>,
    repository: Option<MetaCpanRepo>,
}

#[derive(Debug, Deserialize)]
struct MetaCpanRepo {
    url: Option<String>,
}

impl CpanCollector {
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

        let package_names = read_cpan_seed_file(packages_file)?;
        eprintln!("Loaded {} CPAN distribution names from seed file", package_names.len());

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 100;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, package_names.len());
            }

            match self.fetch_release_with_retry(name, &mut base_delay_ms) {
                Ok(release) => {
                    total_triples += self.emit_distribution_triples(&mut writer, &release)?;
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
        let dist_uri = distro_uri("cpan");
        let rel_uri = release_uri("cpan", "cpan");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "CPAN")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "cpan")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_release_with_retry(
        &self,
        name: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<MetaCpanRelease, String> {
        let url = format!("{}/v1/release/{}", self.api_base, name);
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

                        eprintln!("  Rate limited on {}, waiting {}s...", name, retry_after_secs);
                        std::thread::sleep(Duration::from_secs(retry_after_secs));
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

    fn emit_distribution_triples(
        &self,
        writer: &mut NTriplesWriter,
        release: &MetaCpanRelease,
    ) -> Result<usize> {
        let pkg_uri = package_uri("cpan", "cpan", "any", &release.distribution, &release.version);
        let identity_uri = package_identity_uri("cpan", "cpan", "any", &release.distribution);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{CPAN}Distribution"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &release.distribution)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &release.distribution)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("cpan", "cpan", &release.distribution, &release.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &release.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("cpan");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // CPAN-specific properties
        writer.write_literal(&pkg_uri, &format!("{CPAN}authorPAUSEID"), &release.author)?;
        triples += 1;

        if let Some(abstract_text) = &release.abstract_text {
            writer.write_literal(&pkg_uri, &format!("{CPAN}abstractText"), abstract_text)?;
            triples += 1;
        }
        if let Some(status) = &release.status {
            writer.write_literal(&pkg_uri, &format!("{CPAN}maturity"), status)?;
            triples += 1;
        }
        if let Some(licenses) = &release.license {
            for license in licenses {
                writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
                triples += 1;
            }
        }
        if let Some(resources) = &release.resources {
            if let Some(homepage) = &resources.homepage {
                writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
                triples += 1;
            }
            if let Some(repo) = &resources.repository {
                if let Some(repo_url) = &repo.url {
                    let repo_uri = repo_uri(repo_url);
                    writer.write_triple(&pkg_uri, &format!("{PKG}hasRepository"), &repo_uri)?;
                    triples += 1;
                }
            }
        }

        // Dependencies (runtime phase, requires relationship)
        for dep in &release.dependency {
            if dep.phase == "runtime" && dep.relationship == "requires" {
                let target_uri = package_identity_uri("cpan", "cpan", "any", &dep.module);
                writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
                triples += 1;
            }
        }

        Ok(triples)
    }
}

pub fn read_cpan_seed_file(path: &str) -> Result<Vec<String>> {
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
    fn test_read_cpan_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment").unwrap();
        writeln!(temp, "Moose").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "DBI").unwrap();
        temp.flush().unwrap();

        let names = read_cpan_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "DBI");
        assert_eq!(names[1], "Moose");
    }

    #[test]
    fn test_metacpan_release_deserialization() {
        let json = r#"{
            "distribution": "Moose",
            "version": "2.2206",
            "abstract": "A postmodern object system for Perl 5",
            "author": "ETHER",
            "status": "latest",
            "license": ["perl_5"],
            "dependency": []
        }"#;

        let release: MetaCpanRelease = serde_json::from_str(json).unwrap();
        assert_eq!(release.distribution, "Moose");
        assert_eq!(release.version, "2.2206");
        assert_eq!(release.author, "ETHER");
    }

    #[test]
    fn test_emit_cpan_distribution_dual_typing() {
        let collector = CpanCollector::new("https://fastapi.metacpan.org".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let release = MetaCpanRelease {
            distribution: "DBI".to_string(),
            version: "1.643".to_string(),
            abstract_text: Some("Database independent interface for Perl".to_string()),
            author: "TIMB".to_string(),
            status: Some("latest".to_string()),
            license: Some(vec!["perl_5".to_string()]),
            dependency: vec![],
            resources: None,
        };

        let triples = collector.emit_distribution_triples(&mut writer, &release).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("cpan#Distribution"));
        assert!(content.contains("\"DBI\""));
        assert!(content.contains("\"1.643\""));
        assert!(content.contains("cpan#authorPAUSEID"));
        assert!(content.contains("\"TIMB\""));
        assert!(triples > 10);
    }
}
