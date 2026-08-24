use crate::npm::read_seed_file;
use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::sparql::{SparqlAuth, SparqlBackend};
use crate::uris::*;
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use std::collections::HashSet;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct GoModCollector {
    client: Client,
    proxy_url: String,
    /// Cache of verified module roots (paths that have versions on the proxy)
    known_modules: std::cell::RefCell<HashSet<String>>,
    /// Cache of paths known NOT to be modules
    known_non_modules: std::cell::RefCell<HashSet<String>>,
    pub graph_uri: Option<String>,
}

impl GoModCollector {
    pub fn new(proxy_url: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            proxy_url,
            known_modules: std::cell::RefCell::new(HashSet::new()),
            known_non_modules: std::cell::RefCell::new(HashSet::new()),
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    /// Resolve an import path to its module root by trying progressively shorter
    /// prefixes against the proxy. Caches results to avoid repeated lookups.
    ///
    /// For `cloud.google.com/go/storage/internal/apiv2`, tries:
    ///   1. cloud.google.com/go/storage/internal/apiv2 (miss)
    ///   2. cloud.google.com/go/storage/internal (miss)
    ///   3. cloud.google.com/go/storage (HIT → module root)
    ///
    /// Returns None if no module root found at any prefix.
    fn resolve_module_root(&self, import_path: &str) -> Option<String> {
        // Check if this exact path or a known parent is already cached
        {
            let known = self.known_modules.borrow();
            // Check if any known module is a prefix of this path
            for module in known.iter() {
                if import_path == module || import_path.starts_with(&format!("{}/", module)) {
                    return Some(module.clone());
                }
            }
        }

        // Check negative cache
        if self.known_non_modules.borrow().contains(import_path) {
            return None;
        }

        // Try progressively shorter prefixes
        let parts: Vec<&str> = import_path.split('/').collect();
        // Minimum module path: domain + at least one segment (e.g., "golang.org/x")
        let min_segments = if parts.first().map(|d| d.contains('.')) == Some(true) {
            2
        } else {
            2
        };

        // Start from the full path and work down
        for end in (min_segments..=parts.len()).rev() {
            let candidate = parts[..end].join("/");

            // Skip if we already know this isn't a module
            if self.known_non_modules.borrow().contains(&candidate) {
                continue;
            }

            // Check if proxy has versions for this candidate
            let encoded = encode_go_module_path(&candidate);
            let list_url = format!("{}/{}/@v/list", self.proxy_url, encoded);

            match self.fetch_text(&list_url) {
                Ok(text) if !text.trim().is_empty() => {
                    // Found a module root with versions
                    self.known_modules.borrow_mut().insert(candidate.clone());
                    return Some(candidate);
                }
                _ => {
                    self.known_non_modules.borrow_mut().insert(candidate);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }

        None
    }

    pub fn collect_discover(
        &self,
        endpoint: &str,
        auth: &SparqlAuth,
        backend: SparqlBackend,
        max_depth: u32,
        max_packages: usize,
        output_path: &str,
    ) -> Result<(usize, usize)> {
        let names = crate::seed::discover_by_ecosystem(endpoint, "gomod", auth, backend.clone())?;
        let seed_path = "/tmp/seed-gomod-discover.txt";
        std::fs::write(seed_path, names.join("\n"))?;
        self.collect(seed_path, max_depth, max_packages, output_path)
    }

    pub fn collect(
        &self,
        packages_file: &str,
        max_depth: u32,
        max_packages: usize,
        output_path: &str,
    ) -> Result<(usize, usize)> {
        use std::collections::{HashMap, HashSet, VecDeque};

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        let seeds = read_seed_file(packages_file)?;
        eprintln!("Loaded {} seed modules", seeds.len());
        eprintln!(
            "Spider config: max_depth={}, max_packages={}",
            max_depth, max_packages
        );

        // BFS state
        let mut queue: VecDeque<String> = seeds.into_iter().collect();
        let mut visited: HashSet<String> = HashSet::new();
        let mut depth_map: HashMap<String, u32> = HashMap::new();

        for path in queue.iter() {
            depth_map.insert(path.clone(), 0);
        }

        let mut total_packages = 0;
        let mut total_triples = 0;

        while let Some(raw_path) = queue.pop_front() {
            // Sanitize: strip commit annotations like ")(commit=...)"
            let sanitized = sanitize_module_path(&raw_path);
            if sanitized.is_empty() {
                continue;
            }

            // Resolve to module root (import paths → module paths)
            let module_path = match self.resolve_module_root(&sanitized) {
                Some(root) => root,
                None => {
                    // No module found at any prefix — skip entirely
                    continue;
                }
            };

            if !visited.insert(module_path.clone()) {
                continue;
            }

            if visited.len() > max_packages {
                eprintln!("Reached max_packages limit ({})", max_packages);
                break;
            }

            let depth = *depth_map.get(&module_path).unwrap_or(&0);

            if visited.len() % 100 == 0 {
                eprintln!("Progress: {} modules (depth {})", visited.len(), depth);
            }

            // collect_module fetches and emits, returns (triples, deps)
            match self.collect_module_with_deps(&mut writer, &module_path) {
                Ok((triples, dep_paths)) => {
                    total_triples += triples;
                    total_packages += 1;

                    // Enqueue requires (both direct and indirect)
                    if depth < max_depth {
                        for dep_path in dep_paths {
                            if !visited.contains(&dep_path) && !depth_map.contains_key(&dep_path) {
                                depth_map.insert(dep_path.clone(), depth + 1);
                                queue.push_back(dep_path);
                            }
                        }
                    }
                }
                Err(e) => eprintln!("  Error collecting {}: {}", module_path, e),
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        eprintln!(
            "Collected {} modules ({} total in graph)",
            total_packages,
            visited.len()
        );
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
        let (triples, _deps) = self.collect_module_with_deps(writer, module_path)?;
        Ok(triples)
    }

    fn collect_module_with_deps(
        &self,
        writer: &mut NTriplesWriter,
        module_path: &str,
    ) -> std::result::Result<(usize, Vec<String>), String> {
        let encoded = encode_go_module_path(module_path);

        // Fetch version list
        let list_url = format!("{}/{}/@v/list", self.proxy_url, encoded);
        let versions = self.fetch_text(&list_url)?;
        let version_list: Vec<&str> = versions.lines().filter(|l| !l.is_empty()).collect();

        if version_list.is_empty() {
            // Soft failure: proxy has no versions (untagged repo, cached 404, etc.)
            // Record as zero-version module rather than hard error
            eprintln!("  Skipping {} (no versions on proxy)", module_path);
            return Ok((0, vec![]));
        }

        // Use latest version
        let version = version_list.last().unwrap();

        // Fetch go.mod
        let mod_url = format!("{}/{}/@v/{}.mod", self.proxy_url, encoded, version);
        let go_mod_content = self.fetch_text(&mod_url).unwrap_or_default();

        // Parse go.mod for dependencies
        let go_mod = parse_go_mod(&go_mod_content);

        // Extract dep paths before emitting (for spidering)
        let dep_paths: Vec<String> = go_mod
            .requires
            .iter()
            .map(|r| r.module_path.clone())
            .collect();

        // Emit triples
        let triples = self
            .emit_module_triples(writer, module_path, version, &go_mod)
            .map_err(|e| e.to_string())?;

        Ok((triples, dep_paths))
    }

    fn fetch_text(&self, url: &str) -> std::result::Result<String, String> {
        let response = self.client.get(url).send().map_err(|e| e.to_string())?;

        if response.status() == StatusCode::NOT_FOUND || response.status() == StatusCode::GONE {
            // Return empty string for 404/410 — caller handles as "no data"
            return Ok(String::new());
        }

        if !response.status().is_success() {
            return Err(format!("HTTP {}: {}", response.status(), url));
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
            writer.write_bnode_subject(
                &bnode,
                &format!("{PKG}dependencyType"),
                &dep_type_uri("depends"),
            )?;
            triples += 4;

            if !dep.version.is_empty() {
                let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, dep.module_path));
                writer.write_bnode_to_bnode(&bnode, &format!("{PKG}hasVersionConstraint"), &cb)?;
                writer.write_bnode_subject(&cb, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
                writer.write_bnode_literal(
                    &cb,
                    &format!("{PKG}versionConstraintOperator"),
                    "exact",
                )?;
                writer.write_bnode_literal(
                    &cb,
                    &format!("{PKG}versionConstraintValue"),
                    &dep.version,
                )?;
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

/// Sanitize a module path from seed files or dependency lists.
///
/// 1. Strips commit annotations: `bazil.org/fuse)(commit=fb710f7...)` → `bazil.org/fuse`
/// 2. Strips parentheses and junk suffixes
/// 3. Rejects paths with invalid characters (spaces, brackets, etc.)
pub fn sanitize_module_path(raw: &str) -> String {
    let mut path = raw.trim().to_string();

    // Strip anything from first '(' or ')' onward (commit annotations, metadata)
    if let Some(idx) = path.find(|c: char| c == '(' || c == ')') {
        path.truncate(idx);
    }

    // Trim trailing slashes
    let path = path.trim_end_matches('/');

    // Reject obviously invalid paths
    if path.is_empty()
        || path.contains(' ')
        || path.contains('[')
        || path.contains(']')
        || !path.contains('.')
    {
        return String::new();
    }

    path.to_string()
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
    fn test_sanitize_module_path() {
        // Normal paths pass through
        assert_eq!(
            sanitize_module_path("github.com/go-chi/chi"),
            "github.com/go-chi/chi"
        );

        // Strip commit annotations
        assert_eq!(
            sanitize_module_path("bazil.org/fuse)(commit=fb710f7dfd05)"),
            "bazil.org/fuse"
        );
        assert_eq!(
            sanitize_module_path("bazil.org/fuse/fs)(commit=fb710f7dfd05)"),
            "bazil.org/fuse/fs"
        );

        // Reject invalid paths
        assert_eq!(sanitize_module_path(""), "");
        assert_eq!(sanitize_module_path("no-dots"), "");
        assert_eq!(sanitize_module_path("has spaces.com/foo"), "");

        // Trim trailing slashes
        assert_eq!(
            sanitize_module_path("github.com/foo/bar/"),
            "github.com/foo/bar"
        );
    }

    #[test]
    fn test_encode_go_module_path() {
        assert_eq!(
            encode_go_module_path("github.com/go-chi/chi"),
            "github.com/go-chi/chi"
        );
        assert_eq!(
            encode_go_module_path("github.com/Azure/go-autorest"),
            "github.com/!azure/go-autorest"
        );
        assert_eq!(
            encode_go_module_path("github.com/BurntSushi/toml"),
            "github.com/!burnt!sushi/toml"
        );
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
        assert_eq!(
            go_mod.requires[0].module_path,
            "github.com/stretchr/testify"
        );
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
            requires: vec![GoRequire {
                module_path: "github.com/stretchr/testify".into(),
                version: "v1.9.0".into(),
                indirect: false,
            }],
        };

        let triples = collector
            .emit_module_triples(&mut writer, "github.com/go-chi/chi/v5", "v5.0.12", &go_mod)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

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
