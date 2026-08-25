use crate::cache::{FileCache, MinioConfig};
use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use std::collections::HashMap;
use std::io::Result;
use std::time::Duration;

/// GitHub SLSA attestation enricher for OpenWrt binary packages
pub struct OpenwrtAttestationEnricher {
    github_token: Option<String>,
    cache: Option<FileCache>,
}

impl OpenwrtAttestationEnricher {
    pub fn new(
        github_token: Option<String>,
        cache_dir: Option<&str>,
        minio_config: Option<MinioConfig>,
    ) -> std::io::Result<Self> {
        let cache = if let Some(dir) = cache_dir {
            Some(FileCache::new(
                dir,
                "openwrt-attestation",
                8760,
                minio_config,
            )?)
        } else {
            None
        };

        Ok(Self {
            github_token,
            cache,
        })
    }

    pub fn enrich(
        &self,
        writer: &mut NTriplesWriter,
        digest_map: &HashMap<String, String>,
    ) -> Result<usize> {
        let mut total_triples = 0;

        eprintln!("GitHub Attestation Check: OpenWrt does not publish attestations yet (verified 2026-04-27).");
        eprintln!(
            "Enricher ready for when openwrt/openwrt adopts actions/attest-build-provenance."
        );

        // Stub: when OpenWrt starts publishing attestations, iterate digest_map and call GitHub API
        // for (digest_key, binary_uri) in digest_map {
        //     if digest_key.starts_with("sha256:") {
        //         let hex = &digest_key[7..];
        //         total_triples += self.fetch_and_emit_attestation(writer, hex, binary_uri)?;
        //     }
        // }

        Ok(total_triples)
    }

    // Placeholder for future implementation
    #[allow(dead_code)]
    fn fetch_and_emit_attestation(
        &self,
        writer: &mut NTriplesWriter,
        sha256_hex: &str,
        binary_uri: &str,
    ) -> Result<usize> {
        // Will implement: GET /repos/openwrt/openwrt/attestations/sha256:<hex>
        // Parse attestations array, decode DSSE, emit ProvenanceAttestation + BuildActivity chain
        // Follow enrich_npm_provenance.rs pattern
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_enricher_ready_for_adoption() {
        // Verify enricher can be constructed and called (returns 0 for now)
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let enricher = OpenwrtAttestationEnricher::new(None, None, None).unwrap();

        let mut digest_map = HashMap::new();
        digest_map.insert(
            "sha256:abc123".to_string(),
            "https://packagegraph.github.io/d/pkg/openwrt/24.10/mips_24kc/test/1.0".to_string(),
        );

        let triples = enricher.enrich(&mut writer, &digest_map).unwrap();

        assert_eq!(
            triples, 0,
            "Should return 0 triples until OpenWrt publishes attestations"
        );
    }
}
