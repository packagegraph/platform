use crate::fetch_error::FetchError;
use crate::http_transport::{HttpTransport, StatsSnapshot};
use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::sparql::{SparqlAuth, SparqlBackend};
use crate::uris::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use crate::emit::rdf::write_package_identity;

pub struct NpmCollector {
    transport: HttpTransport,
    registry_url: String,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NpmPackageDoc {
    name: String,
    description: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
    #[serde(rename = "dist-tags")]
    dist_tags: Option<HashMap<String, String>>,
    versions: Option<HashMap<String, NpmVersion>>,
}

#[derive(Debug, Deserialize)]
struct NpmVersion {
    dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies")]
    dev_dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "peerDependencies")]
    peer_dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "optionalDependencies")]
    optional_dependencies: Option<HashMap<String, String>>,
    dist: Option<NpmDist>,
}

#[derive(Debug, Deserialize)]
struct NpmDist {
    shasum: Option<String>,
    integrity: Option<String>,
}

impl NpmCollector {
    pub fn new(registry_url: String) -> Self {
        Self {
            transport: HttpTransport::new(),
            registry_url,
            graph_uri: None,
        }
    }

    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    /// Override the transport, for tests that need fast retries.
    pub fn with_transport(mut self, transport: HttpTransport) -> Self {
        self.transport = transport;
        self
    }

    /// One-line fetch summary for the end of a run.
    pub fn transport_stats(&self) -> StatsSnapshot {
        self.transport.stats()
    }

