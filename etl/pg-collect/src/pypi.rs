use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::npm::read_seed_file;
use crate::uris::*;
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct PypiCollector {
    client: Client,
}

#[derive(Debug, Deserialize)]
struct PypiProjectResponse {
    info: PypiInfo,
}

#[derive(Debug, Deserialize)]
struct PypiInfo {
    name: String,
    version: String,
    summary: Option<String>,
    license: Option<String>,
    home_page: Option<String>,
    requires_python: Option<String>,
    requires_dist: Option<Vec<String>>,
    classifiers: Option<Vec<String>>,
}

impl PypiCollector {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let package_names = read_seed_file(packages_file)?;
        eprintln!("Loaded {} package names from seed file", package_names.len());

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 200;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 50 == 0 {
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
        let dist_uri = distro_uri("pypi");
        let rel_uri = release_uri("pypi", "index");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Python Package Index")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "index")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_package_with_retry(
        &self,
        name: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<PypiProjectResponse, String> {
        let url = format!("https://pypi.org/pypi/{}/json", name);
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
                        std::thread::sleep(delay);
                        continue;
                    }
                    return Err(e.to_string());
                }
            }
        }

        Err(format!("Max retries exceeded for {}", name))
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        response: &PypiProjectResponse,
    ) -> Result<usize> {
        let info = &response.info;
        let pkg_uri = package_uri("pypi", "index", "any", &info.name, &info.version);
        let identity_uri = package_identity_uri("pypi", "index", "any", &info.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PYPI}PythonPackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &info.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &info.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("pypi", "index", &info.name, &info.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &info.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("pypi");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(summary) = &info.summary {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), summary)?;
            triples += 1;
        }
        if let Some(homepage) = &info.home_page {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(license) = &info.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
        }

        // PyPI-specific
        if let Some(req_python) = &info.requires_python {
            writer.write_literal(&pkg_uri, &format!("{PYPI}requiresPython"), req_python)?;
            triples += 1;
        }

        // Classifiers
        if let Some(classifiers) = &info.classifiers {
            for classifier in classifiers {
                writer.write_literal(&pkg_uri, &format!("{PYPI}classifierString"), classifier)?;
                triples += 1;
            }
        }

        // Dependencies (requires_dist format: "package (>=1.0,<2.0)")
        if let Some(requires_dist) = &info.requires_dist {
            triples += self.parse_requires_dist(writer, &pkg_uri, requires_dist)?;
        }

        Ok(triples)
    }

    fn parse_requires_dist(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        requires_dist: &[String],
    ) -> Result<usize> {
        let dep_re = Regex::new(r"^([a-zA-Z0-9._-]+)\s*(\(.*\))?").unwrap();
        let mut triples = 0;

        for req in requires_dist {
            // Skip extras markers like "foo[extra]" - just parse the base name
            let cleaned = req.split(';').next().unwrap_or(req).trim();

            if let Some(caps) = dep_re.captures(cleaned) {
                let dep_name = caps.get(1).unwrap().as_str();
                let version_spec = caps.get(2).map(|m| m.as_str());

                let target_uri = package_identity_uri("pypi", "index", "any", dep_name);

                writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
                triples += 1;

                let bnode = bnode_id("depends", &format!("{}-{}", pkg_uri, dep_name));
                writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
                writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
                writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
                writer.write_bnode_literal(&bnode, &format!("{PKG}dependencyType"), "depends")?;
                triples += 4;

                if let Some(spec) = version_spec {
                    let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, dep_name));
                    writer.write_bnode_object(&bnode, &format!("{PKG}hasVersionConstraint"), &cb)?;
                    writer.write_bnode_subject(&cb, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
                    writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintOperator"), "pep440")?;
                    writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintValue"), spec)?;
                    triples += 4;
                }
            }
        }

        Ok(triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_pypi_response_deserialization() {
        let json = r#"{
            "info": {
                "name": "requests",
                "version": "2.31.0",
                "summary": "Python HTTP for Humans",
                "license": "Apache 2.0",
                "home_page": "https://requests.readthedocs.io",
                "requires_python": ">=3.7",
                "requires_dist": ["charset-normalizer (<4,>=2)", "idna (<4,>=2.5)"],
                "classifiers": ["Development Status :: 5 - Production/Stable"]
            }
        }"#;

        let resp: PypiProjectResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.info.name, "requests");
        assert_eq!(resp.info.version, "2.31.0");
        assert_eq!(resp.info.requires_dist.unwrap().len(), 2);
    }

    #[test]
    fn test_emit_pypi_package_dual_typing() {
        let collector = PypiCollector::new();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let response = PypiProjectResponse {
            info: PypiInfo {
                name: "requests".into(),
                version: "2.31.0".into(),
                summary: Some("Python HTTP library".into()),
                license: Some("Apache 2.0".into()),
                home_page: Some("https://requests.readthedocs.io".into()),
                requires_python: Some(">=3.7".into()),
                requires_dist: Some(vec!["charset-normalizer (<4,>=2)".into()]),
                classifiers: Some(vec!["Development Status :: 5 - Production/Stable".into()]),
            },
        };

        let triples = collector.emit_package_triples(&mut writer, &response).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("pypi#PythonPackage"));
        assert!(content.contains("\"requests\""));
        assert!(content.contains("\"2.31.0\""));
        assert!(content.contains("requiresPython"));
        assert!(content.contains("classifierString"));
        assert!(content.contains("directlyDependsOn"));
        assert!(triples > 15);
    }

    #[test]
    fn test_parse_requires_dist() {
        let collector = PypiCollector::new();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg_uri = package_uri("pypi", "index", "any", "requests", "2.31.0");
        let requires = vec![
            "charset-normalizer (<4,>=2)".into(),
            "idna (<4,>=2.5)".into(),
            "urllib3 (<3,>=1.21.1)".into(),
        ];

        let triples = collector.parse_requires_dist(&mut writer, &pkg_uri, &requires).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("charset-normalizer"));
        assert!(content.contains("idna"));
        assert!(content.contains("urllib3"));
        assert!(content.contains("directlyDependsOn"));
        assert!(content.contains("dependencyTarget"));
        assert!(triples >= 15); // 3 deps * 5 triples
    }
}
