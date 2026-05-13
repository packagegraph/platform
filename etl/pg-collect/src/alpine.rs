use crate::forge::emit_dq_issue;
use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use flate2::read::MultiGzDecoder;
use regex::Regex;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Result};
use tar::Archive;

pub struct AlpineCollector {
    client: Client,
    mirror_url: String,
    distro_name: String,
    branch: String,
    repos: Vec<String>,
    arch: String,
    source_cache: Option<SourceCache>,
}

impl AlpineCollector {
    pub fn new(
        mirror_url: String,
        distro_name: String,
        branch: String,
        repos: Vec<String>,
        arch: String,
    ) -> Self {
        let client = crate::enricher::default_http_client();

        Self {
            client,
            mirror_url,
            distro_name,
            branch,
            repos,
            arch,
            source_cache: None,
        }
    }

    pub fn with_cache(mut self, cache_dir: &str) -> Result<Self> {
        self.source_cache = Some(SourceCache::new(cache_dir, "alpine")?);
        Ok(self)
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        // Emit distribution metadata
        self.emit_distribution_metadata(&mut writer)?;

        let mut total_packages = 0;
        let mut total_triples = 0;

        for repo in &self.repos {
            eprintln!("\nProcessing {}/{}/{}...", self.branch, repo, self.arch);

            match self.process_repo(&mut writer, repo) {
                Ok((pkgs, triples)) => {
                    total_packages += pkgs;
                    total_triples += triples;
                    eprintln!("  {} packages, {} triples", pkgs, triples);
                }
                Err(e) => {
                    eprintln!("  Error processing {}: {}", repo, e);
                    total_triples += emit_dq_issue(
                        &mut writer,
                        "alpine-collector",
                        &format!("repo_{}", repo),
                        &e.to_string(),
                        "parse_error",
                        "high",
                    )?;
                }
            }
        }

        // Fetch security data from secdb
        eprintln!("\nFetching Alpine security database...");
        match self.collect_secdb(&mut writer) {
            Ok(sec_triples) => {
                total_triples += sec_triples;
                eprintln!("Security: {} triples", sec_triples);
            }
            Err(e) => {
                eprintln!("Warning: secdb collection failed: {}", e);
                total_triples += emit_dq_issue(
                    &mut writer,
                    "alpine-collector",
                    "secdb",
                    &e.to_string(),
                    "fetch_error",
                    "medium",
                )?;
            }
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.branch);
        let arch_uri_val = arch_uri(&self.arch);
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Alpine Linux")?;
        writer.write_literal(&dist_uri, RDFS_LABEL, "Alpine")?;
        triples += 3;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        if is_numeric_release(&self.branch) {
            writer.write_literal(&rel_uri, &format!("{PKG}releaseVersion"), &self.branch)?;
        } else {
            writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), &self.branch)?;
        }
        // partOfDistribution auto-emits hasRelease inverse
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        writer.write_triple(&arch_uri_val, RDF_TYPE, &format!("{PKG}Architecture"))?;
        writer.write_literal(&arch_uri_val, &format!("{PKG}packageName"), &self.arch)?;
        triples += 2;

        Ok(triples)
    }

    fn process_repo(
        &self,
        writer: &mut NTriplesWriter,
        repo: &str,
    ) -> Result<(usize, usize)> {
        let url = format!(
            "{}/{}/{}/{}/APKINDEX.tar.gz",
            self.mirror_url.trim_end_matches('/'),
            self.branch,
            repo,
            self.arch
        );

        eprintln!("  Fetching {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let bytes = response
            .bytes()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        // Decompress gzip to memory (MultiGzDecoder handles concatenated gzip members
        // used by Alpine's signed APKINDEX archives), then parse tar
        let mut gz = MultiGzDecoder::new(&bytes[..]);
        let mut tar_bytes = Vec::new();
        std::io::Read::read_to_end(&mut gz, &mut tar_bytes)?;

        let mut archive = Archive::new(std::io::Cursor::new(&tar_bytes));

        let mut apkindex_content = String::new();

        for entry_result in archive.entries()? {
            let mut entry = entry_result?;
            let path = entry.path()?.to_string_lossy().to_string();
            if path == "APKINDEX" {
                entry.read_to_string(&mut apkindex_content)?;
                break;
            }
        }

        if apkindex_content.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "APKINDEX not found in archive",
            ));
        }

        let packages = parse_apkindex(&apkindex_content);
        eprintln!("  Parsed {} packages", packages.len());

        let mut total_triples = 0;
        for pkg in &packages {
            total_triples += self.emit_package_triples(writer, pkg, repo)?;
        }

        Ok((packages.len(), total_triples))
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &HashMap<String, String>,
        repo: &str,
    ) -> Result<usize> {
        let name = match pkg.get("P") {
            Some(n) => n,
            None => {
                eprintln!("  Warning: package missing P (name) field, skipping");
                return emit_dq_issue(
                    writer,
                    "alpine-collector",
                    "package_name",
                    "<missing>",
                    "missing_field",
                    "high",
                );
            }
        };
        let version = match pkg.get("V") {
            Some(v) => v,
            None => {
                eprintln!("  Warning: package {} missing V (version) field, skipping", name);
                return emit_dq_issue(
                    writer,
                    "alpine-collector",
                    "package_version",
                    name,
                    "missing_field",
                    "high",
                );
            }
        };

        let pkg_uri = package_uri(&self.distro_name, &self.branch, &self.arch, name, version);
        let identity_uri = package_identity_uri(&self.distro_name, &self.branch, &self.arch, name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{APK}ApkPackage"))?;
        triples += 2;

        // Package identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        // Core properties
        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri(&self.distro_name, &self.branch, name, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Architecture
        let arch_uri_val = arch_uri(&self.arch);
        writer.write_triple(&pkg_uri, &format!("{PKG}targetArchitecture"), &arch_uri_val)?;
        triples += 1;

        // Distribution and release
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.branch);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
        triples += 2;

        // Alpine-specific: repository name
        writer.write_literal(&pkg_uri, &format!("{APK}repoName"), repo)?;
        triples += 1;

        // Optional properties
        if let Some(desc) = pkg.get("T") {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(url) = pkg.get("U") {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), url)?;
            triples += 1;
        }
        if let Some(license) = pkg.get("L") {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }
        if let Some(size) = pkg.get("S") {
            if let Ok(s) = size.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}packageSize"), s)?;
                triples += 1;
            }
        }
        if let Some(isize_str) = pkg.get("I") {
            if let Ok(s) = isize_str.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}installSize"), s)?;
                triples += 1;
            }
        }
        if let Some(checksum) = pkg.get("C") {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), checksum)?;
            triples += 1;
        }
        if let Some(ts) = pkg.get("t") {
            writer.write_literal(&pkg_uri, &format!("{APK}buildDate"), ts)?;
            triples += 1;
        }

        // Maintainer
        if let Some(maintainer_str) = pkg.get("m") {
            let maint_re = Regex::new(r"^(.+?)\s*<(.+?)>$").unwrap();
            if let Some(caps) = maint_re.captures(maintainer_str) {
                let maint_name = caps.get(1).unwrap().as_str().trim();
                let maint_email = caps.get(2).unwrap().as_str().trim();
                let maint_uri = maintainer_uri(maint_email);
                writer.write_triple(&maint_uri, RDF_TYPE, &format!("{PKG}Person"))?;
                writer.write_literal(&maint_uri, &format!("{FOAF}name"), maint_name)?;
                writer.write_literal(&maint_uri, RDFS_LABEL, maint_name)?;
                writer.write_triple(&pkg_uri, &format!("{PKG}maintainedBy"), &maint_uri)?;
                triples += 4;
            }
        }

        // Origin → SourcePackage link
        if let Some(origin) = pkg.get("o") {
            let src_uri = source_uri(&self.distro_name, &self.branch, origin, version);
            writer.write_triple(&src_uri, RDF_TYPE, &format!("{PKG}SourcePackage"))?;
            writer.write_literal(&src_uri, &format!("{PKG}packageName"), origin)?;
            writer.write_triple(&pkg_uri, &format!("{PKG}builtFromSource"), &src_uri)?;
            writer.write_literal(&pkg_uri, &format!("{APK}apkOrigin"), origin)?;
            triples += 4;
        }

        // Dependencies
        if let Some(deps_str) = pkg.get("D") {
            triples += self.emit_dependencies(writer, &pkg_uri, &identity_uri, deps_str, "depends")?;
        }

        // Provides
        if let Some(provides_str) = pkg.get("p") {
            for prov in provides_str.split_whitespace() {
                let prov_name = prov.split('=').next().unwrap_or(prov);
                writer.write_literal(&pkg_uri, &format!("{PKG}provides"), prov_name)?;
                triples += 1;
            }
        }

        // Install-if
        if let Some(install_if) = pkg.get("i") {
            writer.write_literal(&pkg_uri, &format!("{APK}installIf"), install_if)?;
            triples += 1;
        }

        // Replaces
        if let Some(replaces) = pkg.get("r") {
            for rep in replaces.split_whitespace() {
                writer.write_literal(&pkg_uri, &format!("{PKG}replaces"), rep)?;
                triples += 1;
            }
        }

        Ok(triples)
    }

    fn emit_dependencies(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        _identity_uri: &str,
        deps_str: &str,
        dep_type: &str,
    ) -> Result<usize> {
        let dep_re = Regex::new(r"^([a-zA-Z0-9_.+-]+)([><=!~]+)?(.+)?$").unwrap();
        let mut triples = 0;

        for dep_entry in deps_str.split_whitespace() {
            // Skip virtual dependencies starting with ! or so:
            if dep_entry.starts_with('!') {
                continue;
            }

            let dep_name;
            let mut constraint_op = None;
            let mut constraint_val = None;

            if let Some(caps) = dep_re.captures(dep_entry) {
                dep_name = caps.get(1).unwrap().as_str();
                if let Some(op) = caps.get(2) {
                    constraint_op = Some(op.as_str().to_string());
                    if let Some(val) = caps.get(3) {
                        constraint_val = Some(val.as_str().to_string());
                    }
                }
            } else {
                dep_name = dep_entry;
            }

            let target_uri =
                package_identity_uri(&self.distro_name, &self.branch, &self.arch, dep_name);

            // Direct dependency link
            writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            // Reified dependency
            let bnode = bnode_id(dep_type, &format!("{}-{}", pkg_uri, dep_name));
            writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            triples += 3;

            // Dependency type as property URI (v0.6.0 properties-as-taxonomy)
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyType"), &dep_type_uri(dep_type))?;
            triples += 1;

            if let (Some(op), Some(val)) = (&constraint_op, &constraint_val) {
                let constraint_bnode = bnode_id("constraint", &format!("{}-{}", pkg_uri, dep_name));
                writer.write_bnode_to_bnode(&bnode, &format!("{PKG}hasVersionConstraint"), &constraint_bnode)?;
                writer.write_bnode_subject(&constraint_bnode, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
                writer.write_bnode_literal(&constraint_bnode, &format!("{PKG}versionConstraintOperator"), op)?;
                writer.write_bnode_literal(&constraint_bnode, &format!("{PKG}versionConstraintValue"), val)?;
                triples += 4;
            }
        }

        Ok(triples)
    }
}

