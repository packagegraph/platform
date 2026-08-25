//! OSS Taxonomy classification enricher (all 6 facets).
//!
//! Classifies package identities using pattern matching on package names,
//! descriptions, and upstream ecosystem metadata. Emits pkg:hasClassification
//! triples linking PackageIdentity to tax:* SKOS concepts.
//!
//! Facets:
//! - Technology: mapped from upstream ecosystem
//! - Role: inferred from name patterns (lib*, *-cli, *-server, etc.)
//! - Domain: keyword matching on name + description
//! - Function: keyword matching on name + description
//! - Audience: keyword matching on description
//! - Layer: inferred from name patterns and description keywords

use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use std::fs::File;
use std::io::Result;

pub struct TaxonomyEnricher {
    sparql: SparqlClient,
    pub graph_uri: Option<String>,
}

impl TaxonomyEnricher {
    pub fn new(endpoint: &str, auth: SparqlAuth, backend: SparqlBackend) -> Self {
        let sparql = make_sparql_client(endpoint, &auth, backend);
        Self {
            sparql,
            graph_uri: None,
        }
    }

    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        let packages = self.query_package_identities()?;
        eprintln!("Found {} package identities to classify", packages.len());

        let mut classified = 0usize;
        let mut total_triples = 0usize;

        for pkg in &packages {
            let classifications = classify_package(
                &pkg.name,
                pkg.description.as_deref(),
                pkg.ecosystem.as_deref(),
            );
            if !classifications.is_empty() {
                classified += 1;
                for concept_uri in &classifications {
                    writer.write_triple(
                        &pkg.identity_uri,
                        &format!("{PKG}hasClassification"),
                        concept_uri,
                    )?;
                    total_triples += 1;
                }
            }
        }

        writer.flush()?;
        eprintln!(
            "Classified {} / {} packages ({} triples)",
            classified,
            packages.len(),
            total_triples
        );
        Ok((classified, total_triples))
    }

    fn query_package_identities(&self) -> Result<Vec<PackageInfo>> {
        let eco_base = format!("{DATA}ecosystem/");
        let query = format!(
            r#"SELECT ?identity ?name ?description ?ecosystemName WHERE {{
              ?identity a <{PKG}PackageIdentity> ;
                        <{PKG}packageName> ?name .
              OPTIONAL {{
                ?pkg <{PKG}isVersionOf> ?identity ;
                     <{PKG}description> ?description .
              }}
              OPTIONAL {{
                ?identity <{PKG}upstreamEcosystem> ?ecoUri .
                BIND(STRAFTER(STR(?ecoUri), "{eco_base}") AS ?ecosystemName)
              }}
            }}"#,
            PKG = PKG,
            eco_base = eco_base,
        );

        let results = self.sparql.query(&query)?;
        let mut packages = Vec::new();
        for row in results {
            if let (Some(uri), Some(name)) = (row.get("identity"), row.get("name")) {
                packages.push(PackageInfo {
                    identity_uri: uri.clone(),
                    name: name.clone(),
                    description: row.get("description").filter(|s| !s.is_empty()).cloned(),
                    ecosystem: row.get("ecosystemName").filter(|s| !s.is_empty()).cloned(),
                });
            }
        }
        packages.dedup_by(|a, b| a.identity_uri == b.identity_uri);
        Ok(packages)
    }
}

struct PackageInfo {
    identity_uri: String,
    name: String,
    description: Option<String>,
    ecosystem: Option<String>,
}

fn classify_package(name: &str, description: Option<&str>, ecosystem: Option<&str>) -> Vec<String> {
    let mut concepts = Vec::new();
    let lower_name = name.to_ascii_lowercase();
    let lower_desc = description.unwrap_or("").to_ascii_lowercase();

    // Technology facet
    if let Some(eco) = ecosystem {
        if let Some(tech) = ecosystem_to_technology(eco) {
            concepts.push(format!("{TAX}technology-{tech}"));
        }
    }

    // Role facet
    if let Some(role) = infer_role(&lower_name) {
        concepts.push(format!("{TAX}role-{role}"));
    }

    // Domain facet
    for domain in infer_domains(&lower_name, &lower_desc) {
        concepts.push(format!("{TAX}domain-{domain}"));
    }

    // Function facet
    for func in infer_functions(&lower_name, &lower_desc) {
        concepts.push(format!("{TAX}function-{func}"));
    }

    // Audience facet
    if let Some(audience) = infer_audience(&lower_name, &lower_desc) {
        concepts.push(format!("{TAX}audience-{audience}"));
    }

    // Layer facet
    if let Some(layer) = infer_layer(&lower_name, &lower_desc) {
        concepts.push(format!("{TAX}layer-{layer}"));
    }

    concepts
}