    pub fn collect_discover(
        &self,
        endpoint: &str,
        auth: &SparqlAuth,
        backend: SparqlBackend,
        output_path: &str,
    ) -> Result<(usize, usize)> {
        let names = crate::seed::discover_by_ecosystem(endpoint, "npm", auth, backend.clone())?;
        let seed_path = "/tmp/seed-npm-discover.txt";
        std::fs::write(seed_path, names.join("\n"))?;
        self.collect(seed_path, output_path)
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        let package_names = read_seed_file(packages_file)?;
        eprintln!(
            "Loaded {} package names from seed file",
            package_names.len()
        );

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 200;

        for (idx, name) in package_names.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, package_names.len());
            }

            match self.fetch_package_with_retry(name, &mut base_delay_ms) {
                Ok(pkg) => {
                    total_triples += self.emit_package_triples(&mut writer, &pkg)?;
                    total_packages += 1;
                }
                Err(e) => eprintln!("  Error fetching {}: {}", name, e),
            }
        }

        eprintln!("  {}", self.transport_stats());

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("npm");
        let rel_uri = release_uri("npm", "registry");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "NPM Registry")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "registry")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    /// Fetch one package document.
    ///
    /// Retry, backoff, `Retry-After`, and inter-request pacing all live in
    /// `HttpTransport`, so what remains here is URL construction and JSON
    /// decoding. `_base_delay_ms` is vestigial -- the transport's per-host
    /// limiter paces requests now -- and goes away when the remaining
    /// registry collectors migrate.
    fn fetch_package_with_retry(
        &self,
        name: &str,
        _base_delay_ms: &mut u64,
    ) -> std::result::Result<NpmPackageDoc, String> {
        let url = format!("{}/{}", self.registry_url, name);

        match self.transport.get(&url, None) {
            Ok(response) => {
                let text = std::str::from_utf8(&response.bytes).map_err(|e| e.to_string())?;
                serde_json::from_str(text).map_err(|e| e.to_string())
            }
            Err(FetchError::NotFound { .. }) => Err(format!("404: {}", name)),
            Err(e) => Err(e.to_string()),
        }
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &NpmPackageDoc,
    ) -> Result<usize> {
        let version = pkg
            .dist_tags
            .as_ref()
            .and_then(|tags| tags.get("latest"))
            .map(|s| s.as_str())
            .unwrap_or("unknown");

        let pkg_uri = package_uri("npm", "registry", "any", &pkg.name, version);
        let identity_uri = package_identity_uri("npm", "registry", "any", &pkg.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{NPM}NpmPackage"))?;
        triples += 2;

        // Identity
        triples += write_package_identity(writer, &identity_uri, &pkg.name)?;
        // identityName + rdfs:label, not packageName: see
        // emit::rdf::write_package_identity for why.
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 1;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("npm", "registry", &pkg.name, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("npm");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(desc) = &pkg.description {
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

        // Dependencies from the latest version
        if let Some(versions) = &pkg.versions {
            if let Some(ver_data) = versions.get(version) {
                if let Some(deps) = &ver_data.dependencies {
                    triples += self.emit_npm_deps(writer, &pkg_uri, deps, "depends")?;
                }
                // Parsed but never emitted until now (#45). They inflate
                // reverse-dependency counts, because enrich_revdeps counts
                // pkg:directlyDependsOn without filtering by dependency type
                // -- but they are real edges, and a dependency graph that
                // omits what was present at build time answers build-
                // provenance and supply-chain questions wrongly. dep_type_uri
                // already routes "dev_depends" to buildDependsOn, so they
                // remain distinguishable from runtime edges.
                if let Some(deps) = &ver_data.dev_dependencies {
                    triples += self.emit_npm_deps(writer, &pkg_uri, deps, "dev_depends")?;
                }
                if let Some(deps) = &ver_data.peer_dependencies {
                    triples += self.emit_npm_deps(writer, &pkg_uri, deps, "peer_depends")?;
                }
                if let Some(deps) = &ver_data.optional_dependencies {
                    triples += self.emit_npm_deps(writer, &pkg_uri, deps, "optional_depends")?;
                }

                // Integrity hash
                if let Some(dist) = &ver_data.dist {
                    if let Some(integrity) = &dist.integrity {
                        writer.write_literal(&pkg_uri, &format!("{NPM}integrity"), integrity)?;
                        triples += 1;
                    }
                    if let Some(shasum) = &dist.shasum {
                        writer.write_literal(&pkg_uri, &format!("{NPM}shasum"), shasum)?;
                        triples += 1;
                    }
                }
            }
        }

        Ok(triples)
    }

    fn emit_npm_deps(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        deps: &HashMap<String, String>,
        dep_type: &str,
    ) -> Result<usize> {
        let mut triples = 0;
        for (dep_key, raw_spec) in deps {
            let spec = NpmSpec::classify(raw_spec);

            // For every spec form but an alias, the map key IS the registry
            // package name -- `"lodash": "git+https://.../lodash"` still
            // depends on lodash, it just says where to get it from. Only
            // `npm:` renames the package, and only there does the key point
            // at something that was never published (#41).
            let target_name = spec.registry_name().unwrap_or(dep_key.as_str());
            let target_uri = package_identity_uri("npm", "registry", "any", target_name);

            writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
            triples += 1;

            // Keyed by the map key, not the target: a package may alias the
            // same target twice under different local names.
            let bnode = bnode_id(dep_type, &format!("{}-{}", pkg_uri, dep_key));
            writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
            writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
            writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
            writer.write_bnode_subject(
                &bnode,
                &format!("{PKG}dependencyType"),
                &dep_type_uri(dep_type),
            )?;
            triples += 4;

            // A constraint is emitted only where the spec really is a version
            // range. Labelling "file:../local" or "git+ssh://..." as semver
            // does not merely mislead: a consumer doing semver arithmetic on
            // it either fails to parse or parses a prefix and is confidently
            // wrong.
            match spec.semver_range() {
                Some(range) if !range.is_empty() => {
                    let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, dep_key));
                    writer.write_bnode_to_bnode(
                        &bnode,
                        &format!("{PKG}hasVersionConstraint"),
                        &cb,
                    )?;
                    writer.write_bnode_subject(
                        &cb,
                        RDF_TYPE,
                        &format!("{PKG}VersionConstraint"),
                    )?;
                    writer.write_bnode_literal(
                        &cb,
                        &format!("{PKG}versionConstraintOperator"),
                        "semver",
                    )?;
                    writer.write_bnode_literal(
                        &cb,
                        &format!("{PKG}versionConstraintValue"),
                        range,
                    )?;
                    triples += 4;
                }
                _ => {}
            }

            // What was not representable is recorded rather than guessed at.
            if let Some((issue_type, severity)) = spec.dq_issue() {
                triples += crate::forge::emit_dq_issue(
                    writer,
                    "collect-npm",
                    "dependency-spec",
                    &format!("{dep_key} -> {raw_spec}"),
                    issue_type,
                    severity,
                )?;
            }
        }
        Ok(triples)
    }
}

