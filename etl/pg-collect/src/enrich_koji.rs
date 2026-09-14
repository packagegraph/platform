//! Koji RPM build metadata enricher via XML-RPC.
//!
//! Queries Fuseki for RPM packages, looks up build metadata from Koji,
//! and emits BuildActivity + SLSA attestation triples.

use crate::cache::FileCache;
use crate::forge::emit_dq_issue;
use crate::http_transport::HttpTransport;
use crate::ntriples::NTriplesWriter;
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::*;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Result, Write};

pub struct KojiEnricher {
    sparql: Option<SparqlClient>,
    transport: HttpTransport,
    cache: Option<FileCache>,
    koji_hub: String,
    pub distro: String,
    pub release: String,
    graph: Option<String>,
    pub graph_uri: Option<String>,
    /// Set when ANY RPC in this item's chain was inconclusive, so the whole
    /// per-NVR fragment is Retryable -- a build whose getBuild succeeded but
    /// whose queryRPMSigs faulted must not be checkpointed as unsigned.
    item_inconclusive: std::cell::Cell<bool>,
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
        let cache = cache_dir.map(|dir| {
            // Nominally 30 days. NOTE: the TTL is only enforced for local
            // entries — FileCache::read_minio does not check age and rewrites
            // the local file, so a Minio-backed entry never expires. Tracked
            // separately; do not rely on expiry to retire a bad entry, bump
            // KOJI_RPC_CACHE_VERSION instead.
            FileCache::new(dir, "koji", 720, None)
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
            transport: HttpTransport::new(),
            cache,
            koji_hub: koji_hub.to_string(),
            distro: distro.to_string(),
            release: release.to_string(),
            graph,
            graph_uri: None,
            item_inconclusive: std::cell::Cell::new(false),
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
            transport: HttpTransport::new(),
            cache,
            koji_hub: koji_hub.to_string(),
            distro: distro.to_string(),
            release: release.to_string(),
            graph,
            graph_uri: None,
            item_inconclusive: std::cell::Cell::new(false),
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

        }

        writer.flush()?;
        Ok((total_builds, total_triples))
    }

    fn get_build<W: Write>(&self, nvr: &str, writer: &mut NTriplesWriter<W>) -> Result<usize> {
        let cache_key = RpcCacheKey {
            hub: &self.koji_hub,
            method: "getBuild",
            argument: nvr,
        }
        .to_key();

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

                let resp = self.transport.post(
                    &self.koji_hub,
                    &[("Content-Type", "text/xml")],
                    xml_body.into_bytes(),
                );

                let resp = match resp {
                    Ok(r) => r,
                    Err(_) => {
                        emit_dq_issue(
                            writer,
                            "koji-enricher",
                            "getBuild",
                            nvr,
                            "koji-api-error",
                            "warning",
                        )?;
                        self.item_inconclusive.set(true);
                        return Ok(0);
                    }
                };

                let body = match String::from_utf8(resp.bytes) {
                    Ok(b) => b,
                    Err(_) => {
                        // An undecodable body is a transport-level problem,
                        // not an answer about this build.
                        emit_dq_issue(
                            writer,
                            "koji-enricher",
                            "getBuild",
                            nvr,
                            "koji-api-error",
                            "warning",
                        )?;
                        self.item_inconclusive.set(true);
                        return Ok(0);
                    }
                };

                let data = match parse_build_response(&body) {
                    KojiRpcResult::ValidNonempty(d) => d,
                    KojiRpcResult::ValidEmpty => {
                        // A real answer: Koji has no such build. Checkpointable.
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
                    KojiRpcResult::ApiFault(d) | KojiRpcResult::Malformed(d) => {
                        eprintln!("  {} → koji response inconclusive: {}", nvr, d);
                        emit_dq_issue(
                            writer,
                            "koji-enricher",
                            "getBuild",
                            nvr,
                            "koji-api-error",
                            "warning",
                        )?;
                        self.item_inconclusive.set(true);
                        return Ok(0);
                    }
                };

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
    fn query_rpm_signatures<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
        nvr: &str,
        build_id: &str,
    ) -> Result<usize> {
        // The hub is part of the identity because Koji build ids are
        // hub-relative: build 1 on one hub is not build 1 on another.
        let cache_key = RpcCacheKey {
            hub: &self.koji_hub,
            method: "queryRPMSigs",
            argument: build_id,
        }
        .to_key();

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

        let resp = self.transport.post(
            &self.koji_hub,
            &[("Content-Type", "text/xml")],
            list_rpms_xml.into_bytes(),
        );

        let resp = match resp {
            Ok(r) => r,
            Err(_) => {
                emit_dq_issue(
                    writer,
                    "koji-enricher",
                    "listBuildRPMs",
                    build_id,
                    "koji-api-error",
                    "warning",
                )?;
                self.item_inconclusive.set(true);
                return Ok(0);
            }
        };

        let body = match String::from_utf8(resp.bytes) {
            Ok(b) => b,
            Err(_) => {
                emit_dq_issue(
                    writer,
                    "koji-enricher",
                    "listBuildRPMs",
                    build_id,
                    "koji-api-error",
                    "warning",
                )?;
                self.item_inconclusive.set(true);
                return Ok(0);
            }
        };

        let rpms = match parse_array_response(&body, &LIST_BUILD_RPMS) {
            KojiRpcResult::ValidNonempty(v) => v,
            // A real answer: this build genuinely has no binary RPMs.
            KojiRpcResult::ValidEmpty => return Ok(0),
            KojiRpcResult::ApiFault(d) | KojiRpcResult::Malformed(d) => {
                eprintln!("  {} → listBuildRPMs inconclusive: {}", nvr, d);
                emit_dq_issue(
                    writer,
                    "koji-enricher",
                    "listBuildRPMs",
                    build_id,
                    "koji-api-error",
                    "warning",
                )?;
                self.item_inconclusive.set(true);
                return Ok(0);
            }
        };
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

        let resp = self.transport.post(
            &self.koji_hub,
            &[("Content-Type", "text/xml")],
            query_sigs_xml.into_bytes(),
        );

        let resp = match resp {
            Ok(r) => r,
            Err(_) => {
                emit_dq_issue(
                    writer,
                    "koji-enricher",
                    "queryRPMSigs",
                    &rpm_id,
                    "koji-api-error",
                    "warning",
                )?;
                self.item_inconclusive.set(true);
                return Ok(0);
            }
        };

        let body = match String::from_utf8(resp.bytes) {
            Ok(b) => b,
            Err(_) => {
                emit_dq_issue(
                    writer,
                    "koji-enricher",
                    "queryRPMSigs",
                    &rpm_id,
                    "koji-api-error",
                    "warning",
                )?;
                self.item_inconclusive.set(true);
                return Ok(0);
            }
        };

        let sigs = match parse_array_response(&body, &QUERY_RPM_SIGS) {
            KojiRpcResult::ValidNonempty(v) => v,
            // A real answer: this RPM has no signature rows.
            KojiRpcResult::ValidEmpty => return Ok(0),
            KojiRpcResult::ApiFault(d) | KojiRpcResult::Malformed(d) => {
                eprintln!("  {} → queryRPMSigs inconclusive: {}", nvr, d);
                emit_dq_issue(
                    writer,
                    "koji-enricher",
                    "queryRPMSigs",
                    &rpm_id,
                    "koji-api-error",
                    "warning",
                )?;
                self.item_inconclusive.set(true);
                return Ok(0);
            }
        };
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
    fn emit_signature_triples<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
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

    fn emit_build_triples<W: Write>(
        &self,
        writer: &mut NTriplesWriter<W>,
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

/// Bump this for any change to this stage's emitted triples, including
/// changes to shared serialization or ontology helpers it calls. A stale
/// checkpoint fragment is indistinguishable from a correct one.
pub const KOJI_SCHEMA_VERSION: &str = "koji-v1";

/// Namespaces the 30-day `koji-build-*` / `koji-sigs-*` source-cache keys.
///
/// Distinct from `KOJI_SCHEMA_VERSION`, which versions *emitted triples*; this
/// versions *parsed RPC responses*. Bump it whenever a parser change makes
/// previously-stored entries untrustworthy -- as this task's does, since the
/// old lossy parser persisted partial maps built from truncated bodies.
///
/// It is also a field of the Koji stage's `CanonicalContext` (Task 7), so a
/// bump invalidates output checkpoints as well as source-cache entries. Those
/// are two different caches and a bump has to reach both: the source cache
/// sits *behind* the output checkpoint, so an interrupted generation would
/// otherwise replay fragments the old parser produced without ever consulting
/// the source cache at all.
pub const KOJI_RPC_CACHE_VERSION: &str = "v2";

/// Outcome of one Koji XML-RPC call.
///
/// The lossy parsers this type replaced returned an empty collection for
/// a fault, a malformed body, and a legitimately empty result alike. Only
/// conclusive results may contribute to a checkpointable item.
///
/// `Debug` is required, not cosmetic: the tests below print the unexpected
/// variant with `{other:?}` when an assertion fails, and without it they do
/// not compile.
#[derive(Debug)]
pub enum KojiRpcResult<T> {
    ValidNonempty(T),
    ValidEmpty,
    ApiFault(String),
    Malformed(String),
}

impl<T> KojiRpcResult<T> {
    pub fn is_conclusive(&self) -> bool {
        matches!(self, KojiRpcResult::ValidNonempty(_) | KojiRpcResult::ValidEmpty)
    }
}

/// A parsed XML-RPC value, restricted to the shapes Koji actually returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcValue {
    /// Top-level scalar members only, matching what the callers read. A
    /// nested container inside a member is grammar-legal and gets validated,
    /// but is not stored -- the same fields the enricher read before.
    Struct(HashMap<String, String>),
    /// Koji's arrays are arrays of structs (listBuildRPMs, queryRPMSigs).
    Array(Vec<HashMap<String, String>>),
    Nil,
}

/// One XML token. Tokenizing first, then doing recursive descent over a
/// slice, is far easier to get right (and to read) than threading quick-xml's
/// borrow-checked buffer through a recursive parser.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Open(String),
    Close(String),
    Text(String),
}

/// A self-closing `<x/>` becomes `Open(x), Close(x)`, so the grammar below
/// never has to special-case it -- forgetting that is how `<fault/>` slipped
/// through an earlier revision of this code.
fn tokenize(xml: &str) -> std::result::Result<Vec<Tok>, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;

    let mut toks = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                toks.push(Tok::Open(String::from_utf8_lossy(e.name().as_ref()).into_owned()))
            }
            Ok(Event::End(e)) => {
                toks.push(Tok::Close(String::from_utf8_lossy(e.name().as_ref()).into_owned()))
            }
            Ok(Event::Empty(e)) => {
                let n = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                toks.push(Tok::Open(n.clone()));
                toks.push(Tok::Close(n));
            }
            Ok(Event::Text(e)) => match e.unescape() {
                Ok(t) => {
                    let t = t.to_string();
                    if !t.is_empty() {
                        toks.push(Tok::Text(t));
                    }
                }
                // A body we cannot decode is not a body we may trust.
                Err(err) => return Err(format!("undecodable text: {err}")),
            },
            // CDATA is text that is raw by definition -- no unescaping. It
            // must be tokenized, not dropped: a `<string>` delivered as CDATA
            // would otherwise parse to "", and an empty field is exactly the
            // shape that gets checkpointed as missing metadata.
            Ok(Event::CData(e)) => {
                let t = String::from_utf8_lossy(&e).into_owned();
                if !t.is_empty() {
                    toks.push(Tok::Text(t));
                }
            }
            Ok(Event::Eof) => break,
            // Truncated or malformed input. This must never reach the caller
            // as an empty result.
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(toks)
}

