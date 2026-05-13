//! npm SLSA provenance attestation enricher.
//!
//! Queries Fuseki for npm packages, checks the npm registry for attestation
//! bundles (--provenance flag), and emits SLSA provenance triples.

use crate::enricher::rate_limit;
use crate::forge::emit_dq_issue;
use crate::ntriples::NTriplesWriter;
use crate::sparql::SparqlClient;
use crate::uris::*;
use reqwest::blocking::Client;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct NpmProvenanceEnricher {
    sparql: SparqlClient,
    client: Client,
}

impl NpmProvenanceEnricher {
    pub fn new(endpoint: &str) -> Self {
        let sparql = SparqlClient::new(endpoint);
        let client = crate::enricher::default_http_client();

        Self { sparql, client }
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        // Emit static Sigstore infrastructure metadata once per run
        emit_infrastructure_metadata(&mut writer)?;

        let packages = self.sparql.query_packages_by_type(
            &format!("{NPM}NpmPackage")
        )?;

        eprintln!("Found {} npm packages to check for provenance", packages.len());

        let mut total_checked = 0;
        let mut total_triples = 0;

        for (pkg_uri, name, version) in &packages {
            total_checked += 1;
            if total_checked % 100 == 0 {
                eprintln!("Progress: {} packages checked", total_checked);
            }

            match self.check_attestations(&mut writer, pkg_uri, name, version) {
                Ok(triples) => total_triples += triples,
                Err(e) => eprintln!("  Error checking {}: {}", name, e),
            }

            rate_limit(Duration::from_millis(200));
        }

        writer.flush()?;
        Ok((total_checked, total_triples))
    }

    fn check_attestations(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        name: &str,
        version: &str,
    ) -> Result<usize> {
        // npm attestation API v1 endpoint (the /tgz/attestations pattern was removed)
        let url = format!("https://registry.npmjs.org/-/npm/v1/attestations/{}@{}", name, version);

        let response = self.client.get(&url)
            .header("Accept", "application/json")
            .send();

        let resp = match response {
            Ok(r) if r.status().is_success() => r,
            _ => return Ok(0), // No attestations or error
        };

        let data: serde_json::Value = match resp.json() {
            Ok(d) => d,
            Err(_) => return Ok(0),
        };

        let attestations = match data.get("attestations").and_then(|v| v.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return Ok(0),
        };

        // Prefer the SLSA provenance attestation over the npm publish attestation.
        // npm returns two attestations per package:
        //   [0] npm publish (predicateType: github.com/npm/attestation/...)  — no x509 certs
        //   [1] SLSA provenance (predicateType: slsa.dev/provenance/v1)      — has certs + builder info
        let slsa_att = attestations.iter()
            .find(|a| a.get("predicateType")
                .and_then(|v| v.as_str())
                .map(|s| s.contains("slsa.dev/provenance"))
                .unwrap_or(false))
            .or_else(|| attestations.first());

        let slsa_att = match slsa_att {
            Some(a) => a,
            None => return Ok(0),
        };

        let mut triples = 0;
        let att_uri = attestation_uri("npm", "registry", name, version);

        // Determine the actual predicate type from the attestation
        let predicate_type = slsa_att.get("predicateType")
            .and_then(|v| v.as_str())
            .unwrap_or("https://slsa.dev/provenance/v1");

        // ProvenanceAttestation (not "Attestation") per slsa.ttl
        writer.write_triple(&att_uri, RDF_TYPE, &format!("{SLSA}ProvenanceAttestation"))?;
        writer.write_literal(&att_uri, &format!("{SLSA}predicateType"), predicate_type)?;
        triples += 2;

        // Link to package via slsa:hasProvenance (not hasAttestation)
        writer.write_triple(pkg_uri, &format!("{SLSA}hasProvenance"), &att_uri)?;
        triples += 1;

        // npm provenance via GitHub Actions = SLSA Build L2 (hosted build service)
        writer.write_triple(&att_uri, &format!("{SLSA}attestsBuildLevel"),
            &format!("{SLSA}L2"))?;
        triples += 1;

        if let Some(bundle) = slsa_att.get("bundle").or_else(|| slsa_att.get("attestationBundle")) {
            // Extract timestamp from DSSE payload
            if let Some(ts) = extract_attestation_timestamp(bundle) {
                writer.write_datetime(&att_uri, &format!("{SLSA}attestationTimestamp"), &ts)?;
                triples += 1;
            }

            // Extract SLSA provenance metadata from the DSSE payload
            triples += emit_provenance_metadata(writer, &att_uri, bundle)?;

            // Emit att:DigitalSignature + transparency log triples (v0.8.0)
            triples += emit_signing_triples(writer, &att_uri, bundle, name, version)?;
        } else {
            eprintln!("  Attestation for {}@{} missing bundle/attestationBundle field", name, version);
            triples += emit_dq_issue(writer, "npm-provenance-enricher", "bundle", &format!("{}@{}", name, version), "attestation-incomplete", "info")?;
        }

        Ok(triples)
    }
}

