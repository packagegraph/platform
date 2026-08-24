//! Koji RPM build metadata enricher via XML-RPC.
//!
//! Queries Fuseki for RPM packages, looks up build metadata from Koji,
//! and emits BuildActivity + SLSA attestation triples.

use crate::cache::FileCache;
use crate::enricher::rate_limit;
use crate::forge::emit_dq_issue;
use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct KojiEnricher {
    sparql: Option<SparqlClient>,
    client: Client,
    cache: Option<FileCache>,
    koji_hub: String,
    pub distro: String,
    pub release: String,
    graph: Option<String>,
    pub graph_uri: Option<String>,
}

impl KojiEnricher {
    pub fn new(
        endpoint: &str,
        koji_hub: &str,
        distro: &str,
        release: &str,
        cache_dir: Option<&str>,
        auth: SparqlAuth,
        backend: SparqlBackend,
    ) -> Self {
        let sparql = Some(make_sparql_client(endpoint, &auth, backend));
        let client = crate::enricher::default_http_client();

        let cache = cache_dir.map(|dir| {
            FileCache::new(dir, "koji", 720, None) // 30 days TTL
                .expect("Failed to create cache")
        });

        // Auto-derive graph URI from distro/release if both are non-empty
        let graph = if !distro.is_empty() && !release.is_empty() {
            Some(format!(
                "https://packagegraph.github.io/graph/{}/{}",
                distro, release
            ))
        } else {
            None
        };

        Self {
            sparql,
            client,
            cache,
            koji_hub: koji_hub.to_string(),
            distro: distro.to_string(),
            release: release.to_string(),
            graph,
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph_uri(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    /// Create a standalone enricher without a SPARQL endpoint.
    /// For use with --srpm-list or when colocated with the RPM collector.
    pub fn new_standalone(
        koji_hub: &str,
        distro: &str,
        release: &str,
        cache_dir: Option<&str>,
    ) -> Self {
        Self::new_standalone_with_minio(koji_hub, distro, release, cache_dir, None)
    }

    /// Create a standalone enricher with optional Minio-backed cache.
    /// Minio sync ensures Koji API responses survive pod restarts.
    pub fn new_standalone_with_minio(
        koji_hub: &str,
        distro: &str,
        release: &str,
        cache_dir: Option<&str>,
        minio: Option<crate::cache::MinioConfig>,
    ) -> Self {
        let client = crate::enricher::default_http_client();

        let cache = cache_dir
            .map(|dir| FileCache::new(dir, "koji", 720, minio).expect("Failed to create cache"));

        let graph = if !distro.is_empty() && !release.is_empty() {
            Some(format!(
                "https://packagegraph.github.io/graph/{}/{}",
                distro, release
            ))
        } else {
            None
        };

        Self {
            sparql: None,
            client,
            cache,
            koji_hub: koji_hub.to_string(),
            distro: distro.to_string(),
            release: release.to_string(),
            graph,
            graph_uri: None,
        }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        self.enrich_with_limit(output_path, None)
    }

    pub fn enrich_with_limit(
        &self,
        output_path: &str,
        limit: Option<usize>,
    ) -> Result<(usize, usize)> {
        let sparql = self.sparql.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput,
                "enrich_with_limit requires a SPARQL endpoint. Use enrich_from_nvrs() with --srpm-list instead.")
        })?;

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        let packages = match &self.graph {
            Some(graph_uri) => {
                eprintln!("Querying graph: {}", graph_uri);
                sparql.query_packages_by_type_in_graph(&format!("{RPM}BinaryRPM"), graph_uri)?
            }
            None => sparql.query_packages_by_type(&format!("{RPM}BinaryRPM"))?,
        };
        eprintln!("Found {} RPM packages to query Koji for", packages.len());

        let mut total_builds = 0;
        let mut total_triples = 0;
        let mut seen_nvrs: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (_pkg_uri, name, version) in &packages {
            // version is "{ver}-{release}.{arch}" from repo data — strip arch suffix for Koji NVR
            let nvr = match version.rfind('.') {
                Some(dot) => format!("{}-{}", name, &version[..dot]),
                None => format!("{}-{}", name, version),
            };

            // Skip duplicates (multiple arch builds share the same NVR)
            if !seen_nvrs.insert(nvr.clone()) {
                continue;
            }

            if let Some(max) = limit {
                if seen_nvrs.len() > max {
                    break;
                }
            }

            match self.get_build(&nvr, &mut writer) {
                Ok(triples) if triples > 0 => {
                    total_builds += 1;
                    total_triples += triples;
                    eprintln!("  {} → {} triples", nvr, triples);
                }
                Ok(_) => {
                    eprintln!("  {} → not found", nvr);
                }
                Err(e) => eprintln!("  {} → error: {}", nvr, e),
            }

            rate_limit(Duration::from_millis(500));
        }

        writer.flush()?;
        Ok((total_builds, total_triples))
    }