/// Cursor over the token slice.
struct Cur<'a> {
    t: &'a [Tok],
    i: usize,
}

impl<'a> Cur<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.t.get(self.i)
    }
    fn open(&mut self, name: &str) -> std::result::Result<(), String> {
        match self.peek() {
            Some(Tok::Open(n)) if n == name => {
                self.i += 1;
                Ok(())
            }
            other => Err(format!("expected <{name}>, found {other:?}")),
        }
    }
    fn close(&mut self, name: &str) -> std::result::Result<(), String> {
        match self.peek() {
            Some(Tok::Close(n)) if n == name => {
                self.i += 1;
                Ok(())
            }
            other => Err(format!("expected </{name}>, found {other:?}")),
        }
    }
    fn at_open(&self, name: &str) -> bool {
        matches!(self.peek(), Some(Tok::Open(n)) if n == name)
    }
    /// Consumes every adjacent text run and concatenates them. A mixed body
    /// such as `a<![CDATA[b]]>c` tokenizes to three `Text`s; taking only the
    /// first would silently truncate the value to "a".
    fn text(&mut self) -> String {
        let mut out = String::new();
        while let Some(Tok::Text(s)) = self.peek() {
            out.push_str(s);
            self.i += 1;
        }
        out
    }
}

const SCALARS: [&str; 7] =
    ["int", "i4", "string", "double", "boolean", "dateTime.iso8601", "base64"];

/// `<value>` content. Returns `None` for a container we validate but do not
/// store (a nested struct/array inside a member), preserving exactly the
/// fields the enricher read before this change.
fn parse_value(c: &mut Cur) -> std::result::Result<Option<String>, String> {
    let name = match c.peek() {
        Some(Tok::Open(n)) => n.clone(),
        // <value>text</value> with no type element is a string in XML-RPC.
        Some(Tok::Text(_)) => return Ok(Some(c.text())),
        // <value></value> is the empty string, not a grammar violation.
        // Rejecting it would make a legitimate response retryable forever.
        Some(Tok::Close(n)) if n == "value" => return Ok(Some(String::new())),
        other => return Err(format!("expected a value, found {other:?}")),
    };

    if SCALARS.contains(&name.as_str()) {
        c.open(&name)?;
        let v = c.text();
        c.close(&name)?;
        return Ok(Some(v));
    }
    match name.as_str() {
        "nil" => {
            c.open("nil")?;
            c.close("nil")?;
            Ok(None)
        }
        "struct" => {
            parse_struct(c)?;
            Ok(None)
        }
        "array" => {
            parse_array(c)?;
            Ok(None)
        }
        other => Err(format!("unexpected value type <{other}>")),
    }
}