// --- secdb CVE integration ---

/// A single CVE entry from Alpine's secdb.
#[derive(Debug, Deserialize)]
struct SecdbCve {
    #[serde(rename = "CVE")]
    cve_id: Option<String>,
}

/// A fixed-version entry from secdb: version → list of CVEs.
#[derive(Debug, Deserialize)]
struct SecdbFixedEntry {
    #[serde(flatten)]
    versions: HashMap<String, Vec<SecdbCve>>,
}

/// Secdb distroversion entry.
#[derive(Debug, Deserialize)]
struct SecdbDistro {
    distroversion: Option<String>,
    reponame: Option<String>,
    archs: Option<Vec<String>>,
    packages: Vec<SecdbPackage>,
}

/// A package entry from secdb.
#[derive(Debug, Deserialize)]
struct SecdbPackage {
    pkg: SecdbPkg,
}

#[derive(Debug, Deserialize)]
struct SecdbPkg {
    name: String,
    secfixes: Option<HashMap<String, Vec<String>>>,
}

impl AlpineCollector {
    /// Fetch and emit vulnerability triples from Alpine's secdb.
    pub fn collect_secdb(
        &self,
        writer: &mut NTriplesWriter,
    ) -> Result<usize> {
        let mut total_triples = 0;

        for repo in &self.repos {
            let url = format!(
                "https://secdb.alpinelinux.org/{}/{}.json",
                self.branch, repo
            );
            eprintln!("  Fetching secdb: {}", url);

            match self.fetch_secdb(&url) {
                Ok(secdb) => {
                    let triples = self.emit_secdb_triples(writer, &secdb)?;
                    total_triples += triples;
                    eprintln!("  {} security triples from {}", triples, repo);
                }
                Err(e) => {
                    eprintln!("  Warning: secdb fetch failed for {}: {}", repo, e);
                    total_triples += emit_dq_issue(
                        writer,
                        "alpine-collector",
                        &format!("secdb_{}", repo),
                        &e.to_string(),
                        "fetch_error",
                        "medium",
                    )?;
                }
            }
        }

        Ok(total_triples)
    }

