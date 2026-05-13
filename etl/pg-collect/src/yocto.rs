use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::uris::*;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Result};
use std::path::Path;
use walkdir::WalkDir;

// Regex for parsing BitBake variable assignments
static VAR_ASSIGN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^\s*([A-Z_]+)\s*(=|\?=|\+=|=\+|\.=|=\.)\s*"([^"]*)""#).unwrap()
});

#[derive(Debug, Clone)]
struct YoctoRecipe {
    name: String,
    version: Option<String>,
    layer: String,
    summary: Option<String>,
    description: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
    src_uri: Vec<String>,
    section: Option<String>,
    depends: Vec<String>,
    rdepends: Vec<String>,
    rrecommends: Vec<String>,
    inherits: Vec<String>,
}

pub struct YoctoCollector {
    distro_name: String,
    release_name: String,
    layers: Vec<String>,
}

impl YoctoCollector {
    pub fn new(distro_name: String, release_name: String, layers: Vec<String>) -> Self {
        Self { distro_name, release_name, layers }
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);
        self.emit_distribution_metadata(&mut writer)?;

        let mut total_packages = 0;
        let mut total_triples = 0;

        // First pass: collect all .bb recipes
        let mut recipes: HashMap<String, YoctoRecipe> = HashMap::new();

        for layer_path in &self.layers {
            let layer_name = Path::new(layer_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            for entry in WalkDir::new(layer_path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path().extension().and_then(|s| s.to_str()) == Some("bb")
                })
            {
                match self.parse_recipe(entry.path(), layer_name) {
                    Ok(recipe) => {
                        let key = format!("{}_{}", recipe.name, recipe.version.as_ref().unwrap_or(&"0".to_string()));
                        recipes.insert(key, recipe);
                    }
                    Err(e) => {
                        eprintln!("  Error parsing {:?}: {}", entry.path(), e);
                    }
                }
            }
        }