fn parse_struct(c: &mut Cur) -> std::result::Result<HashMap<String, String>, String> {
    c.open("struct")?;
    let mut out = HashMap::new();
    while c.at_open("member") {
        c.open("member")?;
        c.open("name")?;
        let key = c.text();
        c.close("name")?;
        c.open("value")?;
        let val = parse_value(c)?;
        c.close("value")?;
        c.close("member")?;
        if let Some(v) = val {
            out.insert(key, v);
        }
    }
    c.close("struct")?;
    Ok(out)
}

/// Grammar-level array: `<array><data>` holding any sequence of values.
///
/// Element *type* is the caller's business, not the grammar's. Each element
/// yields `Some(map)` when it was a struct and `None` otherwise; the
/// top-level payload in `parse_response` rejects the `None`s, while a nested
/// array reached through `parse_value` accepts them. Folding the
/// array-of-records rule in here would make a perfectly valid nested array of
/// scalars -- inside a member we do not even store -- turn the whole response
/// retryable.
fn parse_array(c: &mut Cur) -> std::result::Result<Vec<Option<HashMap<String, String>>>, String> {
    c.open("array")?;
    // Exactly one <data>. `<array><bogus/></array>` fails here rather than
    // yielding an empty vec.
    c.open("data")?;
    let mut out = Vec::new();
    while c.at_open("value") {
        c.open("value")?;
        if c.at_open("struct") {
            out.push(Some(parse_struct(c)?));
        } else {
            // Validated, but not a record.
            parse_value(c)?;
            out.push(None);
        }
        c.close("value")?;
    }
    c.close("data")?;
    c.close("array")?;
    Ok(out)
}

/// Parse a whole methodResponse. Any grammar violation is `Malformed`; a
/// `<fault>` is `ApiFault`. There is no path from a structural problem to an
/// empty-but-valid result.
pub fn parse_response(xml: &str) -> KojiRpcResult<RpcValue> {
    let toks = match tokenize(xml) {
        Ok(t) => t,
        Err(e) => return KojiRpcResult::Malformed(e),
    };
    let mut c = Cur { t: &toks, i: 0 };

    // The response must BE the root, not merely appear inside some other
    // document (e.g. an HTML error page that embeds it).
    if let Err(e) = c.open("methodResponse") {
        return KojiRpcResult::Malformed(format!("root: {e}"));
    }

    // The fault subtree is parsed, not scanned: "the whole document is
    // validated" has to be true of this branch too, and reading faultString
    // out of a parsed struct beats hunting for a Text token that happens to
    // equal "faultString".
    if c.at_open("fault") {
        return match parse_fault(&mut c) {
            Ok(d) => KojiRpcResult::ApiFault(d),
            // Either way the item is inconclusive; `Malformed` just names the
            // real problem instead of blaming the hub for a fault we could
            // not read.
            Err(e) => KojiRpcResult::Malformed(format!("fault: {e}")),
        };
    }

    let parsed = (|| -> std::result::Result<RpcValue, String> {
        c.open("params")?;
        c.open("param")?;
        c.open("value")?;
        let v = match c.peek() {
            Some(Tok::Open(n)) if n == "struct" => RpcValue::Struct(parse_struct(&mut c)?),
            Some(Tok::Open(n)) if n == "array" => {
                // Here -- and only here -- Koji's arrays must be arrays of
                // records. A scalar element is a schema violation, not an
                // empty array.
                let mut recs = Vec::new();
                for (i, e) in parse_array(&mut c)?.into_iter().enumerate() {
                    match e {
                        Some(m) => recs.push(m),
                        None => return Err(format!("array element {i} is not a <struct>")),
                    }
                }
                RpcValue::Array(recs)
            }
            Some(Tok::Open(n)) if n == "nil" => {
                c.open("nil")?;
                c.close("nil")?;
                RpcValue::Nil
            }
            other => return Err(format!("payload is {other:?}, expected struct, array or nil")),
        };
        c.close("value")?;
        c.close("param")?;
        // A second <param> would be a second payload.
        if c.at_open("param") {
            return Err("more than one <param> payload".into());
        }
        c.close("params")?;
        c.close("methodResponse")?;
        if c.peek().is_some() {
            return Err(format!("trailing content after </methodResponse>: {:?}", c.peek()));
        }
        Ok(v)
    })();

    match parsed {
        Ok(v) => KojiRpcResult::ValidNonempty(v),
        Err(e) => KojiRpcResult::Malformed(e),
    }
}

/// `<fault><value><struct>faultCode, faultString</struct></value></fault>`,
/// through to the end of the document. `<fault/>` carries no detail but is
/// still unambiguously a fault.
fn parse_fault(c: &mut Cur) -> std::result::Result<String, String> {
    c.open("fault")?;
    let detail = if c.at_open("value") {
        c.open("value")?;
        let m = parse_struct(c)?;
        c.close("value")?;
        m.get("faultString")
            .cloned()
            .unwrap_or_else(|| "unknown fault".to_string())
    } else {
        "unknown fault".to_string()
    };
    c.close("fault")?;
    c.close("methodResponse")?;
    if c.peek().is_some() {
        return Err(format!(
            "trailing content after </methodResponse>: {:?}",
            c.peek()
        ));
    }
    Ok(detail)
}

