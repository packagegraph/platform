use crate::forge::emit_dq_issue;
use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct HomebrewCollector {
    client: Client,
    api_base: String,
    distro_name: String,
    release_name: String,
    source_cache: Option<SourceCache>,
    pub graph_uri: Option<String>,
}

/// Minimal serde model for Homebrew formula JSON.
#[derive(Debug, Deserialize)]
pub struct Formula {
    pub name: String,
    pub full_name: Option<String>,
    pub desc: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    pub versions: Option<FormulaVersions>,
    pub dependencies: Option<Vec<String>>,
    pub build_dependencies: Option<Vec<String>>,
    pub optional_dependencies: Option<Vec<String>>,
    pub deprecated: Option<bool>,
    pub disabled: Option<bool>,
    pub deprecation_reason: Option<String>,
    pub disable_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FormulaVersions {
    pub stable: Option<String>,
    pub head: Option<String>,
}

/// Minimal serde model for Homebrew cask JSON.
#[derive(Debug, Deserialize)]
pub struct Cask {
    pub token: String,
    pub name: Option<Vec<String>>,
    pub desc: Option<String>,
    pub homepage: Option<String>,
    pub version: Option<String>,
    pub url: Option<String>,
    pub sha256: Option<String>,
    pub deprecated: Option<bool>,
    pub disabled: Option<bool>,
}

impl HomebrewCollector {
    pub fn new(api_base: String, distro_name: String, release_name: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            api_base,
            distro_name,
            release_name,
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
        self.source_cache = Some(SourceCache::new(cache_dir, "homebrew")?);
        Ok(self)
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        // Emit distribution metadata
        self.emit_distribution_metadata(&mut writer)?;

        let mut total_packages = 0;
        let mut total_triples = 0;

        // Fetch and process formulae
        eprintln!("Fetching formulae from {}...", self.api_base);
        match self.fetch_formulae() {
            Ok(formulae) => {
                eprintln!("Parsed {} formulae", formulae.len());
                for formula in &formulae {
                    total_triples += self.emit_formula_triples(&mut writer, formula)?;
                }
                total_packages += formulae.len();
            }
            Err(e) => {
                eprintln!("Error fetching formulae: {}", e);
                total_triples += emit_dq_issue(
                    &mut writer,
                    "homebrew-collector",
                    "formula_api",
                    &e.to_string(),
                    "fetch_error",
                    "high",
                )?;
            }
        }

        // Fetch and process casks
        eprintln!("Fetching casks...");
        match self.fetch_casks() {
            Ok(casks) => {
                eprintln!("Parsed {} casks", casks.len());
                for cask in &casks {
                    total_triples += self.emit_cask_triples(&mut writer, cask)?;
                }
                total_packages += casks.len();
            }
            Err(e) => {
                eprintln!("Error fetching casks: {}", e);
                total_triples += emit_dq_issue(
                    &mut writer,
                    "homebrew-collector",
                    "cask_api",
                    &e.to_string(),
                    "fetch_error",
                    "high",
                )?;
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
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "macOS")?;
        writer.write_literal(&dist_uri, RDFS_LABEL, "Homebrew")?;
        triples += 3;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "homebrew")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_formulae(&self) -> std::result::Result<Vec<Formula>, String> {
        let url = format!("{}/formula.json", self.api_base);
        let response = self.client.get(&url).send().map_err(|e| e.to_string())?;
        let text = response.text().map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn fetch_casks(&self) -> std::result::Result<Vec<Cask>, String> {
        let url = format!("{}/cask.json", self.api_base);
        let response = self.client.get(&url).send().map_err(|e| e.to_string())?;
        let text = response.text().map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn emit_formula_triples(
        &self,
        writer: &mut NTriplesWriter,
        formula: &Formula,
    ) -> Result<usize> {
        let version = formula
            .versions
            .as_ref()
            .and_then(|v| v.stable.as_deref())
            .unwrap_or("unknown");

        let pkg_uri = package_uri(
            &self.distro_name,
            &self.release_name,
            "any",
            &formula.name,
            version,
        );
        let identity_uri =
            package_identity_uri(&self.distro_name, &self.release_name, "any", &formula.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{BREW}Formula"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &formula.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        // Core properties
        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &formula.name)?;
        triples += 1;

        if let Some(full_name) = &formula.full_name {
            writer.write_literal(&pkg_uri, &format!("{BREW}formulaName"), full_name)?;
            triples += 1;
        }

        // Version
        let ver_uri = version_uri(
            &self.distro_name,
            &self.release_name,
            &formula.name,
            version,
        );
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.release_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
        triples += 2;

        // Optional properties
        if let Some(desc) = &formula.desc {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(homepage) = &formula.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(license) = &formula.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }

        // Deprecated/disabled status
        if formula.deprecated == Some(true) {
            writer.write_boolean(&pkg_uri, &format!("{BREW}deprecated"), true)?;
            triples += 1;
            if let Some(reason) = &formula.deprecation_reason {
                writer.write_literal(&pkg_uri, &format!("{BREW}deprecationReason"), reason)?;
                triples += 1;
            }
        }
        if formula.disabled == Some(true) {
            writer.write_boolean(&pkg_uri, &format!("{BREW}disabled"), true)?;
            triples += 1;
        }

        // Dependencies
        if let Some(deps) = &formula.dependencies {
            triples += self.emit_brew_deps(writer, &pkg_uri, deps, "depends")?;
        }
        if let Some(deps) = &formula.build_dependencies {
            triples += self.emit_brew_deps(writer, &pkg_uri, deps, "build_depends")?;
        }
        if let Some(deps) = &formula.optional_dependencies {
            triples += self.emit_brew_deps(writer, &pkg_uri, deps, "optional_depends")?;
        }

        Ok(triples)
    }

    fn emit_cask_triples(&self, writer: &mut NTriplesWriter, cask: &Cask) -> Result<usize> {
        let version = cask.version.as_deref().unwrap_or("latest");
        let pkg_uri = package_uri(
            &self.distro_name,
            &self.release_name,
            "any",
            &cask.token,
            version,
        );
        let identity_uri =
            package_identity_uri(&self.distro_name, &self.release_name, "any", &cask.token);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{BREW}Cask"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &cask.token)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &cask.token)?;
        writer.write_literal(&pkg_uri, &format!("{BREW}token"), &cask.token)?;
        triples += 2;

        // Version
        let ver_uri = version_uri(&self.distro_name, &self.release_name, &cask.token, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        if let Some(desc) = &cask.desc {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(homepage) = &cask.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }
        if let Some(sha) = &cask.sha256 {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), sha)?;
            triples += 1;
        }

        Ok(triples)
    }

    fn emit_brew_deps(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        deps: &[String],
        dep_type: &str,
    ) -> Result<usize> {
        let mut triples = 0;
        for dep_name in deps {
            let target_uri =
                package_identity_uri(&self.distro_name, &self.release_name, "any", dep_name);

            writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id(dep_type, &format!("{}-{}", pkg_uri, dep_name));
            writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_subject(
                &bnode,
                &format!("{PKG}dependencyType"),
                &dep_type_uri(dep_type),
            )?;
            triples += 4;
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
    fn test_formula_deserialization() {
        let json = r#"[{
            "name": "curl",
            "full_name": "curl",
            "desc": "Get a file from an HTTP, HTTPS or FTP server",
            "license": "curl",
            "homepage": "https://curl.se",
            "versions": {"stable": "8.7.1", "head": null},
            "dependencies": ["brotli", "libidn2", "libnghttp2"],
            "build_dependencies": ["pkg-config"],
            "optional_dependencies": [],
            "deprecated": false,
            "disabled": false
        }]"#;

        let formulae: Vec<Formula> = serde_json::from_str(json).unwrap();
        assert_eq!(formulae.len(), 1);
        assert_eq!(formulae[0].name, "curl");
        assert_eq!(
            formulae[0].versions.as_ref().unwrap().stable.as_deref(),
            Some("8.7.1")
        );
        assert_eq!(formulae[0].dependencies.as_ref().unwrap().len(), 3);
    }

    #[test]
    fn test_cask_deserialization() {
        let json = r#"[{
            "token": "firefox",
            "name": ["Mozilla Firefox"],
            "desc": "Web browser",
            "homepage": "https://www.mozilla.org/firefox/",
            "version": "125.0",
            "url": "https://download-installer.cdn.mozilla.net/pub/firefox/releases/125.0/mac/en-US/Firefox%20125.0.dmg",
            "sha256": "abc123",
            "deprecated": false,
            "disabled": false
        }]"#;

        let casks: Vec<Cask> = serde_json::from_str(json).unwrap();
        assert_eq!(casks.len(), 1);
        assert_eq!(casks[0].token, "firefox");
        assert_eq!(casks[0].version.as_deref(), Some("125.0"));
    }

