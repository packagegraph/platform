use crate::ntriples::NTriplesWriter;
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Result};
use std::path::Path;
use walkdir::WalkDir;

/// Matches simple variable assignments: VAR="value" or VAR=value
static VAR_RE_QUOTED: Lazy<Regex> = Lazy::new(|| Regex::new(r#"^\s*([A-Z_]+)="([^"]*)""#).unwrap());
static VAR_RE_UNQUOTED: Lazy<Regex> = Lazy::new(|| Regex::new(r#"^\s*([A-Z_]+)=([^\s]+)"#).unwrap());

/// Matches Gentoo dependency atoms: category/package-name (no version).
/// Anchored to avoid capturing version suffixes like "glib-2" as part of the name.
static DEP_ATOM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|[\s(])(?:[<>=!~]*)?([a-z0-9_-]+/[a-z0-9_.+-]+?)(?:[-:]\d|[\s)\n]|$)").unwrap());

pub struct GentooCollector {
    distro_name: String,
    release_name: String,
    repo_path: String,
    source_cache: Option<SourceCache>,
}

impl GentooCollector {
    pub fn new(distro_name: String, release_name: String, repo_path: String) -> Self {
        Self { distro_name, release_name, repo_path, source_cache: None }
    }

    pub fn with_cache(mut self, cache_dir: &str) -> Result<Self> {
        self.source_cache = Some(SourceCache::new(cache_dir, "gentoo")?);
        Ok(self)
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);
        self.emit_distribution_metadata(&mut writer)?;

        let mut total_packages = 0;
        let mut total_triples = 0;

        for entry in fs::read_dir(&self.repo_path)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let category_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) if !n.starts_with('.') && !n.starts_with('_') && n.contains('-') => n,
                _ => continue,
            };

            for pkg_entry in fs::read_dir(&path)? {
                let pkg_entry = pkg_entry?;
                let pkg_path = pkg_entry.path();
                if !pkg_path.is_dir() {
                    continue;
                }
                let pkg_name = match pkg_path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };

                for ebuild_file in WalkDir::new(&pkg_path)
                    .max_depth(1)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path().extension().and_then(|s| s.to_str()) == Some("ebuild")
                    })
                {
                    match self.parse_ebuild(category_name, pkg_name, ebuild_file.path()) {
                        Ok(pkg) => {
                            total_triples += self.emit_package_triples(&mut writer, &pkg)?;
                            total_packages += 1;
                            if total_packages % 1000 == 0 {
                                eprintln!("Progress: {} packages", total_packages);
                            }
                        }
                        Err(e) => {
                            eprintln!("  Error parsing {:?}: {}", ebuild_file.path(), e);
                        }
                    }
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
        writer.write_literal(&dist_uri, RDFS_LABEL, "Gentoo")?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Gentoo")?;
        triples += 3;
        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "gentoo")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;
        Ok(triples)
    }

    fn parse_ebuild(
        &self,
        category: &str,
        pkg_name: &str,
        ebuild_path: &Path,
    ) -> std::result::Result<GentooPackage, String> {
        let filename = ebuild_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("Invalid filename")?;

        let version = filename
            .strip_prefix(&format!("{}-", pkg_name))
            .ok_or("Filename doesn't match package name")?
            .to_string();

        let file = File::open(ebuild_path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);
        let mut pkg = GentooPackage {
            category: category.to_string(),
            name: pkg_name.to_string(),
            version,
            eapi: None,
            description: None,
            homepage: None,
            license: None,
            slot: None,
            subslot: None,
            keywords: Vec::new(),
            iuse: Vec::new(),
            depend: Vec::new(),
            rdepend: Vec::new(),
            bdepend: Vec::new(),
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
                "EAPI" => pkg.eapi = Some(value.to_string()),
                "DESCRIPTION" => pkg.description = Some(value.to_string()),
                "HOMEPAGE" => pkg.homepage = Some(value.to_string()),
                "LICENSE" => pkg.license = Some(value.to_string()),
                "SLOT" => {
                    let parts: Vec<&str> = value.split('/').collect();
                    pkg.slot = Some(parts[0].to_string());
                    if parts.len() > 1 {
                        pkg.subslot = Some(parts[1].to_string());
                    }
                }
                "KEYWORDS" => {
                    pkg.keywords = value.split_whitespace().map(|s| s.to_string()).collect()
                }
                "IUSE" => {
                    pkg.iuse = value.split_whitespace().map(|s| s.to_string()).collect()
                }
                "DEPEND" => {
                    pkg.depend = parse_dependencies(value);
                }
                "RDEPEND" => {
                    pkg.rdepend = parse_dependencies(value);
                }
                "BDEPEND" => {
                    pkg.bdepend = parse_dependencies(value);
                }
                _ => {}
            }
        }

        Ok(pkg)
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &GentooPackage,
    ) -> Result<usize> {
        let full_name = format!("{}/{}", pkg.category, pkg.name);
        // Gentoo has no arch in the same sense as RPM/Debian — use category as the
        // organizational axis. URIs: d/pkg/gentoo/gentoo/{category}/{name}/{version}
        let pkg_uri = package_uri(&self.distro_name, &self.release_name, &pkg.category, &pkg.name, &pkg.version);
        let identity_uri =
            package_identity_uri(&self.distro_name, &self.release_name, &pkg.category, &pkg.name);
        let mut triples = 0;

        // Gentoo ebuilds are source packages — they compile from source
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}SourcePackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{GENTOO}PortagePackage"))?;
        triples += 2;

        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &full_name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &full_name)?;
        triples += 1;

        // Category as a separate property for filtering
        writer.write_literal(&pkg_uri, &format!("{GENTOO}category"), &pkg.category)?;
        triples += 1;

        let ver_uri = version_uri(&self.distro_name, &self.release_name, &full_name, &pkg.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &pkg.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        let rel_uri = release_uri(&self.distro_name, &self.release_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
        triples += 1;

        if let Some(desc) = &pkg.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }

        if let Some(homepage) = &pkg.homepage {
            // Gentoo HOMEPAGE can have multiple space-separated URLs.
            // First URL goes to pkg:homepage; try all URLs for upstream repo.
            let urls: Vec<&str> = homepage.split_whitespace()
                .filter(|u| !u.contains("${") && !u.is_empty())
                .collect();

            if let Some(first_url) = urls.first() {
                writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), first_url)?;
                triples += 1;
            }

            // Try all HOMEPAGE URLs for upstream repo extraction (best confidence wins)
            let candidates: Vec<(&str, &str)> = urls.iter()
                .map(|u| ("homepage", *u))
                .collect();
            if let Some(extraction) = crate::forge::extract_best_repo(&candidates) {
                triples += crate::forge::emit_upstream_repo(writer, &identity_uri, &extraction, None)?;
            }
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

        if let Some(eapi) = &pkg.eapi {
            writer.write_literal(&pkg_uri, &format!("{GENTOO}eapi"), eapi)?;
            triples += 1;
        }

        if let Some(slot) = &pkg.slot {
            writer.write_literal(&pkg_uri, &format!("{GENTOO}slot"), slot)?;
            triples += 1;
        }

        if let Some(subslot) = &pkg.subslot {
            writer.write_literal(&pkg_uri, &format!("{GENTOO}subslot"), subslot)?;
            triples += 1;
        }

        // USE flags as literal values (flag names, stripped of +/- defaults)
        for flag in &pkg.iuse {
            let clean_flag = flag.trim_start_matches('+').trim_start_matches('-');
            if !clean_flag.is_empty() {
                writer.write_literal(&pkg_uri, &format!("{GENTOO}hasUseFlag"), clean_flag)?;
                triples += 1;
            }
        }

        // Dependencies — deduplicate across DEPEND/RDEPEND/BDEPEND
        let mut seen_deps: HashSet<String> = HashSet::new();
        for dep in pkg.depend.iter().chain(pkg.rdepend.iter()).chain(pkg.bdepend.iter()) {
            if seen_deps.insert(dep.clone()) {
                if let Some((dep_cat, dep_name)) = dep.split_once('/') {
                    let target_uri =
                        package_identity_uri(&self.distro_name, &self.release_name, dep_cat, dep_name);
                    writer.write_triple(&target_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
                    writer.write_literal(&target_uri, &format!("{PKG}packageName"), dep)?;
                    writer.write_triple(
                        &pkg_uri,
                        &format!("{PKG}directlyDependsOn"),
                        &target_uri,
                    )?;
                    triples += 3;
                }
            }
        }

        Ok(triples)
    }
}

