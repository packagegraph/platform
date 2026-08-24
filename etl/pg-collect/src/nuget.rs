use crate::ntriples::NTriplesWriter;
use crate::sparql::{SparqlAuth, SparqlBackend};
use crate::uris::*;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

pub struct NugetCollector {
    client: Client,
    registration_base: String,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ServiceIndex {
    resources: Vec<ServiceResource>,
}

#[derive(Debug, Deserialize)]
struct ServiceResource {
    #[serde(rename = "@type")]
    resource_type: String,
    #[serde(rename = "@id")]
    id: String,
}

#[derive(Debug, Deserialize)]
struct RegistrationIndex {
    items: Vec<RegistrationPage>,
}

#[derive(Debug, Deserialize)]
struct RegistrationPage {
    items: Vec<RegistrationLeaf>,
}

#[derive(Debug, Deserialize)]
struct RegistrationLeaf {
    #[serde(rename = "catalogEntry")]
    catalog_entry: CatalogEntry,
}

#[derive(Debug, Deserialize, Clone)]
struct CatalogEntry {
    id: String,
    version: String,
    description: Option<String>,
    #[serde(rename = "projectUrl")]
    project_url: Option<String>,
    #[serde(rename = "iconUrl")]
    icon_url: Option<String>,
    tags: Option<Vec<String>>,
    #[serde(rename = "licenseExpression")]
    license_expression: Option<String>,
    listed: Option<bool>,
    #[serde(rename = "dependencyGroups")]
    #[serde(default)]
    dependency_groups: Vec<DependencyGroup>,
}

#[derive(Debug, Deserialize, Clone)]
struct DependencyGroup {
    #[serde(rename = "targetFramework")]
    target_framework: Option<String>,
    #[serde(default)]
    dependencies: Vec<NugetDependency>,
}

#[derive(Debug, Deserialize, Clone)]
struct NugetDependency {
    id: String,
    range: Option<String>,
}

impl NugetCollector {
    pub fn new_from_service_index(service_index_url: &str) -> std::result::Result<Self, String> {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| e.to_string())?;

        let response = client
            .get(service_index_url)
            .send()
            .map_err(|e| e.to_string())?;

        let text = response.text().map_err(|e| e.to_string())?;
        let index: ServiceIndex = serde_json::from_str(&text).map_err(|e| e.to_string())?;

        let registration_base = index
            .resources
            .iter()
            .find(|r| r.resource_type.contains("RegistrationsBaseUrl"))
            .map(|r| r.id.trim_end_matches('/').to_string())
            .ok_or("RegistrationsBaseUrl not found in service index")?;