/// Decode the DSSE payload from a bundle into a JSON Value.
fn decode_dsse_payload(bundle: &serde_json::Value) -> Option<serde_json::Value> {
    let payload_b64 = bundle
        .get("dsseEnvelope")
        .and_then(|e| e.get("payload"))
        .and_then(|p| p.as_str())?;

    use base64::Engine;
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
        .ok()?;

    serde_json::from_slice(&payload_bytes).ok()
}

/// Extract a timestamp from an npm attestation bundle.
///
/// Priority order:
/// 1. SLSA v1: predicate.runDetails.metadata.startedOn
/// 2. SLSA v0.2: predicate.metadata.buildStartedOn
/// 3. Transparency log: verificationMaterial.tlogEntries[0].integratedTime (Unix epoch)
fn extract_attestation_timestamp(bundle: &serde_json::Value) -> Option<String> {
    // Try DSSE payload timestamps first
    if let Some(statement) = decode_dsse_payload(bundle) {
        // SLSA provenance v1: predicate.runDetails.metadata.startedOn
        if let Some(ts) = statement
            .pointer("/predicate/runDetails/metadata/startedOn")
            .and_then(|v| v.as_str())
        {
            return Some(ts.to_string());
        }

        // SLSA v0.2: predicate.metadata.buildStartedOn
        if let Some(ts) = statement
            .pointer("/predicate/metadata/buildStartedOn")
            .and_then(|v| v.as_str())
        {
            return Some(ts.to_string());
        }
    }

    // Fallback: transparency log integratedTime (Unix epoch → ISO 8601)
    let integrated = bundle
        .pointer("/verificationMaterial/tlogEntries/0/integratedTime")
        .and_then(|v| v.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| v.as_i64()))?;

    chrono::DateTime::from_timestamp(integrated, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Extract SLSA provenance metadata from the DSSE payload.
///
/// Emits:
///   slsa:buildInvocationId — GitHub Actions run URL
///   slsa:buildType — workflow build type URI
///   slsa:Builder entity with slsa:builderId (proper domain)
///   slsa:attestsBuildActivity → pkg:BuildActivity → slsa:builtBy → slsa:Builder
///   slsa:hasSourceCommit → vcs:Commit entity (ObjectProperty, not literal)
///   slsa:sourceRepository — git repository URL from resolvedDependencies
///   slsa:workflowRef — branch/tag that triggered the build
fn emit_provenance_metadata(
    writer: &mut NTriplesWriter,
    att_uri: &str,
    bundle: &serde_json::Value,
) -> std::io::Result<usize> {
    let statement = match decode_dsse_payload(bundle) {
        Some(s) => s,
        None => {
            eprintln!("  Failed to decode DSSE payload for attestation");
            let dq = emit_dq_issue(writer, "npm-provenance-enricher", "dsseEnvelope.payload", att_uri, "dsse-parse-failed", "warning")?;
            return Ok(dq);
        }
    };

    let mut triples = 0;

    // Build invocation ID (GitHub Actions run URL)
    if let Some(invocation_id) = statement
        .pointer("/predicate/runDetails/metadata/invocationId")
        .and_then(|v| v.as_str())
    {
        writer.write_literal(att_uri, &format!("{SLSA}buildInvocationId"), invocation_id)?;
        triples += 1;
    }

    // Build type (URI)
    if let Some(build_type) = statement
        .pointer("/predicate/buildDefinition/buildType")
        .and_then(|v| v.as_str())
    {
        writer.write_typed_literal(att_uri, &format!("{SLSA}buildType"), build_type, &format!("{XSD}anyURI"))?;
        triples += 1;
    }

    // Builder entity (slsa:Builder) with proper domain for slsa:builderId.
    // Chain: ProvenanceAttestation → attestsBuildActivity → BuildActivity → builtBy → Builder
    if let Some(builder_id) = statement
        .pointer("/predicate/runDetails/builder/id")
        .and_then(|v| v.as_str())
    {
        let builder = builder_uri(builder_id);
        writer.write_triple(&builder, RDF_TYPE, &format!("{SLSA}Builder"))?;
        writer.write_literal(&builder, &format!("{SLSA}builderId"), builder_id)?;

        let build_activity_uri = format!("{att_uri}/build");
        writer.write_triple(&build_activity_uri, RDF_TYPE, &format!("{PKG}BuildActivity"))?;
        writer.write_triple(&build_activity_uri, &format!("{SLSA}builtBy"), &builder)?;
        writer.write_triple(att_uri, &format!("{SLSA}attestsBuildActivity"), &build_activity_uri)?;
        triples += 5;
    }

    // Source commit and repository from resolvedDependencies
    if let Some(deps) = statement
        .pointer("/predicate/buildDefinition/resolvedDependencies")
        .and_then(|v| v.as_array())
    {
        if let Some(first_dep) = deps.first() {
            // Source repository URI
            if let Some(uri) = first_dep.get("uri").and_then(|v| v.as_str()) {
                writer.write_literal(att_uri, &format!("{SLSA}sourceRepository"), uri)?;
                triples += 1;
            }
            // Source commit → vcs:Commit entity via slsa:hasSourceCommit (ObjectProperty)
            if let Some(commit_hash) = first_dep
                .pointer("/digest/gitCommit")
                .and_then(|v| v.as_str())
            {
                let commit_uri = format!("{DATA}commit/{}", commit_hash);
                writer.write_triple(&commit_uri, RDF_TYPE, &format!("{VCS}Commit"))?;
                writer.write_literal(&commit_uri, &format!("{VCS}commitHash"), commit_hash)?;
                writer.write_triple(att_uri, &format!("{SLSA}hasSourceCommit"), &commit_uri)?;
                triples += 3;
            }
        }
    }

    // Workflow ref (which branch/tag triggered the build)
    if let Some(workflow_ref) = statement
        .pointer("/predicate/buildDefinition/externalParameters/workflow/ref")
        .and_then(|v| v.as_str())
    {
        writer.write_literal(att_uri, &format!("{SLSA}workflowRef"), workflow_ref)?;
        triples += 1;
    }

    Ok(triples)
}

/// Emit static Sigstore infrastructure metadata.
///
/// Emits well-known infrastructure triples for:
/// - att:SigstorePublicGood (Rekor transparency log with logPublicKey, logUri)
/// - att:SigstoreFulcio (Fulcio CA with caUri, caRootCertFingerprint, issuesEphemeralCertificates)
///
/// This is emitted once per enricher run. N-Triples deduplicates on load, so
/// repeated emissions across runs are harmless.
fn emit_infrastructure_metadata(writer: &mut NTriplesWriter) -> std::io::Result<()> {
    // Rekor public good transparency log
    // Source: https://rekor.sigstore.dev/api/v1/log/publicKey (retrieved 2026-04-26)
    // This is the public key used to verify Signed Entry Timestamps (SETs) from Rekor
    let rekor_pubkey = "-----BEGIN PUBLIC KEY-----\n\
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE2G2Y+2tabdTV5BcGiBIx0a9fAFwr\n\
kBbmLSGtks4L3qX6yYY0zufBnhC8Ur/iy55GhWP/9A/bY2LhC30M9+RYtw==\n\
-----END PUBLIC KEY-----";

    writer.write_typed_literal(&format!("{ATT}SigstorePublicGood"), &format!("{ATT}logUri"), "https://rekor.sigstore.dev", &format!("{XSD}anyURI"))?;
    writer.write_literal(&format!("{ATT}SigstorePublicGood"), &format!("{ATT}logPublicKey"), rekor_pubkey)?;
    writer.write_literal(&format!("{ATT}SigstorePublicGood"), RDFS_LABEL, "Sigstore Public Good Rekor")?;

    // Fulcio certificate authority
    // Source: Sigstore TUF root https://github.com/sigstore/root-signing (fulcio_v1.crt.pem)
    // SHA-256 fingerprint of the Fulcio root certificate (retrieved 2026-04-26)
    // This anchors the certificate chain for offline verification
    let fulcio_root_fingerprint = "2c7e9f3576de7f72c807f3c1c4a8b59b67a579e85e0e3cfc0f81c1e5fd56cf02";

    writer.write_typed_literal(&format!("{ATT}SigstoreFulcio"), &format!("{ATT}caUri"), "https://fulcio.sigstore.dev", &format!("{XSD}anyURI"))?;
    writer.write_literal(&format!("{ATT}SigstoreFulcio"), &format!("{ATT}caRootCertFingerprint"), fulcio_root_fingerprint)?;
    writer.write_boolean(&format!("{ATT}SigstoreFulcio"), &format!("{ATT}issuesEphemeralCertificates"), true)?;
    writer.write_literal(&format!("{ATT}SigstoreFulcio"), RDFS_LABEL, "Sigstore Fulcio")?;

    Ok(())
}

/// Emit att:DigitalSignature and att:TransparencyLogEntry triples from a Sigstore bundle.
///
/// npm attestation bundles follow the Sigstore bundle format:
/// - `verificationMaterial.tlogEntries[0]` — Rekor transparency log entry
/// - `verificationMaterial.x509CertificateChain` — Fulcio ephemeral certificate
/// - `dsseEnvelope.signatures[0].sig` — the DSSE signature value
fn emit_signing_triples(
    writer: &mut NTriplesWriter,
    attestation_uri: &str,
    bundle: &serde_json::Value,
    name: &str,
    version: &str,
) -> std::io::Result<usize> {
    let mut triples = 0;
    let sig_uri = signature_uri("npm", name, version);

    // DigitalSignature entity
    writer.write_triple(attestation_uri, &format!("{ATT}hasSignature"), &sig_uri)?;
    writer.write_triple(&sig_uri, RDF_TYPE, &format!("{ATT}DigitalSignature"))?;
    writer.write_triple(&sig_uri, &format!("{ATT}signatureMethod"),
        &format!("{ATT}SigstoreKeyless"))?;
    // npm registry pre-verifies attestations before storing them
    writer.write_literal(&sig_uri, &format!("{ATT}signatureStatus"), "verified")?;
    writer.write_triple(&sig_uri, &format!("{ATT}hasOIDCProvider"),
        &format!("{ATT}GitHubActionsOIDC"))?;
    triples += 5;

    // DSSE signature value
    if let Some(sig_val) = bundle
        .pointer("/dsseEnvelope/signatures/0/sig")
        .and_then(|v| v.as_str())
    {
        writer.write_literal(&sig_uri, &format!("{ATT}signatureValue"), sig_val)?;
        triples += 1;
    }

    // Transparency log entry from verificationMaterial
    if let Some(tlog) = bundle
        .pointer("/verificationMaterial/tlogEntries/0")
    {
        if let Some(log_index) = tlog.get("logIndex").and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| tlog.get("logIndex").and_then(|v| v.as_i64()))
        {
            let tlog_uri = tlog_entry_uri("rekor", log_index);
            writer.write_triple(&sig_uri, &format!("{ATT}hasTransparencyLogEntry"), &tlog_uri)?;
            writer.write_triple(&tlog_uri, RDF_TYPE, &format!("{ATT}TransparencyLogEntry"))?;
            writer.write_integer(&tlog_uri, &format!("{ATT}logIndex"), log_index)?;
            writer.write_triple(&tlog_uri, &format!("{ATT}inTransparencyLog"),
                &format!("{ATT}SigstorePublicGood"))?;
            triples += 4;

            // integratedTime (Unix epoch → xsd:dateTime)
            if let Some(integrated) = tlog.get("integratedTime").and_then(|v| v.as_str())
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| tlog.get("integratedTime").and_then(|v| v.as_i64()))
            {
                let dt = chrono::DateTime::from_timestamp(integrated, 0)
                    .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string());
                if let Some(ts) = dt {
                    writer.write_datetime(&tlog_uri, &format!("{ATT}integratedTime"), &ts)?;
                    triples += 1;
                }
            }
        }
    }

    // Signing certificate (Fulcio ephemeral)
    // Sigstore bundle v0.3+ uses verificationMaterial.certificate (singular)
    // Older bundles use verificationMaterial.x509CertificateChain.certificates[0]
    let has_cert = bundle.pointer("/verificationMaterial/certificate").is_some()
        || bundle.pointer("/verificationMaterial/x509CertificateChain/certificates/0").is_some();
    if has_cert {
        let cert_uri = format!("{sig_uri}/cert");
        writer.write_triple(&sig_uri, &format!("{ATT}hasCertificate"), &cert_uri)?;
        writer.write_triple(&cert_uri, RDF_TYPE, &format!("{ATT}SigningCertificate"))?;
        writer.write_boolean(&cert_uri, &format!("{ATT}isEphemeralCertificate"), true)?;
        writer.write_triple(&cert_uri, &format!("{ATT}certificateIssuer"),
            &format!("{ATT}SigstoreFulcio"))?;
        triples += 4;

        // Parse x509 certificate for Fulcio extensions and metadata
        triples += parse_fulcio_extensions(writer, &cert_uri, bundle)?;
    }

    Ok(triples)
}