/// Parse Gentoo dependency string into clean category/name atoms.
/// Strips version constraints, slot operators, and USE conditionals.
fn parse_dependencies(dep_string: &str) -> Vec<String> {
    let mut deps = Vec::new();
    // Simple tokenizer: extract category/name atoms
    for token in dep_string.split_whitespace() {
        // Strip leading quotes (ebuild parsing artifacts) and version operators
        let cleaned = token
            .trim_start_matches('"')
            .trim_start_matches(">=")
            .trim_start_matches("<=")
            .trim_start_matches('>')
            .trim_start_matches('<')
            .trim_start_matches('=')
            .trim_start_matches('~')
            .trim_start_matches('!');

        // Must contain a slash (category/name)
        if let Some(slash_pos) = cleaned.find('/') {
            let candidate = &cleaned[..];
            // Find where the name ends (before version, slot, or end)
            // Version starts with -DIGIT after the package name
            let after_slash = &candidate[slash_pos + 1..];
            let name_end = find_version_start(after_slash);
            let atom = if let Some(end) = name_end {
                &candidate[..slash_pos + 1 + end]
            } else {
                // Strip trailing slot operators
                candidate
                    .split(':')
                    .next()
                    .unwrap_or(candidate)
                    .split('[')
                    .next()
                    .unwrap_or(candidate)
            };

            if atom.contains('/') && !atom.contains('$') && !atom.contains('(') {
                deps.push(atom.to_string());
            }
        }
    }
    deps
}