    /// Enrich from a pre-built list of SRPM NVRs, bypassing the Fuseki discovery query.
    /// This is the entry point for --srpm-list mode and for colocated enrichment via rpm-full.
    pub fn enrich_from_nvrs(
        &self,
        nvrs: &[String],
        output_path: &str,
        limit: Option<usize>,
    ) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        eprintln!(
            "Processing {} NVRs from pre-built list (no SPARQL query)",
            nvrs.len()
        );

        let mut total_builds = 0;
        let mut total_triples = 0;
        let mut seen_nvrs: std::collections::HashSet<String> = std::collections::HashSet::new();

        for nvr in nvrs {
            if !seen_nvrs.insert(nvr.clone()) {
                continue;
            }

            if let Some(max) = limit {
                if seen_nvrs.len() > max {
                    break;
                }
            }

            match self.get_build(nvr, &mut writer) {
                Ok(triples) if triples > 0 => {
                    total_builds += 1;
                    total_triples += triples;
                    eprintln!("  {} → {} triples", nvr, triples);
                }
                Ok(_) => {
                    eprintln!("  {} → not found", nvr);
                }
                Err(e) => eprintln!("  {} → error: {}", nvr, e),
            }

            rate_limit(Duration::from_millis(500));
        }

        writer.flush()?;
        Ok((total_builds, total_triples))
    }

    fn get_build(&self, nvr: &str, writer: &mut NTriplesWriter) -> Result<usize> {
        let cache_key = format!("koji-build-{}", nvr);

        let data = match self.cached_get(&cache_key) {
            Some(d) => d,
            None => {
                // XML-RPC call: system.methodCall getBuild(nvr)
                let xml_body = format!(
                    r#"<?xml version="1.0"?>
<methodCall>
  <methodName>getBuild</methodName>
  <params>
    <param><value><string>{}</string></value></param>
  </params>
</methodCall>"#,
                    nvr
                );

                let resp = self
                    .client
                    .post(&self.koji_hub)
                    .header("Content-Type", "text/xml")
                    .body(xml_body)
                    .send()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                if !resp.status().is_success() {
                    emit_dq_issue(
                        writer,
                        "koji-enricher",
                        "getBuild",
                        nvr,
                        "koji-api-error",
                        "warning",
                    )?;
                    return Ok(0);
                }

                let body = resp
                    .text()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                let data = parse_xmlrpc_struct(&body);
                if data.is_empty() {
                    emit_dq_issue(
                        writer,
                        "koji-enricher",
                        "getBuild",
                        nvr,
                        "koji-build-not-found",
                        "info",
                    )?;
                    return Ok(0);
                }

                let json_data = serde_json::to_value(&data)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

                self.cache_put(&cache_key, &json_data);
                json_data
            }
        };

        let mut triples = self.emit_build_triples(writer, nvr, &data)?;

        // Query RPM signatures: listBuildRPMs(build_id) → queryRPMSigs(rpm_id)
        let build_id = data.get("build_id").and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        });
        if let Some(bid) = build_id {
            triples += self.query_rpm_signatures(writer, nvr, &bid)?;
        }

        Ok(triples)
    }

    /// Query Koji for GPG signing metadata on a build.
    ///
    /// Two-step API chain:
    ///   1. listBuildRPMs(build_id) → get rpm_ids for this build
    ///   2. queryRPMSigs(rpm_id)    → get sigkeys for the first binary RPM
    fn query_rpm_signatures(
        &self,
        writer: &mut NTriplesWriter,
        nvr: &str,
        build_id: &str,
    ) -> Result<usize> {
        let cache_key = format!("koji-sigs-{}", build_id);

        if let Some(cached) = self.cached_get(&cache_key) {
            return self.emit_signature_triples(writer, nvr, &cached);
        }

        // Step 1: listBuildRPMs(build_id) → find first non-src RPM
        let list_rpms_xml = format!(
            r#"<?xml version="1.0"?>
<methodCall>
  <methodName>listBuildRPMs</methodName>
  <params>
    <param><value><int>{}</int></value></param>
  </params>
</methodCall>"#,
            build_id
        );

        let resp = self
            .client
            .post(&self.koji_hub)
            .header("Content-Type", "text/xml")
            .body(list_rpms_xml)
            .send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        if !resp.status().is_success() {
            emit_dq_issue(
                writer,
                "koji-enricher",
                "listBuildRPMs",
                build_id,
                "koji-api-error",
                "warning",
            )?;
            return Ok(0);
        }

        let body = resp
            .text()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let rpms = parse_xmlrpc_array(&body);
        // Find first non-src RPM with an id
        let rpm_id = rpms
            .iter()
            .find(|r| r.get("arch").map_or(true, |a| a != "src"))
            .and_then(|r| r.get("id"))
            .cloned();

        let rpm_id = match rpm_id {
            Some(id) => id,
            None => return Ok(0),
        };

        // Step 2: queryRPMSigs(rpm_id) → get sigkeys
        rate_limit(Duration::from_millis(200));

        let query_sigs_xml = format!(
            r#"<?xml version="1.0"?>
<methodCall>
  <methodName>queryRPMSigs</methodName>
  <params>
    <param><value><int>{}</int></value></param>
  </params>
</methodCall>"#,
            rpm_id
        );

        let resp = self
            .client
            .post(&self.koji_hub)
            .header("Content-Type", "text/xml")
            .body(query_sigs_xml)
            .send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        if !resp.status().is_success() {
            emit_dq_issue(
                writer,
                "koji-enricher",
                "queryRPMSigs",
                &rpm_id,
                "koji-api-error",
                "warning",
            )?;
            return Ok(0);
        }

        let body = resp
            .text()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let sigs = parse_xmlrpc_array(&body);
        // Find first entry with a non-empty sigkey
        let sigkey = sigs
            .iter()
            .filter_map(|s| s.get("sigkey"))
            .find(|k| !k.is_empty())
            .cloned();

        let json_data = match sigkey {
            Some(key) => serde_json::json!({"sigkey": key}),
            None => return Ok(0),
        };

        self.cache_put(&cache_key, &json_data);
        self.emit_signature_triples(writer, nvr, &json_data)
    }

    /// Emit att:DigitalSignature triples for a signed RPM build.
    fn emit_signature_triples(
        &self,
        writer: &mut NTriplesWriter,
        nvr: &str,
        data: &serde_json::Value,
    ) -> Result<usize> {
        let sigkey = match data.get("sigkey").and_then(|v| v.as_str()) {
            Some(k) if !k.is_empty() => k,
            _ => return Ok(0),
        };

        let parts: Vec<&str> = nvr.rsplitn(3, '-').collect();
        let (name, version) = if parts.len() >= 3 {
            (parts[2], format!("{}-{}", parts[1], parts[0]))
        } else {
            (nvr, "unknown".to_string())
        };

        let build_uri = format!(
            "{DATA}build/{}/{}/{}/{}",
            self.distro, self.release, name, version
        );
        let sig_uri = format!("{build_uri}/sig");

        writer.write_triple(&build_uri, &format!("{ATT}hasSignature"), &sig_uri)?;
        writer.write_triple(&sig_uri, RDF_TYPE, &format!("{ATT}DigitalSignature"))?;
        writer.write_triple(
            &sig_uri,
            &format!("{ATT}signatureMethod"),
            &format!("{ATT}GPG"),
        )?;
        writer.write_literal(&sig_uri, &format!("{ATT}signingKeyFingerprint"), sigkey)?;
        // Koji only stores verified signatures
        writer.write_literal(&sig_uri, &format!("{ATT}signatureStatus"), "verified")?;

        Ok(5)
    }

    fn emit_build_triples(
        &self,
        writer: &mut NTriplesWriter,
        nvr: &str,
        data: &serde_json::Value,
    ) -> Result<usize> {
        let mut triples = 0;

        // Extract fields from Koji build data
        let owner = data
            .get("owner_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let start_time = data.get("start_time").and_then(|v| v.as_str());
        let end_time = data.get("completion_time").and_then(|v| v.as_str());

        // Parse NVR to extract name and version
        let parts: Vec<&str> = nvr.rsplitn(3, '-').collect();
        let (name, version) = if parts.len() >= 3 {
            (parts[2], format!("{}-{}", parts[1], parts[0]))
        } else {
            (nvr, "unknown".to_string())
        };

        let build_uri = format!(
            "{DATA}build/{}/{}/{}/{}",
            self.distro, self.release, name, version
        );

        // BuildActivity with pkg: namespace properties (per core.ttl)
        writer.write_triple(&build_uri, RDF_TYPE, &format!("{PKG}BuildActivity"))?;
        writer.write_literal(&build_uri, &format!("{PKG}packageName"), name)?;
        triples += 2;

        // Builder node (slsa:Builder → prov:Agent) linked via slsa:builtBy
        let koji_builder_uri = builder_uri("https://koji.fedoraproject.org");
        writer.write_triple(&koji_builder_uri, RDF_TYPE, &format!("{SLSA}Builder"))?;
        writer.write_literal(
            &koji_builder_uri,
            &format!("{SLSA}builderId"),
            "https://koji.fedoraproject.org",
        )?;
        writer.write_triple(&build_uri, &format!("{SLSA}builtBy"), &koji_builder_uri)?;
        triples += 3;

        // Owner as prov:wasAttributedTo agent node
        let owner_uri = format!("{DATA}agent/koji/{}", owner);
        writer.write_triple(&owner_uri, RDF_TYPE, &format!("{PROV}Agent"))?;
        writer.write_literal(&owner_uri, RDFS_LABEL, owner)?;
        writer.write_triple(&build_uri, &format!("{PROV}wasAttributedTo"), &owner_uri)?;
        triples += 3;

        // Timestamps use pkg: namespace (per core.ttl on pkg:BuildActivity)
        if let Some(start) = start_time {
            writer.write_datetime(&build_uri, &format!("{PKG}activityStartTime"), start)?;
            triples += 1;
        }

        if let Some(end) = end_time {
            writer.write_datetime(&build_uri, &format!("{PKG}activityEndTime"), end)?;
            triples += 1;
        }

        Ok(triples)
    }

    fn cached_get(&self, key: &str) -> Option<serde_json::Value> {
        self.cache.as_ref()?.get(key)
    }

    fn cache_put(&self, key: &str, data: &serde_json::Value) {
        if let Some(ref cache) = self.cache {
            cache.put(key, data);
        }
    }
}