/// What a value in an npm dependency map actually says.
///
/// Every value was treated as a semver range and every key as a registry
/// package name. npm permits neither assumption (#41).
#[derive(Debug, PartialEq)]
enum NpmSpec<'a> {
    /// A semver range, dist-tag, or `*`. The map key names the package.
    Range(&'a str),
    /// `npm:<name>@<range>`: the map key is a local rename and `name` is the
    /// package that actually exists. This is how a package depends on two
    /// major versions of one library at once -- `string-width-cjs` and
    /// friends in the `cliui`/`wrap-ansi` chain.
    Alias { name: &'a str, range: &'a str },
    /// `file:`, `link:`, `workspace:`, `portal:` -- resolved from the
    /// checkout. The key still names the package; the value is not a version.
    Local(&'a str),
    /// `git+...`, `github:owner/repo`, a tarball URL -- says where to get the
    /// package, not which version of it.
    Source(&'a str),
    /// A scheme we do not know. Recorded, not guessed at.
    Unknown(&'a str),
}

impl<'a> NpmSpec<'a> {
    fn classify(spec: &'a str) -> Self {
        const LOCAL: [&str; 4] = ["file:", "link:", "workspace:", "portal:"];
        const SOURCE: [&str; 7] = [
            "git+",
            "git:",
            "github:",
            "gitlab:",
            "bitbucket:",
            "http:",
            "https:",
        ];

        if let Some(rest) = spec.strip_prefix("npm:") {
            // A scoped name starts with '@', so the separator is the LAST '@'
            // that is not the scope marker: npm:@scope/pkg@^1 -> @scope/pkg.
            return match rest.rfind('@') {
                Some(at) if at > 0 => NpmSpec::Alias {
                    name: &rest[..at],
                    range: &rest[at + 1..],
                },
                // npm:string-width, npm:@scope/pkg -- an alias with no range.
                _ => NpmSpec::Alias {
                    name: rest,
                    range: "",
                },
            };
        }
        if LOCAL.iter().any(|p| spec.starts_with(p)) {
            return NpmSpec::Local(spec);
        }
        if SOURCE.iter().any(|p| spec.starts_with(p)) || spec.ends_with(".tgz") {
            return NpmSpec::Source(spec);
        }
        // A semver range never contains a colon, so one we did not recognise
        // above is a scheme, not a version.
        if spec.contains(':') {
            return NpmSpec::Unknown(spec);
        }
        NpmSpec::Range(spec)
    }

    /// The registry package this depends on, when the map key is not it.
    fn registry_name(&self) -> Option<&'a str> {
        match self {
            NpmSpec::Alias { name, .. } if !name.is_empty() => Some(name),
            _ => None,
        }
    }

    /// The semver range, where the value really carries one.
    fn semver_range(&self) -> Option<&'a str> {
        match self {
            NpmSpec::Range(r) => Some(r),
            NpmSpec::Alias { range, .. } => Some(range),
            NpmSpec::Local(_) | NpmSpec::Source(_) | NpmSpec::Unknown(_) => None,
        }
    }

    /// `(issue_type, severity)` for a spec whose meaning the graph cannot
    /// carry, or `None` where nothing was lost.
    fn dq_issue(&self) -> Option<(&'static str, &'static str)> {
        match self {
            NpmSpec::Range(_) => None,
            // The local rename is real information the dependency edge no
            // longer carries, now that the edge points at the real package.
            NpmSpec::Alias { .. } => Some(("npm-aliased-dependency", "info")),
            NpmSpec::Local(_) => Some(("npm-local-dependency-spec", "info")),
            NpmSpec::Source(_) => Some(("npm-source-dependency-spec", "info")),
            NpmSpec::Unknown(_) => Some(("npm-unrecognized-dependency-spec", "warning")),
        }
    }
}

/// Read package names from a seed file (one per line).
pub fn read_seed_file(path: &str) -> Result<Vec<String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut names = Vec::new();

    for line_result in reader.lines() {
        let line = line_result?;
        let trimmed = line.trim();

        // Skip comments and blank lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        names.push(trimmed.to_string());
    }

    // Dedup
    names.sort();
    names.dedup();

    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_transport::{HostLimiter, RetryPolicy};
    use std::io::{Read, Write};
    use std::time::Duration;
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment line").unwrap();
        writeln!(temp, "express").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "lodash").unwrap();
        writeln!(temp, "express").unwrap();
        temp.flush().unwrap();

        let names = read_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "express");
        assert_eq!(names[1], "lodash");
    }

    #[test]
    fn test_npm_package_deserialization() {
        let json = r#"{
            "name": "express",
            "description": "Fast web framework",
            "license": "MIT",
            "homepage": "https://expressjs.com",
            "dist-tags": {"latest": "4.19.2"},
            "versions": {
                "4.19.2": {
                    "dependencies": {"accepts": "~1.3.8", "body-parser": "1.20.2"},
                    "peerDependencies": {},
                    "dist": {"shasum": "abc123", "integrity": "sha512-xyz"}
                }
            }
        }"#;

        let pkg: NpmPackageDoc = serde_json::from_str(json).unwrap();
        assert_eq!(pkg.name, "express");
        assert_eq!(pkg.dist_tags.unwrap().get("latest").unwrap(), "4.19.2");
    }

    #[test]
    fn test_emit_npm_package_dual_typing() {
        let collector = NpmCollector::new("https://registry.npmjs.org".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut versions = HashMap::new();
        let mut deps = HashMap::new();
        deps.insert("accepts".into(), "~1.3.8".into());

        versions.insert(
            "4.19.2".into(),
            NpmVersion {
                dependencies: Some(deps),
                dev_dependencies: None,
                peer_dependencies: None,
                optional_dependencies: None,
                dist: Some(NpmDist {
                    shasum: Some("abc123".into()),
                    integrity: Some("sha512-xyz".into()),
                }),
            },
        );

        let mut dist_tags = HashMap::new();
        dist_tags.insert("latest".into(), "4.19.2".into());

        let pkg = NpmPackageDoc {
            name: "express".into(),
            description: Some("Fast web framework".into()),
            license: Some("MIT".into()),
            homepage: Some("https://expressjs.com".into()),
            dist_tags: Some(dist_tags),
            versions: Some(versions),
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
        assert!(content.contains("npm#NpmPackage"));
        assert!(content.contains("\"express\""));
        assert!(content.contains("\"4.19.2\""));
        assert!(content.contains("directlyDependsOn"));
        assert!(content.contains("npm#integrity"));
        assert!(triples > 15);
    }

    // --- npm dependency specs are not all semver ranges (#41) ---

    /// Emit one package whose only dependency is `key: spec`, and return the
    /// N-Triples. Drives the real emitter, because the shape that hurts is
    /// what reaches the graph, not what a classifier returns.
    fn deps_output(key: &str, spec: &str) -> String {
        deps_output_of_kind(key, spec, "dependencies")
    }

    /// As above, but choosing which of package.json's dependency maps the
    /// entry goes in.
    fn deps_output_of_kind(key: &str, spec: &str, kind: &str) -> String {
        let collector = NpmCollector::new("https://registry.npmjs.org".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut deps = HashMap::new();
        deps.insert(key.to_string(), spec.to_string());
        let mut version = NpmVersion {
            dependencies: None,
            dev_dependencies: None,
            peer_dependencies: None,
            optional_dependencies: None,
            dist: None,
        };
        match kind {
            "dependencies" => version.dependencies = Some(deps),
            "devDependencies" => version.dev_dependencies = Some(deps),
            "peerDependencies" => version.peer_dependencies = Some(deps),
            "optionalDependencies" => version.optional_dependencies = Some(deps),
            other => panic!("no such dependency map: {other}"),
        }
        let mut versions = HashMap::new();
        versions.insert("1.0.0".into(), version);
        let mut dist_tags = HashMap::new();
        dist_tags.insert("latest".into(), "1.0.0".into());

        let pkg = NpmPackageDoc {
            name: "cliui".into(),
            description: None,
            license: None,
            homepage: None,
            dist_tags: Some(dist_tags),
            versions: Some(versions),
        };
        collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        content
    }

    fn identity(name: &str) -> String {
        package_identity_uri("npm", "registry", "any", name)
    }

    #[test]
    fn an_alias_depends_on_the_package_that_exists_not_on_the_local_rename() {
        // "string-width-cjs" was minted as a registry identity. Nothing will
        // ever collect it, so the edge pointed at a node with no describing
        // triples -- and the real dependency on string-width, along with any
        // CVE reachable through it, was never recorded at all.
        let out = deps_output("string-width-cjs", "npm:string-width@^4.2.0");
        assert!(
            out.contains(&format!(
                "directlyDependsOn> <{}>",
                identity("string-width")
            )),
            "{out}"
        );
        assert!(
            !out.contains(&identity("string-width-cjs")),
            "the local rename was minted as a registry package:\n{out}"
        );
        // The range still belongs to the constraint.
        assert!(out.contains("versionConstraintValue> \"^4.2.0\""), "{out}");
    }

    #[test]
    fn a_scoped_alias_keeps_its_scope() {
        // The scope marker is itself an '@', so splitting on the first one
        // would yield an empty name and a target of "@".
        let out = deps_output("pkg-cjs", "npm:@scope/pkg@^1.2.3");
        assert!(
            out.contains(&format!("directlyDependsOn> <{}>", identity("@scope/pkg"))),
            "{out}"
        );
        assert!(out.contains("versionConstraintValue> \"^1.2.3\""), "{out}");
    }

    #[test]
    fn an_alias_without_a_range_still_names_the_right_package() {
        let out = deps_output("sw", "npm:string-width");
        assert!(
            out.contains(&format!(
                "directlyDependsOn> <{}>",
                identity("string-width")
            )),
            "{out}"
        );
    }

    #[test]
    fn a_non_registry_spec_keeps_the_key_as_the_package() {
        // "lodash": "git+https://.../lodash" still depends on lodash; it only
        // says where to get it. Dropping the edge would lose a real one.
        for spec in [
            "file:../local",
            "link:../local",
            "workspace:*",
            "git+https://github.com/me/lodash.git",
            "github:me/lodash#branch",
            "https://example.invalid/lodash-1.0.0.tgz",
        ] {
            let out = deps_output("lodash", spec);
            assert!(
                out.contains(&format!("directlyDependsOn> <{}>", identity("lodash"))),
                "{spec} lost the dependency edge:\n{out}"
            );
            assert!(
                !out.contains("versionConstraintOperator"),
                "{spec} was labelled a version constraint:\n{out}"
            );
            assert!(
                !out.contains(&format!("versionConstraintValue> \"{spec}\"")),
                "{spec} was written as a constraint value:\n{out}"
            );
        }
    }

    #[test]
    fn a_plain_semver_range_is_unchanged() {
        // The common case, and the regression guard for everything above.
        let out = deps_output("accepts", "~1.3.8");
        assert!(out.contains(&format!("directlyDependsOn> <{}>", identity("accepts"))));
        assert!(
            out.contains("versionConstraintOperator> \"semver\""),
            "{out}"
        );
        assert!(out.contains("versionConstraintValue> \"~1.3.8\""), "{out}");
        assert!(
            !out.contains("dependency-spec"),
            "nothing was lost, so nothing should be reported:\n{out}"
        );
    }

    #[test]
    fn what_could_not_be_represented_is_recorded() {
        // A dropped constraint that leaves no trace is the same failure in a
        // quieter form.
        for (spec, issue) in [
            ("npm:string-width@^4.2.0", "npm-aliased-dependency"),
            ("file:../local", "npm-local-dependency-spec"),
            ("git+ssh://git@host/x.git", "npm-source-dependency-spec"),
            (
                "patch:lodash@^4#./p.patch",
                "npm-unrecognized-dependency-spec",
            ),
        ] {
            let out = deps_output("lodash", spec);
            assert!(
                out.contains(issue),
                "{spec} was not recorded as {issue}:\n{out}"
            );
            assert!(
                out.contains(spec),
                "the raw spec is what a reader needs:\n{out}"
            );
        }
    }

    #[test]
    fn dev_dependencies_reach_the_graph_as_build_time_edges() {
        // #45: the field was deserialized and silently dropped, so the npm
        // graph could not answer what was present at build time. They are
        // emitted as dev_depends, which dep_type_uri already routes to
        // buildDependsOn -- inflating reverse-dependency counts, but
        // accurately, and distinguishably from runtime edges.
        let out = deps_output_of_kind("eslint", "^9.0.0", "devDependencies");
        assert!(
            out.contains(&format!("directlyDependsOn> <{}>", identity("eslint"))),
            "{out}"
        );
        assert!(
            out.contains("dependencyType> <") && out.contains("buildDependsOn"),
            "a dev dependency must stay distinguishable from a runtime one:\n{out}"
        );
        assert!(out.contains("versionConstraintValue> \"^9.0.0\""), "{out}");
    }

    #[test]
    fn each_dependency_map_keeps_its_own_kind() {
        // Non-vacuous counterpart: if every map produced the same predicate,
        // the assertion above would say nothing.
        let runtime = deps_output_of_kind("accepts", "^1.0.0", "dependencies");
        let dev = deps_output_of_kind("accepts", "^1.0.0", "devDependencies");
        let optional = deps_output_of_kind("accepts", "^1.0.0", "optionalDependencies");
        assert!(runtime.contains("dependsOn>"), "{runtime}");
        assert!(!runtime.contains("buildDependsOn"), "{runtime}");
        assert!(dev.contains("buildDependsOn"), "{dev}");
        assert!(optional.contains("suggests"), "{optional}");
    }

    #[test]
    fn classification_covers_the_forms_npm_actually_permits() {
        use NpmSpec::*;
        assert_eq!(NpmSpec::classify("^1.2.3"), Range("^1.2.3"));
        assert_eq!(NpmSpec::classify("*"), Range("*"));
        assert_eq!(NpmSpec::classify("latest"), Range("latest"));
        assert_eq!(
            NpmSpec::classify("npm:string-width@^4.2.0"),
            Alias {
                name: "string-width",
                range: "^4.2.0"
            }
        );
        assert_eq!(
            NpmSpec::classify("npm:@scope/pkg@^1"),
            Alias {
                name: "@scope/pkg",
                range: "^1"
            }
        );
        assert_eq!(
            NpmSpec::classify("npm:@scope/pkg"),
            Alias {
                name: "@scope/pkg",
                range: ""
            }
        );
        assert_eq!(NpmSpec::classify("workspace:^"), Local("workspace:^"));
        assert_eq!(NpmSpec::classify("github:a/b"), Source("github:a/b"));
        assert_eq!(
            NpmSpec::classify("https://h/x.tgz"),
            Source("https://h/x.tgz")
        );
        // A semver range never contains a colon, so one that does is a
        // scheme we have not taught this function about.
        assert_eq!(NpmSpec::classify("patch:x@^1"), Unknown("patch:x@^1"));
    }

    // ── Characterization: what fetch_package_with_retry does TODAY ──────
    // These pin existing behaviour so the shared-transport migration can
    // prove it changed nothing unintended. No transport-error test: those
    // retries sleep 1+2+4+8 = 15 seconds.

    #[test]
    fn characterize_fetch_returns_doc_on_200() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/left-pad")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"name":"left-pad","versions":{}}"#)
            .expect(1)
            .create();

        let collector = NpmCollector::new(server.url());
        let mut base_delay_ms = 200u64;
        let doc = collector
            .fetch_package_with_retry("left-pad", &mut base_delay_ms)
            .expect("200 should yield a package doc");

        mock.assert();
        assert_eq!(doc.name, "left-pad");
    }

    #[test]
    fn characterize_fetch_404_returns_prefixed_error_without_retry() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/definitely-missing")
            .with_status(404)
            .expect(1) // exactly one request: 404 is terminal today
            .create();

        let collector = NpmCollector::new(server.url());
        let mut base_delay_ms = 200u64;
        let err = collector
            .fetch_package_with_retry("definitely-missing", &mut base_delay_ms)
            .expect_err("404 should be an error");

        mock.assert();
        assert_eq!(err, "404: definitely-missing");
    }

    #[test]
    fn characterize_429_with_retry_after_is_retried_then_succeeds() {
        let mut server = mockito::Server::new();
        let rate_limited = server
            .mock("GET", "/slowpkg")
            .with_status(429)
            .with_header("retry-after", "1")
            .expect(1)
            .create();
        let ok = server
            .mock("GET", "/slowpkg")
            .with_status(200)
            .with_body(r#"{"name":"slowpkg"}"#)
            .expect(1)
            .create();

        let collector = NpmCollector::new(server.url());
        let mut base_delay_ms = 200u64;
        let doc = collector
            .fetch_package_with_retry("slowpkg", &mut base_delay_ms)
            .expect("should succeed after the rate limit clears");

        rate_limited.assert();
        ok.assert();
        assert_eq!(doc.name, "slowpkg");
        // Pre-migration this also asserted base_delay_ms doubled to 400.
        // That described npm's manual inter-request pacing, which the
        // transport's per-host limiter replaces; the retry behaviour this
        // test exists to pin is unchanged.
    }

    #[test]
    fn migrated_500_is_retried_then_reported_as_an_http_error() {
        // Deliberate change from the pre-migration behaviour: npm used to
        // hand a 500 body to serde_json after a single request, because it
        // checked only for 429 and 404. is_retryable() now treats 5xx as
        // transient.
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/brokenpkg")
            .with_status(500)
            .with_body("upstream exploded")
            .expect(3)
            .create();

        let collector = NpmCollector::new(server.url()).with_transport(
            HttpTransport::new()
                .with_policy(RetryPolicy {
                    max_attempts: 3,
                    base_delay: Duration::from_millis(1),
                    max_delay: Duration::from_millis(4),
                    jitter: false,
                })
                .with_limiter(HostLimiter::new(Duration::from_millis(0))),
        );
        let mut base_delay_ms = 200u64;
        let err = collector
            .fetch_package_with_retry("brokenpkg", &mut base_delay_ms)
            .expect_err("500 should still be an error after retries");

        mock.assert();
        assert!(
            err.contains("500"),
            "error should name the status, got: {err}"
        );
    }

    #[test]
    fn characterize_malformed_json_body_is_a_parse_error() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/badjson")
            .with_status(200)
            .with_body("{not json")
            .expect(1)
            .create();

        let collector = NpmCollector::new(server.url());
        let mut base_delay_ms = 200u64;
        let err = collector
            .fetch_package_with_retry("badjson", &mut base_delay_ms)
            .expect_err("malformed JSON should fail");

        mock.assert();
        assert!(
            !err.starts_with("404:"),
            "parse failure must not be reported as a 404, got: {err}"
        );
    }
}