/// Find where the version starts in a package name.
/// In Gentoo, versions start with -DIGIT (e.g., glib-2.78 → version starts at "-2").
/// But package names can contain digits (gtk+3, python3), so we look for
/// -DIGIT.DIGIT or -DIGIT followed by end/non-alpha to be safe.
fn find_version_start(name: &str) -> Option<usize> {
    let bytes = name.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            // Check if this looks like a version: -DIGIT. or -DIGIT at end
            if i + 2 >= bytes.len() || bytes[i + 2] == b'.' || bytes[i + 2] == b'-' {
                return Some(i);
            }
        }
    }
    None
}

#[derive(Debug)]
struct GentooPackage {
    category: String,
    name: String,
    version: String,
    eapi: Option<String>,
    description: Option<String>,
    homepage: Option<String>,
    license: Option<String>,
    slot: Option<String>,
    subslot: Option<String>,
    keywords: Vec<String>,
    iuse: Vec<String>,
    depend: Vec<String>,
    rdepend: Vec<String>,
    bdepend: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::{NamedTempFile, TempDir};

    fn make_test_package() -> GentooPackage {
        GentooPackage {
            category: "dev-libs".to_string(),
            name: "openssl".to_string(),
            version: "3.2.1".to_string(),
            eapi: Some("8".to_string()),
            description: Some("TLS/SSL toolkit".to_string()),
            homepage: Some("https://www.openssl.org/ https://github.com/openssl/openssl".to_string()),
            license: Some("Apache-2.0".to_string()),
            slot: Some("0".to_string()),
            subslot: Some("3".to_string()),
            keywords: vec!["amd64".to_string(), "~arm64".to_string()],
            iuse: vec!["+asm".to_string(), "cpu_flags_x86_sse2".to_string(), "doc".to_string()],
            depend: vec!["sys-libs/zlib".to_string(), "dev-lang/perl".to_string()],
            rdepend: vec!["sys-libs/zlib".to_string()],  // duplicate with DEPEND
            bdepend: vec!["dev-build/make".to_string()],
        }
    }

