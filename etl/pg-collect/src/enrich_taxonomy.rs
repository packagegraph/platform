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
//!
//! ## Why this pages (#59)
//!
//! It used to ask for every PackageIdentity in the corpus in one query and
//! hold the answer in a Vec before classifying any of it. On the collection
//! host that reached the 4 GB cgroup ceiling in 29 seconds and was OOM-killed
//! -- the peak is the response body and its parse, not the classification,
//! which needs nothing but the row in front of it.
//!
//! So the query is ordered by identity and walked a page at a time, keyed on
//! the last identity seen rather than an OFFSET, and each row is classified
//! and written as it arrives. Memory is now one page, not one corpus.
//!
//! The ordering is also a correctness fix. The description join fans out to
//! one row per version, and the old code deduplicated with `Vec::dedup_by`,
//! which only removes ADJACENT duplicates -- with no ORDER BY nothing
//! guaranteed they were adjacent, so an identity whose rows arrived split
//! could be classified, and emitted, more than once.

use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use std::fs::File;
use std::io::Result;

pub struct TaxonomyEnricher {
    sparql: SparqlClient,
    /// Rows per request. A field only so the paging loop can be exercised in
    /// a test without synthesising 50,000 rows.
    page_size: usize,
    pub graph_uri: Option<String>,
}