    fn fetch_secdb(&self, url: &str) -> std::result::Result<SecdbDistro, String> {
        let response = self
            .client
            .get(url)
            .send()
            .map_err(|e| e.to_string())?;

        let text = response.text().map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn emit_secdb_triples(
        &self,
        writer: &mut NTriplesWriter,
        secdb: &SecdbDistro,
    ) -> Result<usize> {
        let mut triples = 0;

        for pkg_entry in &secdb.packages {
            let pkg_name = &pkg_entry.pkg.name;

            if let Some(secfixes) = &pkg_entry.pkg.secfixes {
                for (fixed_version, cve_ids) in secfixes {
                    for cve_id_str in cve_ids {
                        // Skip non-CVE entries (some entries are descriptions)
                        if !cve_id_str.starts_with("CVE-") {
                            continue;
                        }

                        let cve_uri = format!("{DATA}cve/{}", encode_uri_component(cve_id_str));

                        // Vulnerability type
                        writer.write_triple(&cve_uri, RDF_TYPE, &format!("{SEC}Vulnerability"))?;
                        writer.write_literal(&cve_uri, &format!("{SEC}cveId"), cve_id_str)?;
                        triples += 2;

                        // Fixed version link
                        let fixed_ver_uri = version_uri(&self.distro_name, &self.branch, pkg_name, fixed_version);
                        writer.write_triple(&cve_uri, &format!("{SEC}fixedInVersion"), &fixed_ver_uri)?;
                        triples += 1;

                        // Affected package link (the package identity)
                        let identity = package_identity_uri(&self.distro_name, &self.branch, &self.arch, pkg_name);
                        writer.write_triple(&cve_uri, &format!("{SEC}affectsPackage"), &identity)?;
                        triples += 1;
                    }
                }
            }
        }

        Ok(triples)
    }
}

fn encode_uri_component(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// Parse APKINDEX text into a list of package field maps.
pub fn parse_apkindex(content: &str) -> Vec<HashMap<String, String>> {
    let mut packages = Vec::new();
    let mut current: HashMap<String, String> = HashMap::new();

    for line in content.lines() {
        if line.is_empty() {
            if !current.is_empty() {
                packages.push(current.clone());
                current.clear();
            }
            continue;
        }

        if let Some(idx) = line.find(':') {
            let key = &line[..idx];
            let value = &line[idx + 1..];
            current.insert(key.to_string(), value.to_string());
        }
    }

    // Don't forget last package if no trailing newline
    if !current.is_empty() {
        packages.push(current);
    }

    packages
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    const SAMPLE_APKINDEX: &str = "\
C:Q1abcdef123456==
P:curl
V:8.7.1-r0
A:x86_64
S:247432
I:1064960
T:URL retrieval utility and library
U:https://curl.se/
L:curl
o:curl
m:Natanael Copa <ncopa@alpinelinux.org>
t:1714000000
c:abc123def456
D:ca-certificates libcurl>=8.7.1-r0
p:cmd:curl=8.7.1-r0

C:Q1xyz789==
P:bash
V:5.2.26-r0
A:x86_64
S:500000
I:2000000
T:The GNU Bourne Again shell
U:https://www.gnu.org/software/bash/
L:GPL-3.0-or-later
o:bash
m:Soren Tempel <stempel@alpinelinux.org>
D:readline ncurses-libs

";

    #[test]
    fn test_parse_apkindex() {
        let packages = parse_apkindex(SAMPLE_APKINDEX);
        assert_eq!(packages.len(), 2);

        let curl = &packages[0];
        assert_eq!(curl.get("P").unwrap(), "curl");
        assert_eq!(curl.get("V").unwrap(), "8.7.1-r0");
        assert_eq!(curl.get("A").unwrap(), "x86_64");
        assert_eq!(curl.get("D").unwrap(), "ca-certificates libcurl>=8.7.1-r0");
        assert_eq!(curl.get("o").unwrap(), "curl");

        let bash = &packages[1];
        assert_eq!(bash.get("P").unwrap(), "bash");
        assert_eq!(bash.get("L").unwrap(), "GPL-3.0-or-later");
    }

    #[test]
    fn test_emit_package_triples_produces_dual_typing() {
        let collector = AlpineCollector::new(
            "https://mirror.example.com/alpine".into(),
            "alpine".into(),
            "v3.20".into(),
            vec!["main".into()],
            "x86_64".into(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut pkg = HashMap::new();
        pkg.insert("P".into(), "curl".into());
        pkg.insert("V".into(), "8.7.1-r0".into());
        pkg.insert("T".into(), "URL retrieval utility".into());

        let triples = collector.emit_package_triples(&mut writer, &pkg, "main").unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Verify dual typing
        assert!(content.contains("core#BinaryPackage"));
        assert!(content.contains("apk#ApkPackage"));
        assert!(content.contains("\"curl\""));
        assert!(triples > 10);
    }

    #[test]
    fn test_emit_dependencies_with_version_constraint() {
        let collector = AlpineCollector::new(
            "https://mirror.example.com/alpine".into(),
            "alpine".into(),
            "v3.20".into(),
            vec!["main".into()],
            "x86_64".into(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg_uri = package_uri("alpine", "v3.20", "x86_64", "curl", "8.7.1-r0");
        let identity_uri = package_identity_uri("alpine", "v3.20", "x86_64", "curl");

        let triples = collector.emit_dependencies(
            &mut writer,
            &pkg_uri,
            &identity_uri,
            "libcurl>=8.7.1-r0 ca-certificates",
            "depends",
        ).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("directlyDependsOn"));
        assert!(content.contains("hasDependency"));
        assert!(content.contains("Dependency"));
        assert!(triples >= 8); // 2 deps * 4 triples each minimum
    }

    #[test]
    fn test_secdb_parsing_and_triples() {
        let collector = AlpineCollector::new(
            "https://mirror.example.com/alpine".into(),
            "alpine".into(),
            "v3.20".into(),
            vec!["main".into()],
            "x86_64".into(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let secdb_json = r#"{
            "distroversion": "v3.20",
            "reponame": "main",
            "packages": [
                {
                    "pkg": {
                        "name": "curl",
                        "secfixes": {
                            "8.7.1-r0": ["CVE-2024-2398", "CVE-2024-2379"],
                            "8.6.0-r0": ["CVE-2024-0853"]
                        }
                    }
                }
            ]
        }"#;

        let secdb: SecdbDistro = serde_json::from_str(secdb_json).unwrap();
        let triples = collector.emit_secdb_triples(&mut writer, &secdb).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should have CVE triples
        assert!(content.contains("CVE-2024-2398"));
        assert!(content.contains("CVE-2024-2379"));
        assert!(content.contains("CVE-2024-0853"));
        assert!(content.contains("Vulnerability"));
        assert!(content.contains("fixedInVersion"));
        assert!(content.contains("affectsPackage"));
        // 3 CVEs × 4 triples each = 12
        assert_eq!(triples, 12);
    }

    #[test]
    fn test_maintainer_parsing() {
        let collector = AlpineCollector::new(
            "https://mirror.example.com/alpine".into(),
            "alpine".into(),
            "v3.20".into(),
            vec!["main".into()],
            "x86_64".into(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut pkg = HashMap::new();
        pkg.insert("P".into(), "test".into());
        pkg.insert("V".into(), "1.0-r0".into());
        pkg.insert("m".into(), "Natanael Copa <ncopa@alpinelinux.org>".into());

        collector.emit_package_triples(&mut writer, &pkg, "main").unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("maintainedBy"));
        assert!(content.contains("Natanael Copa"));
        assert!(content.contains("ncopa@alpinelinux.org"));
    }
}