/// A Koji identifier must be a non-empty run of digits. `build_id` and `id`
/// arrive as `<int>` or `<string>` depending on hub version, and both are
/// interpolated straight into the next call's `<int>` argument, so the check
/// belongs on the value rather than on the XML type that carried it.
fn require_numeric_id(
    r: &HashMap<String, String>,
    field: &str,
) -> std::result::Result<(), String> {
    match r.get(field) {
        Some(v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => Ok(()),
        Some(v) => Err(format!("{field} is {v:?}, not a numeric id")),
        None => Err(format!("record has no {field}")),
    }
}

/// The record contract for one array-returning RPC. Naming a schema is
/// mandatory at every call site: a parser that defaults to "no record
/// validation" is how a `<struct/>` element becomes a conclusive empty.
pub struct ArraySchema {
    pub method: &'static str,
    pub validate: fn(&HashMap<String, String>) -> std::result::Result<(), String>,
}

fn validate_rpm_record(r: &HashMap<String, String>) -> std::result::Result<(), String> {
    require_numeric_id(r, "id")?;
    match r.get("arch") {
        Some(a) if !a.is_empty() => Ok(()),
        // Absent arch is not neutral: `:368` treats it as non-src and picks
        // the record.
        _ => Err("rpm record has no arch; the src filter would misread it".into()),
    }
}

fn validate_sig_record(r: &HashMap<String, String>) -> std::result::Result<(), String> {
    // Presence, not non-emptiness -- an empty sigkey is Koji's own way of
    // saying "unsigned", and that is a real answer worth checkpointing.
    if r.contains_key("sigkey") {
        Ok(())
    } else {
        Err("signature record has no sigkey; absence would read as unsigned".into())
    }
}

pub const LIST_BUILD_RPMS: ArraySchema = ArraySchema {
    method: "listBuildRPMs",
    validate: validate_rpm_record,
};

pub const QUERY_RPM_SIGS: ArraySchema = ArraySchema {
    method: "queryRPMSigs",
    validate: validate_sig_record,
};

/// `getBuild`: a struct, or nil.
///
/// Koji's own `getBuild` returns `None` -- serialized by the hub as `<nil/>`
/// -- when no such build exists, and returns a map *containing* `build_id`
/// when one does. So `<nil/>` is the conclusive no-build answer, and **every**
/// struct must carry a numeric `build_id`, including an empty one.
///
/// Treating `<struct/>` as "not found" is the old code's behaviour
/// (`data.is_empty()` at `enrich_koji.rs:267`) and it is what let a fault or a
/// truncated body read as conclusive absence. Koji does not produce it; a hub
/// that did is telling us something we cannot act on, which is retryable.
pub fn parse_build_response(xml: &str) -> KojiRpcResult<HashMap<String, String>> {
    match parse_response(xml) {
        KojiRpcResult::ValidNonempty(RpcValue::Struct(m)) => {
            // An early return rather than if/else: an `if let Err(_) =
            // require_numeric_id(&m, ..)` holds the borrow of `m` across the
            // else branch under edition 2021, which then cannot move `m`.
            if let Err(e) = require_numeric_id(&m, "build_id") {
                return KojiRpcResult::Malformed(format!(
                    "getBuild: {e}; cannot complete enrichment"
                ));
            }
            KojiRpcResult::ValidNonempty(m)
        }
        KojiRpcResult::ValidNonempty(RpcValue::Nil) => KojiRpcResult::ValidEmpty,
        KojiRpcResult::ValidNonempty(other) => KojiRpcResult::Malformed(format!(
            "getBuild returned {other:?}, expected a struct or nil"
        )),
        KojiRpcResult::ValidEmpty => KojiRpcResult::ValidEmpty,
        KojiRpcResult::ApiFault(d) => KojiRpcResult::ApiFault(d),
        KojiRpcResult::Malformed(d) => KojiRpcResult::Malformed(d),
    }
}

/// `listBuildRPMs` / `queryRPMSigs`: an array of structs (or nil), every
/// record of which must satisfy `schema`. An empty array is a real answer; an
/// array of records the caller cannot read is not -- a `<struct/>` element
/// reaches the same "no RPMs" / "unsigned" conclusion the old lossy parser's
/// empty vec did.
pub fn parse_array_response(
    xml: &str,
    schema: &ArraySchema,
) -> KojiRpcResult<Vec<HashMap<String, String>>> {
    match parse_response(xml) {
        KojiRpcResult::ValidNonempty(RpcValue::Array(v)) => {
            if v.is_empty() {
                return KojiRpcResult::ValidEmpty;
            }
            for (i, r) in v.iter().enumerate() {
                if let Err(e) = (schema.validate)(r) {
                    return KojiRpcResult::Malformed(format!(
                        "{} record {}: {}",
                        schema.method, i, e
                    ));
                }
            }
            KojiRpcResult::ValidNonempty(v)
        }
        KojiRpcResult::ValidNonempty(RpcValue::Nil) => KojiRpcResult::ValidEmpty,
        KojiRpcResult::ValidNonempty(other) => {
            KojiRpcResult::Malformed(format!("expected an array payload, got {other:?}"))
        }
        KojiRpcResult::ValidEmpty => KojiRpcResult::ValidEmpty,
        KojiRpcResult::ApiFault(d) => KojiRpcResult::ApiFault(d),
        KojiRpcResult::Malformed(d) => KojiRpcResult::Malformed(d),
    }
}

/// Identity of one cached Koji RPC response.
///
/// Every component is load-bearing, and leaving any of them out is a
/// correctness bug rather than a missed optimisation:
///
/// - **version** — a parser fix must retire the entries it invalidates.
/// - **hub** — Koji ids are hub-relative. Build 1 on Fedora's hub and build 1
///   on CentOS's are different builds, so `queryRPMSigs(1)` means different
///   things against different hubs. The *output* checkpoint already treats the
///   hub as part of derived identity; if the source key did not, changing hubs
///   would miss the checkpoint as intended and then recompute from the other
///   hub's cached response — quietly wrong data.
/// - **method** — `getBuild` and `queryRPMSigs` must not share a slot.
/// - **argument** — as the method actually received it.
///
/// Encoding is length-prefixed, so no two distinct identities collide:
/// `(hub "ab", method "c")` and `(hub "a", method "bc")` hash differently. The
/// `koji-rpc-` prefix is chosen so no legacy `koji-build-*` / `koji-sigs-*`
/// key can ever equal a new one — appending a version segment to the old
/// prefix would not be disjoint, since RPM names contain hyphens and an NVR of
/// `v2-zlib-1.3-1.fc44` would land on the new key for `zlib-1.3-1.fc44`.
pub struct RpcCacheKey<'a> {
    pub hub: &'a str,
    pub method: &'a str,
    pub argument: &'a str,
}

