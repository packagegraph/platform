//! Bodhi advisory collector — Fedora security updates via RSS feed.
//!
//! Fetches security advisories from Bodhi RSS feed, parses advisory metadata,
//! and extracts NVRs and CVE references for package resolution.

use crate::cache::FileCache;
use crate::enricher::rate_limit;
use crate::forge::emit_dq_issue;
use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use once_cell::sync::Lazy;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use regex::Regex;
use reqwest::blocking::Client;
use serde_json;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

/// CVE identifier regex: CVE-YYYY-NNNNN
static CVE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"CVE-\d{4}-\d{4,}").unwrap());

/// Bodhi advisory collector with SPARQL-based package resolution.
pub struct BodhiCollector {
    client: Client,
    sparql: SparqlClient,
    /// Bodhi release tag for API queries (e.g., "F43")
    release: String,
    /// Numeric release for graph URIs (e.g., "43"), stripped from Bodhi tag
    graph_release: String,
    since: Option<String>,
    cache: Option<FileCache>,
    pub graph_uri: Option<String>,
}

impl BodhiCollector {
    /// Create a new Bodhi collector.
    ///
    /// - `endpoint`: Fuseki SPARQL endpoint URL for NVR→binary resolution
    /// - `release`: Fedora release tag (e.g., "F43")
    /// - `since`: Optional date filter (ISO format YYYY-MM-DD) — only advisories after this date
    /// - `cache_dir`: Optional cache directory for RSS feeds
    pub fn new(
        endpoint: &str,
        release: String,
        since: Option<String>,
        cache_dir: Option<&str>,
        auth: SparqlAuth,
        backend: SparqlBackend,
    ) -> Result<Self> {
        let client = crate::enricher::default_http_client();

        let cache = cache_dir
            .map(|dir| FileCache::new(dir, "bodhi", 168, None))
            .transpose()?;

        // Normalize release: strip "F"/"f" prefix for graph URIs
        // Bodhi uses "F43" but graph URIs use "43"
        let graph_release = release
            .strip_prefix('F')
            .or_else(|| release.strip_prefix('f'))
            .unwrap_or(&release)
            .to_string();

        Ok(Self {
            client,
            sparql: make_sparql_client(endpoint, &auth, backend),
            release,
            graph_release,
            since,
            cache,
            graph_uri: None,
        })
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    /// Collect Bodhi advisories and emit N-Triples.
    ///
    /// Returns (advisories_count, triples_count).
    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        let mut total_advisories = 0;
        let mut total_triples = 0;
        let mut total_resolved_packages = 0;
        let mut unresolved_nvrs = 0;
        let mut page = 1;

        loop {
            let url = format!(
                "https://bodhi.fedoraproject.org/rss/updates/?type=security&releases={}&status=stable&page={}",
                self.release, page
            );

            eprintln!("Fetching page {}...", page);

            let cache_key = format!("bodhi-{}-page-{}", self.release, page);
            let xml = match self.cached_get(&cache_key) {
                Some(data) => data,
                None => {
                    let resp = self.client.get(&url).send().map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    })?;

                    if !resp.status().is_success() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Bodhi RSS returned {}", resp.status()),
                        ));
                    }

                    let xml = resp.text().map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    })?;

                    self.cache_put(&cache_key, &xml);
                    xml
                }
            };

            // Parse RSS items
            let advisories = self.parse_rss_feed(&xml)?;
            if advisories.is_empty() {
                eprintln!("No more advisories, stopping pagination");
                break;
            }

            for advisory in advisories {
                // Apply --since date filter if specified
                if let Some(ref cutoff) = self.since {
                    // Convert advisory date to ISO format for comparison
                    if let Ok(advisory_date_iso) = parse_rfc2822_to_iso8601(&advisory.date) {
                        if &advisory_date_iso < cutoff {
                            continue; // Skip advisories before cutoff
                        }
                    }
                }

                let advisory_uri = format!(
                    "{DATA}advisory/fedora/{}/{}",
                    self.graph_release, advisory.id
                );

                let mut advisory_triples = 0;

                // Advisory entity
                writer.write_triple(&advisory_uri, RDF_TYPE, &format!("{SEC}SecurityAdvisory"))?;
                writer.write_literal(&advisory_uri, &format!("{SEC}advisoryId"), &advisory.id)?;
                writer.write_triple(
                    &advisory_uri,
                    &format!("{SEC}advisoryType"),
                    &advisory_category_uri("security"),
                )?;
                advisory_triples += 3;

                // Publication date as xsd:dateTime (convert RFC 2822 → ISO 8601)
                if let Ok(dt) = parse_rfc2822_to_iso8601(&advisory.date) {
                    writer.write_datetime(&advisory_uri, &format!("{SEC}advisoryDate"), &dt)?;
                    advisory_triples += 1;
                }

                // Resolve NVRs to binary packages
                for nvr in &advisory.nvrs {
                    if let Some((name, version_release)) = Self::parse_nvr(nvr) {
                        match self.resolve_nvr_to_binaries(&name, &version_release) {
                            Ok(binaries) if !binaries.is_empty() => {
                                for pkg_uri in binaries {
                                    writer.write_triple(
                                        &advisory_uri,
                                        &format!("{SEC}advisoryForPackage"),
                                        &pkg_uri,
                                    )?;
                                    advisory_triples += 1;
                                    total_resolved_packages += 1;
                                }
                            }
                            Ok(_) => {
                                eprintln!(
                                    "  Warning: NVR {} has no matching binaries in graph",
                                    nvr
                                );
                                advisory_triples += emit_dq_issue(
                                    &mut writer,
                                    "bodhi-collector",
                                    "nvr",
                                    nvr,
                                    "nvr-unresolved",
                                    "info",
                                )?;
                                unresolved_nvrs += 1;
                            }
                            Err(e) => {
                                eprintln!("  Warning: SPARQL query failed for NVR {}: {}", nvr, e);
                                advisory_triples += emit_dq_issue(
                                    &mut writer,
                                    "bodhi-collector",
                                    "nvr",
                                    nvr,
                                    "nvr-query-failed",
                                    "warning",
                                )?;
                                unresolved_nvrs += 1;
                            }
                        }
                    } else {
                        eprintln!("  Warning: Could not parse NVR: {}", nvr);
                        advisory_triples += emit_dq_issue(
                            &mut writer,
                            "bodhi-collector",
                            "nvr",
                            nvr,
                            "nvr-parse-failed",
                            "warning",
                        )?;
                        unresolved_nvrs += 1;
                    }
                }

                // CVE cross-references
                for cve_id in &advisory.cves {
                    let cve_uri = cve_entity_uri(cve_id);
                    writer.write_triple(
                        &advisory_uri,
                        &format!("{SEC}addressesVulnerability"),
                        &cve_uri,
                    )?;
                    advisory_triples += 1;
                }

                total_advisories += 1;
                total_triples += advisory_triples;
            }

            page += 1;
            rate_limit(Duration::from_secs(1)); // 1 request/second
        }

        writer.flush()?;

        eprintln!();
        eprintln!("Collected {} advisories", total_advisories);
        eprintln!("Resolved {} package links", total_resolved_packages);
        eprintln!("Unresolved NVRs: {}", unresolved_nvrs);

        Ok((total_advisories, total_triples))
    }

    /// Parse RSS feed XML and extract all <item> elements as BodhiAdvisory objects.
    fn parse_rss_feed(&self, xml: &str) -> Result<Vec<BodhiAdvisory>> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut advisories = Vec::new();
        let mut in_item = false;
        let mut item_buf = String::new();
        let mut depth = 0;

        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    if e.name().as_ref() == b"item" {
                        in_item = true;
                        depth = 1;
                        item_buf.clear();
                        item_buf.push_str("<item>");
                    } else if in_item {
                        depth += 1;
                        item_buf
                            .push_str(&format!("<{}>", String::from_utf8_lossy(e.name().as_ref())));
                    }
                }
                Ok(Event::End(e)) => {
                    if in_item {
                        item_buf.push_str(&format!(
                            "</{}>",
                            String::from_utf8_lossy(e.name().as_ref())
                        ));
                        depth -= 1;
                        if depth == 0 {
                            // Item complete
                            if let Some(advisory) = BodhiAdvisory::from_rss_item(&item_buf) {
                                advisories.push(advisory);
                            }
                            in_item = false;
                        }
                    }
                }
                Ok(Event::Text(text)) if in_item => {
                    item_buf.push_str(&text.unescape().unwrap_or_default());
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("RSS XML parse error: {}", e),
                    ));
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(advisories)
    }

    fn cached_get(&self, key: &str) -> Option<String> {
        self.cache
            .as_ref()?
            .get(key)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
    }

    fn cache_put(&self, key: &str, data: &str) {
        if let Some(ref cache) = self.cache {
            cache.put(key, &serde_json::Value::String(data.to_string()));
        }
    }

    /// Parse NVR into (name, version-release) components.
    ///
    /// Example: "openssl-3.0.9-1.fc43" → ("openssl", "3.0.9-1.fc43")
    ///
    /// RPM NVR format: {name}-{version}-{release}. The name can contain hyphens
    /// (e.g., "rust-openssl-sys"), but version always starts with a digit.
    /// This function finds the first hyphen-delimited segment that starts with
    /// a digit, treating that as the beginning of version-release.
    fn parse_nvr(nvr: &str) -> Option<(String, String)> {
        let parts: Vec<&str> = nvr.split('-').collect();
        if parts.len() < 3 {
            return None; // Need at least name-version-release
        }

        // Find first segment starting with a digit (marks version start)
        for i in 1..parts.len() {
            if parts[i].chars().next()?.is_ascii_digit() {
                let name = parts[..i].join("-");
                let version_release = parts[i..].join("-");
                return Some((name, version_release));
            }
        }

        None
    }

    /// Resolve NVR to binary package URIs via SPARQL query.
    ///
    /// Queries the Fedora graph for binary RPM packages whose rpm:sourceRPM
    /// starts with the given name-version-release pattern.
    fn resolve_nvr_to_binaries(&self, name: &str, version_release: &str) -> Result<Vec<String>> {
        let graph_uri = format!(
            "https://packagegraph.github.io/graph/fedora/{}",
            self.graph_release
        );
        let nvr_prefix = format!("{}-{}", name, version_release);

        let sparql = format!(
            r#"PREFIX pkg: <{PKG}>
PREFIX rpm: <{RPM}>
SELECT ?pkg WHERE {{
  GRAPH <{graph_uri}> {{
    ?pkg rpm:sourceRPM ?srpm .
    FILTER(STRSTARTS(?srpm, "{nvr_prefix}"))
  }}
}}"#
        );

        let bindings = self.sparql.query(&sparql)?;
        Ok(bindings
            .into_iter()
            .filter_map(|b| b.get("pkg").cloned())
            .collect())
    }
}