    #[test]
    fn test_parse_ebuild_basic() {
        let collector = GentooCollector::new("gentoo".into(), "gentoo".into(), "/tmp".to_string());
        let content = r#"
EAPI=8
DESCRIPTION="A text editor for GNOME"
HOMEPAGE="https://gedit-technology.github.io/apps/gedit/"
LICENSE="GPL-2+"
SLOT="0/46"
KEYWORDS="~amd64 ~x86"
IUSE="+spell debug"
DEPEND="dev-libs/glib x11-libs/gtk+:3"
RDEPEND="x11-libs/gtk+:3"
BDEPEND="sys-devel/gettext"
"#;

        let temp_dir = TempDir::new().unwrap();
        let ebuild_path = temp_dir.path().join("gedit-46.2.ebuild");
        File::create(&ebuild_path)
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();

        let pkg = collector.parse_ebuild("app-editors", "gedit", &ebuild_path).unwrap();

        assert_eq!(pkg.category, "app-editors");
        assert_eq!(pkg.name, "gedit");
        assert_eq!(pkg.version, "46.2");
        assert_eq!(pkg.eapi, Some("8".to_string()));
        assert_eq!(pkg.description, Some("A text editor for GNOME".to_string()));
        assert_eq!(pkg.slot, Some("0".to_string()));
        assert_eq!(pkg.subslot, Some("46".to_string()));
        assert!(pkg.iuse.contains(&"+spell".to_string()));
        assert!(pkg.iuse.contains(&"debug".to_string()));
        assert_eq!(pkg.keywords, vec!["~amd64", "~x86"]);
    }

    #[test]
    fn test_package_uri_no_double_category() {
        let collector = GentooCollector::new("gentoo".into(), "gentoo".into(), "/tmp".to_string());
        let pkg = make_test_package();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
        collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Package URI should use category as path segment, name without category prefix
        // e.g., d/pkg/gentoo/gentoo/dev-libs/openssl/3.2.1
        // The encode() function in uris.rs percent-encodes special characters.
        // For Gentoo, the package name "openssl" has no special chars, so it
        // should appear unencoded. But the full_name "dev-libs/openssl" would
        // have the slash encoded. Check that we're passing name (not full_name)
        // to the URI builder.
        let pkg_line = content.lines()
            .find(|l| l.contains("3.2.1") && l.contains("rdf-syntax"))
            .unwrap_or("NOT FOUND");
        assert!(
            !pkg_line.contains("dev-libs%2F"),
            "Category should not be encoded in package name. Got: {}", pkg_line
        );
    }

    #[test]
    fn test_dependency_uri_no_double_slash() {
        let collector = GentooCollector::new("gentoo".into(), "gentoo".into(), "/tmp".to_string());
        let pkg = make_test_package();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
        collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Dependency URI should be d/pkg/gentoo/gentoo/sys-libs/zlib (no double slash)
        assert!(content.contains("d/pkg/gentoo/gentoo/sys-libs/zlib"), "Dependency URI should use category as arch segment");
        assert!(!content.contains("gentoo//"), "No double slash in dependency URIs");
    }

