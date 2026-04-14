use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::uris::*;
use flate2::read::GzDecoder;
use regex::Regex;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Result};
use std::time::Duration;
use tar::Archive;

pub struct ArchCollector {
    client: Client,
    mirror_url: String,
    repos: Vec<String>,
    include_aur: bool,
}

impl ArchCollector {
    pub fn new(mirror_url: String, repos: Vec<String>, include_aur: bool) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            mirror_url,
            repos,
            include_aur,
        }
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let mut total_packages = 0;
        let mut total_triples = 0;

        // Official repos
        for repo in &self.repos {
            eprintln!("\nProcessing official repo: {}", repo);
            match self.process_repo(&mut writer, repo) {
                Ok((pkgs, triples)) => {
                    total_packages += pkgs;
                    total_triples += triples;
                    eprintln!("  {} packages, {} triples", pkgs, triples);
                }
                Err(e) => eprintln!("  Error: {}", e),
            }
        }

        // AUR
        if self.include_aur {
            eprintln!("\nProcessing AUR...");
            match self.process_aur(&mut writer) {
                Ok((pkgs, triples)) => {
                    total_packages += pkgs;
                    total_triples += triples;
                    eprintln!("AUR: {} packages, {} triples", pkgs, triples);
                }
                Err(e) => eprintln!("AUR error: {}", e),
            }
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("arch");
        let rel_uri = release_uri("arch", "rolling");
        let arch_uri_val = arch_uri("x86_64");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Arch Linux")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "rolling")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        writer.write_triple(&arch_uri_val, RDF_TYPE, &format!("{PKG}Architecture"))?;
        triples += 1;

        Ok(triples)
    }

    fn process_repo(
        &self,
        writer: &mut NTriplesWriter,
        repo: &str,
    ) -> Result<(usize, usize)> {
        let url = format!(
            "{}/{}/os/x86_64/{}.db.tar.gz",
            self.mirror_url.trim_end_matches('/'),
            repo,
            repo
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

        let gz = GzDecoder::new(&bytes[..]);
        let mut archive = Archive::new(gz);

        // Parse all desc and depends files
        let mut packages: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();

        for entry_result in archive.entries()? {
            let mut entry = entry_result?;
            let path = entry.path()?.to_string_lossy().to_string();

            // path is like "curl-8.7.1-3/desc" or "curl-8.7.1-3/depends"
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() != 2 {
                continue;
            }

            let pkg_dir = parts[0].to_string();
            let file_type = parts[1];

            if file_type != "desc" && file_type != "depends" {
                continue;
            }

            let mut content = String::new();
            entry.read_to_string(&mut content)?;

            let sections = parse_desc_file(&content);
            let pkg_entry = packages.entry(pkg_dir).or_default();
            for (key, values) in sections {
                pkg_entry.insert(key, values);
            }
        }

        let mut total_triples = 0;
        let pkg_count = packages.len();

        for (_dir, fields) in &packages {
            total_triples += self.emit_arch_package_triples(writer, fields, repo)?;
        }

        Ok((pkg_count, total_triples))
    }

    fn emit_arch_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        fields: &HashMap<String, Vec<String>>,
        repo: &str,
    ) -> Result<usize> {
        let name = match fields.get("%NAME%").and_then(|v| v.first()) {
            Some(n) => n.clone(),
            None => return Ok(0),
        };
        let version = match fields.get("%VERSION%").and_then(|v| v.first()) {
            Some(v) => v.clone(),
            None => return Ok(0),
        };

        let pkg_uri = package_uri("arch", "rolling", "x86_64", &name, &version);
        let identity_uri = package_identity_uri("arch", "rolling", "x86_64", &name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{ARCH}ArchPackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        // Core properties
        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("arch", "rolling", &name, &version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("arch");
        let rel_uri = release_uri("arch", "rolling");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
        triples += 2;

        // Architecture
        if let Some(arch) = fields.get("%ARCH%").and_then(|v| v.first()) {
            let arch_uri_val = arch_uri(arch);
            writer.write_triple(&pkg_uri, &format!("{PKG}targetArchitecture"), &arch_uri_val)?;
            triples += 1;
        }

        // Arch-specific: repository
        writer.write_literal(&pkg_uri, &format!("{ARCH}inGroup"), repo)?;
        triples += 1;

        // Optional properties
        if let Some(desc) = fields.get("%DESC%").and_then(|v| v.first()) {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(url) = fields.get("%URL%").and_then(|v| v.first()) {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), url)?;
            triples += 1;
        }
        if let Some(license_vals) = fields.get("%LICENSE%") {
            for lic in license_vals {
                writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), lic)?;
                triples += 1;
            }
        }
        if let Some(isize_str) = fields.get("%ISIZE%").and_then(|v| v.first()) {
            if let Ok(s) = isize_str.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}installSize"), s)?;
                triples += 1;
            }
        }
        if let Some(csize_str) = fields.get("%CSIZE%").and_then(|v| v.first()) {
            if let Ok(s) = csize_str.parse::<i64>() {
                writer.write_integer(&pkg_uri, &format!("{PKG}packageSize"), s)?;
                triples += 1;
            }
        }
        if let Some(sha) = fields.get("%SHA256SUM%").and_then(|v| v.first()) {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), sha)?;
            triples += 1;
        }
        if let Some(builddate) = fields.get("%BUILDDATE%").and_then(|v| v.first()) {
            writer.write_literal(&pkg_uri, &format!("{ARCH}lastModified"), builddate)?;
            triples += 1;
        }

        // Maintainer (PACKAGER field: "Name <email>")
        if let Some(packager) = fields.get("%PACKAGER%").and_then(|v| v.first()) {
            let maint_re = Regex::new(r"^(.+?)\s*<(.+?)>$").unwrap();
            if let Some(caps) = maint_re.captures(packager) {
                let maint_name = caps.get(1).unwrap().as_str().trim();
                let maint_email = caps.get(2).unwrap().as_str().trim();
                let maint_uri = maintainer_uri(maint_email);
                writer.write_triple(&maint_uri, RDF_TYPE, &format!("{PKG}Maintainer"))?;
                writer.write_literal(&maint_uri, &format!("{FOAF}name"), maint_name)?;
                writer.write_triple(&pkg_uri, &format!("{PKG}maintainedBy"), &maint_uri)?;
                triples += 3;
            }
        }

        // Dependencies
        if let Some(deps) = fields.get("%DEPENDS%") {
            triples += self.emit_arch_deps(writer, &pkg_uri, deps, "depends")?;
        }
        if let Some(deps) = fields.get("%MAKEDEPENDS%") {
            triples += self.emit_arch_deps(writer, &pkg_uri, deps, "makedepends")?;
        }
        if let Some(deps) = fields.get("%OPTDEPENDS%") {
            // optdepends format: "dep: description"
            let dep_names: Vec<String> = deps
                .iter()
                .map(|d| d.split(':').next().unwrap_or(d).trim().to_string())
                .collect();
            triples += self.emit_arch_deps(writer, &pkg_uri, &dep_names, "optdepends")?;
        }
        if let Some(deps) = fields.get("%CONFLICTS%") {
            for dep in deps {
                let dep_name = dep.split(|c: char| c == '>' || c == '<' || c == '=')
                    .next().unwrap_or(dep);
                writer.write_literal(&pkg_uri, &format!("{PKG}conflicts"), dep_name)?;
                triples += 1;
            }
        }
        if let Some(deps) = fields.get("%REPLACES%") {
            for dep in deps {
                writer.write_literal(&pkg_uri, &format!("{PKG}replaces"), dep)?;
                triples += 1;
            }
        }
        if let Some(deps) = fields.get("%PROVIDES%") {
            for dep in deps {
                let prov_name = dep.split('=').next().unwrap_or(dep);
                writer.write_literal(&pkg_uri, &format!("{PKG}provides"), prov_name)?;
                triples += 1;
            }
        }

        Ok(triples)
    }

    fn emit_arch_deps(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        deps: &[String],
        dep_type: &str,
    ) -> Result<usize> {
        let dep_re = Regex::new(r"^([a-zA-Z0-9_.@+-]+)([><=]+)?(.+)?$").unwrap();
        let mut triples = 0;

        for dep_entry in deps {
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

            let target_uri = package_identity_uri("arch", "rolling", "x86_64", dep_name);

            writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id(dep_type, &format!("{}-{}", pkg_uri, dep_name));
            writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_literal(&bnode, &format!("{PKG}dependencyType"), dep_type)?;
            triples += 4;

            if let (Some(op), Some(val)) = (&constraint_op, &constraint_val) {
                let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, dep_name));
                writer.write_bnode_object(&bnode, &format!("{PKG}hasVersionConstraint"), &cb)?;
                writer.write_bnode_subject(&cb, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
                writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintOperator"), op)?;
                writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintValue"), val)?;
                triples += 4;
            }
        }

        Ok(triples)
    }

    // --- AUR ---

    fn process_aur(&self, writer: &mut NTriplesWriter) -> Result<(usize, usize)> {
        // Fetch AUR package list
        let pkg_list = self.fetch_aur_package_list()?;
        eprintln!("  AUR package list: {} packages", pkg_list.len());

        let mut total_packages = 0;
        let mut total_triples = 0;
        let batch_size = 100;

        for chunk in pkg_list.chunks(batch_size) {
            match self.fetch_aur_batch(chunk) {
                Ok(results) => {
                    for pkg in &results {
                        total_triples += self.emit_aur_package_triples(writer, pkg)?;
                        total_packages += 1;
                    }
                }
                Err(e) => eprintln!("  AUR batch error: {}", e),
            }

            // Rate limiting
            std::thread::sleep(Duration::from_secs(1));

            if total_packages % 1000 == 0 && total_packages > 0 {
                eprintln!("  AUR progress: {} packages", total_packages);
            }
        }

        Ok((total_packages, total_triples))
    }

    fn fetch_aur_package_list(&self) -> Result<Vec<String>> {
        let url = "https://aur.archlinux.org/packages.gz";
        eprintln!("  Fetching AUR package list from {}", url);

        let response = self
            .client
            .get(url)
            .send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let bytes = response
            .bytes()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let mut gz = GzDecoder::new(&bytes[..]);
        let mut content = String::new();
        gz.read_to_string(&mut content)?;

        let names: Vec<String> = content
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.to_string())
            .collect();

        Ok(names)
    }

    fn fetch_aur_batch(&self, names: &[String]) -> std::result::Result<Vec<AurPackage>, String> {
        let mut url = "https://aur.archlinux.org/rpc/v5/info?".to_string();
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                url.push('&');
            }
            url.push_str(&format!("arg[]={}", name));
        }

        let response = self.client.get(&url).send().map_err(|e| e.to_string())?;
        let text = response.text().map_err(|e| e.to_string())?;
        let rpc: AurRpcResponse = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok(rpc.results)
    }

    fn emit_aur_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &AurPackage,
    ) -> Result<usize> {
        let version = pkg.version.as_deref().unwrap_or("unknown");
        let pkg_uri = package_uri("arch", "aur", "x86_64", &pkg.name, version);
        let identity_uri = package_identity_uri("arch", "aur", "x86_64", &pkg.name);
        let mut triples = 0;

        // Dual typing + AUR type
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}BinaryPackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{ARCH}ArchPackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{ARCH}AUR"))?;
        triples += 3;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &pkg.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("arch", "aur", &pkg.name, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("arch");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        if let Some(desc) = &pkg.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(url) = &pkg.url {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), url)?;
            triples += 1;
        }

        // AUR-specific
        if let Some(votes) = pkg.num_votes {
            writer.write_integer(&pkg_uri, &format!("{ARCH}aurVotes"), votes)?;
            triples += 1;
        }
        if let Some(pop) = &pkg.popularity {
            writer.write_literal(&pkg_uri, &format!("{ARCH}aurPopularity"), &pop.to_string())?;
            triples += 1;
        }
        if pkg.out_of_date.is_some() {
            writer.write_boolean(&pkg_uri, &format!("{ARCH}outOfDate"), true)?;
            triples += 1;
        }

        // Maintainer
        if let Some(maint) = &pkg.maintainer {
            let maint_uri = maintainer_uri(&format!("{}@aur.archlinux.org", maint));
            writer.write_triple(&maint_uri, RDF_TYPE, &format!("{PKG}Maintainer"))?;
            writer.write_literal(&maint_uri, &format!("{FOAF}name"), maint)?;
            writer.write_triple(&pkg_uri, &format!("{PKG}maintainedBy"), &maint_uri)?;
            triples += 3;
        }

        // Dependencies
        if let Some(deps) = &pkg.depends {
            let dep_strs: Vec<String> = deps.clone();
            triples += self.emit_arch_deps(writer, &pkg_uri, &dep_strs, "depends")?;
        }
        if let Some(deps) = &pkg.make_depends {
            let dep_strs: Vec<String> = deps.clone();
            triples += self.emit_arch_deps(writer, &pkg_uri, &dep_strs, "makedepends")?;
        }

        // License
        if let Some(licenses) = &pkg.license {
            for lic in licenses {
                writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), lic)?;
                triples += 1;
            }
        }

        Ok(triples)
    }
}

