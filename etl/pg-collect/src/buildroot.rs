use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::uris::*;
use once_cell::sync::Lazy;
use regex::Regex;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Result};
use std::path::Path;

// Regex for parsing Buildroot variable assignments: FOO_VERSION = bar
static VAR_ASSIGN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^\s*([A-Z][A-Z0-9_]*)_(VERSION|SOURCE|SITE|SITE_METHOD|LICENSE|LICENSE_FILES|DEPENDENCIES|HOST_DEPENDENCIES|CPE_ID_VENDOR|CPE_ID_PRODUCT|INSTALL_STAGING|INSTALL_TARGET|CONF_OPTS)\s*=\s*(.*)$"#).unwrap()
});

// Regex to detect infrastructure type: $(eval $(autotools-package))
static INFRA_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\$\(eval\s+\$\(([a-z0-9-]+)\)\)"#).unwrap());

#[derive(Debug)]
struct BuildrootPackage {
    name: String,
    version: Option<String>,
    source: Option<String>,
    site: Option<String>,
    site_method: Option<String>,
    license: Option<String>,
    license_files: Option<String>,
    dependencies: Vec<String>,
    host_dependencies: Vec<String>,
    install_staging: bool,
    install_target: bool,
    cpe_vendor: Option<String>,
    cpe_product: Option<String>,
    conf_opts: Option<String>,
    infrastructure: Option<String>,
}

pub struct BuildrootCollector {
    distro_name: String,
    release_name: String,
    repo_path: String,
    pub graph_uri: Option<String>,
}