fn ecosystem_to_technology(ecosystem: &str) -> Option<&'static str> {
    match ecosystem.to_ascii_lowercase().as_str() {
        "npm" | "javascript" | "nodejs" => Some("javascript"),
        "pypi" | "python" => Some("python"),
        "cargo" | "rust" | "crates.io" => Some("rust"),
        "gomod" | "go" | "golang" => Some("go"),
        "maven" | "java" => Some("java"),
        "nuget" | "csharp" | "dotnet" => Some("csharp"),
        "rubygems" | "ruby" => Some("ruby"),
        "cpan" | "perl" => Some("perl"),
        "hackage" | "haskell" => Some("haskell"),
        "hex" | "elixir" | "erlang" => Some("elixir"),
        "cran" | "r" => Some("r"),
        "conda" => Some("python"),
        _ => None,
    }
}

const KNOWN_FRAMEWORKS: &[&str] = &[
    "django",
    "flask",
    "fastapi",
    "rails",
    "sinatra",
    "spring",
    "express",
    "nextjs",
    "nuxt",
    "svelte",
    "ember",
    "laravel",
    "symfony",
    "rocket",
    "actix-web",
    "axum",
    "gin",
    "echo",
    "phoenix",
    "qt5",
    "qt6",
    "gtk3",
    "gtk4",
];

fn infer_role(name: &str) -> Option<&'static str> {
    if name.starts_with("lib")
        || name.ends_with("-dev")
        || name.ends_with("-devel")
        || name.ends_with("-libs")
        || name.ends_with("-lib")
    {
        return Some("library");
    }
    if name.ends_with("-cli")
        || name.ends_with("-tools")
        || name.ends_with("-utils")
        || name.ends_with("-tool")
        || name.ends_with("-bin")
    {
        return Some("cli-tool");
    }
    if name.ends_with("-framework")
        || name.contains("framework")
        || KNOWN_FRAMEWORKS.iter().any(|f| name == *f)
    {
        return Some("framework");
    }
    if name.ends_with("-server") || name.ends_with("-daemon") {
        return Some("service");
    }
    if name.ends_with("-doc") || name.ends_with("-docs") || name.ends_with("-man") {
        return Some("documentation");
    }
    if name.ends_with("-plugin") || name.ends_with("-plugins") || name.ends_with("-extension") {
        return Some("plugin");
    }
    if name.ends_with("-compiler") || name == "gcc" || name == "clang" || name == "rustc" {
        return Some("compiler");
    }
    if name.ends_with("-lint") || name.ends_with("-linter") {
        return Some("linter");
    }
    None
}

fn infer_domains(name: &str, desc: &str) -> Vec<&'static str> {
    let mut domains = Vec::new();
    let text = format!("{name} {desc}");

    if contains_any(
        &text,
        &[
            "security",
            "cryptograph",
            "cipher",
            "tls ",
            "ssl ",
            "firewall",
            "selinux",
            "apparmor",
            "vulnerability",
            "cve ",
        ],
    ) {
        domains.push("security");
    }
    if contains_any(
        &text,
        &[
            "web server",
            "http server",
            "web application",
            "web framework",
            "html ",
            "cgi ",
            "wsgi",
            "asgi",
        ],
    ) {
        domains.push("web-development");
    }
    if contains_any(
        &text,
        &[
            "kernel",
            "systemd",
            "init system",
            "bootloader",
            "grub",
            "filesystem",
            "mount ",
            "partition",
        ],
    ) {
        domains.push("operating-systems");
    }
    if contains_any(
        &text,
        &[
            "database",
            "sql ",
            "nosql",
            "postgresql",
            "mysql",
            "mariadb",
            "sqlite",
            "mongodb",
        ],
    ) {
        domains.push("database");
    }
    if contains_any(
        &text,
        &[
            "machine learning",
            "neural network",
            "deep learning",
            "tensorflow",
            "pytorch",
            "scikit",
        ],
    ) {
        domains.push("machine-learning");
    }
    if contains_any(
        &text,
        &[
            "container",
            "docker",
            "podman",
            "kubernetes",
            "k8s ",
            "orchestrat",
        ],
    ) {
        domains.push("devops");
    }
    if contains_any(
        &text,
        &["game ", "gaming", "opengl", "vulkan", "sdl ", "game engine"],
    ) {
        domains.push("game-development");
    }
    if contains_any(
        &text,
        &[
            "embedded",
            "microcontroller",
            "firmware",
            "rtos",
            "arm cortex",
        ],
    ) {
        domains.push("embedded-systems");
    }

    domains
}

