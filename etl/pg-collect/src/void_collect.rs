use crate::ntriples::NTriplesWriter;
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use once_cell::sync::Lazy;
use regex::Regex;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Result};
use std::path::Path;

static VAR_RE_QUOTED: Lazy<Regex> = Lazy::new(|| Regex::new(r#"^([a-z_]+)="([^"]*)""#).unwrap());
static VAR_RE_UNQUOTED: Lazy<Regex> = Lazy::new(|| Regex::new(r#"^([a-z_]+)=([^\s]+)"#).unwrap());

pub struct VoidCollector {
    distro_name: String,
    release_name: String,
    repo_path: String,
    source_cache: Option<SourceCache>,
    pub graph_uri: Option<String>,
}

impl VoidCollector {
    pub fn new(distro_name: String, release_name: String, repo_path: String) -> Self {
        Self {
            distro_name,
            release_name,
            repo_path,
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
        self.source_cache = Some(SourceCache::new(cache_dir, "void")?);
        Ok(self)
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());
        self.emit_distribution_metadata(&mut writer)?;

        let srcpkgs_path = Path::new(&self.repo_path).join("srcpkgs");
        let mut total_packages = 0;
        let mut total_triples = 0;

        // Walk through srcpkgs directory
        for entry in fs::read_dir(&srcpkgs_path)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let pkg_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) if !n.starts_with('.') => n,
                _ => continue,
            };

            let template_path = path.join("template");
            if !template_path.exists() {
                continue;
            }

            match self.parse_template(pkg_name, &template_path) {
                Ok(pkg) => {
                    total_triples += self.emit_package_triples(&mut writer, &pkg)?;
                    total_packages += 1;
                    if total_packages % 1000 == 0 {
                        eprintln!("Progress: {} packages", total_packages);
                    }
                }
                Err(e) => {
                    eprintln!("  Error parsing {:?}: {}", template_path, e);
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
        writer.write_literal(&dist_uri, RDFS_LABEL, "Void")?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Void Linux")?;
        triples += 3;
        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "void")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;
        Ok(triples)
    }

    fn parse_template(
        &self,
        pkg_name: &str,
        template_path: &Path,
    ) -> std::result::Result<VoidPackage, String> {
        let file = File::open(template_path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);

        let mut pkg = VoidPackage {
            pkgname: pkg_name.to_string(),
            version: None,
            revision: None,
            short_desc: None,
            homepage: None,
            license: None,
            build_style: None,
            depends: Vec::new(),
            makedepends: Vec::new(),
            hostmakedepends: Vec::new(),
        };

        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;

            let (var_name, value) = if let Some(caps) = VAR_RE_QUOTED.captures(&line) {
                (caps.get(1).unwrap().as_str(), caps.get(2).unwrap().as_str())
            } else if let Some(caps) = VAR_RE_UNQUOTED.captures(&line) {
                (caps.get(1).unwrap().as_str(), caps.get(2).unwrap().as_str())
            } else {
                continue;
            };

            match var_name {
                "pkgname" => pkg.pkgname = value.to_string(),
                "version" => pkg.version = Some(value.to_string()),
                "revision" => pkg.revision = Some(value.to_string()),
                "short_desc" => pkg.short_desc = Some(value.to_string()),
                "homepage" => pkg.homepage = Some(value.to_string()),
                "license" => pkg.license = Some(value.to_string()),
                "build_style" => pkg.build_style = Some(value.to_string()),
                "depends" => {
                    pkg.depends = value.split_whitespace().map(|s| s.to_string()).collect()
                }
                "makedepends" => {
                    pkg.makedepends = value.split_whitespace().map(|s| s.to_string()).collect()
                }
                "hostmakedepends" => {
                    pkg.hostmakedepends = value.split_whitespace().map(|s| s.to_string()).collect()
                }
                _ => {}
            }
        }

        if pkg.version.is_none() {
            return Err("Missing version".to_string());
        }

        Ok(pkg)
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &VoidPackage,
    ) -> Result<usize> {
        let version = pkg.version.as_ref().unwrap();
        let pkg_uri = package_uri(
            &self.distro_name,
            &self.release_name,
            "any",
            &pkg.pkgname,
            version,
        );
        let identity_uri =
            package_identity_uri(&self.distro_name, &self.release_name, "any", &pkg.pkgname);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{VOID}XbpsPackage"))?;
        triples += 2;

        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &pkg.pkgname)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.pkgname)?;
        triples += 1;

        let ver_uri = version_uri(&self.distro_name, &self.release_name, &pkg.pkgname, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        if let Some(desc) = &pkg.short_desc {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }

        if let Some(homepage) = &pkg.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
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

        if let Some(revision) = &pkg.revision {
            writer.write_literal(&pkg_uri, &format!("{VOID}revision"), revision)?;
            triples += 1;
        }

        if let Some(build_style) = &pkg.build_style {
            writer.write_literal(&pkg_uri, &format!("{VOID}buildStyle"), build_style)?;
            triples += 1;
        }

        // Dependencies
        for dep in &pkg.depends {
            let target_uri =
                package_identity_uri(&self.distro_name, &self.release_name, "any", dep);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;
        }
        for dep in &pkg.makedepends {
            let target_uri =
                package_identity_uri(&self.distro_name, &self.release_name, "any", dep);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;
        }
        for dep in &pkg.hostmakedepends {
            let target_uri =
                package_identity_uri(&self.distro_name, &self.release_name, "any", dep);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;
        }

        Ok(triples)
    }
}

#[derive(Debug)]
struct VoidPackage {
    pkgname: String,
    version: Option<String>,
    revision: Option<String>,
    short_desc: Option<String>,
    homepage: Option<String>,
    license: Option<String>,
    build_style: Option<String>,
    depends: Vec<String>,
    makedepends: Vec<String>,
    hostmakedepends: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_parse_template() {
        let collector = VoidCollector::new("void".into(), "void".into(), "/tmp".to_string());
        let template_content = r#"
pkgname=firefox
version=125.0
revision=1
short_desc="Mozilla Firefox web browser"
homepage="https://www.mozilla.org/firefox/"
license="MPL-2.0"
build_style=gnu-configure
depends="gtk+3 dbus-glib"
makedepends="rust llvm"
hostmakedepends="python3 nodejs"
"#;

        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("template");
        let mut file = File::create(&template_path).unwrap();
        file.write_all(template_content.as_bytes()).unwrap();

        let pkg = collector.parse_template("firefox", &template_path).unwrap();

        assert_eq!(pkg.pkgname, "firefox");
        assert_eq!(pkg.version, Some("125.0".to_string()));
        assert_eq!(pkg.revision, Some("1".to_string()));
        assert_eq!(pkg.build_style, Some("gnu-configure".to_string()));
        assert!(pkg.depends.contains(&"gtk+3".to_string()));
        assert!(pkg.makedepends.contains(&"rust".to_string()));
    }

    #[test]
    fn test_emit_void_package_dual_typing() {
        let collector = VoidCollector::new("void".into(), "void".into(), "/tmp".to_string());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg = VoidPackage {
            pkgname: "firefox".to_string(),
            version: Some("125.0".to_string()),
            revision: Some("1".to_string()),
            short_desc: Some("Web browser".to_string()),
            homepage: Some("https://firefox.com".to_string()),
            license: Some("MPL-2.0".to_string()),
            build_style: Some("gnu-configure".to_string()),
            depends: vec!["gtk+3".to_string()],
            makedepends: vec!["rust".to_string()],
            hostmakedepends: vec!["python3".to_string()],
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
        assert!(content.contains("xbps#XbpsPackage"));
        assert!(content.contains("\"firefox\""));
        assert!(content.contains("xbps#buildStyle"));
        assert!(content.contains("\"gnu-configure\""));
        assert!(content.contains("xbps#revision"));
        assert!(content.contains("\"1\""));
        assert!(content.contains("directlyDependsOn"));
        assert!(triples > 15);
    }
}
