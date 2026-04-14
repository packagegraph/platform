use crate::ntriples::{NTriplesWriter, bnode_id};
use crate::uris::*;
use flate2::read::GzDecoder;
use regex::Regex;
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

pub struct DebianCollector {
    client: Client,
    repo_url: String,
    distribution: String,
    component: String,
}

#[derive(Debug)]
struct ReleaseInfo {
    codename: String,
    suite: String,
    origin: String,
}

impl DebianCollector {
    pub fn new(repo_url: String, distribution: String, component: String) -> Self {
        // HTTP client with timeout and retry configuration
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            repo_url,
            distribution,
            component,
        }
    }

    pub fn collect(&self, arches: &[String], output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        // Get release info
        let release_info = self.get_release_info()?;
        eprintln!(
            "Resolved '{}' to Origin='{}', Suite='{}', Codename='{}'",
            self.distribution, release_info.origin, release_info.suite, release_info.codename
        );

        // Emit distribution metadata
        self.emit_distribution_metadata(&mut writer, &release_info, arches)?;

        let mut total_packages = 0;
        let mut total_triples = 0;

        // Process each architecture
        for arch in arches {
            eprintln!("\nProcessing architecture: {}", arch);

            // Strip "binary-" prefix for URI building
            let arch_name = if arch.contains('-') {
                arch.split('-').next_back().unwrap()
            } else {
                arch.as_str()
            };

            let (pkg_count, triple_count) = self.process_arch(
                &mut writer,
                arch,
                arch_name,
                &release_info.codename,
                &release_info.suite,
            )?;

            total_packages += pkg_count;
            total_triples += triple_count;

            eprintln!("Processed {} packages for {}", pkg_count, arch);
        }

        writer.flush()?;

        Ok((total_packages, total_triples))
    }

    fn get_release_info(&self) -> Result<ReleaseInfo> {
        let release_url = format!(
            "{}/dists/{}/Release",
            self.repo_url.trim_end_matches('/'),
            self.distribution
        );

        eprintln!("Fetching Release info from {}", release_url);

        let response = self.client_get_with_retry(&release_url, 3)?;
        let text = response
            .text()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let mut codename = None;
        let mut suite = None;
        let mut origin = None;

        for line in text.lines() {
            if let Some(value) = line.strip_prefix("Codename:") {
                codename = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("Suite:") {
                suite = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("Origin:") {
                origin = Some(value.trim().to_string());
            }
        }

        match (codename, suite, origin) {
            (Some(codename), Some(suite), Some(origin)) => Ok(ReleaseInfo {
                codename,
                suite,
                origin,
            }),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Incomplete release information",
            )),
        }
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

    fn emit_distribution_metadata(
        &self,
        writer: &mut NTriplesWriter,
        release_info: &ReleaseInfo,
        arches: &[String],
    ) -> Result<()> {
        // Distribution
        let dist_uri = distro_uri("debian");
        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}distributionName"), "debian")?;

        // Release
        let rel_uri = release_uri("debian", &release_info.codename);
        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), &release_info.codename)?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseSuite"), &release_info.suite)?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseOrigin"), &release_info.origin)?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;

        // Architectures
        for arch in arches {
            let arch_name = if arch.contains('-') {
                arch.split('-').next_back().unwrap()
            } else {
                arch.as_str()
            };

            let arch_uri_val = arch_uri(arch_name);
            writer.write_triple(&arch_uri_val, RDF_TYPE, &format!("{PKG}Architecture"))?;
            writer.write_literal(&arch_uri_val, &format!("{PKG}architectureName"), arch_name)?;
        }

        Ok(())
    }

    fn process_arch(
        &self,
        writer: &mut NTriplesWriter,
        arch: &str,
        arch_name: &str,
        codename: &str,
        suite: &str,
    ) -> Result<(usize, usize)> {
        let packages_url = format!(
            "{}/dists/{}/{}/{}/Packages.gz",
            self.repo_url.trim_end_matches('/'),
            self.distribution,
            self.component,
            arch
        );

        eprintln!("Downloading {}", packages_url);

        let response = self.client_get_with_retry(&packages_url, 3)?;

        // Streaming decompression
        let decoder = GzDecoder::new(response);
        let reader = BufReader::new(decoder);

        let mut pkg_count = 0;
        let mut triple_count = 0;

        // Parse packages line-by-line as a state machine
        let mut current_pkg: HashMap<String, String> = HashMap::new();
        let mut last_key = String::new();

        for line in reader.lines() {
            let line = line?;

            if line.is_empty() {
                // End of package entry
                if !current_pkg.is_empty() && current_pkg.contains_key("Package") && current_pkg.contains_key("Version") {
                    triple_count += self.emit_package_triples(
                        writer,
                        &current_pkg,
                        codename,
                        suite,
                        arch_name,
                    )?;
                    pkg_count += 1;
                }
                current_pkg.clear();
                last_key.clear();
            } else if line.starts_with(' ') || line.starts_with('\t') {
                // Continuation of previous field
                if !last_key.is_empty() {
                    if let Some(value) = current_pkg.get_mut(&last_key) {
                        value.push(' ');
                        value.push_str(line.trim());
                    }
                }
            } else if let Some((key, value)) = line.split_once(':') {
                // New field
                let key = key.trim().to_string();
                last_key = key.clone();
                current_pkg.insert(key, value.trim().to_string());
            }
        }

        // Process last package if file doesn't end with blank line
        if !current_pkg.is_empty() && current_pkg.contains_key("Package") && current_pkg.contains_key("Version") {
            triple_count += self.emit_package_triples(
                writer,
                &current_pkg,
                codename,
                suite,
                arch_name,
            )?;
            pkg_count += 1;
        }

        Ok((pkg_count, triple_count))
    }

    pub fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_data: &HashMap<String, String>,
        codename: &str,
        suite: &str,
        arch_name: &str,
    ) -> Result<usize> {
        let pkg_name = pkg_data.get("Package").unwrap();
        let pkg_version = pkg_data.get("Version").unwrap();

        let pkg_uri = package_uri("debian", codename, arch_name, pkg_name, pkg_version);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{DEB}BinaryPackage"))?;
        triples += 2;

        // Core properties
        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), pkg_name)?;
        triples += 1;

        // Version resource
        let ver_uri = version_uri("debian", codename, pkg_name, pkg_version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), pkg_version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Architecture
        let arch_uri_val = arch_uri(arch_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}targetArchitecture"), &arch_uri_val)?;
        triples += 1;

        // Distribution and release
        let dist_uri = distro_uri("debian");
        let rel_uri = release_uri("debian", codename);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
        triples += 2;

        // Optional properties
        if let Some(desc) = pkg_data.get("Description") {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(homepage) = pkg_data.get("Homepage") {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(install_size_str) = pkg_data.get("Installed-Size") {
            if let Ok(install_size_kb) = install_size_str.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}installSize"), install_size_kb * 1024)?;
                triples += 1;
            }
        }
        if let Some(size) = pkg_data.get("Size") {
            if let Ok(size_val) = size.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}packageSize"), size_val)?;
                triples += 1;
            }
        }
        if let Some(checksum) = pkg_data.get("SHA256") {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), checksum)?;
            triples += 1;
        }

        // Debian-specific properties
        writer.write_literal(&pkg_uri, &format!("{DEB}inSuite"), suite)?;
        writer.write_literal(&pkg_uri, &format!("{DEB}inComponent"), &self.component)?;
        triples += 2;

        // Maintainer
        if let Some(maintainer_str) = pkg_data.get("Maintainer") {
            triples += self.emit_maintainer_triples(writer, &pkg_uri, maintainer_str)?;
        }

        // Source package
        triples += self.emit_source_package_triples(writer, &pkg_uri, pkg_data, codename, pkg_name, pkg_version)?;

        // Dependencies
        triples += self.emit_dependency_triples(writer, &pkg_uri, pkg_data, codename, arch_name)?;

        Ok(triples)
    }

    fn emit_maintainer_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        maintainer_str: &str,
    ) -> Result<usize> {
        // Parse "Name <email>"
        let re = Regex::new(r"^(.+?)\s*<(.+?)>$").unwrap();
        if let Some(caps) = re.captures(maintainer_str) {
            let name = caps.get(1).unwrap().as_str().trim();
            let email = caps.get(2).unwrap().as_str().trim();

            let maint_uri = maintainer_uri(email);

            writer.write_triple(&maint_uri, RDF_TYPE, &format!("{PKG}Maintainer"))?;
            writer.write_literal(&maint_uri, &format!("{FOAF}name"), name)?;
            writer.write_triple(&maint_uri, &format!("{FOAF}mbox"), &format!("mailto:{email}"))?;
            writer.write_triple(pkg_uri, &format!("{PKG}maintainedBy"), &maint_uri)?;

            return Ok(4);
        }

        Ok(0)
    }

    fn emit_source_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        pkg_data: &HashMap<String, String>,
        codename: &str,
        pkg_name: &str,
        pkg_version: &str,
    ) -> Result<usize> {
        let (source_name, source_version) = if let Some(source_str) = pkg_data.get("Source") {
            // Format can be "sourcename" or "sourcename (version)"
            let re = Regex::new(r"^([^\s]+)(?:\s+\(([^)]+)\))?$").unwrap();
            if let Some(caps) = re.captures(source_str) {
                let name = caps.get(1).unwrap().as_str();
                let version = caps.get(2).map(|m| m.as_str()).unwrap_or(pkg_version);
                (name.to_string(), version.to_string())
            } else {
                (source_str.clone(), pkg_version.to_string())
            }
        } else {
            // No Source field means source name = binary name
            (pkg_name.to_string(), pkg_version.to_string())
        };

        let src_uri = source_uri("debian", codename, &source_name, &source_version);

        writer.write_triple(&src_uri, RDF_TYPE, &format!("{PKG}SourcePackage"))?;
        writer.write_literal(&src_uri, &format!("{PKG}packageName"), &source_name)?;

        // Version resource for source
        let src_ver_uri = version_uri("debian", codename, &source_name, &source_version);
        writer.write_triple(&src_ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&src_ver_uri, &format!("{PKG}versionString"), &source_version)?;
        writer.write_triple(&src_uri, &format!("{PKG}hasVersion"), &src_ver_uri)?;

        // Link binary to source
        writer.write_triple(pkg_uri, &format!("{PKG}builtFromSource"), &src_uri)?;

        Ok(6)
    }

    fn emit_dependency_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        pkg_data: &HashMap<String, String>,
        codename: &str,
        arch_name: &str,
    ) -> Result<usize> {
        let dep_mappings = vec![
            ("Depends", "runtime", Some(format!("{DEB}debDepends"))),
            ("Pre-Depends", "runtime", Some(format!("{DEB}debDepends"))),
            ("Recommends", "recommends", Some(format!("{DEB}debRecommends"))),
            ("Suggests", "suggests", Some(format!("{DEB}debSuggests"))),
            ("Conflicts", "conflicts", Some(format!("{DEB}debConflicts"))),
            ("Breaks", "breaks", Some(format!("{DEB}debConflicts"))),
        ];

        let mut triples = 0;

        for (field, dep_type, distro_prop) in dep_mappings {
            if let Some(dep_string) = pkg_data.get(field) {
                triples += self.parse_and_emit_dependencies(
                    writer,
                    pkg_uri,
                    dep_string,
                    dep_type,
                    distro_prop.as_deref(),
                    codename,
                    arch_name,
                )?;
            }
        }

        Ok(triples)
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_and_emit_dependencies(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        dep_string: &str,
        dep_type: &str,
        distro_prop: Option<&str>,
        codename: &str,
        arch_name: &str,
    ) -> Result<usize> {
        // Regex to parse dependency entries
        let dep_re = Regex::new(r"([\w.-]+)(?:\s+\(([^)]+)\))?").unwrap();

        let mut triples = 0;

        for part in dep_string.split(',') {
            // Handle alternatives by taking the first one
            let first_alternative = part.split('|').next().unwrap_or(part).trim();

            if let Some(caps) = dep_re.captures(first_alternative) {
                let dep_name = caps.get(1).unwrap().as_str();
                let version_constraint = caps.get(2).map(|m| m.as_str());

                // Build target package URI (version is unknown)
                let dep_uri = package_uri("debian", codename, arch_name, dep_name, "unknown");

                // Ensure dependency target stub has basic properties for graph traversal.
                // Without pkg:packageName and rdf:type, stubs are invisible to typed queries
                // and name-based joins, breaking transitive dependency traversal.
                writer.write_triple(&dep_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
                writer.write_literal(&dep_uri, &format!("{PKG}packageName"), dep_name)?;
                triples += 2;

                // Emit generic property based on dep_type
                if dep_type == "conflicts" || dep_type == "breaks" {
                    writer.write_triple(pkg_uri, &format!("{PKG}directlyConflictsWith"), &dep_uri)?;
                } else {
                    writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &dep_uri)?;
                }
                triples += 1;

                // Emit distro-specific property if provided
                if let Some(prop) = distro_prop {
                    writer.write_triple(pkg_uri, prop, &dep_uri)?;
                    triples += 1;
                }

                // Create reified Dependency
                let dep_bnode = bnode_id("dep", &format!("{pkg_uri}_{dep_name}"));

                writer.write_bnode_subject(&dep_bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
                writer.write_bnode_subject(&dep_bnode, &format!("{PKG}dependencyTarget"), &dep_uri)?;
                writer.write_bnode_literal(&dep_bnode, &format!("{PKG}dependencyType"), dep_type)?;
                writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &dep_bnode)?;
                triples += 4;

                // Add VersionConstraint if specified
                if let Some(constraint_str) = version_constraint {
                    let (operator, value) = self.parse_version_constraint(constraint_str);
                    if let (Some(op), Some(val)) = (operator, value) {
                        let constraint_bnode = bnode_id("constraint", &format!("{dep_bnode}_{val}"));

                        writer.write_bnode_subject(&constraint_bnode, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
                        writer.write_bnode_literal(&constraint_bnode, &format!("{PKG}versionConstraintOperator"), &op)?;
                        writer.write_bnode_literal(&constraint_bnode, &format!("{PKG}versionConstraintValue"), &val)?;
                        writer.write_bnode_subject(&dep_bnode, &format!("{PKG}hasVersionConstraint"), &format!("_{constraint_bnode}"))?;
                        triples += 4;
                    }
                }
            }
        }

        Ok(triples)
    }

    fn parse_version_constraint(&self, constraint_str: &str) -> (Option<String>, Option<String>) {
        // Match operator and version
        let re = Regex::new(r"^\s*([<>=]+)\s*(.+)$").unwrap();
        if let Some(caps) = re.captures(constraint_str) {
            let op_str = caps.get(1).unwrap().as_str();
            let value = caps.get(2).unwrap().as_str().trim();

            // Map Debian operators to symbols
            let operator = match op_str {
                "<<" => "<",
                "<=" => "≤",
                "=" => "=",
                ">=" => "≥",
                ">>" => ">",
                _ => op_str,
            };

            return (Some(operator.to_string()), Some(value.to_string()));
        }

        (None, None)
    }
}
