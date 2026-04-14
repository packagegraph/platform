use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::npm::read_seed_file;
use crate::uris::*;
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct GoModCollector {
    client: Client,
    proxy_url: String,
}

impl GoModCollector {
    pub fn new(proxy_url: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, proxy_url }
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let module_paths = read_seed_file(packages_file)?;
        eprintln!("Loaded {} module paths from seed file", module_paths.len());

        let mut total_packages = 0;
        let mut total_triples = 0;

        for (idx, module_path) in module_paths.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, module_paths.len());
            }

            match self.collect_module(&mut writer, module_path) {
                Ok(triples) => {
                    total_triples += triples;
                    total_packages += 1;
                }
                Err(e) => eprintln!("  Error collecting {}: {}", module_path, e),
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("go");
        let rel_uri = release_uri("go", "modules");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Go Modules")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "modules")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn collect_module(
        &self,
        writer: &mut NTriplesWriter,
        module_path: &str,
    ) -> std::result::Result<usize, String> {
        let encoded = encode_go_module_path(module_path);

        // Fetch version list
        let list_url = format!("{}/{}/@v/list", self.proxy_url, encoded);
        let versions = self.fetch_text(&list_url)?;
        let version_list: Vec<&str> = versions.lines().filter(|l| !l.is_empty()).collect();

        if version_list.is_empty() {
            return Err(format!("No versions found for {}", module_path));
        }

        // Use latest version
        let version = version_list.last().unwrap();

        // Fetch go.mod
        let mod_url = format!("{}/{}/@v/{}.mod", self.proxy_url, encoded, version);
        let go_mod_content = self.fetch_text(&mod_url).unwrap_or_default();

        // Parse go.mod for dependencies
        let go_mod = parse_go_mod(&go_mod_content);

        // Emit triples
        self.emit_module_triples(writer, module_path, version, &go_mod)
            .map_err(|e| e.to_string())
    }

    fn fetch_text(&self, url: &str) -> std::result::Result<String, String> {
        let response = self.client.get(url).send().map_err(|e| e.to_string())?;

        if response.status() == StatusCode::NOT_FOUND || response.status() == StatusCode::GONE {
            return Err(format!("404/410: {}", url));
        }

        response.text().map_err(|e| e.to_string())
    }

    fn emit_module_triples(
        &self,
        writer: &mut NTriplesWriter,
        module_path: &str,
        version: &str,
        go_mod: &GoMod,
    ) -> Result<usize> {
        let pkg_uri = package_uri("go", "modules", "any", module_path, version);
        let identity_uri = package_identity_uri("go", "modules", "any", module_path);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{GOMOD}GoModule"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), module_path)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), module_path)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("go", "modules", module_path, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("go");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Go-specific properties
        writer.write_literal(&pkg_uri, &format!("{GOMOD}modulePath"), module_path)?;
        triples += 1;

        if let Some(go_version) = &go_mod.go_version {
            writer.write_literal(&pkg_uri, &format!("{GOMOD}goVersion"), go_version)?;
            triples += 1;
        }

        // Dependencies from go.mod require block
        for dep in &go_mod.requires {
            let target_uri = package_identity_uri("go", "modules", "any", &dep.module_path);

            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id("depends", &format!("{}-{}", pkg_uri, dep.module_path));
            writer.write_bnode_object(&pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_literal(&bnode, &format!("{PKG}dependencyType"), "depends")?;
            triples += 4;

            if !dep.version.is_empty() {
                let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, dep.module_path));
                writer.write_bnode_object(&bnode, &format!("{PKG}hasVersionConstraint"), &cb)?;
                writer.write_bnode_subject(&cb, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
                writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintOperator"), "exact")?;
                writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintValue"), &dep.version)?;
                triples += 4;
            }

            if dep.indirect {
                writer.write_bnode_literal(&bnode, &format!("{GOMOD}isIndirect"), "true")?;
                triples += 1;
            }
        }

        Ok(triples)
    }
}

