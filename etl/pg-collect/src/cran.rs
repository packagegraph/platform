use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};

pub struct CranCollector {
    client: Client,
    mirror_url: String,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Default)]
struct CranPackage {
    package: String,
    version: String,
    depends: Vec<String>,
    imports: Vec<String>,
    suggests: Vec<String>,
    linking_to: Vec<String>,
    enhances: Vec<String>,
    license: Option<String>,
    needs_compilation: Option<String>,
    title: Option<String>,
    description: Option<String>,
    author: Option<String>,
    maintainer: Option<String>,
    url: Option<String>,
    system_requirements: Option<String>,
    priority: Option<String>,
}

impl CranCollector {
    pub fn new(mirror_url: String) -> Self {
        let client = crate::enricher::default_http_client();

        Self {
            client,
            mirror_url,
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let packages_url = format!(
            "{}/src/contrib/PACKAGES.gz",
            self.mirror_url.trim_end_matches('/')
        );
        eprintln!("Fetching PACKAGES from: {}", packages_url);

        let response = self
            .client
            .get(&packages_url)
            .send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        if !response.status().is_success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("HTTP {}", response.status()),
            ));
        }

        let bytes = response
            .bytes()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let decoder = GzDecoder::new(&bytes[..]);
        let reader = BufReader::new(decoder);

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        let packages = self.parse_packages_file(reader)?;
        eprintln!("Parsed {} CRAN packages", packages.len());

        let mut total_triples = 0;
        for (idx, pkg) in packages.iter().enumerate() {
            if (idx + 1) % 1000 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, packages.len());
            }
            total_triples += self.emit_package_triples(&mut writer, pkg)?;
        }

        writer.flush()?;
        Ok((packages.len(), total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("cran");
        let rel_uri = release_uri("cran", "cran");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, RDFS_LABEL, "CRAN")?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "CRAN")?;
        triples += 3;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "cran")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn parse_packages_file<R: BufRead>(&self, reader: R) -> Result<Vec<CranPackage>> {
        let mut packages = Vec::new();
        let mut current = CranPackage::default();
        let mut current_field = String::new();
        let mut current_value = String::new();

        for line_result in reader.lines() {
            let line = line_result?;

            if line.is_empty() {
                // End of package entry
                if !current.package.is_empty() {
                    // Flush last field
                    if !current_field.is_empty() {
                        self.set_field(&mut current, &current_field, &current_value);
                        current_field.clear();
                        current_value.clear();
                    }
                    packages.push(current);
                    current = CranPackage::default();
                }
                continue;
            }

            if line.starts_with(' ') || line.starts_with('\t') {
                // Continuation line
                current_value.push(' ');
                current_value.push_str(line.trim());
            } else if let Some((key, value)) = line.split_once(':') {
                // New field — flush previous
                if !current_field.is_empty() {
                    self.set_field(&mut current, &current_field, &current_value);
                }
                current_field = key.trim().to_string();
                current_value = value.trim().to_string();
            }
        }

        // Flush final package
        if !current.package.is_empty() {
            if !current_field.is_empty() {
                self.set_field(&mut current, &current_field, &current_value);
            }
            packages.push(current);
        }

        Ok(packages)
    }

    fn set_field(&self, pkg: &mut CranPackage, key: &str, value: &str) {
        match key {
            "Package" => pkg.package = value.to_string(),
            "Version" => pkg.version = value.to_string(),
            "Depends" => pkg.depends = self.parse_dep_list(value),
            "Imports" => pkg.imports = self.parse_dep_list(value),
            "Suggests" => pkg.suggests = self.parse_dep_list(value),
            "LinkingTo" => pkg.linking_to = self.parse_dep_list(value),
            "Enhances" => pkg.enhances = self.parse_dep_list(value),
            "License" => pkg.license = Some(value.to_string()),
            "NeedsCompilation" => pkg.needs_compilation = Some(value.to_string()),
            "Title" => pkg.title = Some(value.to_string()),
            "Description" => pkg.description = Some(value.to_string()),
            "Author" => pkg.author = Some(value.to_string()),
            "Maintainer" => pkg.maintainer = Some(value.to_string()),
            "URL" => pkg.url = Some(value.to_string()),
            "SystemRequirements" => pkg.system_requirements = Some(value.to_string()),
            "Priority" => pkg.priority = Some(value.to_string()),
            _ => {}
        }
    }

    fn parse_dep_list(&self, value: &str) -> Vec<String> {
        value
            .split(',')
            .map(|s| {
                // Strip version constraints like "pkg (>= 1.0)"
                s.trim()
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty() && s != "R")
            .collect()
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &CranPackage,
    ) -> Result<usize> {
        let pkg_uri = package_uri("cran", "cran", "any", &pkg.package, &pkg.version);
        let identity_uri = package_identity_uri("cran", "cran", "any", &pkg.package);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{CRAN}CranPackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &pkg.package)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.package)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("cran", "cran", &pkg.package, &pkg.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &pkg.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("cran");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(title) = &pkg.title {
            writer.write_literal(&pkg_uri, &format!("{CRAN}title"), title)?;
            triples += 1;
        }
        if let Some(desc) = &pkg.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(license) = &pkg.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }
        if let Some(author) = &pkg.author {
            writer.write_literal(&pkg_uri, &format!("{CRAN}author"), author)?;
            triples += 1;
        }
        if let Some(maintainer) = &pkg.maintainer {
            writer.write_literal(&pkg_uri, &format!("{CRAN}maintainer"), maintainer)?;
            triples += 1;
        }
        if let Some(url) = &pkg.url {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), url)?;
            triples += 1;
        }
        if let Some(needs_comp) = &pkg.needs_compilation {
            let is_yes = needs_comp.to_lowercase() == "yes";
            writer.write_boolean(&pkg_uri, &format!("{CRAN}needsCompilation"), is_yes)?;
            triples += 1;
        }
        if let Some(sysreq) = &pkg.system_requirements {
            writer.write_literal(&pkg_uri, &format!("{CRAN}systemRequirements"), sysreq)?;
            triples += 1;
        }
        if let Some(priority) = &pkg.priority {
            writer.write_literal(&pkg_uri, &format!("{CRAN}priority"), priority)?;
            triples += 1;
        }

        // Dependencies
        for dep_name in &pkg.depends {
            let target = package_identity_uri("cran", "cran", "any", dep_name);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target)?;
            triples += 1;
        }
        for dep_name in &pkg.imports {
            let target = package_identity_uri("cran", "cran", "any", dep_name);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target)?;
            triples += 1;
        }

        Ok(triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dep_list() {
        let collector = CranCollector::new("https://cran.r-project.org".into());

        let result = collector.parse_dep_list("ggplot2 (>= 3.0), dplyr, tidyr (>= 1.0.0)");
        assert_eq!(result, vec!["ggplot2", "dplyr", "tidyr"]);

        let result = collector.parse_dep_list("R (>= 4.0.0)");
        assert!(result.is_empty()); // R itself is filtered out

        let result = collector.parse_dep_list("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_packages_file() {
        let data = "Package: ggplot2\nVersion: 3.4.4\nDepends: R (>= 3.3)\nImports: cli, glue\nLicense: MIT\nNeedsCompilation: no\nTitle: Create Elegant Data Visualisations\n\nPackage: dplyr\nVersion: 1.1.4\nImports: rlang, tibble\nLicense: MIT\nNeedsCompilation: yes\n\n";

        let collector = CranCollector::new("https://cran.r-project.org".into());
        let packages = collector
            .parse_packages_file(BufReader::new(data.as_bytes()))
            .unwrap();

        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].package, "ggplot2");
        assert_eq!(packages[0].version, "3.4.4");
        assert_eq!(packages[0].imports, vec!["cli", "glue"]);
        assert_eq!(packages[0].needs_compilation, Some("no".to_string()));

        assert_eq!(packages[1].package, "dplyr");
        assert_eq!(packages[1].imports, vec!["rlang", "tibble"]);
    }

    #[test]
    fn test_emit_cran_package_dual_typing() {
        use std::io::{Read, Write};
        use tempfile::NamedTempFile;

        let collector = CranCollector::new("https://cran.r-project.org".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg = CranPackage {
            package: "ggplot2".to_string(),
            version: "3.4.4".to_string(),
            title: Some("Create Elegant Data Visualisations".to_string()),
            license: Some("MIT".to_string()),
            needs_compilation: Some("no".to_string()),
            imports: vec!["cli".to_string(), "glue".to_string()],
            ..Default::default()
        };

        let triples = collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("cran#CranPackage"));
        assert!(content.contains("\"ggplot2\""));
        assert!(content.contains("\"3.4.4\""));
        assert!(content.contains("cran#needsCompilation"));
        assert!(content.contains("\"false\""));
        assert!(triples > 10);
    }
}