/// Parse Fulcio x509 certificate extensions and emit attestation ontology triples.
///
/// Extracts the 6 Sigstore Fulcio OID extensions (1.3.6.1.4.1.57264.1.{8-13}):
/// - 1.8: OIDC issuer URI
/// - 1.9: Build signer URI (workflow file path)
/// - 1.10: Build signer digest (workflow commit SHA)
/// - 1.11: Runner environment (github-hosted or self-hosted)
/// - 1.12: Source repository URI
/// - 1.13: Source repository digest (commit SHA)
///
/// Also extracts standard x509 metadata: subject, fingerprint, validity period.
///
/// Returns the number of triples emitted (0 if cert parsing fails).
fn parse_fulcio_extensions(
    writer: &mut NTriplesWriter,
    cert_uri: &str,
    bundle: &serde_json::Value,
) -> std::io::Result<usize> {
    use x509_parser::prelude::*;
    use sha2::{Sha256, Digest};
    use base64::Engine;

    let mut triples = 0;

    // Extract base64-encoded DER certificate
    let cert_b64 = bundle
        .pointer("/verificationMaterial/x509CertificateChain/certificates/0/rawBytes")
        .and_then(|v| v.as_str())
        .or_else(|| bundle.pointer("/verificationMaterial/certificate").and_then(|v| v.as_str()));

    let cert_b64 = match cert_b64 {
        Some(s) => s,
        None => return Ok(0), // No certificate found
    };

    // Decode base64 → DER
    let der_bytes = match base64::engine::general_purpose::STANDARD.decode(cert_b64) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("  Malformed base64 in signing certificate");
            let dq = emit_dq_issue(writer, "npm-provenance-enricher", "certificate.rawBytes", cert_b64, "cert-parse-failed", "info")?;
            return Ok(dq);
        }
    };

    // Parse x509 certificate
    let (_, cert) = match parse_x509_certificate(&der_bytes) {
        Ok(parsed) => parsed,
        Err(_) => {
            eprintln!("  Failed to parse x509 certificate DER");
            let dq = emit_dq_issue(writer, "npm-provenance-enricher", "certificate.x509", cert_b64, "cert-parse-failed", "info")?;
            return Ok(dq);
        }
    };

    // Certificate fingerprint (SHA-256 of DER bytes)
    let fingerprint = format!("{:x}", Sha256::digest(&der_bytes));
    writer.write_literal(cert_uri, &format!("{ATT}certificateFingerprint"), &fingerprint)?;
    triples += 1;

    // Subject (from SAN or DN)
    if let Some(san_ext) = cert.tbs_certificate.get_extension_unique(&oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME).ok().flatten() {
        if let ParsedExtension::SubjectAlternativeName(san_data) = san_ext.parsed_extension() {
            for name in &san_data.general_names {
                if let GeneralName::RFC822Name(email) = name {
                    writer.write_literal(cert_uri, &format!("{ATT}certificateSubject"), email)?;
                    triples += 1;
                    break;
                } else if let GeneralName::URI(uri) = name {
                    writer.write_literal(cert_uri, &format!("{ATT}certificateSubject"), uri)?;
                    triples += 1;
                    break;
                }
            }
        }
    }

    // Validity period (convert ASN1Time to datetime string)
    // x509-parser uses the `time` crate's OffsetDateTime
    let not_before = cert.validity().not_before.to_datetime();
    let not_after = cert.validity().not_after.to_datetime();
    // Format as RFC3339/ISO8601
    let not_before_str = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        not_before.year(), not_before.month() as u8, not_before.day(),
        not_before.hour(), not_before.minute(), not_before.second());
    let not_after_str = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        not_after.year(), not_after.month() as u8, not_after.day(),
        not_after.hour(), not_after.minute(), not_after.second());
    writer.write_datetime(cert_uri, &format!("{ATT}certificateNotBefore"), &not_before_str)?;
    writer.write_datetime(cert_uri, &format!("{ATT}certificateNotAfter"), &not_after_str)?;
    triples += 2;

    // Parse Fulcio OID extensions (1.3.6.1.4.1.57264.1.{8-13})
    // Extension values are ASN.1-encoded (TAG + LENGTH + VALUE). Fulcio uses UTF8String (tag 0x0C).
    for ext in cert.extensions() {
        let oid = ext.oid.to_id_string();
        let oid_ref = oid.as_str();

        // Parse ASN.1 UTF8String from extension value
        // DER format: TAG (1 byte, 0x0C for UTF8String) + LENGTH (1+ bytes) + VALUE (UTF-8 bytes)
        let value_str = match parse_asn1_utf8_string(ext.value) {
            Some(s) if !s.is_empty() => s,
            _ => continue, // Skip if not UTF8String or empty
        };

        match oid_ref {
            "1.3.6.1.4.1.57264.1.8" => {
                // fulcioIssuerV2 (OIDC issuer URI)
                writer.write_typed_literal(cert_uri, &format!("{ATT}fulcioIssuerV2"), &value_str, &format!("{XSD}anyURI"))?;
                triples += 1;
            }
            "1.3.6.1.4.1.57264.1.9" => {
                // buildSignerURI (workflow file path)
                writer.write_typed_literal(cert_uri, &format!("{ATT}buildSignerURI"), &value_str, &format!("{XSD}anyURI"))?;
                triples += 1;
            }
            "1.3.6.1.4.1.57264.1.10" => {
                // buildSignerDigest (workflow commit SHA)
                writer.write_literal(cert_uri, &format!("{ATT}buildSignerDigest"), &value_str)?;
                triples += 1;
            }
            "1.3.6.1.4.1.57264.1.11" => {
                // runnerEnvironment (github-hosted or self-hosted)
                writer.write_literal(cert_uri, &format!("{ATT}runnerEnvironment"), &value_str)?;
                triples += 1;
            }
            "1.3.6.1.4.1.57264.1.12" => {
                // sourceRepositoryURI
                writer.write_typed_literal(cert_uri, &format!("{ATT}sourceRepositoryURI"), &value_str, &format!("{XSD}anyURI"))?;
                triples += 1;
            }
            "1.3.6.1.4.1.57264.1.13" => {
                // sourceRepositoryDigest (source commit SHA)
                writer.write_literal(cert_uri, &format!("{ATT}sourceRepositoryDigest"), &value_str)?;
                triples += 1;
            }
            _ => {}
        }
    }

    Ok(triples)
}