/// Convert RFC 2822 date to ISO 8601 (xsd:dateTime format).
///
/// Input: "Tue, 15 Apr 2026 14:23:01 +0000"
/// Output: "2026-04-15T14:23:01Z"
fn parse_rfc2822_to_iso8601(rfc2822: &str) -> Result<String> {
    // Simple parser for common RFC 2822 format
    // Format: "Day, DD Mon YYYY HH:MM:SS +0000"
    let parts: Vec<&str> = rfc2822.split_whitespace().collect();
    if parts.len() < 6 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Invalid RFC 2822 date format",
        ));
    }

    let day = parts[1];
    let month = match parts[2] {
        "Jan" => "01",
        "Feb" => "02",
        "Mar" => "03",
        "Apr" => "04",
        "May" => "05",
        "Jun" => "06",
        "Jul" => "07",
        "Aug" => "08",
        "Sep" => "09",
        "Oct" => "10",
        "Nov" => "11",
        "Dec" => "12",
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Unknown month: {}", parts[2]),
            ))
        }
    };
    let year = parts[3];
    let time = parts[4];

    Ok(format!("{}-{}-{}T{}Z", year, month, day, time))
}

/// Parsed Bodhi security advisory from RSS feed.
#[derive(Debug, Clone, PartialEq)]
pub struct BodhiAdvisory {
    /// Advisory ID (e.g., "FEDORA-2026-abc123def4")
    pub id: String,
    /// NVR builds (e.g., ["openssl-3.0.9-1.fc43"])
    pub nvrs: Vec<String>,
    /// CVE IDs extracted from description
    pub cves: Vec<String>,
    /// Publication date (RFC 2822 format from RSS)
    pub date: String,
}