impl BuildrootCollector {
    pub fn new(distro_name: String, release_name: String, repo_path: String) -> Self {
        Self {
            distro_name,
            release_name,
            repo_path,
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());
        self.emit_distribution_metadata(&mut writer)?;

        let packages_path = Path::new(&self.repo_path).join("package");
        let mut total_packages = 0;
        let mut total_triples = 0;

        // Walk through package directory
        for entry in fs::read_dir(&packages_path)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let pkg_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) if !n.starts_with('.') => n,
                _ => continue,
            };

            let mk_file = path.join(format!("{}.mk", pkg_name));
            if !mk_file.exists() {
                continue;
            }

            match self.parse_package(pkg_name, &mk_file) {
                Ok(pkg) => {
                    total_triples += self.emit_package_triples(&mut writer, &pkg)?;
                    total_packages += 1;
                    if total_packages % 1000 == 0 {
                        eprintln!("Progress: {} packages", total_packages);
                    }
                }
                Err(e) => {
                    eprintln!("  Error parsing {:?}: {}", mk_file, e);
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
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Buildroot")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "buildroot")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn parse_package(
        &self,
        pkg_name: &str,
        mk_path: &Path,
    ) -> std::result::Result<BuildrootPackage, String> {
        let file = File::open(mk_path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);

        // Derive variable prefix: lib-foo → LIB_FOO
        let prefix = pkg_name.to_uppercase().replace('-', "_");

        let mut pkg = BuildrootPackage {
            name: pkg_name.to_string(),
            version: None,
            source: None,
            site: None,
            site_method: None,
            license: None,
            license_files: None,
            dependencies: Vec::new(),
            host_dependencies: Vec::new(),
            install_staging: false,
            install_target: false,
            cpe_vendor: None,
            cpe_product: None,
            conf_opts: None,
            infrastructure: None,
        };

        let mut continued_line = String::new();

        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;

            // Check for infrastructure type: $(eval $(autotools-package))
            if let Some(caps) = INFRA_RE.captures(&line) {
                pkg.infrastructure = Some(caps.get(1).unwrap().as_str().to_string());
            }

            // Handle line continuation
            let full_line = if continued_line.is_empty() {
                line.trim().to_string()
            } else {
                continued_line.push_str(&line);
                if line.trim_end().ends_with('\\') {
                    continued_line = continued_line.trim_end_matches('\\').to_string();
                    continue;
                } else {
                    let result = continued_line.clone();
                    continued_line.clear();
                    result
                }
            };

            if full_line.trim_end().ends_with('\\') {
                continued_line = full_line.trim_end_matches('\\').to_string();
                continue;
            }

            // Use prefix-based matching instead of regex to avoid greedy capture issues
            // (e.g., OPENSSL_HOST_DEPENDENCIES would match prefix OPENSSL_HOST with suffix DEPENDENCIES)
            let expected_prefix = format!("{}_", prefix);
            if let Some(rest) = full_line.trim_start().strip_prefix(&expected_prefix) {
                if let Some(eq_idx) = rest.find('=') {
                    let var_suffix = rest[..eq_idx].trim();
                    let value = rest[eq_idx + 1..].trim();

                    match var_suffix {
                        "VERSION" => pkg.version = Some(value.to_string()),
                        "SOURCE" => pkg.source = Some(value.to_string()),
                        "SITE" => pkg.site = Some(value.to_string()),
                        "SITE_METHOD" => pkg.site_method = Some(value.to_string()),
                        "LICENSE" => pkg.license = Some(value.to_string()),
                        "LICENSE_FILES" => pkg.license_files = Some(value.to_string()),
                        "DEPENDENCIES" => {
                            for dep in value.split_whitespace() {
                                if dep.starts_with("host-") {
                                    pkg.host_dependencies.push(dep.to_string());
                                } else {
                                    pkg.dependencies.push(dep.to_string());
                                }
                            }
                        }
                        "HOST_DEPENDENCIES" => {
                            for dep in value.split_whitespace() {
                                pkg.host_dependencies.push(dep.to_string());
                            }
                        }
                        "INSTALL_STAGING" => pkg.install_staging = value.trim() == "YES",
                        "INSTALL_TARGET" => pkg.install_target = value.trim() == "YES",
                        "CPE_ID_VENDOR" => pkg.cpe_vendor = Some(value.to_string()),
                        "CPE_ID_PRODUCT" => pkg.cpe_product = Some(value.to_string()),
                        "CONF_OPTS" => pkg.conf_opts = Some(value.to_string()),
                        _ => {}
                    }
                }
            }
        }

        Ok(pkg)
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &BuildrootPackage,
    ) -> Result<usize> {
        let default_version = "0".to_string();
        let version = pkg.version.as_ref().unwrap_or(&default_version);
        let pkg_uri = package_uri(
            &self.distro_name,
            &self.release_name,
            "any",
            &pkg.name,
            version,
        );
        let identity_uri =
            package_identity_uri(&self.distro_name, &self.release_name, "any", &pkg.name);
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.release_name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}SourcePackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{BUILDROOT}BuildrootPackage"))?;
        triples += 2;

        // Package name
        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.name)?;
        triples += 1;

        // Link to canonical identity (isVersionOf)
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &pkg.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        // Version resource (separate node with versionString)
        let ver_uri = version_uri(&self.distro_name, &self.release_name, &pkg.name, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution and release links
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
        triples += 2;

        // License (licenseName)
        if let Some(ref license) = pkg.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }

        if let Some(ref site) = pkg.site {
            writer.write_literal(&pkg_uri, &format!("{BUILDROOT}site"), site)?;
            triples += 1;
        }

        if let Some(ref site_method) = pkg.site_method {
            writer.write_literal(&pkg_uri, &format!("{BUILDROOT}siteMethod"), site_method)?;
            triples += 1;
        }

        if let Some(ref source) = pkg.source {
            writer.write_literal(&pkg_uri, &format!("{BUILDROOT}source"), source)?;
            triples += 1;
        }

        if let Some(ref license_files) = pkg.license_files {
            writer.write_literal(&pkg_uri, &format!("{BUILDROOT}licenseFiles"), license_files)?;
            triples += 1;
        }

        if let Some(ref cpe_vendor) = pkg.cpe_vendor {
            writer.write_literal(&pkg_uri, &format!("{BUILDROOT}cpeVendor"), cpe_vendor)?;
            triples += 1;
        }

        if let Some(ref cpe_product) = pkg.cpe_product {
            writer.write_literal(&pkg_uri, &format!("{BUILDROOT}cpeProduct"), cpe_product)?;
            triples += 1;
        }

        if let Some(ref conf_opts) = pkg.conf_opts {
            writer.write_literal(&pkg_uri, &format!("{BUILDROOT}configOpts"), conf_opts)?;
            triples += 1;
        }

        if let Some(ref infra) = pkg.infrastructure {
            writer.write_literal(&pkg_uri, &format!("{BUILDROOT}infrastructure"), infra)?;
            triples += 1;
        }

        if pkg.install_staging {
            writer.write_boolean(&pkg_uri, &format!("{BUILDROOT}installStaging"), true)?;
            triples += 1;
        }

        if pkg.install_target {
            writer.write_boolean(&pkg_uri, &format!("{BUILDROOT}installTarget"), true)?;
            triples += 1;
        }

        // Build dependencies
        for dep in &pkg.dependencies {
            let target_uri =
                package_identity_uri(&self.distro_name, &self.release_name, "any", dep);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id("dep", &format!("{}-{}", pkg_uri, dep));
            writer.write_bnode_object(&pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_subject(
                &bnode,
                &format!("{PKG}dependencyType"),
                &dep_type_uri("build"),
            )?;
            triples += 4;
        }

        // Host dependencies
        for dep in &pkg.host_dependencies {
            let target_uri =
                package_identity_uri(&self.distro_name, &self.release_name, "any", dep);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id("dep", &format!("{}-{}", pkg_uri, dep));
            writer.write_bnode_object(&pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_subject(
                &bnode,
                &format!("{PKG}dependencyType"),
                &dep_type_uri("host"),
            )?;
            triples += 4;
        }

        Ok(triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::{NamedTempFile, TempDir};

    fn create_test_package(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let pkg_dir = dir.join("package").join(name);
        fs::create_dir_all(&pkg_dir).unwrap();

        let mk_file = pkg_dir.join(format!("{}.mk", name));
        let mut file = File::create(&mk_file).unwrap();
        file.write_all(content.as_bytes()).unwrap();

        mk_file
    }

    #[test]
    fn test_parse_simple_package() {
        let temp_dir = TempDir::new().unwrap();
        let content = r#"
LIBFOO_VERSION = 1.2.3
LIBFOO_SITE = https://example.com/releases
LIBFOO_SITE_METHOD = wget
LIBFOO_LICENSE = MIT
LIBFOO_LICENSE_FILES = COPYING
LIBFOO_DEPENDENCIES = zlib openssl
"#;

        create_test_package(temp_dir.path(), "libfoo", content);

        let collector = BuildrootCollector::new(
            "buildroot".into(),
            "buildroot".into(),
            temp_dir.path().to_str().unwrap().to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let output_path = temp_file.path().to_str().unwrap();

        let (packages, triples) = collector.collect(output_path).unwrap();

        assert_eq!(packages, 1, "Should collect 1 package");
        assert!(triples > 0, "Should emit triples");

        // Read output and verify
        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // Check for dual typing
        assert!(
            content.contains("core#SourcePackage"),
            "Should have SourcePackage type"
        );
        assert!(
            content.contains("buildroot#BuildrootPackage"),
            "Should have BuildrootPackage type"
        );

        // Check for metadata
        assert!(
            content.contains("versionString"),
            "Should use versionString on Version node"
        );
        assert!(content.contains("\"1.2.3\""), "Should have version");
        assert!(
            content.contains("licenseName"),
            "Should use licenseName property"
        );
        assert!(content.contains("\"MIT\""), "Should have LICENSE");
        assert!(
            content.contains("\"https://example.com/releases\""),
            "Should have SITE"
        );
        assert!(
            content.contains("isVersionOf"),
            "Should use isVersionOf for identity"
        );
        assert!(
            content.contains("partOfDistribution"),
            "Should link to distribution"
        );
        assert!(content.contains("partOfRelease"), "Should link to release");
        assert!(
            !content.contains("packageVersion"),
            "Should NOT use packageVersion"
        );
    }

    #[test]
    fn test_host_dependencies() {
        let temp_dir = TempDir::new().unwrap();
        let content = r#"
FOO_VERSION = 1.0
FOO_LICENSE = GPL-2.0
FOO_DEPENDENCIES = libbar host-pkgconf
"#;

        create_test_package(temp_dir.path(), "foo", content);

        let collector = BuildrootCollector::new(
            "buildroot".into(),
            "buildroot".into(),
            temp_dir.path().to_str().unwrap().to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let output_path = temp_file.path().to_str().unwrap();

        collector.collect(output_path).unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // Check for dependency types (v0.6.0: property URIs, not string literals)
        assert!(
            content.contains("core#buildDependsOn"),
            "Should have buildDependsOn dependency type for build deps"
        );
        assert!(
            content.contains("core#buildDependsOn"),
            "Should have buildDependsOn dependency type for host- prefix"
        );
    }

    #[test]
    fn test_package_name_to_prefix() {
        let temp_dir = TempDir::new().unwrap();
        // Package name with dash: lib-foo should map to LIB_FOO_VERSION
        let content = r#"
LIB_FOO_VERSION = 2.0
LIB_FOO_LICENSE = BSD-3-Clause
"#;

        create_test_package(temp_dir.path(), "lib-foo", content);

        let collector = BuildrootCollector::new(
            "buildroot".into(),
            "buildroot".into(),
            temp_dir.path().to_str().unwrap().to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let output_path = temp_file.path().to_str().unwrap();

        let (packages, _) = collector.collect(output_path).unwrap();
        assert_eq!(packages, 1, "Should parse package with dash in name");

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("\"2.0\""),
            "Should extract version from LIB_FOO_ prefix"
        );
    }
}
