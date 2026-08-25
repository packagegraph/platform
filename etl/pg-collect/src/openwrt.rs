use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::uris::*;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Result};
use std::path::Path;
use walkdir::WalkDir;

// Regex for parsing PKG_* variables: PKG_VERSION = 1.2.3
static PKG_VAR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^\s*PKG_(NAME|VERSION|RELEASE|SOURCE_URL|SOURCE_PROTO|LICENSE|MAINTAINER|HASH|MIRROR_HASH)\s*:?=\s*(.*)$"#).unwrap()
});

// Regex for define Package/<name> blocks
static DEFINE_PKG: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^\s*define\s+Package/(\S+)\s*$"#).unwrap());

#[derive(Debug)]
struct OpenWrtPackage {
    name: String,
    version: Option<String>,
    release: Option<String>,
    source_url: Option<String>,
    source_proto: Option<String>,
    license: Option<String>,
    maintainer: Option<String>,
    section: Option<String>,
    category: Option<String>,
    title: Option<String>,
    url: Option<String>,
    depends: Vec<String>,
    feed: String,
    source_hash: Option<String>,
    parent_package: Option<String>,
}

/// Metadata extracted from Makefile for downstream stages
#[derive(Debug, Clone)]
pub struct OpenWrtPackageMeta {
    pub source_url: Option<String>,
    pub source_proto: Option<String>,
    pub source_hash: Option<String>,
}

pub struct OpenWrtCollector {
    distro_name: String,
    release_name: String,
    feed_path: String,
    pub graph_uri: Option<String>,
}