/// Encode a Go module path for the module proxy.
/// Uppercase letters are replaced with ! + lowercase.
pub fn encode_go_module_path(module_path: &str) -> String {
    let mut encoded = String::with_capacity(module_path.len());
    for ch in module_path.chars() {
        if ch.is_uppercase() {
            encoded.push('!');
            encoded.push(ch.to_lowercase().next().unwrap());
        } else {
            encoded.push(ch);
        }
    }
    encoded
}

/// Parsed go.mod content.
pub struct GoMod {
    pub go_version: Option<String>,
    pub requires: Vec<GoRequire>,
}

pub struct GoRequire {
    pub module_path: String,
    pub version: String,
    pub indirect: bool,
}

/// Parse go.mod content.
pub fn parse_go_mod(content: &str) -> GoMod {
    let mut go_version = None;
    let mut requires = Vec::new();
    let mut in_require_block = false;

    let go_re = Regex::new(r"^go\s+(\S+)").unwrap();
    let req_re = Regex::new(r"^\s+(\S+)\s+(\S+)(.*)$").unwrap();

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(caps) = go_re.captures(trimmed) {
            go_version = Some(caps.get(1).unwrap().as_str().to_string());
            continue;
        }

        if trimmed == "require (" {
            in_require_block = true;
            continue;
        }

        if trimmed == ")" && in_require_block {
            in_require_block = false;
            continue;
        }

        if in_require_block {
            if let Some(caps) = req_re.captures(line) {
                let module_path = caps.get(1).unwrap().as_str().to_string();
                let version = caps.get(2).unwrap().as_str().to_string();
                let rest = caps.get(3).map(|m| m.as_str()).unwrap_or("");
                let indirect = rest.contains("// indirect");

                requires.push(GoRequire {
                    module_path,
                    version,
                    indirect,
                });
            }
        }
    }

    GoMod {
        go_version,
        requires,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_encode_go_module_path() {
        assert_eq!(encode_go_module_path("github.com/go-chi/chi"), "github.com/go-chi/chi");
        assert_eq!(encode_go_module_path("github.com/Azure/go-autorest"), "github.com/!azure/go-autorest");
        assert_eq!(encode_go_module_path("github.com/BurntSushi/toml"), "github.com/!burnt!sushi/toml");
    }

    #[test]
    fn test_parse_go_mod() {
        let content = r#"module github.com/go-chi/chi/v5

go 1.22

require (
	github.com/stretchr/testify v1.9.0
	golang.org/x/net v0.24.0 // indirect
)
"#;

        let go_mod = parse_go_mod(content);
        assert_eq!(go_mod.go_version.as_deref(), Some("1.22"));
        assert_eq!(go_mod.requires.len(), 2);
        assert_eq!(go_mod.requires[0].module_path, "github.com/stretchr/testify");
        assert_eq!(go_mod.requires[0].version, "v1.9.0");
        assert!(!go_mod.requires[0].indirect);
        assert_eq!(go_mod.requires[1].module_path, "golang.org/x/net");
        assert!(go_mod.requires[1].indirect);
    }

    #[test]
    fn test_emit_module_triples_dual_typing() {
        let collector = GoModCollector::new("https://proxy.golang.org".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let go_mod = GoMod {
            go_version: Some("1.22".into()),
            requires: vec![
                GoRequire {
                    module_path: "github.com/stretchr/testify".into(),
                    version: "v1.9.0".into(),
                    indirect: false,
                },
            ],
        };

        let triples = collector
            .emit_module_triples(&mut writer, "github.com/go-chi/chi/v5", "v5.0.12", &go_mod)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("gomod#GoModule"));
        assert!(content.contains("go-chi/chi/v5"));
        assert!(content.contains("gomod#modulePath"));
        assert!(content.contains("gomod#goVersion"));
        assert!(content.contains("\"1.22\""));
        assert!(content.contains("directlyDependsOn"));
        assert!(triples > 15);
    }
}