impl BodhiAdvisory {
    /// Parse a single RSS <item> and extract advisory metadata.
    ///
    /// Returns None if required fields are missing.
    pub fn from_rss_item(xml: &str) -> Option<Self> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut id: Option<String> = None;
        let mut title: Option<String> = None;
        let mut description: Option<String> = None;
        let mut pub_date: Option<String> = None;

        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    let tag_name = e.name();
                    match tag_name.as_ref() {
                        b"link" => {
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                let link = text.unescape().ok()?.to_string();
                                // Extract advisory ID from URL last segment
                                id = link.split('/').last().map(|s| s.to_string());
                            }
                        }
                        b"title" => {
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                title = Some(text.unescape().ok()?.to_string());
                            }
                        }
                        b"description" => {
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                description = Some(text.unescape().ok()?.to_string());
                            }
                        }
                        b"pubDate" => {
                            if let Ok(Event::Text(text)) = reader.read_event_into(&mut buf) {
                                pub_date = Some(text.unescape().ok()?.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => return None,
                _ => {}
            }
            buf.clear();
        }

        // Extract NVRs from title (space-separated)
        let nvrs: Vec<String> = title?.split_whitespace().map(|s| s.to_string()).collect();

        // Extract CVEs from description using regex
        let cves: Vec<String> = description
            .as_deref()
            .map(|desc| {
                CVE_RE
                    .find_iter(desc)
                    .map(|m| m.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default();

        Some(BodhiAdvisory {
            id: id?,
            nvrs,
            cves,
            date: pub_date?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RSS_ITEM: &str = r#"
        <item>
            <title>rust-openssl-0.10.78-1.fc43 rust-openssl-sys-0.9.114-1.fc43</title>
            <link>https://bodhi.fedoraproject.org/updates/FEDORA-2026-16a3cea414</link>
            <description>
                &lt;html&gt;&lt;body&gt;
                &lt;p&gt;This update fixes CVE-2026-41564 and CVE-2026-41676.&lt;/p&gt;
                &lt;ul&gt;
                &lt;li&gt;rust-openssl-0.10.78-1.fc43&lt;/li&gt;
                &lt;li&gt;rust-openssl-sys-0.9.114-1.fc43&lt;/li&gt;
                &lt;/ul&gt;
                &lt;/body&gt;&lt;/html&gt;
            </description>
            <pubDate>Tue, 15 Apr 2026 14:23:01 +0000</pubDate>
        </item>
    "#;

    #[test]
    fn test_parse_rss_item() {
        let advisory =
            BodhiAdvisory::from_rss_item(SAMPLE_RSS_ITEM).expect("Should parse valid RSS item");

        assert_eq!(advisory.id, "FEDORA-2026-16a3cea414");
        assert_eq!(
            advisory.nvrs,
            vec![
                "rust-openssl-0.10.78-1.fc43",
                "rust-openssl-sys-0.9.114-1.fc43"
            ]
        );
        assert_eq!(advisory.cves, vec!["CVE-2026-41564", "CVE-2026-41676"]);
        assert_eq!(advisory.date, "Tue, 15 Apr 2026 14:23:01 +0000");
    }

    #[test]
    fn test_cve_extraction_from_html() {
        let html = "This update fixes CVE-2025-1234, CVE-2025-5678, and CVE-2026-99999.";
        let cves: Vec<String> = CVE_RE
            .find_iter(html)
            .map(|m| m.as_str().to_string())
            .collect();

        assert_eq!(
            cves,
            vec!["CVE-2025-1234", "CVE-2025-5678", "CVE-2026-99999"]
        );
    }

    #[test]
    fn test_rss_item_no_cves() {
        let xml = r#"
            <item>
                <title>bash-5.2.26-6.fc43</title>
                <link>https://bodhi.fedoraproject.org/updates/FEDORA-2026-xyz789</link>
                <description>Bugfix update with no CVE references.</description>
                <pubDate>Wed, 16 Apr 2026 10:00:00 +0000</pubDate>
            </item>
        "#;

        let advisory = BodhiAdvisory::from_rss_item(xml).expect("Should parse item without CVEs");

        assert_eq!(advisory.id, "FEDORA-2026-xyz789");
        assert_eq!(advisory.nvrs, vec!["bash-5.2.26-6.fc43"]);
        assert_eq!(advisory.cves, Vec::<String>::new());
    }

    #[test]
    fn test_parse_nvr() {
        let (name, vr) =
            BodhiCollector::parse_nvr("openssl-3.0.9-1.fc43").expect("Should parse valid NVR");
        assert_eq!(name, "openssl");
        assert_eq!(vr, "3.0.9-1.fc43");

        let (name2, vr2) = BodhiCollector::parse_nvr("rust-openssl-sys-0.9.114-1.fc43")
            .expect("Should parse NVR with hyphens in name");
        assert_eq!(name2, "rust-openssl-sys");
        assert_eq!(vr2, "0.9.114-1.fc43");

        // Invalid: no version-release
        assert!(BodhiCollector::parse_nvr("openssl").is_none());
    }

    #[test]
    fn test_advisory_triple_emission() {
        use std::io::Read;
        use tempfile::NamedTempFile;

        let tmp = NamedTempFile::new().unwrap();
        let file = tmp.reopen().unwrap();
        let mut writer = NTriplesWriter::new(file);

        let advisory_uri = "https://packagegraph.github.io/d/advisory/fedora/43/FEDORA-2026-test";
        let pkg_uri =
            "https://packagegraph.github.io/d/pkg/fedora/43/x86_64/openssl/3.0.9-1.fc43.x86_64";
        let cve_uri = "https://packagegraph.github.io/d/cve/CVE-2026-1234";

        writer
            .write_triple(advisory_uri, RDF_TYPE, &format!("{SEC}SecurityAdvisory"))
            .unwrap();
        writer
            .write_literal(
                advisory_uri,
                &format!("{SEC}advisoryId"),
                "FEDORA-2026-test",
            )
            .unwrap();
        writer
            .write_triple(
                advisory_uri,
                &format!("{SEC}advisoryType"),
                &advisory_category_uri("security"),
            )
            .unwrap();
        writer
            .write_datetime(
                advisory_uri,
                &format!("{SEC}advisoryDate"),
                "2026-04-15T00:00:00Z",
            )
            .unwrap();
        writer
            .write_triple(advisory_uri, &format!("{SEC}advisoryForPackage"), pkg_uri)
            .unwrap();
        writer
            .write_triple(
                advisory_uri,
                &format!("{SEC}addressesVulnerability"),
                cve_uri,
            )
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        tmp.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("security#SecurityAdvisory"));
        assert!(content.contains("FEDORA-2026-test"));
        assert!(content.contains("advisoryForPackage"));
        assert!(content.contains("/pkg/fedora/43/x86_64/openssl/"));
        assert!(content.contains("addressesVulnerability"));
        assert!(content.contains("CVE-2026-1234"));
    }
}
