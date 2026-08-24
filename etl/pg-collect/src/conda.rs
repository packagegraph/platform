use crate::npm::read_seed_file;
use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use regex::Regex;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::Result;

pub struct CondaCollector {
    distro_name: String,
    release_name: String,
    client: Client,
    channel_url: String,
    subdir: String,
    source_cache: Option<SourceCache>,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RepodataJson {
    packages: Option<HashMap<String, CondaPackageEntry>>,
    #[serde(rename = "packages.conda")]
    packages_conda: Option<HashMap<String, CondaPackageEntry>>,
}

#[derive(Debug, Deserialize)]
struct CondaPackageEntry {
    name: String,
    version: String,
    build: Option<String>,
    build_number: Option<i64>,
    depends: Option<Vec<String>>,
    license: Option<String>,
    subdir: Option<String>,
    timestamp: Option<i64>,
    md5: Option<String>,
    sha256: Option<String>,
    size: Option<i64>,
}

impl CondaCollector {
    pub fn new(
        distro_name: String,
        release_name: String,
        channel_url: String,
        subdir: String,
    ) -> Self {
        let client = crate::enricher::default_http_client();
        Self {
            distro_name,
            release_name,
            client,
            channel_url,
            subdir,
            source_cache: None,
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn with_cache(mut self, cache_dir: &str) -> Result<Self> {
        self.source_cache = Some(SourceCache::new(cache_dir, "conda")?);
        Ok(self)
    }

    pub fn collect_full(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());
        self.emit_distribution_metadata(&mut writer)?;

        let url = format!(
            "{}/{}/repodata.json",
            self.channel_url.trim_end_matches('/'),
            self.subdir
        );
        eprintln!("Fetching {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let reader = std::io::BufReader::new(response);
        let repodata: RepodataJson = serde_json::from_reader(reader)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut seen = std::collections::HashSet::new();

        // Process both .tar.bz2 and .conda packages, dedup by name+version
        for packages in [&repodata.packages, &repodata.packages_conda] {
            if let Some(pkgs) = packages {
                for entry in pkgs.values() {
                    let key = format!("{}-{}", entry.name, entry.version);
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.insert(key);
                    total_triples += self.emit_package_triples(&mut writer, entry)?;
                    total_packages += 1;
                    if total_packages % 5000 == 0 {
                        eprintln!("Progress: {} packages", total_packages);
                    }
                }
            }
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    pub fn collect_seeded(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let names = read_seed_file(packages_file)?;
        eprintln!("Loaded {} package names from seed file", names.len());
        // For seeded mode, fetch full repodata but only emit matching packages
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());
        self.emit_distribution_metadata(&mut writer)?;

        let url = format!(
            "{}/{}/repodata.json",
            self.channel_url.trim_end_matches('/'),
            self.subdir
        );
        eprintln!("Fetching {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let reader = std::io::BufReader::new(response);
        let repodata: RepodataJson = serde_json::from_reader(reader)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let name_set: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut seen = std::collections::HashSet::new();

        for packages in [&repodata.packages, &repodata.packages_conda] {
            if let Some(pkgs) = packages {
                for entry in pkgs.values() {
                    if !name_set.contains(entry.name.as_str()) {
                        continue;
                    }
                    let key = format!("{}-{}", entry.name, entry.version);
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.insert(key);
                    total_triples += self.emit_package_triples(&mut writer, entry)?;
                    total_packages += 1;
                }
            }
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.release_name);
        let mut triples = 0;
        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, RDFS_LABEL, "Conda")?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "conda-forge")?;
        triples += 3;
        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "conda-forge")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;
        Ok(triples)
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        entry: &CondaPackageEntry,
    ) -> Result<usize> {
        let pkg_uri = package_uri(
            &self.distro_name,
            &self.release_name,
            &self.subdir,
            &entry.name,
            &entry.version,
        );
        let identity_uri = package_identity_uri(
            &self.distro_name,
            &self.release_name,
            &self.subdir,
            &entry.name,
        );
        let mut triples = 0;

        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{CONDA}CondaPackage"))?;
        triples += 2;

        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &entry.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &entry.name)?;
        triples += 1;

        let ver_uri = version_uri(
            &self.distro_name,
            &self.release_name,
            &entry.name,
            &entry.version,
        );
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &entry.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Conda-specific
        if let Some(build) = &entry.build {
            writer.write_literal(&pkg_uri, &format!("{CONDA}buildString"), build)?;
            triples += 1;
        }
        if let Some(bn) = entry.build_number {
            writer.write_integer(&pkg_uri, &format!("{CONDA}buildNumber"), bn)?;
            triples += 1;
        }
        if let Some(license) = &entry.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }
        if let Some(subdir) = &entry.subdir {
            writer.write_literal(&pkg_uri, &format!("{CONDA}subdirectory"), subdir)?;
            triples += 1;
        }
        if let Some(ts) = entry.timestamp {
            writer.write_integer(&pkg_uri, &format!("{CONDA}timestamp"), ts)?;
            triples += 1;
        }
        if let Some(sha) = &entry.sha256 {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), sha)?;
            triples += 1;
        }
        if let Some(size) = entry.size {
            writer.write_integer(&pkg_uri, &format!("{PKG}packageSize"), size)?;
            triples += 1;
        }

        // Dependencies
        let has_python_dep = if let Some(deps) = &entry.depends {
            let dep_re = Regex::new(r"^([a-zA-Z0-9_.-]+)\s*(.*)$").unwrap();
            let mut has_python = false;

            for dep_str in deps {
                if let Some(caps) = dep_re.captures(dep_str) {
                    let dep_name = caps.get(1).unwrap().as_str();
                    let constraint = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");

                    if dep_name.starts_with("python") {
                        has_python = true;
                    }

                    let target_uri = package_identity_uri(
                        &self.distro_name,
                        &self.release_name,
                        &self.subdir,
                        dep_name,
                    );
                    writer.write_triple(
                        &pkg_uri,
                        &format!("{PKG}directlyDependsOn"),
                        &target_uri,
                    )?;
                    triples += 1;

                    let bnode = bnode_id("depends", &format!("{}-{}", pkg_uri, dep_name));
                    writer.write_bnode_object(&pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
                    writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
                    writer.write_bnode_subject(
                        &bnode,
                        &format!("{PKG}dependencyTarget"),
                        &target_uri,
                    )?;
                    writer.write_bnode_subject(
                        &bnode,
                        &format!("{PKG}dependencyType"),
                        &dep_type_uri("run"),
                    )?;
                    triples += 4;

                    if !constraint.is_empty() {
                        let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, dep_name));
                        writer.write_bnode_to_bnode(
                            &bnode,
                            &format!("{PKG}hasVersionConstraint"),
                            &cb,
                        )?;
                        writer.write_bnode_subject(
                            &cb,
                            RDF_TYPE,
                            &format!("{PKG}VersionConstraint"),
                        )?;
                        writer.write_bnode_literal(
                            &cb,
                            &format!("{PKG}versionConstraintOperator"),
                            "conda",
                        )?;
                        writer.write_bnode_literal(
                            &cb,
                            &format!("{PKG}versionConstraintValue"),
                            constraint,
                        )?;
                        triples += 4;
                    }
                }
            }
            has_python
        } else {
            false
        };

        // Cross-ecosystem correlation via upstreamPackageName
        // Conda-forge packages often map 1:1 to upstream ecosystems
        let ecosystem = if has_python_dep {
            Some("pypi")
        } else if entry.name.starts_with("r-") {
            Some("cran")
        } else if entry.name.starts_with("rust-") {
            Some("cargo")
        } else {
            None
        };

        if let Some(eco) = ecosystem {
            let eco_entity = ecosystem_uri(eco);
            writer.write_triple(&pkg_uri, &format!("{PKG}upstreamEcosystem"), &eco_entity)?;
            writer.write_triple(&eco_entity, RDF_TYPE, &format!("{PKG}Ecosystem"))?;
            // For r-* packages, strip r- prefix; for rust-*, strip rust-; otherwise use name as-is
            let upstream_name = if entry.name.starts_with("r-") {
                entry.name.strip_prefix("r-").unwrap_or(&entry.name)
            } else if entry.name.starts_with("rust-") {
                entry.name.strip_prefix("rust-").unwrap_or(&entry.name)
            } else {
                &entry.name
            };
            writer.write_literal(
                &pkg_uri,
                &format!("{PKG}upstreamPackageName"),
                upstream_name,
            )?;
            triples += 3;
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
    fn test_repodata_deserialization() {
        let json = r#"{
            "packages": {
                "numpy-1.26.4-py312h8442bc7_0.tar.bz2": {
                    "name": "numpy",
                    "version": "1.26.4",
                    "build": "py312h8442bc7_0",
                    "build_number": 0,
                    "depends": ["python >=3.12,<3.13.0a0", "libblas >=3.9.0,<4.0a0"],
                    "license": "BSD-3-Clause",
                    "subdir": "linux-64",
                    "timestamp": 1714000000,
                    "size": 8000000,
                    "sha256": "abc123"
                }
            }
        }"#;

        let repodata: RepodataJson = serde_json::from_str(json).unwrap();
        let pkgs = repodata.packages.unwrap();
        assert_eq!(pkgs.len(), 1);
        let numpy = pkgs.values().next().unwrap();
        assert_eq!(numpy.name, "numpy");
        assert_eq!(numpy.depends.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_emit_conda_package_dual_typing() {
        let collector = CondaCollector::new(
            "conda".into(),
            "conda-forge".into(),
            "https://conda.anaconda.org/conda-forge".into(),
            "linux-64".into(),
        );
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let entry = CondaPackageEntry {
            name: "numpy".into(),
            version: "1.26.4".into(),
            build: Some("py312h8442bc7_0".into()),
            build_number: Some(0),
            depends: Some(vec!["python >=3.12".into(), "libblas >=3.9.0".into()]),
            license: Some("BSD-3-Clause".into()),
            subdir: Some("linux-64".into()),
            timestamp: Some(1714000000),
            md5: None,
            sha256: Some("abc123".into()),
            size: Some(8000000),
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
        assert!(content.contains("conda#CondaPackage"));
        assert!(content.contains("\"numpy\""));
        assert!(content.contains("conda#buildString"));
        assert!(content.contains("directlyDependsOn"));
        assert!(triples > 20);
    }
}