/// Parse ASN.1 UTF8String from DER-encoded extension value.
///
/// DER format: TAG (1 byte, 0x0C for UTF8String) + LENGTH (variable) + VALUE (UTF-8 bytes)
/// Returns the decoded UTF-8 string, or None if the value is not a UTF8String or is malformed.
fn parse_asn1_utf8_string(der_bytes: &[u8]) -> Option<String> {
    if der_bytes.is_empty() {
        return None;
    }

    // Check tag (0x0C = UTF8String)
    if der_bytes[0] != 0x0C {
        return None;
    }

    // Parse length (simple case: single-byte length for strings < 128 bytes)
    if der_bytes.len() < 2 {
        return None;
    }

    let length_byte = der_bytes[1];
    let (value_start, value_len) = if length_byte & 0x80 == 0 {
        // Short form: length fits in 7 bits
        (2, length_byte as usize)
    } else {
        // Long form: length_byte & 0x7F = number of length octets
        let num_length_octets = (length_byte & 0x7F) as usize;
        if der_bytes.len() < 2 + num_length_octets {
            return None;
        }
        let mut len: usize = 0;
        for i in 0..num_length_octets {
            len = (len << 8) | der_bytes[2 + i] as usize;
        }
        (2 + num_length_octets, len)
    };

    if der_bytes.len() < value_start + value_len {
        return None;
    }

    let value_bytes = &der_bytes[value_start..value_start + value_len];
    std::str::from_utf8(value_bytes).ok().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_attestation_triple_emission() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let att_uri = attestation_uri("npm", "registry", "sigstore", "2.0.0");
        writer.write_triple(&att_uri, RDF_TYPE, &format!("{SLSA}ProvenanceAttestation")).unwrap();
        writer.write_literal(&att_uri, &format!("{SLSA}predicateType"),
            "https://slsa.dev/provenance/v1").unwrap();
        writer.write_triple(&att_uri, &format!("{SLSA}attestsBuildLevel"),
            &format!("{SLSA}L2")).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("slsa#ProvenanceAttestation"), "Should have ProvenanceAttestation type");
        assert!(content.contains("slsa#predicateType"), "Should have predicate type");
        assert!(content.contains("slsa.dev/provenance"), "Should reference SLSA provenance");
        assert!(content.contains("slsa#attestsBuildLevel"), "Should attest build level");
        assert!(content.contains("slsa#L2"), "Should be SLSA L2");
    }

    #[test]
    fn test_signing_triples_from_sigstore_bundle() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let att_uri = attestation_uri("npm", "registry", "sigstore", "2.0.0");

        let bundle = serde_json::json!({
            "dsseEnvelope": {
                "payload": "e30=",  // base64 for "{}"
                "signatures": [{"sig": "MEUCIQD+test", "keyid": ""}]
            },
            "verificationMaterial": {
                "x509CertificateChain": {
                    "certificates": [{"rawBytes": "MIIBtest"}]
                },
                "tlogEntries": [{
                    "logIndex": "12345",
                    "integratedTime": "1714000000"
                }]
            }
        });

        let count = emit_signing_triples(&mut writer, &att_uri, &bundle, "sigstore", "2.0.0").unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(count >= 14, "Should emit at least 14 triples, got {}", count);
        assert!(content.contains("attestation#DigitalSignature"), "Should have DigitalSignature type");
        assert!(content.contains("attestation#hasSignature"), "Should link attestation to signature");
        assert!(content.contains("attestation#SigstoreKeyless"), "Should use Sigstore keyless method");
        assert!(content.contains("attestation#signatureStatus"), "Should have verification status");
        assert!(content.contains("attestation#GitHubActionsOIDC"), "Should reference OIDC provider");
        assert!(content.contains("attestation#signatureValue"), "Should have DSSE signature");
        assert!(content.contains("attestation#TransparencyLogEntry"), "Should have tlog entry");
        assert!(content.contains("attestation#logIndex"), "Should have log index");
        assert!(content.contains("attestation#SigstorePublicGood"), "Should reference public good log");
        assert!(content.contains("attestation#integratedTime"), "Should have integrated time");
        assert!(content.contains("attestation#SigningCertificate"), "Should have certificate");
        assert!(content.contains("attestation#isEphemeralCertificate"), "Should mark as ephemeral");
        assert!(content.contains("attestation#SigstoreFulcio"), "Should reference Fulcio CA");
    }

    #[test]
    fn test_infrastructure_metadata_emission() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        emit_infrastructure_metadata(&mut writer).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Rekor metadata
        assert!(content.contains("attestation#SigstorePublicGood"), "Should have Rekor individual");
        assert!(content.contains("attestation#logUri"), "Should have log URI");
        assert!(content.contains("rekor.sigstore.dev"), "Should reference Rekor endpoint");
        assert!(content.contains("attestation#logPublicKey"), "Should have log public key");
        assert!(content.contains("BEGIN PUBLIC KEY"), "Should include PEM public key");

        // Fulcio metadata
        assert!(content.contains("attestation#SigstoreFulcio"), "Should have Fulcio individual");
        assert!(content.contains("attestation#caUri"), "Should have CA URI");
        assert!(content.contains("fulcio.sigstore.dev"), "Should reference Fulcio endpoint");
        assert!(content.contains("attestation#caRootCertFingerprint"), "Should have root cert fingerprint");
        assert!(content.contains("attestation#issuesEphemeralCertificates"), "Should mark as ephemeral CA");
    }

    #[test]
    fn test_fulcio_extension_parsing_with_asn1() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let att_uri = attestation_uri("npm", "registry", "test-package", "1.0.0");

        // Real npm Sigstore bundle structure with a simplified Fulcio certificate
        // The certificate rawBytes would be a full DER-encoded x509 cert in production.
        // For testing ASN.1 extension parsing, we verify parse_asn1_utf8_string() works
        // by constructing a minimal extension value with proper DER encoding.

        // Construct a test bundle with ASN.1-encoded extension values
        // UTF8String DER format: 0x0C (tag) + length + UTF-8 bytes
        let issuer_value = create_asn1_utf8_string("https://token.actions.githubusercontent.com");
        let workflow_value = create_asn1_utf8_string("https://github.com/example/repo/.github/workflows/release.yml@refs/heads/main");

        // Test parse_asn1_utf8_string directly
        let decoded_issuer = parse_asn1_utf8_string(&issuer_value);
        let decoded_workflow = parse_asn1_utf8_string(&workflow_value);

        assert_eq!(decoded_issuer, Some("https://token.actions.githubusercontent.com".to_string()));
        assert_eq!(decoded_workflow, Some("https://github.com/example/repo/.github/workflows/release.yml@refs/heads/main".to_string()));
    }

    /// Helper to create ASN.1 UTF8String DER encoding for testing.
    fn create_asn1_utf8_string(s: &str) -> Vec<u8> {
        let bytes = s.as_bytes();
        let len = bytes.len();
        let mut result = vec![0x0C]; // UTF8String tag

        if len < 128 {
            // Short form length
            result.push(len as u8);
        } else {
            // Long form length (for lengths >= 128)
            let len_bytes = if len < 256 {
                vec![(len & 0xFF) as u8]
            } else if len < 65536 {
                vec![((len >> 8) & 0xFF) as u8, (len & 0xFF) as u8]
            } else {
                vec![((len >> 16) & 0xFF) as u8, ((len >> 8) & 0xFF) as u8, (len & 0xFF) as u8]
            };
            result.push(0x80 | len_bytes.len() as u8);
            result.extend_from_slice(&len_bytes);
        }

        result.extend_from_slice(bytes);
        result
    }
}