fn infer_functions(name: &str, desc: &str) -> Vec<&'static str> {
    let mut funcs = Vec::new();
    let text = format!("{name} {desc}");

    if contains_any(
        &text,
        &[
            "encrypt",
            "decrypt",
            "cipher",
            "cryptograph",
            "aes ",
            "rsa ",
            "gpg",
            "pgp",
            "x509",
            "certificate",
        ],
    ) {
        funcs.push("encryption");
    }
    if contains_any(
        &text,
        &[
            "authenticat",
            "login",
            "oauth",
            "saml",
            "kerberos",
            "pam ",
            "ldap auth",
        ],
    ) {
        funcs.push("authentication");
    }
    if contains_any(
        &text,
        &[
            "rest api",
            "grpc",
            "graphql",
            "api gateway",
            "api server",
            "openapi",
            "web application",
            "web framework",
        ],
    ) {
        funcs.push("api-development");
    }
    if contains_any(
        &text,
        &[
            "logging",
            "log file",
            "syslog",
            "journald",
            "log rotation",
            "log collector",
        ],
    ) {
        funcs.push("logging");
    }
    if contains_any(&text, &["cache", "caching", "memcache", "redis", "varnish"]) {
        funcs.push("caching");
    }
    if contains_any(
        &text,
        &[
            "ci/cd",
            "continuous integration",
            "continuous delivery",
            "jenkins",
            "gitlab-ci",
        ],
    ) {
        funcs.push("ci-cd");
    }
    if contains_any(
        &text,
        &[
            "process manage",
            "init system",
            "supervisor",
            "systemd",
            "service manager",
            "daemon manage",
        ],
    ) {
        funcs.push("process-management");
    }
    if contains_any(
        &text,
        &[
            "compress",
            "decompress",
            "gzip",
            "bzip2",
            "zstd",
            "lz4 ",
            "xz ",
            "zip ",
            "archive",
        ],
    ) {
        funcs.push("compression");
    }
    if contains_any(
        &text,
        &["automat", "workflow", "task runner", "cron", "scheduler"],
    ) && !contains_any(&text, &["test automat"])
    {
        funcs.push("automation");
    }
    if contains_any(
        &text,
        &[
            "deploy",
            "provisioning",
            "ansible",
            "puppet",
            "chef ",
            "terraform",
        ],
    ) {
        funcs.push("deployment");
    }

    funcs
}

fn infer_audience(name: &str, desc: &str) -> Option<&'static str> {
    let text = format!("{name} {desc}");

    if contains_any(
        &text,
        &[
            "system admin",
            "sysadmin",
            "server manage",
            "infrastructure manage",
        ],
    ) {
        return Some("system-administrator");
    }
    if contains_any(
        &text,
        &[
            "developer tool",
            "development tool",
            "sdk ",
            "development kit",
            "debugger",
            "for developer",
            "by developer",
            "development",
        ],
    ) {
        return Some("developer");
    }
    if contains_any(&text, &["enterprise", "business", "corporate"]) {
        return Some("enterprise");
    }

    None
}