        // Second pass: collect and merge .bbappend files
        for layer_path in &self.layers {
            for entry in WalkDir::new(layer_path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path().extension().and_then(|s| s.to_str()) == Some("bbappend")
                })
            {
                if let Err(e) = self.apply_bbappend(entry.path(), &mut recipes) {
                    eprintln!("  Error applying {:?}: {}", entry.path(), e);
                }
            }
        }

        // Emit triples for all collected recipes
        for recipe in recipes.values() {
            total_triples += self.emit_package_triples(&mut writer, recipe)?;
            total_packages += 1;
            if total_packages % 1000 == 0 {
                eprintln!("Progress: {} packages", total_packages);
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
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Yocto Project")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "yocto")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn parse_recipe(&self, recipe_path: &Path, layer: &str) -> std::result::Result<YoctoRecipe, String> {
        let filename = recipe_path.file_stem().and_then(|s| s.to_str()).ok_or("Invalid filename")?;

        // Extract name and version from filename: packagename_1.2.3.bb
        let (name, version) = if let Some(idx) = filename.rfind('_') {
            (filename[..idx].to_string(), Some(filename[idx + 1..].to_string()))
        } else {
            (filename.to_string(), None)
        };

        let file = File::open(recipe_path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);

        let mut variables: HashMap<String, String> = HashMap::new();

        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;

            if let Some(caps) = VAR_ASSIGN.captures(&line) {
                let var_name = caps.get(1).unwrap().as_str();
                let operator = caps.get(2).unwrap().as_str();
                let value = caps.get(3).unwrap().as_str();

                self.apply_variable_operation(&mut variables, var_name, operator, value);
            }
        }

        Ok(YoctoRecipe {
            name,
            version,
            layer: layer.to_string(),
            summary: variables.get("SUMMARY").cloned(),
            description: variables.get("DESCRIPTION").cloned(),
            license: variables.get("LICENSE").cloned(),
            homepage: variables.get("HOMEPAGE").cloned(),
            src_uri: variables.get("SRC_URI").map(|s| vec![s.clone()]).unwrap_or_default(),
            section: variables.get("SECTION").cloned(),
            depends: variables.get("DEPENDS").map(|s| s.split_whitespace().map(|x| x.to_string()).collect()).unwrap_or_default(),
            rdepends: variables.get("RDEPENDS").map(|s| s.split_whitespace().map(|x| x.to_string()).collect()).unwrap_or_default(),
            rrecommends: variables.get("RRECOMMENDS").map(|s| s.split_whitespace().map(|x| x.to_string()).collect()).unwrap_or_default(),
            inherits: variables.get("inherit").map(|s| s.split_whitespace().map(|x| x.to_string()).collect()).unwrap_or_default(),
        })
    }

    fn apply_variable_operation(&self, variables: &mut HashMap<String, String>, var_name: &str, operator: &str, value: &str) {
        match operator {
            "=" => {
                // Set/override
                variables.insert(var_name.to_string(), value.to_string());
            }
            "?=" => {
                // Set if unset
                variables.entry(var_name.to_string()).or_insert_with(|| value.to_string());
            }
            "+=" => {
                // Append with space
                let entry = variables.entry(var_name.to_string()).or_insert_with(String::new);
                if !entry.is_empty() {
                    entry.push(' ');
                }
                entry.push_str(value);
            }
            "=+" => {
                // Prepend with space
                let entry = variables.entry(var_name.to_string()).or_insert_with(String::new);
                let new_value = if entry.is_empty() {
                    value.to_string()
                } else {
                    format!("{} {}", value, entry)
                };
                *entry = new_value;
            }
            ".=" => {
                // Concatenate append (no space)
                variables.entry(var_name.to_string()).or_insert_with(String::new).push_str(value);
            }
            "=." => {
                // Concatenate prepend (no space)
                let entry = variables.entry(var_name.to_string()).or_insert_with(String::new);
                let new_value = format!("{}{}", value, entry);
                *entry = new_value;
            }
            _ => {}
        }
    }

    fn apply_bbappend(&self, append_path: &Path, recipes: &mut HashMap<String, YoctoRecipe>) -> std::result::Result<(), String> {
        let filename = append_path.file_stem().and_then(|s| s.to_str()).ok_or("Invalid filename")?;

        // Extract name and version from .bbappend filename: packagename_1.2.3.bbappend
        let (name, version_pattern) = if let Some(idx) = filename.rfind('_') {
            (filename[..idx].to_string(), filename[idx + 1..].to_string())
        } else {
            return Err("Invalid bbappend filename".to_string());
        };

        // Find matching recipe
        let recipe_key = format!("{}_{}", name, version_pattern);

        if let Some(recipe) = recipes.get_mut(&recipe_key) {
            // Parse .bbappend and merge variables
            let file = File::open(append_path).map_err(|e| e.to_string())?;
            let reader = BufReader::new(file);

            let mut variables: HashMap<String, String> = HashMap::new();

            for line in reader.lines() {
                let line = line.map_err(|e| e.to_string())?;

                if let Some(caps) = VAR_ASSIGN.captures(&line) {
                    let var_name = caps.get(1).unwrap().as_str();
                    let operator = caps.get(2).unwrap().as_str();
                    let value = caps.get(3).unwrap().as_str();

                    self.apply_variable_operation(&mut variables, var_name, operator, value);
                }
            }

            // Merge into recipe
            if let Some(summary) = variables.get("SUMMARY") {
                recipe.summary = Some(summary.clone());
            }
            if let Some(description) = variables.get("DESCRIPTION") {
                recipe.description = Some(description.clone());
            }
            if let Some(depends) = variables.get("DEPENDS") {
                recipe.depends.extend(depends.split_whitespace().map(|s| s.to_string()));
            }
            if let Some(src_uri) = variables.get("SRC_URI") {
                recipe.src_uri.push(src_uri.clone());
            }
        }

        Ok(())
    }

    fn emit_package_triples(&self, writer: &mut NTriplesWriter, recipe: &YoctoRecipe) -> Result<usize> {
        let default_version = "0".to_string();
        let version = recipe.version.as_ref().unwrap_or(&default_version);
        let pkg_uri = package_uri(&self.distro_name, &self.release_name, "any", &recipe.name, version);
        let identity_uri = package_identity_uri(&self.distro_name, &self.release_name, "any", &recipe.name);
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.release_name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}SourcePackage"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{YOCTO}BitBakeRecipe"))?;
        triples += 2;

        // Package name
        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &recipe.name)?;
        triples += 1;

        // Link to canonical identity (isVersionOf, not hasVersion)
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &recipe.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        // Version resource (separate node with versionString)
        let ver_uri = version_uri(&self.distro_name, &self.release_name, &recipe.name, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution and release links
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfRelease"), &rel_uri)?;
        triples += 2;

        // Layer (both string literal and object link)
        writer.write_literal(&pkg_uri, &format!("{YOCTO}layer"), &recipe.layer)?;
        triples += 1;

        // Description (use SUMMARY as description if no DESCRIPTION present)
        if let Some(ref description) = recipe.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), description)?;
            triples += 1;
        } else if let Some(ref summary) = recipe.summary {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), summary)?;
            triples += 1;
        }

        // License (licenseName, not license)
        if let Some(ref license) = recipe.license {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
            // License entity (SPDX)
            let license_uri = crate::uris::spdx_license_uri(license);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
            writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
            triples += 2;
        }

        if let Some(ref homepage) = recipe.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
        }

        if let Some(ref section) = recipe.section {
            writer.write_literal(&pkg_uri, &format!("{YOCTO}section"), section)?;
            triples += 1;
        }

        // SRC_URI emission + upstream repo extraction
        let mut repo_emitted = false;
        for uri in &recipe.src_uri {
            writer.write_literal(&pkg_uri, &format!("{YOCTO}srcUri"), uri)?;
            triples += 1;

            // Extract forge URL from SRC_URI (handles git://, archive URLs, Yocto params)
            if !repo_emitted {
                if let Some(extraction) = crate::forge::extract_forge_url(uri) {
                    triples += crate::forge::emit_upstream_repo(writer, &identity_uri, &extraction, None)?;
                    repo_emitted = true;
                }
            }
        }

        // Inherits emission (bbclass names)
        for class in &recipe.inherits {
            writer.write_literal(&pkg_uri, &format!("{YOCTO}inherits"), class)?;
            triples += 1;
        }

        // Dependencies with types
        for dep in &recipe.depends {
            let dep_type = if dep.ends_with("-native") {
                "native"
            } else {
                "build"
            };

            let target_uri = package_identity_uri(&self.distro_name, &self.release_name, "any", dep);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id("dep", &format!("{}-{}", pkg_uri, dep));
            writer.write_bnode_object(&pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyType"), &dep_type_uri(dep_type))?;
            triples += 4;
        }

        for dep in &recipe.rdepends {
            let target_uri = package_identity_uri(&self.distro_name, &self.release_name, "any", dep);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id("dep", &format!("{}-{}", pkg_uri, dep));
            writer.write_bnode_object(&pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyType"), &dep_type_uri("runtime"))?;
            triples += 4;
        }

        for dep in &recipe.rrecommends {
            let target_uri = package_identity_uri(&self.distro_name, &self.release_name, "any", dep);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            let bnode = bnode_id("dep", &format!("{}-{}", pkg_uri, dep));
            writer.write_bnode_object(&pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyType"), &dep_type_uri("recommended"))?;
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

    fn create_test_recipe(dir: &Path, layer: &str, name: &str, version: &str, content: &str) -> std::path::PathBuf {
        let layer_path = dir.join(layer);
        let recipe_dir = layer_path.join("recipes-test").join(name);
        fs::create_dir_all(&recipe_dir).unwrap();

        let recipe_file = recipe_dir.join(format!("{}_{}.bb", name, version));
        let mut file = File::create(&recipe_file).unwrap();
        file.write_all(content.as_bytes()).unwrap();

        recipe_file
    }

    #[test]
    fn test_parse_simple_recipe() {
        let temp_dir = TempDir::new().unwrap();
        let content = r#"
SUMMARY = "Test package"
DESCRIPTION = "A test package for unit tests"
LICENSE = "MIT"
HOMEPAGE = "https://example.com"
SRC_URI = "https://example.com/test-1.0.tar.gz"
DEPENDS = "libfoo libbar"
SECTION = "devel"
"#;

        create_test_recipe(temp_dir.path(), "meta-test", "testpkg", "1.0", content);

        let collector = YoctoCollector::new("yocto".into(), "yocto".into(), vec![
            temp_dir.path().join("meta-test").to_str().unwrap().to_string()
        ]);

        let temp_file = NamedTempFile::new().unwrap();
        let output_path = temp_file.path().to_str().unwrap();

        let (packages, triples) = collector.collect(output_path).unwrap();

        assert_eq!(packages, 1, "Should collect 1 package");
        assert!(triples > 0, "Should emit triples");

        // Read output and verify
        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Check for dual typing
        assert!(content.contains("core#SourcePackage"), "Should have SourcePackage type");
        assert!(content.contains("bitbake#BitBakeRecipe"), "Should have BitBakeRecipe type");

        // Check for metadata — DESCRIPTION takes precedence over SUMMARY
        assert!(content.contains("\"A test package for unit tests\""), "Should have DESCRIPTION");
        assert!(content.contains("licenseName"), "Should use licenseName property");
        assert!(content.contains("\"MIT\""), "Should have LICENSE");
        assert!(content.contains("\"https://example.com\""), "Should have HOMEPAGE");
        assert!(content.contains("\"meta-test\""), "Should have layer name");

        // Check correct ontology alignment
        assert!(content.contains("isVersionOf"), "Should use isVersionOf for identity link");
        assert!(content.contains("core#Version"), "Should create Version node");
        assert!(content.contains("versionString"), "Should use versionString on Version node");
        assert!(content.contains("partOfDistribution"), "Should link to distribution");
        assert!(content.contains("partOfRelease"), "Should link to release");
        assert!(content.contains("bitbake#srcUri"), "Should emit SRC_URI");
        assert!(!content.contains("packageVersion"), "Should NOT use packageVersion");
    }

    #[test]
    fn test_bbappend_merging() {
        let temp_dir = TempDir::new().unwrap();

        // Base recipe
        let base_content = r#"
SUMMARY = "Base package"
DEPENDS = "libfoo"
SRC_URI = "https://example.com/pkg.tar.gz"
"#;
        create_test_recipe(temp_dir.path(), "meta-base", "pkg", "1.0", base_content);

        // Create .bbappend in another layer
        let layer2_path = temp_dir.path().join("meta-extra");
        let append_dir = layer2_path.join("recipes-test").join("pkg");
        fs::create_dir_all(&append_dir).unwrap();

        let append_content = r#"
SUMMARY = "Extended package"
DEPENDS += "libbar"
SRC_URI += "file://extra.patch"
"#;
        let append_file = append_dir.join("pkg_1.0.bbappend");
        let mut file = File::create(&append_file).unwrap();
        file.write_all(append_content.as_bytes()).unwrap();

        let collector = YoctoCollector::new("yocto".into(), "yocto".into(), vec![
            temp_dir.path().join("meta-base").to_str().unwrap().to_string(),
            temp_dir.path().join("meta-extra").to_str().unwrap().to_string(),
        ]);

        let temp_file = NamedTempFile::new().unwrap();
        let output_path = temp_file.path().to_str().unwrap();

        let (packages, _) = collector.collect(output_path).unwrap();
        assert_eq!(packages, 1, "Should collect 1 package after merging");

        // Read output and verify merged values
        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // SUMMARY should be overridden (= operator)
        assert!(content.contains("\"Extended package\""), "SUMMARY should be overridden");
        assert!(!content.contains("\"Base package\""), "Old SUMMARY should be replaced");
    }

    #[test]
    fn test_dependency_types() {
        let temp_dir = TempDir::new().unwrap();
        let content = r#"
SUMMARY = "Test deps"
LICENSE = "MIT"
DEPENDS = "libfoo libbar-native"
RDEPENDS = "runtime-lib"
RRECOMMENDS = "optional-tool"
"#;

        create_test_recipe(temp_dir.path(), "meta-test", "testdeps", "1.0", content);

        let collector = YoctoCollector::new("yocto".into(), "yocto".into(), vec![
            temp_dir.path().join("meta-test").to_str().unwrap().to_string()
        ]);

        let temp_file = NamedTempFile::new().unwrap();
        let output_path = temp_file.path().to_str().unwrap();

        collector.collect(output_path).unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Check for dependency type property URIs (v0.6.0)
        assert!(content.contains("core#buildDependsOn"), "Should have buildDependsOn dependency type");
        assert!(content.contains("core#dependsOn"), "Should have dependsOn dependency type for runtime");
        assert!(content.contains("core#recommends"), "Should have recommends dependency type");
        assert!(content.contains("core#buildDependsOn"), "Should have buildDependsOn for native deps");
    }

    #[test]
    fn test_version_from_filename() {
        let temp_dir = TempDir::new().unwrap();
        let content = r#"
SUMMARY = "Version test"
LICENSE = "MIT"
"#;

        create_test_recipe(temp_dir.path(), "meta-test", "versiontest", "2.3.4", content);

        let collector = YoctoCollector::new("yocto".into(), "yocto".into(), vec![
            temp_dir.path().join("meta-test").to_str().unwrap().to_string()
        ]);

        let temp_file = NamedTempFile::new().unwrap();
        let output_path = temp_file.path().to_str().unwrap();

        collector.collect(output_path).unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should extract version from filename
        assert!(content.contains("\"2.3.4\""), "Should extract version from filename");
    }
}
