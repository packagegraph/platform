use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::npm::read_seed_file;
use crate::uris::*;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct CargoCollector {
    client: Client,
}

#[derive(Debug, Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    crate_data: CrateData,
    versions: Option<Vec<CrateVersion>>,
}

#[derive(Debug, Deserialize)]
struct CrateData {
    name: String,
    description: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
    repository: Option<String>,
    downloads: Option<i64>,
    max_stable_version: Option<String>,
    categories: Option<Vec<String>>,
    keywords: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CrateVersion {
    num: String,
    edition: Option<String>,
    rust_version: Option<String>,
    features: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct DepsResponse {
    dependencies: Vec<CrateDep>,
}

#[derive(Debug, Deserialize)]
struct CrateDep {
    crate_id: String,
    req: String,
    kind: String,
    optional: bool,
}

impl CargoCollector {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent("pg-collect/0.1.0 (https://github.com/packagegraph)")
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }

    pub fn collect(&self, packages_file: &str, max_depth: u32, max_packages: usize, output_path: &str) -> Result<(usize, usize)> {
        use std::collections::{HashSet, HashMap, VecDeque};

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let seeds = read_seed_file(packages_file)?;
        eprintln!("Loaded {} seed crates", seeds.len());
        eprintln!("Spider config: max_depth={}, max_packages={}", max_depth, max_packages);

        // BFS state
        let mut queue: VecDeque<String> = seeds.into_iter().collect();
        let mut visited: HashSet<String> = HashSet::new();
        let mut depth_map: HashMap<String, u32> = HashMap::new();

        for name in queue.iter() {
            depth_map.insert(name.clone(), 0);
        }

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms: u64 = 200;

        while let Some(name) = queue.pop_front() {
            if !visited.insert(name.clone()) {
                continue;
            }

            if visited.len() > max_packages {
                eprintln!("Reached max_packages limit ({})", max_packages);
                break;
            }

            let depth = *depth_map.get(&name).unwrap_or(&0);

            if visited.len() % 100 == 0 {
                eprintln!("Progress: {} crates (depth {})", visited.len(), depth);
            }

            match self.fetch_crate_with_retry(&name, &mut base_delay_ms) {
                Ok((crate_resp, deps)) => {
                    total_triples += self.emit_crate_triples(&mut writer, &crate_resp, &deps)?;
                    total_packages += 1;

                    // Enqueue runtime + build deps (skip dev)
                    if depth < max_depth {
                        for dep in deps {
                            if dep.kind != "dev" && !visited.contains(&dep.crate_id) && !depth_map.contains_key(&dep.crate_id) {
                                depth_map.insert(dep.crate_id.clone(), depth + 1);
                                queue.push_back(dep.crate_id);
                            }
                        }
                    }
                }
                Err(e) => eprintln!("  Error fetching {}: {}", name, e),
            }

            std::thread::sleep(Duration::from_millis(base_delay_ms));
        }

        eprintln!("Collected {} crates ({} total in graph)", total_packages, visited.len());
        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("cargo");
        let rel_uri = release_uri("cargo", "crates.io");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "crates.io")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "crates.io")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_crate_with_retry(
        &self,
        name: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<(CratesResponse, Vec<CrateDep>), String> {
        // Fetch crate metadata
        let url = format!("https://crates.io/api/v1/crates/{}", name);
        let crate_resp: CratesResponse = self.fetch_json_with_retry(&url, base_delay_ms)?;

        // Fetch deps for the stable version
        let version = crate_resp
            .crate_data
            .max_stable_version
            .as_deref()
            .or_else(|| crate_resp.versions.as_ref()?.first().map(|v| v.num.as_str()))
            .unwrap_or("0.0.0");

        let deps_url = format!("https://crates.io/api/v1/crates/{}/{}/dependencies", name, version);
        let deps = match self.fetch_json_with_retry::<DepsResponse>(&deps_url, base_delay_ms) {
            Ok(r) => r.dependencies,
            Err(_) => vec![],
        };

        Ok((crate_resp, deps))
    }

    fn fetch_json_with_retry<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<T, String> {
        let max_attempts = 5;

        for attempt in 0..max_attempts {
            match self.client.get(url).send() {
                Ok(response) => {
                    if response.status() == StatusCode::TOO_MANY_REQUESTS {
                        let retry_after_secs = response
                            .headers()
                            .get("retry-after")
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or_else(|| 2u64.pow(attempt as u32));

                        eprintln!("  Rate limited, waiting {}s...", retry_after_secs);
                        std::thread::sleep(Duration::from_millis(retry_after_secs * 1000));
                        *base_delay_ms = (*base_delay_ms * 2).min(5000);
                        continue;
                    }

                    if response.status() == StatusCode::NOT_FOUND {
                        return Err(format!("404: {}", url));
                    }

                    let text = response.text().map_err(|e| e.to_string())?;
                    return serde_json::from_str(&text).map_err(|e| e.to_string());
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

        Err(format!("Max retries exceeded for {}", url))
    }

    fn emit_crate_triples(
        &self,
        writer: &mut NTriplesWriter,
        resp: &CratesResponse,
        deps: &[CrateDep],
    ) -> Result<usize> {
        let crate_data = &resp.crate_data;
        let version = crate_data.max_stable_version.as_deref().unwrap_or("0.0.0");
        let pkg_uri = package_uri("cargo", "crates.io", "any", &crate_data.name, version);
        let identity_uri = package_identity_uri("cargo", "crates.io", "any", &crate_data.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{CARGO}Crate"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &crate_data.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &crate_data.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("cargo", "crates.io", &crate_data.name, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("cargo");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Properties
        if let Some(desc) = &crate_data.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(homepage) = &crate_data.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(repo) = &crate_data.repository {
            writer.write_literal(&pkg_uri, &format!("{PKG}projectUrl"), repo)?;
            triples += 1;
        }
        if let Some(license) = &crate_data.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
        }
        if let Some(downloads) = crate_data.downloads {
            writer.write_integer(&pkg_uri, &format!("{CARGO}downloads"), downloads)?;
            triples += 1;
        }

        // Edition and MSRV from versions
        if let Some(versions) = &resp.versions {
            if let Some(ver_data) = versions.iter().find(|v| v.num == version) {
                if let Some(edition) = &ver_data.edition {
                    writer.write_literal(&pkg_uri, &format!("{CARGO}edition"), edition)?;
                    triples += 1;
                }
                if let Some(msrv) = &ver_data.rust_version {
                    writer.write_literal(&pkg_uri, &format!("{CARGO}msrv"), msrv)?;
                    triples += 1;
                }

                // Features
                if let Some(features) = &ver_data.features {
                    if let Some(features_map) = features.as_object() {
                        for feature_name in features_map.keys() {
                            writer.write_literal(&pkg_uri, &format!("{CARGO}featureName"), feature_name)?;
                            triples += 1;
                        }
                    }
                }
            }
        }

        // Dependencies
        for dep in deps {
            let dep_type = match dep.kind.as_str() {
                "dev" => "dev_depends",
                "build" => "build_depends",
                _ => "depends",
            };

            let target_uri = package_identity_uri("cargo", "crates.io", "any", &dep.crate_id);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id(dep_type, &format!("{}-{}", pkg_uri, dep.crate_id));
            writer.write_bnode_object(&pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_literal(&bnode, &format!("{PKG}dependencyType"), dep_type)?;
            triples += 4;

            if !dep.req.is_empty() {
                let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, dep.crate_id));
                writer.write_bnode_object(&bnode, &format!("{PKG}hasVersionConstraint"), &cb)?;
                writer.write_bnode_subject(&cb, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
                writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintOperator"), "semver")?;
                writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintValue"), &dep.req)?;
                triples += 4;
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
    fn test_crate_response_deserialization() {
        let json = r#"{
            "crate": {
                "name": "serde",
                "description": "A serialization framework",
                "license": "MIT OR Apache-2.0",
                "homepage": "https://serde.rs",
                "repository": "https://github.com/serde-rs/serde",
                "downloads": 300000000,
                "max_stable_version": "1.0.200",
                "categories": ["encoding"],
                "keywords": ["serialization"]
            },
            "versions": [{"num": "1.0.200", "edition": "2021", "rust_version": "1.56.0", "features": {"derive": ["serde_derive"]}}]
        }"#;

        let resp: CratesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.crate_data.name, "serde");
        assert_eq!(resp.crate_data.max_stable_version.as_deref(), Some("1.0.200"));
        assert_eq!(resp.versions.unwrap()[0].edition.as_deref(), Some("2021"));
    }

    #[test]
    fn test_emit_crate_triples_dual_typing() {
        let collector = CargoCollector::new();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let resp = CratesResponse {
            crate_data: CrateData {
                name: "serde".into(),
                description: Some("Serialization framework".into()),
                license: Some("MIT OR Apache-2.0".into()),
                homepage: Some("https://serde.rs".into()),
                repository: Some("https://github.com/serde-rs/serde".into()),
                downloads: Some(300000000),
                max_stable_version: Some("1.0.200".into()),
                categories: None,
                keywords: None,
            },
            versions: Some(vec![CrateVersion {
                num: "1.0.200".into(),
                edition: Some("2021".into()),
                rust_version: Some("1.56.0".into()),
                features: Some(serde_json::json!({"derive": ["serde_derive"], "default": ["std"]})),
            }]),
        };

        let deps = vec![CrateDep {
            crate_id: "serde_derive".into(),
            req: "^1.0".into(),
            kind: "normal".into(),
            optional: true,
        }];

        let triples = collector.emit_crate_triples(&mut writer, &resp, &deps).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("cargo#Crate"));
        assert!(content.contains("\"serde\""));
        assert!(content.contains("cargo#edition"));
        assert!(content.contains("\"2021\""));
        assert!(content.contains("cargo#msrv"));
        assert!(content.contains("directlyDependsOn"));
        assert!(content.contains("featureName"));
        assert!(triples > 20);
    }
}