    #[test]
    fn test_emit_formula_triples_produces_dual_typing() {
        let collector = HomebrewCollector::new(
            "https://formulae.brew.sh/api".into(),
            "homebrew".into(),
            "homebrew".into(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let formula = Formula {
            name: "curl".into(),
            full_name: Some("curl".into()),
            desc: Some("URL retrieval utility".into()),
            license: Some("curl".into()),
            homepage: Some("https://curl.se".into()),
            versions: Some(FormulaVersions {
                stable: Some("8.7.1".into()),
                head: None,
            }),
            dependencies: Some(vec!["brotli".into(), "libidn2".into()]),
            build_dependencies: None,
            optional_dependencies: None,
            deprecated: Some(false),
            disabled: Some(false),
            deprecation_reason: None,
            disable_reason: None,
        };

        let triples = collector
            .emit_formula_triples(&mut writer, &formula)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // Verify dual typing
        assert!(content.contains("core#Package"));
        assert!(content.contains("homebrew#Formula"));
        assert!(content.contains("\"curl\""));
        assert!(content.contains("\"8.7.1\""));
        assert!(content.contains("directlyDependsOn"));
        assert!(triples > 15);
    }

    #[test]
    fn test_emit_cask_triples() {
        let collector = HomebrewCollector::new(
            "https://formulae.brew.sh/api".into(),
            "homebrew".into(),
            "homebrew".into(),
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let cask = Cask {
            token: "firefox".into(),
            name: Some(vec!["Mozilla Firefox".into()]),
            desc: Some("Web browser".into()),
            homepage: Some("https://www.mozilla.org/firefox/".into()),
            version: Some("125.0".into()),
            url: None,
            sha256: Some("abc123".into()),
            deprecated: None,
            disabled: None,
        };

        let triples = collector.emit_cask_triples(&mut writer, &cask).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("homebrew#Cask"));
        assert!(content.contains("\"firefox\""));
        assert!(content.contains("\"125.0\""));
        assert!(triples > 10);
    }
}