impl RpcCacheKey<'_> {
    pub fn to_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for part in [
            KOJI_RPC_CACHE_VERSION,
            self.hub,
            self.method,
            self.argument,
        ] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        let digest: [u8; 32] = h.finalize().into();
        let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
        format!("koji-rpc-{}-{}", KOJI_RPC_CACHE_VERSION, hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

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

    #[test]
    fn only_valid_rpc_results_are_conclusive() {
        assert!(KojiRpcResult::ValidNonempty(1).is_conclusive());
        assert!(KojiRpcResult::<i32>::ValidEmpty.is_conclusive());
        assert!(!KojiRpcResult::<i32>::ApiFault("500".into()).is_conclusive());
        assert!(!KojiRpcResult::<i32>::Malformed("bad xml".into()).is_conclusive());
    }

    #[test]
    fn an_xmlrpc_fault_is_not_an_empty_result() {
        // The bug this type exists to fix: today a fault body parses to an empty
        // struct and is indistinguishable from "no such build".
        let fault = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
          <member><name>faultCode</name><value><int>1000</int></value></member>
          <member><name>faultString</name><value><string>no such build</string></value></member>
        </struct></value></fault></methodResponse>"#;
        assert!(matches!(parse_build_response(fault), KojiRpcResult::ApiFault(_)));
        assert!(matches!(parse_array_response(fault, &LIST_BUILD_RPMS), KojiRpcResult::ApiFault(_)));
    }

    #[test]
    fn truncated_xml_is_malformed_not_empty() {
        // The case a substring classifier cannot catch: this CONTAINS
        // "<methodResponse" and no "<fault>", yet the parser aborts partway and
        // would otherwise yield an empty collection that looks conclusive.
        let truncated = "<?xml version=\"1.0\"?><methodResponse><params><param><value><struct>\
                         <member><name>id</name><value><int>7</int";
        assert!(matches!(parse_build_response(truncated), KojiRpcResult::Malformed(_)),
            "a body that fails to parse must never be reported as empty");
        assert!(matches!(parse_array_response(truncated, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn non_xml_is_malformed() {
        assert!(matches!(parse_build_response("not xml at all"), KojiRpcResult::Malformed(_)));
        assert!(matches!(parse_array_response("<html>503</html>", &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn a_genuinely_empty_result_is_valid_empty() {
        // getBuild's conclusive "no such build" is <nil/>: koji's own getBuild
        // returns None, which the hub serializes with allow_none. An empty array
        // is likewise a real answer for the list RPCs.
        let nil = r#"<?xml version="1.0"?><methodResponse><params><param>
          <value><nil/></value></param></params></methodResponse>"#;
        assert!(matches!(parse_build_response(nil), KojiRpcResult::ValidEmpty));
        let empty_array = r#"<?xml version="1.0"?><methodResponse><params><param>
          <value><array><data></data></array></value></param></params></methodResponse>"#;
        assert!(matches!(parse_array_response(empty_array, &LIST_BUILD_RPMS), KojiRpcResult::ValidEmpty));
    }

    #[test]
    fn an_empty_build_struct_is_malformed_not_absence() {
        // The old code's `data.is_empty()` check (`:267`) called this "not
        // found", which is exactly how a fault or a truncated body became a
        // conclusive answer. Koji signals absence with <nil/>, never <struct/>.
        let empty_struct = r#"<?xml version="1.0"?><methodResponse><params><param>
          <value><struct></struct></value></param></params></methodResponse>"#;
        assert!(matches!(parse_build_response(empty_struct), KojiRpcResult::Malformed(_)),
            "an empty struct is not koji's way of saying 'no such build'");
    }

    #[test]
    fn eof_with_unclosed_elements_is_malformed() {
        // quick-xml's pull parser reaches EOF happily with elements still open,
        // so EOF alone proves nothing. Each of these contains "<methodResponse",
        // has no "<fault>", and would otherwise parse to an empty result.
        for truncated in [
            "<?xml version=\"1.0\"?><methodResponse><params>",
            "<?xml version=\"1.0\"?><methodResponse><params><param><value><struct>",
            "<?xml version=\"1.0\"?><methodResponse>",
        ] {
            assert!(matches!(parse_build_response(truncated), KojiRpcResult::Malformed(_)),
                "unclosed document must be Malformed: {truncated:?}");
            assert!(matches!(parse_array_response(truncated, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
        }
    }

    #[test]
    fn a_self_closing_fault_is_still_a_fault() {
        // <fault/> is Event::Empty, not Event::Start. A scanner that only watches
        // Start would report this as a valid empty result.
        let x = r#"<?xml version="1.0"?><methodResponse><fault/></methodResponse>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::ApiFault(_)));
        assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::ApiFault(_)));
    }

    #[test]
    fn a_self_closing_method_response_carries_no_payload() {
        let x = r#"<?xml version="1.0"?><methodResponse/>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn a_response_with_no_params_payload_is_malformed() {
        let x = r#"<?xml version="1.0"?><methodResponse></methodResponse>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)),
            "a response carrying neither params nor fault is not a conclusive empty");
    }

    #[test]
    fn a_well_formed_but_empty_params_element_is_malformed() {
        // Balanced, has a methodResponse, has exactly one <params> -- a tag
        // census accepts it, yet it carries no value at all.
        let x = r#"<?xml version="1.0"?><methodResponse><params/></methodResponse>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)),
            "<params/> carries no payload and must not read as a conclusive empty");
        assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn a_method_response_nested_in_another_document_is_malformed() {
        // e.g. an HTML error page that happens to embed the text. The response
        // must BE the root, not merely appear somewhere inside.
        let x = r#"<html><methodResponse><params><param><value><struct/>
          </value></param></params></methodResponse></html>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn an_unexpected_payload_type_is_malformed() {
        let x = r#"<?xml version="1.0"?><methodResponse><params><param>
          <value><boolean>1</boolean></value></param></params></methodResponse>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)),
            "koji answers with struct, array or nil; anything else is unexpected");
    }

    #[test]
    fn a_nil_payload_is_a_conclusive_empty() {
        // XML-RPC's explicit "no value" -- a real answer, so checkpointable.
        // Accepted by both parsers.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param>
          <value><nil/></value></param></params></methodResponse>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::ValidEmpty));
        assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::ValidEmpty));
    }

    #[test]
    fn a_struct_payload_is_rejected_by_the_array_parser() {
        // Well-formed, no fault, balanced -- but the old lossy parser returned
        // an empty vec for it, and that empty would be checkpointed as a
        // conclusive "this build has no RPMs / no signatures".
        let struct_body = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><int>7</int></value></member>
        </struct></value></param></params></methodResponse>"#;
        assert!(matches!(parse_array_response(struct_body, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)),
            "an array RPC handed a struct must not read as an empty array");
        // ...while the struct parser accepts it.
        assert!(matches!(parse_build_response(struct_body), KojiRpcResult::ValidNonempty(_)));
    }

    #[test]
    fn an_array_payload_is_rejected_by_the_struct_parser() {
        let array_body = r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
          <value><struct>
            <member><name>id</name><value><int>5</int></value></member>
            <member><name>arch</name><value><string>x86_64</string></value></member>
          </struct></value>
        </data></array></value></param></params></methodResponse>"#;
        assert!(matches!(parse_build_response(array_body), KojiRpcResult::Malformed(_)),
            "getBuild handed an array must not read as an empty struct");
        assert!(matches!(parse_array_response(array_body, &LIST_BUILD_RPMS), KojiRpcResult::ValidNonempty(_)));
    }

    #[test]
    fn more_than_one_payload_is_malformed() {
        let x = r#"<?xml version="1.0"?><methodResponse><params>
          <param><value><struct/></value></param>
          <param><value><struct/></value></param>
        </params></methodResponse>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn an_array_without_a_data_element_is_malformed() {
        // Outer type is right, inner content is nonsense. The lossy parser would
        // return an empty vec and this would read as "no RPMs for this build".
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value>
          <array><bogus/></array>
        </value></param></params></methodResponse>"#;
        assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn array_elements_that_are_not_structs_are_malformed() {
        // Same trap one level deeper: well-formed array>data, wrong element type.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value>
          <array><data><value><string>wrong element type</string></value></data></array>
        </value></param></params></methodResponse>"#;
        assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)),
            "an array of scalars is a schema violation, not an empty array");
    }

    #[test]
    fn an_empty_array_is_a_conclusive_empty() {
        // The legitimate counterpart: a build really can have no signatures.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value>
          <array><data></data></array>
        </value></param></params></methodResponse>"#;
        assert!(matches!(parse_array_response(x, &LIST_BUILD_RPMS), KojiRpcResult::ValidEmpty));
    }

    #[test]
    fn a_build_struct_without_build_id_is_malformed() {
        // Non-empty, well-formed, but missing the field the enrichment chain
        // keys on (enrich_koji.rs:299). Checkpointing it would cache a build
        // that can never produce complete enrichment.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>name</name><value><string>zlib</string></value></member>
        </struct></value></param></params></methodResponse>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)),
            "a build struct with no build_id cannot complete enrichment");
    }

    #[test]
    fn a_build_struct_with_build_id_is_valid() {
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><int>7</int></value></member>
          <member><name>name</name><value><string>zlib</string></value></member>
        </struct></value></param></params></methodResponse>"#;
        match parse_build_response(x) {
            KojiRpcResult::ValidNonempty(m) => {
                assert_eq!(m.get("build_id").map(String::as_str), Some("7"));
                assert_eq!(m.get("name").map(String::as_str), Some("zlib"));
            }
            other => panic!("expected ValidNonempty, got {other:?}"),
        }
    }

    #[test]
    fn a_non_numeric_build_id_is_malformed() {
        // Key present, value unusable: this gets interpolated into the next
        // call's <int> argument, so "presence" is not the property that matters.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><string>n/a</string></value></member>
        </struct></value></param></params></methodResponse>"#;
        assert!(matches!(parse_build_response(x), KojiRpcResult::Malformed(_)));
    }

    /// Wraps array records in a full methodResponse, so the record-level tests
    /// below differ only in the records themselves.
    fn array_of(records: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
            {records}
            </data></array></value></param></params></methodResponse>"#
        )
    }

    const RPM_RECORD: &str = r#"<value><struct>
      <member><name>id</name><value><int>5</int></value></member>
      <member><name>arch</name><value><string>x86_64</string></value></member>
    </struct></value>"#;

    const SIG_RECORD: &str = r#"<value><struct>
      <member><name>sigkey</name><value><string>abc123</string></value></member>
    </struct></value>"#;

    #[test]
    fn an_empty_struct_array_element_is_malformed() {
        // Grammatically perfect and utterly unreadable. Under the old parser and
        // under grammar-only validation alike, this array reaches `rpm_id == None`
        // at :372 and is treated as the conclusive "this build has no RPMs".
        let x = array_of("<value><struct/></value>");
        assert!(matches!(parse_array_response(&x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)),
            "a record the caller cannot read is not an answer");
    }

    #[test]
    fn an_rpm_record_missing_a_required_field_is_malformed() {
        // Missing id -> rpm_id is None -> "no binary RPMs".
        let no_id = array_of(
            "<value><struct><member><name>arch</name>\
             <value><string>x86_64</string></value></member></struct></value>");
        assert!(matches!(parse_array_response(&no_id, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));

        // Missing arch -> `:368`'s map_or(true, ...) treats it as non-src and
        // selects it, so absence is not neutral here.
        let no_arch = array_of(
            "<value><struct><member><name>id</name>\
             <value><int>5</int></value></member></struct></value>");
        assert!(matches!(parse_array_response(&no_arch, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));

        // Present but unusable.
        let bad_id = array_of(
            "<value><struct><member><name>id</name><value><string>x</string></value></member>\
             <member><name>arch</name><value><string>x86_64</string></value></member></struct></value>");
        assert!(matches!(parse_array_response(&bad_id, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));

        // A record that meets the contract still passes.
        assert!(matches!(
            parse_array_response(&array_of(RPM_RECORD), &LIST_BUILD_RPMS),
            KojiRpcResult::ValidNonempty(_)));
    }

    #[test]
    fn a_signature_record_without_a_sigkey_is_malformed() {
        let x = array_of(
            "<value><struct><member><name>rpm_id</name>\
             <value><int>5</int></value></member></struct></value>");
        assert!(matches!(parse_array_response(&x, &QUERY_RPM_SIGS), KojiRpcResult::Malformed(_)),
            "a record with no sigkey would read as a conclusive 'unsigned'");
    }

    #[test]
    fn an_empty_sigkey_is_a_real_unsigned_answer() {
        // The deliberate asymmetry with the rpm schema: Koji reports unsigned
        // RPMs as an empty sigkey, and :419 already filters for non-empty. That
        // is a conclusive answer, not a schema violation.
        let x = array_of(
            "<value><struct><member><name>sigkey</name>\
             <value><string></string></value></member></struct></value>");
        assert!(matches!(parse_array_response(&x, &QUERY_RPM_SIGS), KojiRpcResult::ValidNonempty(_)));
    }

    #[test]
    fn each_array_rpc_validates_against_its_own_schema() {
        // The assertion that proves the schema argument is actually consulted
        // rather than decorative: the same body passes under one and fails under
        // the other, in both directions.
        let rpms = array_of(RPM_RECORD);
        let sigs = array_of(SIG_RECORD);
        assert!(matches!(parse_array_response(&rpms, &LIST_BUILD_RPMS), KojiRpcResult::ValidNonempty(_)));
        assert!(matches!(parse_array_response(&rpms, &QUERY_RPM_SIGS), KojiRpcResult::Malformed(_)));
        assert!(matches!(parse_array_response(&sigs, &QUERY_RPM_SIGS), KojiRpcResult::ValidNonempty(_)));
        assert!(matches!(parse_array_response(&sigs, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn one_bad_record_among_good_ones_is_malformed() {
        // Validation is over every record, not just the first. The old selection
        // logic scans the list, so a later broken record matters just as much.
        let x = array_of(&format!("{RPM_RECORD}<value><struct/></value>"));
        assert!(matches!(parse_array_response(&x, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn cdata_text_is_not_dropped() {
        // quick-xml reports CDATA as Event::CData, not Event::Text. A catch-all
        // that ignores it turns a valid string field into an empty one -- and an
        // empty field is precisely what gets checkpointed as missing metadata.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><int>7</int></value></member>
          <member><name>name</name><value><string><![CDATA[zlib]]></string></value></member>
        </struct></value></param></params></methodResponse>"#;
        match parse_build_response(x) {
            KojiRpcResult::ValidNonempty(m) => {
                assert_eq!(m.get("name").map(String::as_str), Some("zlib"))
            }
            other => panic!("expected ValidNonempty, got {other:?}"),
        }
    }

    #[test]
    fn text_interleaved_with_cdata_is_concatenated() {
        // Three adjacent text runs. Taking only the first truncates the value
        // without erroring -- the same silent-loss shape as dropping CDATA.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><int>7</int></value></member>
          <member><name>name</name><value><string>a<![CDATA[b]]>c</string></value></member>
        </struct></value></param></params></methodResponse>"#;
        match parse_build_response(x) {
            KojiRpcResult::ValidNonempty(m) => {
                assert_eq!(m.get("name").map(String::as_str), Some("abc"))
            }
            other => panic!("expected ValidNonempty, got {other:?}"),
        }
    }

    #[test]
    fn a_nested_array_of_scalars_is_legal() {
        // The array-of-records rule belongs to the top-level payload, not to the
        // grammar. Koji build structs carry things like an array of tag names in
        // members we do not even store; rejecting them here would make an
        // ordinary response retryable forever.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><int>7</int></value></member>
          <member><name>tags</name><value><array><data>
            <value><string>f44</string></value>
            <value><string>f44-updates</string></value>
          </data></array></value></member>
        </struct></value></param></params></methodResponse>"#;
        match parse_build_response(x) {
            KojiRpcResult::ValidNonempty(m) => {
                assert_eq!(m.get("build_id").map(String::as_str), Some("7"));
                assert!(!m.contains_key("tags"), "nested containers are not stored as scalars");
            }
            other => panic!("expected ValidNonempty, got {other:?}"),
        }
        // ...while a TOP-LEVEL array of scalars is still a schema violation.
        let top = r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
          <value><string>f44</string></value>
        </data></array></value></param></params></methodResponse>"#;
        assert!(matches!(parse_array_response(top, &LIST_BUILD_RPMS), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn an_empty_value_element_is_the_empty_string() {
        // `<value></value>` is XML-RPC's empty string. Treating it as a grammar
        // violation would make a legitimate response permanently retryable.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><int>7</int></value></member>
          <member><name>note</name><value></value></member>
        </struct></value></param></params></methodResponse>"#;
        match parse_build_response(x) {
            KojiRpcResult::ValidNonempty(m) => {
                assert_eq!(m.get("note").map(String::as_str), Some(""))
            }
            other => panic!("expected ValidNonempty, got {other:?}"),
        }
    }

    #[test]
    fn the_fault_subtree_is_parsed_not_scanned() {
        // The detail comes out of a parsed struct, so it survives a faultString
        // whose value merely *contains* the word, and a malformed fault body is
        // reported as such rather than passed off as a hub fault.
        let ok = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
          <member><name>faultCode</name><value><int>1000</int></value></member>
          <member><name>faultString</name><value><string>no such build</string></value></member>
        </struct></value></fault></methodResponse>"#;
        match parse_build_response(ok) {
            KojiRpcResult::ApiFault(d) => assert_eq!(d, "no such build"),
            other => panic!("expected ApiFault, got {other:?}"),
        }
        // Truncated inside the fault: not a readable fault, and certainly not a
        // conclusive result.
        let truncated = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
          <member><name>faultCode</name><value><int>1000</int></value></member>"#;
        assert!(matches!(parse_build_response(truncated), KojiRpcResult::Malformed(_)));
    }

    #[test]
    fn a_nested_container_member_is_validated_but_not_stored() {
        // Koji structs can carry nested values. They must not break parsing, and
        // must not be silently treated as scalars either.
        let x = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><int>7</int></value></member>
          <member><name>extra</name><value><struct>
            <member><name>inner</name><value><string>v</string></value></member>
          </struct></value></member>
        </struct></value></param></params></methodResponse>"#;
        match parse_build_response(x) {
            KojiRpcResult::ValidNonempty(m) => {
                assert_eq!(m.get("build_id").map(String::as_str), Some("7"));
                assert!(!m.contains_key("extra"), "nested containers are not stored as scalars");
            }
            other => panic!("expected ValidNonempty, got {other:?}"),
        }
    }

    #[test]
    fn a_populated_response_parses_to_valid_nonempty() {
        let ok = r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><int>7</int></value></member>
        </struct></value></param></params></methodResponse>"#;
        match parse_build_response(ok) {
            KojiRpcResult::ValidNonempty(m) => {
                assert_eq!(m.get("build_id").map(String::as_str), Some("7"))
            }
            _ => panic!("a well-formed populated response must be ValidNonempty"),
        }
    }

    #[test]
    fn the_two_stage_schema_versions_are_distinct_literals() {
        // The only assertion that actually catches a copy-paste sharing one
        // constant. Both path tests pass regardless, because the stage segment
        // already differs.
        assert_ne!(
            KOJI_SCHEMA_VERSION,
            crate::collect_spec::SPEC_SCHEMA_VERSION,
            "each stage must version independently, or bumping one silently \
             invalidates the other's checkpoints"
        );
    }

    #[test]
    fn koji_cache_path_carries_its_own_schema_version() {
        // Mirror of the spec-stage test: this stage's path must contain THIS
        // stage's version, and a bump must invalidate.
        let d = tempfile::TempDir::new().unwrap();
        let ctx = crate::output_cache::CanonicalContext::new();
        let mine = crate::output_cache::OutputCache::new(
            d.path(), "20260912T010203Z-abcdef01", "koji", KOJI_SCHEMA_VERSION).unwrap();
        let bumped = crate::output_cache::OutputCache::new(
            d.path(), "20260912T010203Z-abcdef01", "koji", "koji-vNEXT").unwrap();
        let p = mine.entry_path("k", &ctx).unwrap();
        assert!(p.to_string_lossy().contains(KOJI_SCHEMA_VERSION), "got {p:?}");
        assert_ne!(p, bumped.entry_path("k", &ctx).unwrap());
        // And it must not collide with the spec stage for the same key.
        let spec = crate::output_cache::OutputCache::new(
            d.path(), "20260912T010203Z-abcdef01", "spec",
            crate::collect_spec::SPEC_SCHEMA_VERSION).unwrap();
        assert_ne!(p, spec.entry_path("k", &ctx).unwrap());
    }
    // ---- shared mock fixtures -------------------------------------------------
    // All three RPCs POST to the same hub URL, so the mocks are distinguished by
    // methodName in the body. Task 7's whole-chain tests reuse these.

    use mockito::Matcher;

    fn ok_build() -> &'static str {
        r#"<?xml version="1.0"?><methodResponse><params><param><value><struct>
          <member><name>build_id</name><value><int>1</int></value></member>
          <member><name>name</name><value><string>zlib</string></value></member>
          <member><name>owner_name</name><value><string>freshowner</string></value></member>
        </struct></value></param></params></methodResponse>"#
    }
    /// Each array RPC needs a body that satisfies ITS schema -- one shared
    /// `ok_array()` would fail `QUERY_RPM_SIGS` validation and make the
    /// "successful chain" test assert the opposite of its name.
    fn ok_rpms() -> &'static str {
        r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
          <value><struct>
            <member><name>id</name><value><int>5</int></value></member>
            <member><name>arch</name><value><string>x86_64</string></value></member>
          </struct></value>
        </data></array></value></param></params></methodResponse>"#
    }
    fn ok_sigs() -> &'static str {
        r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
          <value><struct>
            <member><name>sigkey</name><value><string>abc123</string></value></member>
          </struct></value>
        </data></array></value></param></params></methodResponse>"#
    }
    fn fault() -> &'static str {
        r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
          <member><name>faultCode</name><value><int>1000</int></value></member>
          <member><name>faultString</name><value><string>backend down</string></value></member>
        </struct></value></fault></methodResponse>"#
    }

    /// One mock per RPC on a shared hub, with exact expected call counts.
    ///
    /// The counts are the point: mockito 1.7.2's `Server::new()` sets
    /// `assert_on_drop = false`, so a mock that is never called fails nothing on
    /// its own. The caller must `.assert()` every returned mock, including the
    /// ones expecting zero.
    fn mock_hub(
        server: &mut mockito::Server,
        bodies: [(&str, &str); 3],
        expected_calls: [usize; 3],
    ) -> Vec<mockito::Mock> {
        bodies
            .into_iter()
            .zip(expected_calls)
            .map(|((method, body), n)| {
                server
                    .mock("POST", "/kojihub")
                    .match_body(Matcher::Regex(format!("<methodName>{method}</methodName>")))
                    .with_body(body)
                    .expect(n)
                    .create()
            })
            .collect()
    }

    /// Writes a source-cache entry under a key spelled out literally, as the
    /// pre-`KOJI_RPC_CACHE_VERSION` code would have written it.
    fn seed_legacy_cache(dir: &std::path::Path, key: &str, value: serde_json::Value) {
        let c = crate::cache::FileCache::new(dir.to_str().unwrap(), "koji", 720, None).unwrap();
        c.put(key, &value);
    }

    /// Drives `get_build` directly rather than `enrich_from_nvrs`, whose signature
    /// changes in Task 7. These two tests are about the source cache, not the
    /// checkpoint, and must keep compiling across that change.
    fn run_get_build(hub: &str, cache_dir: &std::path::Path, out: &std::path::Path) -> String {
        let e = KojiEnricher::new_standalone(hub, "fedora", "44", Some(cache_dir.to_str().unwrap()));
        let mut w = NTriplesWriter::new(std::fs::File::create(out).unwrap());
        e.get_build("zlib-1.3-1.fc44", &mut w).unwrap();
        w.flush().unwrap();
        std::fs::read_to_string(out).unwrap()
    }

    #[test]
    fn a_legacy_koji_build_cache_entry_is_not_reused() {
        // The old parser's `Err(_) => break` (:598) persisted partial maps built
        // from truncated bodies. This seeded entry is nonempty and even carries a
        // build_id, so validating the cached JSON would wave it through -- only
        // retiring the key namespace stops it being read.
        let mut server = mockito::Server::new();
        let mocks = mock_hub(
            &mut server,
            [("getBuild", ok_build()), ("listBuildRPMs", ok_rpms()), ("queryRPMSigs", ok_sigs())],
            [1, 1, 1],
        );

        let d = tempfile::TempDir::new().unwrap();
        seed_legacy_cache(
            d.path(),
            "koji-build-zlib-1.3-1.fc44",
            serde_json::json!({"build_id": "1", "owner_name": "legacyowner"}),
        );

        let hub = format!("{}/kojihub", server.url());
        let text = run_get_build(&hub, d.path(), &d.path().join("out.nt"));

        // Without the key change getBuild is never called and this fails.
        for m in &mocks {
            m.assert();
        }
        assert!(
            text.contains("agent/koji/freshowner"),
            "the refetched response must be what reaches the output"
        );
        assert!(!text.contains("legacyowner"), "the legacy entry must not be read");
    }

    #[test]
    fn a_colliding_legacy_key_is_not_reused() {
        // Pins the namespace shape, not just the version bump. Under the obvious
        // spelling `koji-build-{VERSION}-{nvr}` this seeded legacy entry -- a real
        // package whose NVR starts with the version segment -- IS the new key for
        // "zlib-1.3-1.fc44", so it would be read as another package's metadata.
        let mut server = mockito::Server::new();
        let mocks = mock_hub(
            &mut server,
            [("getBuild", ok_build()), ("listBuildRPMs", ok_rpms()), ("queryRPMSigs", ok_sigs())],
            [1, 1, 1],
        );

        let d = tempfile::TempDir::new().unwrap();
        seed_legacy_cache(
            d.path(),
            &format!("koji-build-{}-zlib-1.3-1.fc44", KOJI_RPC_CACHE_VERSION),
            serde_json::json!({"build_id": "1", "owner_name": "legacyowner"}),
        );

        let hub = format!("{}/kojihub", server.url());
        let text = run_get_build(&hub, d.path(), &d.path().join("out.nt"));

        for m in &mocks {
            m.assert();
        }
        assert!(!text.contains("legacyowner"), "the new namespace must not overlap the old one");
    }

    #[test]
    fn a_legacy_koji_sigs_cache_entry_is_not_reused() {
        // The second namespace, reached only after getBuild succeeds. Its key is
        // built from ok_build()'s build_id, so the pre-version spelling is
        // "koji-sigs-1".
        let mut server = mockito::Server::new();
        let mocks = mock_hub(
            &mut server,
            [("getBuild", ok_build()), ("listBuildRPMs", ok_rpms()), ("queryRPMSigs", ok_sigs())],
            [1, 1, 1],
        );

        let d = tempfile::TempDir::new().unwrap();
        seed_legacy_cache(d.path(), "koji-sigs-1", serde_json::json!({"sigkey": "deadbeef"}));

        let hub = format!("{}/kojihub", server.url());
        let text = run_get_build(&hub, d.path(), &d.path().join("out.nt"));

        // Without the key change the cached sigkey short-circuits :324, so
        // neither listBuildRPMs nor queryRPMSigs is called and both fail here.
        for m in &mocks {
            m.assert();
        }
        assert!(text.contains("abc123"), "the refetched sigkey must be what reaches the output");
        assert!(!text.contains("deadbeef"), "the legacy entry must not be read");
    }

    // ─── source-cache identity ──────────────────────────────────────

    fn k(hub: &str, method: &str, argument: &str) -> String {
        RpcCacheKey { hub, method, argument }.to_key()
    }

    #[test]
    fn the_source_key_is_disjoint_from_every_legacy_key() {
        // Legacy keys were "koji-build-<nvr>" and "koji-sigs-<build_id>". No
        // new key may equal one, for any nvr or id -- including an nvr that
        // starts with the version segment, which is why the version is not
        // simply appended to the old prefix.
        let key = k("https://koji.fedoraproject.org/kojihub", "getBuild", "zlib-1.3-1.fc44");
        assert!(key.starts_with("koji-rpc-"), "got {key}");
        assert!(!key.starts_with("koji-build-"));
        assert!(!key.starts_with("koji-sigs-"));
    }

    #[test]
    fn the_source_key_separates_hubs() {
        // The blocker this type exists for. Koji build ids are hub-relative,
        // so queryRPMSigs(1) is a different question on a different hub. The
        // output checkpoint already varies on koji_hub; if the source cache
        // did not, a hub change would miss the checkpoint and then recompute
        // from the other hub's cached response.
        assert_ne!(
            k("https://koji.fedoraproject.org/kojihub", "queryRPMSigs", "1"),
            k("https://kojihub.stream.centos.org/kojihub", "queryRPMSigs", "1"),
        );
        assert_ne!(
            k("https://a.example/kojihub", "getBuild", "zlib-1.3-1.fc44"),
            k("https://b.example/kojihub", "getBuild", "zlib-1.3-1.fc44"),
        );
    }

    #[test]
    fn the_source_key_separates_methods_and_arguments() {
        let hub = "https://koji.example/kojihub";
        assert_ne!(k(hub, "getBuild", "1"), k(hub, "queryRPMSigs", "1"));
        assert_ne!(k(hub, "listBuildRPMs", "1"), k(hub, "queryRPMSigs", "1"));
        assert_ne!(k(hub, "getBuild", "1"), k(hub, "getBuild", "2"));
    }

    #[test]
    fn the_source_key_encoding_is_injective() {
        // Plain concatenation would make these two identical.
        assert_ne!(k("ab", "c", "x"), k("a", "bc", "x"));
        assert_ne!(k("h", "ab", "c"), k("h", "a", "bc"));
    }

    #[test]
    fn the_source_key_carries_the_rpc_cache_version() {
        let key = k("https://koji.example/kojihub", "getBuild", "zlib-1.3-1.fc44");
        assert!(
            key.contains(KOJI_RPC_CACHE_VERSION),
            "the version must be visible in the key namespace, got {key}"
        );
    }
}