    #[test]
    fn test_dependency_dedup_across_dep_types() {
        let collector = GentooCollector::new("gentoo".into(), "gentoo".into(), "/tmp".to_string());
        let pkg = make_test_package();  // sys-libs/zlib appears in both DEPEND and RDEPEND
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
        collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // sys-libs/zlib should appear as directlyDependsOn exactly once (deduped)
        let dep_count = content.matches("directlyDependsOn").count();
        // Should be 3: zlib (deduped), perl, make
        assert_eq!(dep_count, 3, "Dependencies should be deduplicated across DEPEND/RDEPEND/BDEPEND");
        // Each dep target should have PackageIdentity type and packageName
        assert!(content.contains("d/pkg/gentoo/gentoo/sys-libs/zlib> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type>"), "Dep target should have type");
    }

    #[test]
    fn test_source_package_typing() {
        let collector = GentooCollector::new("gentoo".into(), "gentoo".into(), "/tmp".to_string());
        let pkg = make_test_package();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
        collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#SourcePackage"), "Gentoo packages should be typed as SourcePackage");
        assert!(!content.contains("core#Package>"), "Should not use generic Package type");
        assert!(content.contains("portage#PortagePackage"), "Should have Gentoo-specific type");
    }

    #[test]
    fn test_use_flags_not_empty() {
        let collector = GentooCollector::new("gentoo".into(), "gentoo".into(), "/tmp".to_string());
        let pkg = make_test_package();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
        collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // USE flags should have actual names, not empty strings
        assert!(content.contains("\"asm\""), "USE flag 'asm' should be present (+ prefix stripped)");
        assert!(content.contains("\"cpu_flags_x86_sse2\""), "USE flag should be present");
        assert!(content.contains("\"doc\""), "USE flag 'doc' should be present");
        assert!(!content.contains("hasUseFlag> \"\""), "No empty USE flag values");
    }