/// Parse Arch repo desc/depends file format.
/// Sections are delimited by %FIELD% lines.
pub fn parse_desc_file(content: &str) -> HashMap<String, Vec<String>> {
    let mut sections = HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_values: Vec<String> = Vec::new();

    for line in content.lines() {
        if line.starts_with('%') && line.ends_with('%') {
            // Save previous section
            if let Some(key) = current_key.take() {
                sections.insert(key, current_values.clone());
                current_values.clear();
            }
            current_key = Some(line.to_string());
        } else if !line.is_empty() {
            current_values.push(line.to_string());
        }
    }

    // Save last section
    if let Some(key) = current_key {
        sections.insert(key, current_values);
    }

    sections
}

// --- AUR serde types ---

#[derive(Debug, Deserialize)]
struct AurRpcResponse {
    results: Vec<AurPackage>,
}

#[derive(Debug, Deserialize)]
pub struct AurPackage {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Version")]
    pub version: Option<String>,
    #[serde(rename = "Description")]
    pub description: Option<String>,
    #[serde(rename = "URL")]
    pub url: Option<String>,
    #[serde(rename = "Maintainer")]
    pub maintainer: Option<String>,
    #[serde(rename = "NumVotes")]
    pub num_votes: Option<i64>,
    #[serde(rename = "Popularity")]
    pub popularity: Option<f64>,
    #[serde(rename = "OutOfDate")]
    pub out_of_date: Option<i64>,
    #[serde(rename = "Depends")]
    pub depends: Option<Vec<String>>,
    #[serde(rename = "MakeDepends")]
    pub make_depends: Option<Vec<String>>,
    #[serde(rename = "License")]
    pub license: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    const SAMPLE_DESC: &str = "\
%FILENAME%
curl-8.7.1-3-x86_64.pkg.tar.zst

%NAME%
curl

%VERSION%
8.7.1-3

%DESC%
command line tool and library for transferring data with URLs

%URL%
https://curl.se

%LICENSE%
MIT

%ARCH%
x86_64

%BUILDDATE%
1714000000

%PACKAGER%
David Runge <dvzrv@archlinux.org>

%ISIZE%
1064960

%SHA256SUM%
abcdef1234567890
";