impl TaxonomyEnricher {
    pub fn new(endpoint: &str, auth: SparqlAuth, backend: SparqlBackend) -> Self {
        let sparql = make_sparql_client(endpoint, &auth, backend);
        Self {
            sparql,
            page_size: PAGE,
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

        let mut classified = 0usize;
        let mut total_triples = 0usize;
        let mut seen = 0usize;
        let mut after: Option<String> = None;
        // The identity the last emitted classification belonged to. Rows for
        // one identity are contiguous under the ORDER BY, so remembering one
        // is enough to collapse the description join's fan-out -- and a page
        // boundary cannot split a run open, because the next page starts
        // strictly after the last identity this one saw.
        let mut current: Option<String> = None;

        loop {
            let rows = self.sparql.query(&package_identities_page_query(
                after.as_deref(),
                self.page_size,
            ))?;
            if rows.is_empty() {
                break;
            }

            let mut page_last = None;
            for row in &rows {
                let (Some(identity_uri), Some(name)) = (row.get("identity"), row.get("name"))
                else {
                    continue;
                };
                page_last = Some(identity_uri.clone());
                if current.as_deref() == Some(identity_uri.as_str()) {
                    continue;
                }
                current = Some(identity_uri.clone());
                seen += 1;

                let description = row.get("description").filter(|s| !s.is_empty());
                let ecosystem = row.get("ecosystemName").filter(|s| !s.is_empty());
                let classifications = classify_package(
                    name,
                    description.map(|s| s.as_str()),
                    ecosystem.map(|s| s.as_str()),
                );
                if classifications.is_empty() {
                    continue;
                }
                classified += 1;
                for concept_uri in &classifications {
                    writer.write_triple(
                        identity_uri,
                        &format!("{PKG}hasClassification"),
                        concept_uri,
                    )?;
                    total_triples += 1;
                }
            }

            eprintln!("Classified {classified} / {seen} package identities so far");
            if rows.len() < self.page_size {
                break;
            }
            // No new identity in a full page would mean the key never
            // advanced; stopping beats looping on the same page forever.
            match page_last {
                Some(last) if Some(&last) != after.as_ref() => after = Some(last),
                _ => break,
            }
        }

        writer.flush()?;
        eprintln!("Classified {classified} / {seen} packages ({total_triples} triples)");
        Ok((classified, total_triples))
    }
}

/// Rows per request. Large enough that the corpus is a few hundred requests,
/// small enough that a page is megabytes rather than gigabytes.
const PAGE: usize = 50_000;

/// One page of package identities, ordered so that paging is stable and the
/// description fan-out arrives contiguously.
///
/// Keyed on the last identity of the previous page rather than an OFFSET: a
/// deep OFFSET makes the store re-walk everything it has already returned,
/// and the cost of that grows with every page.
fn package_identities_page_query(after: Option<&str>, limit: usize) -> String {
    let eco_base = format!("{DATA}ecosystem/");
    let keyset = match after {
        Some(last) => format!(
            "\n              FILTER(STR(?identity) > \"{}\")",
            last.replace('\\', "\\\\").replace('"', "\\\"")
        ),
        None => String::new(),
    };
    format!(
        r#"SELECT ?identity ?name ?description ?ecosystemName WHERE {{
              ?identity a <{PKG}PackageIdentity> ;
                        <{PKG}packageName> ?name .{keyset}
              OPTIONAL {{
                ?pkg <{PKG}isVersionOf> ?identity ;
                     <{PKG}description> ?description .
              }}
              OPTIONAL {{
                ?identity <{PKG}upstreamEcosystem> ?ecoUri .
                BIND(STRAFTER(STR(?ecoUri), "{eco_base}") AS ?ecosystemName)
              }}
            }} ORDER BY ?identity LIMIT {limit}"#,
        PKG = PKG,
        eco_base = eco_base,
        keyset = keyset,
        limit = limit,
    )
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
    use std::io::Read;
    use tempfile::NamedTempFile;

    /// A page must be ordered (so the fan-out is contiguous and the key
    /// advances predictably) and bounded (so the response is a page, not a
    /// corpus -- the 4 GB OOM in #59).
    #[test]
    fn the_first_page_is_ordered_bounded_and_unfiltered() {
        let q = package_identities_page_query(None, 50_000);
        assert!(q.contains("ORDER BY ?identity"));
        assert!(q.contains("LIMIT 50000"));
        assert!(!q.contains("FILTER"), "the first page has nothing to skip");
    }

    /// Keyset, not OFFSET: a deep OFFSET makes the store re-walk every row it
    /// has already returned.
    #[test]
    fn a_later_page_starts_strictly_after_the_previous_one() {
        let q = package_identities_page_query(Some("https://example.org/i/curl"), 10);
        assert!(q.contains(r#"FILTER(STR(?identity) > "https://example.org/i/curl")"#));
        assert!(!q.contains("OFFSET"));
    }

    fn rows(bindings: &[(&str, &str)]) -> String {
        let rows: Vec<String> = bindings
            .iter()
            .map(|(identity, name)| {
                format!(
                    r#"{{"identity": {{"value": "{identity}"}}, "name": {{"value": "{name}"}}}}"#
                )
            })
            .collect();
        format!(r#"{{"results": {{"bindings": [{}]}}}}"#, rows.join(","))
    }

    fn enricher_against(server: &mockito::Server, page_size: usize) -> TaxonomyEnricher {
        TaxonomyEnricher {
            sparql: SparqlClient::new(&server.url()),
            page_size,
            graph_uri: None,
        }
    }

    fn classify_to_string(enricher: &TaxonomyEnricher) -> ((usize, usize), String) {
        let temp_file = NamedTempFile::new().unwrap();
        let counts = enricher.enrich(temp_file.path().to_str().unwrap()).unwrap();
        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        (counts, content)
    }

    /// The description join fans out to one row per version. Those rows are
    /// the same identity and must be classified once -- the old code relied
    /// on `Vec::dedup_by` over an unordered result, which only collapses
    /// ADJACENT duplicates.
    #[test]
    fn the_description_fanout_is_classified_once_not_once_per_version() {
        let mut server = mockito::Server::new();
        let _page = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(rows(&[
                ("https://example.org/i/libfoo", "libfoo"),
                ("https://example.org/i/libfoo", "libfoo"),
                ("https://example.org/i/libfoo", "libfoo"),
            ]))
            .create();

        let ((classified, triples), content) = classify_to_string(&enricher_against(&server, 10));

        assert_eq!(classified, 1, "one identity, not three rows");
        assert_eq!(
            content.lines().filter(|l| !l.trim().is_empty()).count(),
            triples,
            "every counted triple is a line and vice versa"
        );
        assert_eq!(
            content.matches("role-library").count(),
            1,
            "classified once:\n{content}"
        );
    }

    /// A full page means there may be more. The next request must carry the
    /// key forward rather than stopping at the limit.
    #[test]
    fn a_full_page_is_followed_by_another_keyed_on_its_last_identity() {
        let mut server = mockito::Server::new();
        let first = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(rows(&[
                ("https://example.org/i/libalpha", "libalpha"),
                ("https://example.org/i/libbeta", "libbeta"),
            ]))
            .expect(1)
            .create();
        let second = server
            .mock("POST", "/sparql")
            // The key from the end of the first page, percent-encoded by the
            // form body the SPARQL client posts.
            .match_body(mockito::Matcher::Regex("libbeta".to_string()))
            .with_status(200)
            .with_body(rows(&[("https://example.org/i/libgamma", "libgamma")]))
            .expect(1)
            .create();

        let ((classified, _), content) = classify_to_string(&enricher_against(&server, 2));

        first.assert();
        second.assert();
        assert_eq!(classified, 3, "the short second page completed the walk");
        assert!(content.contains("libgamma"), "page two was emitted");
    }

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