        Ok(Self {
            client,
            registration_base,
            graph_uri: None,
        })
    }

    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn collect_discover(
        &self,
        endpoint: &str,
        auth: &SparqlAuth,
        backend: SparqlBackend,
        output_path: &str,
    ) -> Result<(usize, usize)> {
        let names = crate::seed::discover_by_ecosystem(endpoint, "nuget", auth, backend.clone())?;
        let seed_path = "/tmp/seed-nuget-discover.txt";
        std::fs::write(seed_path, names.join("\n"))?;
        self.collect(seed_path, output_path)
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        let package_names = read_nuget_seed_file(packages_file)?;
        eprintln!(
            "Loaded {} NuGet package IDs from seed file",
            package_names.len()
        );

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 200;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, package_names.len());
            }

            match self.fetch_package_with_retry(name, &mut base_delay_ms) {
                Ok(entry) => {
                    total_triples += self.emit_package_triples(&mut writer, &entry)?;
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
        let dist_uri = distro_uri("nuget");
        let rel_uri = release_uri("nuget", "gallery");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "NuGet Gallery")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "gallery")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_package_with_retry(
        &self,
        package_id: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<CatalogEntry, String> {
        let url = format!(
            "{}/{}/index.json",
            self.registration_base,
            package_id.to_lowercase()
        );
        let max_attempts = 5;

        for attempt in 0..max_attempts {
            match self.client.get(&url).send() {
                Ok(response) => {
                    if response.status() == StatusCode::TOO_MANY_REQUESTS {
                        let retry_secs = 2u64.pow(attempt as u32);
                        eprintln!("  Rate limited, waiting {}s...", retry_secs);
                        std::thread::sleep(Duration::from_secs(retry_secs));
                        *base_delay_ms = (*base_delay_ms * 2).min(5000);
                        continue;
                    }

                    if response.status() == StatusCode::NOT_FOUND {
                        return Err(format!("404: {}", package_id));
                    }

                    let text = response.text().map_err(|e| e.to_string())?;
                    let reg: RegistrationIndex =
                        serde_json::from_str(&text).map_err(|e| e.to_string())?;

                    // Get latest version (last item in last page)
                    if let Some(page) = reg.items.last() {
                        if let Some(leaf) = page.items.last() {
                            return Ok(leaf.catalog_entry.clone());
                        }
                    }

                    return Err(format!("No versions found for {}", package_id));
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

        Err(format!("Max retries exceeded for {}", package_id))
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        entry: &CatalogEntry,
    ) -> Result<usize> {
        let pkg_uri = package_uri("nuget", "gallery", "any", &entry.id, &entry.version);
        let identity_uri = package_identity_uri("nuget", "gallery", "any", &entry.id);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{NUGET}NuGetPackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &entry.id)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &entry.id)?;
        writer.write_literal(&pkg_uri, &format!("{NUGET}packageId"), &entry.id)?;
        triples += 2;

        // Version
        let ver_uri = version_uri("nuget", "gallery", &entry.id, &entry.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &entry.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("nuget");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(desc) = &entry.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(project_url) = &entry.project_url {
            writer.write_literal(&pkg_uri, &format!("{NUGET}projectUrl"), project_url)?;
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), project_url)?;
            triples += 2;
        }
        if let Some(icon_url) = &entry.icon_url {
            writer.write_literal(&pkg_uri, &format!("{NUGET}iconUrl"), icon_url)?;
            triples += 1;
        }
        if let Some(license) = &entry.license_expression {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }
        if let Some(tags) = &entry.tags {
            let tags_str = tags.join(" ");
            writer.write_literal(&pkg_uri, &format!("{NUGET}tags"), &tags_str)?;
            triples += 1;
        }
        if let Some(listed) = entry.listed {
            writer.write_boolean(&pkg_uri, &format!("{NUGET}listed"), listed)?;
            triples += 1;
        }

        // Dependencies
        for dep_name in entry
            .dependency_groups
            .iter()
            .flat_map(|g| &g.dependencies)
            .map(|d| &d.id)
        {
            let target = package_identity_uri("nuget", "gallery", "any", dep_name);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target)?;
            triples += 1;
        }

        Ok(triples)
    }
}

pub fn read_nuget_seed_file(path: &str) -> Result<Vec<String>> {
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
    fn test_read_nuget_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment").unwrap();
        writeln!(temp, "Newtonsoft.Json").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "Serilog").unwrap();
        temp.flush().unwrap();

        let names = read_nuget_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "Newtonsoft.Json");
        assert_eq!(names[1], "Serilog");
    }

    #[test]
    fn test_catalog_entry_deserialization() {
        let json = r#"{
            "id": "Newtonsoft.Json",
            "version": "13.0.3",
            "description": "JSON framework for .NET",
            "projectUrl": "https://www.newtonsoft.com/json",
            "licenseExpression": "MIT",
            "listed": true,
            "dependencyGroups": []
        }"#;

        let entry: CatalogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "Newtonsoft.Json");
        assert_eq!(entry.version, "13.0.3");
        assert_eq!(entry.listed, Some(true));
    }

    #[test]
    fn test_emit_nuget_package() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let entry = CatalogEntry {
            id: "Serilog".to_string(),
            version: "3.1.1".to_string(),
            description: Some("Simple .NET logging".to_string()),
            project_url: Some("https://serilog.net".to_string()),
            icon_url: None,
            tags: Some(vec!["logging".to_string(), "serilog".to_string()]),
            license_expression: Some("Apache-2.0".to_string()),
            listed: Some(true),
            dependency_groups: vec![],
        };

        // Create a minimal collector instance just for testing emit
        let collector = NugetCollector {
            client: Client::new(),
            registration_base: "https://api.nuget.org/v3/registration5-gz-semver2".to_string(),
            graph_uri: None,
        };

        let triples = collector.emit_package_triples(&mut writer, &entry).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("nuget#NuGetPackage"));
        assert!(content.contains("\"Serilog\""));
        assert!(content.contains("nuget#packageId"));
        assert!(triples > 10);
    }
}