/// Parse an XML-RPC response containing a single top-level struct into a HashMap.
///
/// Handles: `<params><param><value><struct>...</struct></value></param></params>`
/// Only captures members at the first struct depth (ignores nested structs).
fn parse_xmlrpc_struct(xml: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut struct_depth: i32 = 0;
    let mut in_member = false;
    let mut in_name = false;
    let mut in_value_child = false;
    let mut current_name = String::new();

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"struct" => {
                    struct_depth += 1;
                }
                b"member" if struct_depth == 1 => in_member = true,
                b"name" if in_member && struct_depth == 1 => in_name = true,
                b"string" | b"int" | b"i4" | b"double" if in_member && struct_depth == 1 => {
                    in_value_child = true;
                }
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"struct" => {
                    struct_depth -= 1;
                }
                b"member" if struct_depth == 1 => {
                    in_member = false;
                    current_name.clear();
                }
                b"name" => in_name = false,
                b"string" | b"int" | b"i4" | b"double" => in_value_child = false,
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if in_name && struct_depth == 1 {
                    current_name = e.unescape().unwrap_or_default().to_string();
                } else if in_value_child && struct_depth == 1 && !current_name.is_empty() {
                    let val = e.unescape().unwrap_or_default().to_string();
                    result.insert(current_name.clone(), val);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    result
}

/// Parse an XML-RPC response containing an array of structs.
///
/// Handles: `<params><param><value><array><data><value><struct>...</struct></value>...</data></array></value></param></params>`
/// Returns a Vec of HashMaps, one per struct in the array.
fn parse_xmlrpc_array(xml: &str) -> Vec<HashMap<String, String>> {
    let mut results: Vec<HashMap<String, String>> = Vec::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_array = false;
    let mut struct_depth: i32 = 0;
    let mut in_member = false;
    let mut in_name = false;
    let mut in_value_child = false;
    let mut current_name = String::new();
    let mut current_struct: HashMap<String, String> = HashMap::new();

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"array" => in_array = true,
                b"struct" if in_array => {
                    struct_depth += 1;
                    if struct_depth == 1 {
                        current_struct.clear();
                    }
                }
                b"member" if struct_depth == 1 => in_member = true,
                b"name" if in_member && struct_depth == 1 => in_name = true,
                b"string" | b"int" | b"i4" | b"double" if in_member && struct_depth == 1 => {
                    in_value_child = true;
                }
                _ => {}
            },
            Ok(Event::End(e)) => {
                match e.name().as_ref() {
                    b"array" => in_array = false,
                    b"struct" if in_array => {
                        if struct_depth == 1 && !current_struct.is_empty() {
                            results.push(current_struct.clone());
                        }
                        struct_depth -= 1;
                    }
                    b"member" if struct_depth == 1 => {
                        in_member = false;
                        current_name.clear();
                    }
                    b"name" => in_name = false,
                    b"string" | b"int" | b"i4" | b"double"
                        if in_value_child && struct_depth == 1 =>
                    {
                        // Empty element (e.g. <string></string>) — store empty string
                        if !current_name.is_empty() {
                            current_struct
                                .entry(current_name.clone())
                                .or_insert_with(String::new);
                        }
                        in_value_child = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                if in_name && struct_depth == 1 {
                    current_name = e.unescape().unwrap_or_default().to_string();
                } else if in_value_child && struct_depth == 1 && !current_name.is_empty() {
                    let val = e.unescape().unwrap_or_default().to_string();
                    current_struct.insert(current_name.clone(), val);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_xmlrpc_struct() {
        let xml = r#"<?xml version="1.0"?>
<methodResponse>
  <params>
    <param>
      <value>
        <struct>
          <member>
            <name>owner_name</name>
            <value><string>releng</string></value>
          </member>
          <member>
            <name>build_id</name>
            <value><int>12345</int></value>
          </member>
          <member>
            <name>start_time</name>
            <value><string>2024-01-15 10:30:00</string></value>
          </member>
        </struct>
      </value>
    </param>
  </params>
</methodResponse>"#;

        let result = parse_xmlrpc_struct(xml);
        assert_eq!(result.get("owner_name"), Some(&"releng".to_string()));
        assert_eq!(result.get("build_id"), Some(&"12345".to_string()));
        assert_eq!(
            result.get("start_time"),
            Some(&"2024-01-15 10:30:00".to_string())
        );
    }

    #[test]
    fn test_parse_xmlrpc_struct_ignores_nested() {
        // getBuild responses can have nested structs (e.g. extra.source).
        // parse_xmlrpc_struct must capture top-level members only.
        let xml = r#"<?xml version="1.0"?>
<methodResponse>
  <params><param><value>
    <struct>
      <member>
        <name>owner_name</name>
        <value><string>dbelyavs</string></value>
      </member>
      <member>
        <name>extra</name>
        <value><struct>
          <member>
            <name>source</name>
            <value><struct>
              <member><name>url</name><value><string>git+https://example.com</string></value></member>
            </struct></value>
          </member>
        </struct></value>
      </member>
      <member>
        <name>build_id</name>
        <value><int>99999</int></value>
      </member>
    </struct>
  </value></param></params>
</methodResponse>"#;

        let result = parse_xmlrpc_struct(xml);
        assert_eq!(result.get("owner_name"), Some(&"dbelyavs".to_string()));
        assert_eq!(result.get("build_id"), Some(&"99999".to_string()));
        // Nested "url" must NOT leak into the top-level result
        assert_eq!(result.get("url"), None);
    }

    #[test]
    fn test_parse_xmlrpc_array() {
        // queryRPMSigs returns an array of structs with rpm_id, sighash, sigkey
        let xml = r#"<?xml version="1.0"?>
<methodResponse>
  <params><param><value>
    <array><data>
      <value><struct>
        <member><name>rpm_id</name><value><int>41803416</int></value></member>
        <member><name>sighash</name><value><string>54316aebb669102ab9b1490b6aea1183</string></value></member>
        <member><name>sigkey</name><value><string></string></value></member>
      </struct></value>
      <value><struct>
        <member><name>rpm_id</name><value><int>41803416</int></value></member>
        <member><name>sighash</name><value><string>88aa80c6c3e02a13d8d3c5f5d49af752</string></value></member>
        <member><name>sigkey</name><value><string>e99d6ad1</string></value></member>
      </struct></value>
    </data></array>
  </value></param></params>
</methodResponse>"#;

        let result = parse_xmlrpc_array(xml);
        assert_eq!(result.len(), 2, "Should parse 2 structs from the array");
        assert_eq!(result[0].get("sigkey"), Some(&"".to_string()));
        assert_eq!(result[1].get("sigkey"), Some(&"e99d6ad1".to_string()));
        assert_eq!(result[1].get("rpm_id"), Some(&"41803416".to_string()));
    }

    #[test]
    fn test_parse_xmlrpc_array_empty() {
        let xml = r#"<?xml version="1.0"?>
<methodResponse>
  <params><param><value>
    <array><data></data></array>
  </value></param></params>
</methodResponse>"#;

        let result = parse_xmlrpc_array(xml);
        assert!(result.is_empty());
    }

    #[test]
    fn test_emit_build_triples() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let enricher = KojiEnricher::new(
            &server.url(),
            "https://koji.fedoraproject.org/kojihub",
            "fedora",
            "41",
            None,
            None,
            SparqlBackend::Fuseki,
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let data = serde_json::json!({
            "owner_name": "releng",
            "start_time": "2024-01-15T10:30:00Z",
            "completion_time": "2024-01-15T10:45:00Z",
            "build_id": 12345
        });

        let triples = enricher
            .emit_build_triples(&mut writer, "gcc-14.0.1-1.fc41", &data)
            .unwrap();
        writer.flush().unwrap();

        assert!(
            triples >= 8,
            "Should emit at least 8 triples (build + builder + owner + timestamps)"
        );

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("core#BuildActivity"),
            "Should have BuildActivity type"
        );
        assert!(content.contains("slsa#Builder"), "Should have Builder type");
        assert!(content.contains("slsa#builderId"), "Should have builder ID");
        assert!(
            content.contains("slsa#builtBy"),
            "Should link build to builder"
        );
        assert!(
            content.contains("prov#Agent"),
            "Should have owner as prov:Agent"
        );
        assert!(
            content.contains("prov#wasAttributedTo"),
            "Should attribute build to owner"
        );
        assert!(
            content.contains("\"releng\""),
            "Should have releng owner label"
        );
        assert!(
            content.contains("core#activityStartTime"),
            "Should use pkg:activityStartTime"
        );
        assert!(
            content.contains("core#activityEndTime"),
            "Should use pkg:activityEndTime"
        );
    }

    #[test]
    fn test_emit_signature_triples() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_body(r#"{"results": {"bindings": []}}"#)
            .create();

        let enricher = KojiEnricher::new(
            &server.url(),
            "https://koji.fedoraproject.org/kojihub",
            "fedora",
            "41",
            None,
            None,
            SparqlBackend::Fuseki,
        );

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let data = serde_json::json!({"sigkey": "e99d6ad1"});

        let triples = enricher
            .emit_signature_triples(&mut writer, "openssl-3.2.4-1.fc41", &data)
            .unwrap();
        writer.flush().unwrap();

        assert_eq!(triples, 5, "Should emit 5 signature triples");

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(
            content.contains("attestation#hasSignature"),
            "Should link build to signature"
        );
        assert!(
            content.contains("attestation#DigitalSignature"),
            "Should type as DigitalSignature"
        );
        assert!(
            content.contains("attestation#GPG"),
            "Should reference att:GPG named individual"
        );
        assert!(
            content.contains("attestation#signingKeyFingerprint"),
            "Should emit key fingerprint"
        );
        assert!(
            content.contains("\"e99d6ad1\""),
            "Should have the sigkey value"
        );
        assert!(
            content.contains("attestation#signatureStatus"),
            "Should emit signature status"
        );
        assert!(content.contains("\"verified\""), "Should be verified");
    }

    #[test]
    fn test_new_standalone_no_sparql() {
        // new_standalone should create a KojiEnricher without requiring a SPARQL endpoint
        let enricher = KojiEnricher::new_standalone(
            "https://koji.fedoraproject.org/kojihub",
            "fedora",
            "43",
            None,
        );

        assert_eq!(enricher.koji_hub, "https://koji.fedoraproject.org/kojihub");
        assert_eq!(enricher.distro, "fedora");
        assert_eq!(enricher.release, "43");
        assert!(
            enricher.sparql.is_none(),
            "Standalone enricher should not have SPARQL client"
        );
    }

    #[test]
    fn test_enrich_from_nvrs_processes_list() {
        // enrich_from_nvrs should iterate the provided NVR list without querying SPARQL
        let mut server = mockito::Server::new();

        // Mock Koji getBuild — return a valid build response
        let _koji_mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(
                r#"<?xml version="1.0"?>
<methodResponse>
  <params><param><value><struct>
    <member><name>owner_name</name><value><string>testuser</string></value></member>
    <member><name>build_id</name><value><int>999</int></value></member>
    <member><name>start_time</name><value><string>2026-04-01 10:00:00</string></value></member>
    <member><name>completion_time</name><value><string>2026-04-01 10:15:00</string></value></member>
  </struct></value></param></params>
</methodResponse>"#,
            )
            .expect_at_least(1)
            .create();

        let enricher = KojiEnricher::new_standalone(&server.url(), "fedora", "43", None);

        let nvrs = vec!["openssl-3.2.1-1.fc43".to_string()];
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_str().unwrap().to_string();

        let result = enricher.enrich_from_nvrs(&nvrs, &path, None);
        assert!(
            result.is_ok(),
            "enrich_from_nvrs should succeed: {:?}",
            result
        );

        let (builds, triples) = result.unwrap();
        assert_eq!(builds, 1, "Should process exactly 1 build");
        assert!(triples > 0, "Should emit at least some triples");
    }
}