impl OpenWrtCollector {
    pub fn new(distro_name: String, release_name: String, feed_path: String) -> Self {
        Self {
            distro_name,
            release_name,
            feed_path,
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

        let mut seen = HashSet::new();
        let mut identity_map = HashMap::new();
        let mut parsed_meta = HashMap::new();
        let mut parent_map = HashMap::new();

        let result = self.collect_with_writer(
            &mut writer,
            &mut seen,
            &mut identity_map,
            &mut parsed_meta,
            &mut parent_map,
            false,
        )?;
        writer.flush()?;
        Ok(result)
    }

    pub fn collect_with_writer(
        &self,
        writer: &mut NTriplesWriter,
        seen: &mut HashSet<(String, String)>,
        identity_map: &mut HashMap<String, String>,
        parsed_meta: &mut HashMap<String, OpenWrtPackageMeta>,
        parent_map: &mut HashMap<String, String>,
        is_secondary: bool,
    ) -> Result<(usize, usize)> {
        let mut total_packages = 0;
        let mut total_triples = 0;

        // Extract feed name from path
        let feed_name = Path::new(&self.feed_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        // Walk through feed looking for Makefiles
        for entry in WalkDir::new(&self.feed_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().file_name().and_then(|s| s.to_str()) == Some("Makefile"))
        {
            match self.parse_makefile(entry.path(), feed_name) {
                Ok(packages) => {
                    for pkg in packages {
                        let default_version = "0".to_string();
                        let version = pkg.version.as_ref().unwrap_or(&default_version);
                        let dedup_key = (pkg.name.clone(), version.clone());

                        if seen.contains(&dedup_key) {
                            // Duplicate from another feed - only emit feed triple
                            let pkg_uri = package_uri(
                                &self.distro_name,
                                &self.release_name,
                                "any",
                                &pkg.name,
                                version,
                            );
                            writer.write_literal(&pkg_uri, &format!("{OPENWRT}feed"), &pkg.feed)?;
                            total_triples += 1;
                        } else {
                            // New package - emit full triples
                            total_triples +=
                                self.emit_package_triples(writer, &pkg, is_secondary)?;
                            total_packages += 1;

                            // Track in dedup set
                            seen.insert(dedup_key);

                            // Build identity map (name → source_uri), first-wins
                            let pkg_uri = package_uri(
                                &self.distro_name,
                                &self.release_name,
                                "any",
                                &pkg.name,
                                version,
                            );
                            identity_map
                                .entry(pkg.name.clone())
                                .or_insert(pkg_uri.clone());

                            // Build parsed metadata, first-wins
                            parsed_meta
                                .entry(pkg.name.clone())
                                .or_insert(OpenWrtPackageMeta {
                                    source_url: pkg.source_url.clone(),
                                    source_proto: pkg.source_proto.clone(),
                                    source_hash: pkg.source_hash.clone(),
                                });

                            // Build parent map
                            if let Some(ref parent) = pkg.parent_package {
                                parent_map.entry(pkg.name.clone()).or_insert(parent.clone());
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("  Error parsing {:?}: {}", entry.path(), e);
                }
            }

            if total_packages % 1000 == 0 && total_packages > 0 {
                eprintln!("Progress: {} packages", total_packages);
            }
        }

        Ok((total_packages, total_triples))
    }

    pub fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.release_name);
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "OpenWRT")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "openwrt")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn parse_makefile(
        &self,
        makefile_path: &Path,
        feed_name: &str,
    ) -> std::result::Result<Vec<OpenWrtPackage>, String> {
        let file = File::open(makefile_path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);

        // Top-level PKG_* variables shared across all sub-packages
        let mut global_vars: HashMap<String, String> = HashMap::new();

        // Package definitions from "define Package/<name>" blocks
        let mut packages: Vec<OpenWrtPackage> = Vec::new();

        #[derive(Debug)]
        enum State {
            TopLevel,
            InPackageDefine(String), // package name
        }

        let mut state = State::TopLevel;
        let mut current_pkg_vars: HashMap<String, String> = HashMap::new();

        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            let trimmed = line.trim();

            match &state {
                State::TopLevel => {
                    // Check for PKG_* variables
                    if let Some(caps) = PKG_VAR.captures(trimmed) {
                        let var_name = caps.get(1).unwrap().as_str();
                        let value = caps.get(2).unwrap().as_str().trim();
                        global_vars.insert(var_name.to_string(), value.to_string());
                    }
                    // Check for define Package/<name>
                    else if let Some(caps) = DEFINE_PKG.captures(trimmed) {
                        let pkg_name = caps.get(1).unwrap().as_str().to_string();
                        state = State::InPackageDefine(pkg_name);
                        current_pkg_vars.clear();
                    }
                }
                State::InPackageDefine(pkg_name) => {
                    if trimmed == "endef" {
                        // Determine primary package name for sub-package linking
                        let primary_name = global_vars.get("NAME").cloned();
                        let parent = if primary_name.as_ref() != Some(pkg_name) {
                            primary_name
                        } else {
                            None
                        };

                        // Resolve hash: prefer MIRROR_HASH, fall back to HASH
                        let hash = global_vars
                            .get("MIRROR_HASH")
                            .or_else(|| global_vars.get("HASH"))
                            .cloned();

                        // End of define block - create package
                        let pkg = OpenWrtPackage {
                            name: pkg_name.clone(),
                            version: global_vars.get("VERSION").cloned(),
                            release: global_vars.get("RELEASE").cloned(),
                            source_url: global_vars.get("SOURCE_URL").cloned(),
                            source_proto: global_vars.get("SOURCE_PROTO").cloned(),
                            license: global_vars.get("LICENSE").cloned(),
                            maintainer: global_vars.get("MAINTAINER").cloned(),
                            section: current_pkg_vars.get("SECTION").cloned(),
                            category: current_pkg_vars.get("CATEGORY").cloned(),
                            title: current_pkg_vars.get("TITLE").cloned(),
                            url: current_pkg_vars.get("URL").cloned(),
                            depends: current_pkg_vars
                                .get("DEPENDS")
                                .map(|d| self.parse_depends(d))
                                .unwrap_or_default(),
                            feed: feed_name.to_string(),
                            source_hash: hash,
                            parent_package: parent,
                        };
                        packages.push(pkg);
                        state = State::TopLevel;
                    } else if let Some(idx) = trimmed.find(":=") {
                        // Variable assignment within define block
                        let var_name = trimmed[..idx].trim();
                        let value = trimmed[idx + 2..].trim();
                        current_pkg_vars.insert(var_name.to_string(), value.to_string());
                    }
                }
            }
        }

        Ok(packages)
    }

    fn parse_depends(&self, depends_str: &str) -> Vec<String> {
        depends_str
            .split_whitespace()
            .filter(|dep| !dep.starts_with('@')) // Skip Kconfig conditionals
            .map(|dep| dep.trim_start_matches('+').to_string()) // Strip required marker
            .collect()
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &OpenWrtPackage,
        is_secondary: bool,
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

        // Type as SourcePackage (OpkgPackage → SourcePackage → Package via subClassOf)
        // Do NOT emit pkg:Package explicitly - redundant after v0.9.0-pre reclassification
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{OPENWRT}OpkgPackage"))?;
        triples += 1;

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

        // Feed
        writer.write_literal(&pkg_uri, &format!("{OPENWRT}feed"), &pkg.feed)?;
        triples += 1;

        // Optional metadata
        if let Some(ref release) = pkg.release {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}pkgRelease"), release)?;
            triples += 1;
        }

        if let Some(ref license) = pkg.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }

        if let Some(ref maintainer) = pkg.maintainer {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}maintainer"), maintainer)?;
            triples += 1;
        }

        if let Some(ref section) = pkg.section {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}section"), section)?;
            triples += 1;
        }

        if let Some(ref category) = pkg.category {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}category"), category)?;
            triples += 1;
        }

        if let Some(ref title) = pkg.title {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}title"), title)?;
            triples += 1;
        }

        if let Some(ref url) = pkg.url {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}url"), url)?;
            triples += 1;
        }

        if let Some(ref source_url) = pkg.source_url {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}sourceUrl"), source_url)?;
            triples += 1;

            // Upstream repo extraction: git proto URLs are explicit repo refs,
            // archive URLs can be normalized to repo root
            if let Some(extraction) = crate::forge::extract_forge_url(source_url) {
                triples +=
                    crate::forge::emit_upstream_repo(writer, &identity_uri, &extraction, None)?;
            }
        }

        if let Some(ref source_proto) = pkg.source_proto {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}sourceProto"), source_proto)?;
            triples += 1;
        }

        if let Some(ref hash) = pkg.source_hash {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}sourceMirrorHash"), hash)?;
            triples += 1;
        }

        // Parent package link (sub-packages link back to primary)
        if let Some(ref parent) = pkg.parent_package {
            let parent_uri =
                package_identity_uri(&self.distro_name, &self.release_name, "any", parent);
            writer.write_triple(&pkg_uri, &format!("{OPENWRT}parentPackage"), &parent_uri)?;
            triples += 1;
        }

        // Dependencies
        for dep in &pkg.depends {
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
                &dep_type_uri("runtime"),
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

    fn create_test_makefile(
        dir: &Path,
        category: &str,
        name: &str,
        content: &str,
    ) -> std::path::PathBuf {
        let pkg_dir = dir.join(category).join(name);
        fs::create_dir_all(&pkg_dir).unwrap();

        let makefile = pkg_dir.join("Makefile");
        let mut file = File::create(&makefile).unwrap();
        file.write_all(content.as_bytes()).unwrap();

        makefile
    }

    #[test]
    fn test_parse_simple_package() {
        let temp_dir = TempDir::new().unwrap();
        let content = r#"
PKG_NAME:=testpkg
PKG_VERSION:=1.0
PKG_RELEASE:=1
PKG_LICENSE:=MIT

define Package/testpkg
  SECTION:=net
  CATEGORY:=Network
  TITLE:=Test package
  DEPENDS:=+libfoo libbar
  URL:=https://example.com
endef
"#;

        create_test_makefile(temp_dir.path(), "network", "testpkg", content);

        let collector = OpenWrtCollector::new(
            "openwrt".into(),
            "openwrt".into(),
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

        // Check for OpkgPackage typing (pkg:Package is inferred via subClassOf, not explicit)
        assert!(
            content.contains("opkg#OpkgPackage"),
            "Should have OpkgPackage type"
        );
        assert!(
            !content.contains("core#Package>"),
            "Should NOT have explicit Package type (inferred)"
        );

        // Check for metadata
        assert!(content.contains("\"1.0\""), "Should have version");
        assert!(content.contains("\"MIT\""), "Should have LICENSE");
        assert!(content.contains("\"Network\""), "Should have CATEGORY");
    }

    #[test]
    fn test_depends_syntax() {
        let temp_dir = TempDir::new().unwrap();
        let content = r#"
PKG_NAME:=deptest
PKG_VERSION:=1.0

define Package/deptest
  TITLE:=Dependency test
  DEPENDS:=+libfoo libbar @FEATURE_X
endef
"#;

        create_test_makefile(temp_dir.path(), "utils", "deptest", content);

        let collector = OpenWrtCollector::new(
            "openwrt".into(),
            "openwrt".into(),
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

        // Should include libfoo and libbar (both required), but skip @FEATURE_X
        assert!(content.contains("libfoo"), "Should have libfoo dependency");
        assert!(content.contains("libbar"), "Should have libbar dependency");
        assert!(
            !content.contains("FEATURE_X"),
            "Should skip @FEATURE_X Kconfig conditional"
        );
    }

    #[test]
    fn test_no_explicit_package_typing() {
        // After v0.9.0-pre: OpkgPackage → SourcePackage → Package via subClassOf
        // Should NOT emit explicit pkg:Package type (redundant)
        let temp_dir = TempDir::new().unwrap();
        let content = r#"
PKG_NAME:=testpkg
PKG_VERSION:=1.0

define Package/testpkg
  TITLE:=Test
endef
"#;

        create_test_makefile(temp_dir.path(), "net", "testpkg", content);

        let collector = OpenWrtCollector::new(
            "openwrt".into(),
            "24.10".into(),
            temp_dir.path().to_str().unwrap().to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let output_path = temp_file.path().to_str().unwrap();

        collector.collect(output_path).unwrap();

        let mut nt_content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut nt_content)
            .unwrap();

        // Should have OpkgPackage type
        assert!(
            nt_content.contains("opkg#OpkgPackage"),
            "Should have OpkgPackage type"
        );
        // Should NOT have explicit Package type (inferred via subClassOf)
        assert!(
            !nt_content.contains("core#Package>"),
            "Should NOT emit explicit Package type (redundant after reclassification)"
        );
    }

    #[test]
    fn test_collect_with_writer_cross_feed_dedup() {
        // Two feeds with the same package (name + version)
        let feed1 = TempDir::new().unwrap();
        let feed2 = TempDir::new().unwrap();

        let makefile_content = r#"
PKG_NAME:=shared
PKG_VERSION:=1.0
PKG_RELEASE:=1
PKG_SOURCE_URL:=https://example.com/shared.tar.gz

define Package/shared
  CATEGORY:=Network
  TITLE:=Shared package
endef
"#;

        create_test_makefile(feed1.path(), "net", "shared", makefile_content);
        create_test_makefile(feed2.path(), "utils", "shared", makefile_content);

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut seen = std::collections::HashSet::new();
        let mut identity_map = std::collections::HashMap::new();
        let mut parsed_meta = std::collections::HashMap::new();
        let mut parent_map = std::collections::HashMap::new();

        let collector1 = OpenWrtCollector::new(
            "openwrt".into(),
            "24.10".into(),
            feed1.path().join("net").to_str().unwrap().to_string(),
        );
        let collector2 = OpenWrtCollector::new(
            "openwrt".into(),
            "24.10".into(),
            feed2.path().join("utils").to_str().unwrap().to_string(),
        );

        // Primary feed
        let (pkgs1, _) = collector1
            .collect_with_writer(
                &mut writer,
                &mut seen,
                &mut identity_map,
                &mut parsed_meta,
                &mut parent_map,
                false,
            )
            .unwrap();
        // Secondary feed (should dedup)
        let (pkgs2, _) = collector2
            .collect_with_writer(
                &mut writer,
                &mut seen,
                &mut identity_map,
                &mut parsed_meta,
                &mut parent_map,
                true,
            )
            .unwrap();

        assert_eq!(pkgs1, 1, "First feed: 1 package");
        assert_eq!(pkgs2, 0, "Second feed: deduped, no new packages");

        writer.flush().unwrap();

        // Verify identity_map and parsed_meta populated
        assert_eq!(identity_map.len(), 1, "Should have 1 entry in identity_map");
        assert!(
            identity_map.contains_key("shared"),
            "identity_map should have 'shared'"
        );
        assert_eq!(parsed_meta.len(), 1, "Should have 1 entry in parsed_meta");

        let meta = parsed_meta.get("shared").unwrap();
        assert_eq!(
            meta.source_url,
            Some("https://example.com/shared.tar.gz".to_string())
        );

        // Read output and verify opkg:feed preserved for both feeds
        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // Should have BOTH feed values emitted (net from first, utils from second)
        assert!(content.contains("opkg#feed"), "Should have feed property");
        // Count occurrences of feed triple - should be 2 (one per feed even with dedup)
        let feed_count = content.matches("opkg#feed").count();
        assert_eq!(
            feed_count, 2,
            "Should preserve feed membership for both feeds despite dedup"
        );
    }

    #[test]
    fn test_multiple_subpackages() {
        let temp_dir = TempDir::new().unwrap();
        let content = r#"
PKG_NAME:=multi
PKG_VERSION:=1.0

define Package/multi
  TITLE:=Main package
endef

define Package/multi-utils
  TITLE:=Utilities package
endef
"#;

        create_test_makefile(temp_dir.path(), "utils", "multi", content);

        let collector = OpenWrtCollector::new(
            "openwrt".into(),
            "openwrt".into(),
            temp_dir.path().to_str().unwrap().to_string(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let output_path = temp_file.path().to_str().unwrap();

        let (packages, _) = collector.collect(output_path).unwrap();

        // Should collect both sub-packages
        assert_eq!(
            packages, 2,
            "Should collect 2 sub-packages from one Makefile"
        );

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("\"Main package\""),
            "Should have main package title"
        );
        assert!(
            content.contains("\"Utilities package\""),
            "Should have utils package title"
        );
    }
}