    const SAMPLE_DEPENDS: &str = "\
%DEPENDS%
brotli
ca-certificates
krb5
libnghttp2
libnghttp3
libpsl
libssh2>=1.0
openssl
zlib
zstd

%PROVIDES%
libcurl.so=4-64

%CONFLICTS%
curl-git

%OPTDEPENDS%
libnghttp3: HTTP/3 support
";

    #[test]
    fn test_parse_desc_file() {
        let sections = parse_desc_file(SAMPLE_DESC);

        assert_eq!(
            sections.get("%NAME%").and_then(|v| v.first()).map(|s| s.as_str()),
            Some("curl")
        );
        assert_eq!(
            sections.get("%VERSION%").and_then(|v| v.first()).map(|s| s.as_str()),
            Some("8.7.1-3")
        );
        assert_eq!(
            sections.get("%LICENSE%").and_then(|v| v.first()).map(|s| s.as_str()),
            Some("MIT")
        );
    }

    #[test]
    fn test_parse_depends_file() {
        let sections = parse_desc_file(SAMPLE_DEPENDS);

        let deps = sections.get("%DEPENDS%").unwrap();
        assert!(deps.contains(&"openssl".to_string()));
        assert!(deps.contains(&"libssh2>=1.0".to_string()));

        let provides = sections.get("%PROVIDES%").unwrap();
        assert!(provides.contains(&"libcurl.so=4-64".to_string()));

        let conflicts = sections.get("%CONFLICTS%").unwrap();
        assert!(conflicts.contains(&"curl-git".to_string()));
    }