    #[test]
    fn test_category_as_separate_property() {
        let collector = GentooCollector::new("gentoo".into(), "gentoo".into(), "/tmp".to_string());
        let pkg = make_test_package();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
        collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("portage#category"), "Category should be a separate property");
        assert!(content.contains("\"dev-libs\""), "Category value should be just the category");
    }

    #[test]
    fn test_homepage_takes_first_url() {
        let collector = GentooCollector::new("gentoo".into(), "gentoo".into(), "/tmp".to_string());
        let pkg = make_test_package();  // homepage has two URLs
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
        collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("\"https://www.openssl.org/\""), "Should take first homepage URL");
        // Second HOMEPAGE URL should appear as upstreamRepository, not in homepage literal
        assert!(content.contains("core#upstreamRepository"), "Should emit upstreamRepository from second HOMEPAGE URL");
        assert!(content.contains("vcs#repositoryURL"), "Should emit repositoryURL for upstream repo");
    }

    #[test]
    fn test_parse_dependencies_strips_versions() {
        let deps = parse_dependencies(">=dev-libs/glib-2.78:2 sys-libs/zlib x11-libs/gtk+-3.24");
        assert!(deps.contains(&"dev-libs/glib".to_string()), "Should strip version from glib-2.78");
        assert!(deps.contains(&"sys-libs/zlib".to_string()));
        assert!(deps.contains(&"x11-libs/gtk+".to_string()), "Should strip version from gtk+-3.24");
    }

    #[test]
    fn test_parse_dependencies_strips_slot_operators() {
        let deps = parse_dependencies("dev-libs/openssl:0= sys-libs/glibc:2.2+");
        assert!(deps.contains(&"dev-libs/openssl".to_string()), "Should strip slot operator :0=");
        assert!(deps.contains(&"sys-libs/glibc".to_string()), "Should strip slot operator :2.2+");
    }

    #[test]
    fn test_parse_dependencies_skips_virtuals_and_bash() {
        let deps = parse_dependencies("use? ( dev-libs/foo ) || ( dev-libs/bar dev-libs/baz ) ${DEPEND}");
        assert!(deps.contains(&"dev-libs/foo".to_string()));
        assert!(deps.contains(&"dev-libs/bar".to_string()));
        assert!(deps.contains(&"dev-libs/baz".to_string()));
        // Should not contain bash variables
        assert!(!deps.iter().any(|d| d.contains('$')), "Should skip bash variables");
    }

    #[test]
    fn test_parse_dependencies_strips_leading_quotes() {
        let deps = parse_dependencies(r#"">=dev-libs/apr-1.5:= ">=dev-libs/apr-util-1.5:=""#);
        assert!(deps.contains(&"dev-libs/apr".to_string()), "Should strip leading quote and version");
        assert!(deps.contains(&"dev-libs/apr-util".to_string()), "Should strip leading quote and version");
        assert!(!deps.iter().any(|d| d.contains('"')), "No quotes in dependency atoms");
    }

    #[test]
    fn test_parse_dependencies_handles_use_flags() {
        let deps = parse_dependencies("dev-libs/libxml2[python] >=dev-libs/json-c-0.13:=[threads]");
        assert!(deps.contains(&"dev-libs/libxml2".to_string()), "Should strip [python] USE flag");
        assert!(deps.contains(&"dev-libs/json-c".to_string()), "Should strip version and [threads]");
    }

    #[test]
    fn test_find_version_start() {
        assert_eq!(find_version_start("glib-2.78"), Some(4));
        assert_eq!(find_version_start("zlib"), None);
        assert_eq!(find_version_start("gtk+"), None);
        assert_eq!(find_version_start("json-c-0.13"), Some(6));
        // gtk+-3.24: the -3 looks like a version start
        assert_eq!(find_version_start("gtk+-3.24"), Some(4));
        // python3 should NOT have a version start (3 is part of the name)
        assert_eq!(find_version_start("python3"), None);
    }

    #[test]
    fn test_full_ebuild_roundtrip() {
        let collector = GentooCollector::new("gentoo".into(), "gentoo".into(), "/tmp".to_string());
        let content = r#"
EAPI=8
DESCRIPTION="SSL/TLS toolkit"
HOMEPAGE="https://www.openssl.org/"
LICENSE="Apache-2.0"
SLOT="0/3"
KEYWORDS="amd64 arm64"
IUSE="+asm doc test"
DEPEND=">=sys-libs/zlib-1.2.13:= dev-lang/perl"
RDEPEND="sys-libs/zlib:="
BDEPEND="dev-build/make"
"#;
        let temp_dir = TempDir::new().unwrap();
        let ebuild_path = temp_dir.path().join("openssl-3.2.1.ebuild");
        File::create(&ebuild_path)
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();

        let pkg = collector.parse_ebuild("dev-libs", "openssl", &ebuild_path).unwrap();

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
        let triple_count = collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Structural checks
        assert!(triple_count >= 15, "Should produce at least 15 triples, got {}", triple_count);
        assert!(content.contains("core#SourcePackage"));
        assert!(content.contains("portage#PortagePackage"));
        assert!(content.contains("d/pkg/gentoo/gentoo/dev-libs/openssl/3.2.1"));
        assert!(content.contains("d/pkg/gentoo/gentoo/dev-libs/openssl>"));  // identity URI
        assert!(content.contains("\"dev-libs/openssl\""));  // packageName
        assert!(content.contains("\"dev-libs\""));  // category
        assert!(content.contains("\"3.2.1\""));  // version
        assert!(content.contains("\"0\""));  // slot
        assert!(content.contains("\"3\""));  // subslot
        assert!(content.contains("\"asm\""));  // USE flag (+ stripped)
        assert!(content.contains("\"doc\""));
        assert!(content.contains("\"test\""));
        // Dependencies
        assert!(content.contains("d/pkg/gentoo/gentoo/sys-libs/zlib"));
        assert!(content.contains("d/pkg/gentoo/gentoo/dev-lang/perl"));
        assert!(content.contains("d/pkg/gentoo/gentoo/dev-build/make"));
        // No double slash
        assert!(!content.contains("gentoo//"));
    }
}