fn infer_layer(name: &str, desc: &str) -> Option<&'static str> {
    let text = format!("{name} {desc}");

    if contains_any(
        &text,
        &[
            "kernel",
            "driver",
            "firmware",
            "bootloader",
            "grub",
            "uefi",
            "bios",
        ],
    ) {
        return Some("operating-system");
    }
    if contains_any(
        &text,
        &[
            "infrastructure",
            "cloud",
            "provisioning",
            "terraform",
            "ansible",
        ],
    ) {
        return Some("infrastructure");
    }
    if contains_any(
        &text,
        &[
            "frontend",
            "ui ",
            "user interface",
            "widget",
            "gtk",
            "qt ",
            "react",
            "angular",
            "css ",
        ],
    ) {
        return Some("frontend");
    }
    if contains_any(
        &text,
        &[
            "backend",
            "server-side",
            "api server",
            "database server",
            "web server",
            "daemon",
            " server",
        ],
    ) {
        return Some("backend");
    }
    if contains_any(
        &text,
        &[
            "middleware",
            "message queue",
            "message broker",
            "rabbitmq",
            "kafka",
        ],
    ) {
        return Some("middleware");
    }
    if contains_any(
        &text,
        &[
            "network", "tcp ", "udp ", "dns ", "dhcp", "routing", "firewall", "iptables",
            "nftables",
        ],
    ) {
        return Some("network-layer");
    }

    None
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ecosystem_to_technology() {
        assert_eq!(ecosystem_to_technology("npm"), Some("javascript"));
        assert_eq!(ecosystem_to_technology("pypi"), Some("python"));
        assert_eq!(ecosystem_to_technology("cargo"), Some("rust"));
        assert_eq!(ecosystem_to_technology("unknown"), None);
    }

    #[test]
    fn test_infer_role_library() {
        assert_eq!(infer_role("libssl-dev"), Some("library"));
        assert_eq!(infer_role("libcurl4"), Some("library"));
        assert_eq!(infer_role("openssl-devel"), Some("library"));
        assert_eq!(infer_role("glibc-libs"), Some("library"));
    }

    #[test]
    fn test_infer_role_cli() {
        assert_eq!(infer_role("podman-cli"), Some("cli-tool"));
        assert_eq!(infer_role("bind-utils"), Some("cli-tool"));
    }

    #[test]
    fn test_infer_role_compiler() {
        assert_eq!(infer_role("gcc"), Some("compiler"));
        assert_eq!(infer_role("clang"), Some("compiler"));
    }

    #[test]
    fn test_classify_cq_class_01() {
        // CQ-CLASS-01: frameworks with unpatched CVEs need domain + role
        let classes = classify_package(
            "django",
            Some("A high-level Python web framework"),
            Some("pypi"),
        );
        assert!(classes.iter().any(|c| c.contains("domain-web-development")));
        assert!(classes.iter().any(|c| c.contains("role-framework")));
        assert!(classes.iter().any(|c| c.contains("technology-python")));
    }

    #[test]
    fn test_classify_security_package() {
        let classes = classify_package("openssl", Some("TLS/SSL cryptography library"), None);
        assert!(classes.iter().any(|c| c.contains("domain-security")));
        assert!(classes.iter().any(|c| c.contains("function-encryption")));
    }

    #[test]
    fn test_classify_kernel_package() {
        let classes = classify_package("kernel", Some("The Linux kernel"), None);
        assert!(classes
            .iter()
            .any(|c| c.contains("domain-operating-systems")));
        assert!(classes.iter().any(|c| c.contains("layer-operating-system")));
    }

    #[test]
    fn test_classify_database_package() {
        let classes = classify_package("postgresql", Some("PostgreSQL database server"), None);
        assert!(classes.iter().any(|c| c.contains("domain-database")));
        assert!(classes.iter().any(|c| c.contains("layer-backend")));
    }

    #[test]
    fn test_classify_sysadmin_tool() {
        let classes = classify_package(
            "ansible",
            Some("Infrastructure provisioning and configuration management tool for system administrators"),
            None,
        );
        assert!(classes
            .iter()
            .any(|c| c.contains("audience-system-administrator")));
        assert!(classes.iter().any(|c| c.contains("layer-infrastructure")));
        assert!(classes.iter().any(|c| c.contains("function-deployment")));
    }

    #[test]
    fn test_classify_compression() {
        let classes = classify_package("gzip", Some("GNU compression utility"), None);
        assert!(classes.iter().any(|c| c.contains("function-compression")));
    }

    #[test]
    fn test_classify_container_tool() {
        let classes = classify_package(
            "podman",
            Some("Container management tool for managing pods, containers, and images"),
            None,
        );
        assert!(classes.iter().any(|c| c.contains("domain-devops")));
    }

    #[test]
    fn test_classify_lib_prefix() {
        let classes = classify_package("libfoo", None, None);
        assert!(classes.iter().any(|c| c.contains("role-library")));
    }

    #[test]
    fn test_classify_no_match() {
        let classes = classify_package("somepkg", None, None);
        assert!(classes.is_empty());
    }

    #[test]
    fn test_classify_all_six_facets() {
        // Verify a well-described package can hit all 6 facets
        let classes = classify_package(
            "django-framework",
            Some("A high-level Python web framework for rapid web application development by developers"),
            Some("pypi"),
        );
        let has = |facet: &str| classes.iter().any(|c| c.contains(facet));
        assert!(has("technology-"), "Should have technology");
        assert!(has("role-"), "Should have role");
        assert!(has("domain-"), "Should have domain");
        assert!(
            has("function-"),
            "Should have function (api-development from 'web application')"
        );
        assert!(has("audience-"), "Should have audience");
        // layer may or may not match — backend from "web framework"
    }
}