    #[test]
    fn test_emit_arch_package_triples_dual_typing() {
        let collector = ArchCollector::new(
            "https://archive.archlinux.org/repos/last".into(),
            vec!["core".into()],
            false,
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut fields = parse_desc_file(SAMPLE_DESC);
        let dep_fields = parse_desc_file(SAMPLE_DEPENDS);
        for (k, v) in dep_fields {
            fields.insert(k, v);
        }

        let triples = collector
            .emit_arch_package_triples(&mut writer, &fields, "core")
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#BinaryPackage"));
        assert!(content.contains("arch#ArchPackage"));
        assert!(content.contains("\"curl\""));
        assert!(content.contains("\"8.7.1-3\""));
        assert!(content.contains("directlyDependsOn"));
        assert!(content.contains("maintainedBy"));
        assert!(content.contains("David Runge"));
        assert!(triples > 20);
    }

    #[test]
    fn test_aur_deserialization() {
        let json = r#"{
            "resultcount": 1,
            "results": [{
                "Name": "yay",
                "Version": "12.3.5-1",
                "Description": "Yet another yogurt. An AUR helper written in Go",
                "URL": "https://github.com/Jguer/yay",
                "Maintainer": "jguer",
                "NumVotes": 2500,
                "Popularity": 45.67,
                "OutOfDate": null,
                "Depends": ["pacman", "git"],
                "MakeDepends": ["go"],
                "License": ["GPL-3.0-or-later"]
            }]
        }"#;

        let rpc: AurRpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(rpc.results.len(), 1);
        assert_eq!(rpc.results[0].name, "yay");
        assert_eq!(rpc.results[0].num_votes, Some(2500));
        assert!(rpc.results[0].out_of_date.is_none());
    }

    #[test]
    fn test_emit_aur_package_triples() {
        let collector = ArchCollector::new(
            "https://archive.archlinux.org/repos/last".into(),
            vec![],
            true,
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg = AurPackage {
            name: "yay".into(),
            version: Some("12.3.5-1".into()),
            description: Some("AUR helper".into()),
            url: Some("https://github.com/Jguer/yay".into()),
            maintainer: Some("jguer".into()),
            num_votes: Some(2500),
            popularity: Some(45.67),
            out_of_date: None,
            depends: Some(vec!["pacman".into(), "git".into()]),
            make_depends: Some(vec!["go".into()]),
            license: Some(vec!["GPL-3.0-or-later".into()]),
        };

        let triples = collector.emit_aur_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // AUR packages get triple typing
        assert!(content.contains("core#BinaryPackage"));
        assert!(content.contains("arch#ArchPackage"));
        assert!(content.contains("arch#AUR"));
        assert!(content.contains("\"yay\""));
        assert!(content.contains("aurVotes"));
        assert!(content.contains("aurPopularity"));
        assert!(triples > 15);
    }
}
