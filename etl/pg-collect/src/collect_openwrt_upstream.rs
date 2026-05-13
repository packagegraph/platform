use crate::ntriples::NTriplesWriter;
use crate::openwrt::OpenWrtPackageMeta;
use crate::uris::*;
use std::collections::HashMap;
use std::io::Result;

/// Creates UpstreamProject entities from OpenWrt Makefile source URLs
pub struct OpenwrtUpstreamCollector {
    distro_name: String,
    release_name: String,
}

impl OpenwrtUpstreamCollector {
    pub fn new(distro_name: String, release_name: String) -> Self {
        Self { distro_name, release_name }
    }

    pub fn collect(
        &self,
        writer: &mut NTriplesWriter,
        identity_map: &HashMap<String, String>,
        parsed_meta: &HashMap<String, OpenWrtPackageMeta>,
        parent_map: &HashMap<String, String>,
    ) -> Result<usize> {
        let mut total_triples = 0;
        let mut emitted_upstream: HashMap<String, String> = HashMap::new();

        for (pkg_name, source_pkg_uri) in identity_map {
            // Resolve to parent if this is a sub-package
            let effective_name = parent_map.get(pkg_name).unwrap_or(pkg_name);

            // Get source metadata (from parent if sub-package)
            if let Some(meta) = parsed_meta.get(effective_name) {
                if let Some(ref source_url) = meta.source_url {
                    // Check if we already created the UpstreamProject for this parent
                    let upstream_uri = if let Some(existing_uri) = emitted_upstream.get(effective_name) {
                        // Reuse existing UpstreamProject URI
                        existing_uri.clone()
                    } else {
                        // Create new UpstreamProject entity
                        let upstream_uri = upstream_uri(&format!("openwrt/{}", effective_name));

                        writer.write_triple(&upstream_uri, RDF_TYPE, &format!("{PKG}UpstreamProject"))?;
                        total_triples += 1;

                        // pkg:projectName (SHACL required)
                        writer.write_literal(&upstream_uri, &format!("{PKG}projectName"), effective_name)?;
                        total_triples += 1;

                        // For git sources: link to VCS repository (reuses forge URI from Stage 1)
                        if meta.source_proto.as_deref() == Some("git") {
                            if let Some(extraction) = crate::forge::extract_forge_url(source_url) {
                                let repo_uri = crate::uris::repo_uri(&extraction.repo_url);
                                writer.write_triple(&upstream_uri, &format!("{PKG}projectRepository"), &repo_uri)?;
                                total_triples += 1;
                            }
                        } else {
                            // For archive sources: emit download URL as projectUrl
                            writer.write_literal(&upstream_uri, &format!("{PKG}projectUrl"), source_url)?;
                            total_triples += 1;
                        }

                        emitted_upstream.insert(effective_name.clone(), upstream_uri.clone());
                        upstream_uri
                    };

                    // Link THIS package (parent or sub-package) to the UpstreamProject
                    writer.write_triple(source_pkg_uri, &format!("{PKG}hasUpstreamProject"), &upstream_uri)?;
                    total_triples += 1;
                }
            }
        }

        Ok(total_triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_upstream_project_with_subpackages() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // Setup: parent package "foo" with sub-package "foo-utils"
        let mut identity_map = HashMap::new();
        let foo_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/foo/1.0";
        let foo_utils_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/foo-utils/1.0";
        identity_map.insert("foo".to_string(), foo_uri.to_string());
        identity_map.insert("foo-utils".to_string(), foo_utils_uri.to_string());

        let mut parsed_meta = HashMap::new();
        parsed_meta.insert("foo".to_string(), OpenWrtPackageMeta {
            source_url: Some("https://github.com/example/foo.git".to_string()),
            source_proto: Some("git".to_string()),
            source_hash: Some("abc123".to_string()),
        });

        let mut parent_map = HashMap::new();
        parent_map.insert("foo-utils".to_string(), "foo".to_string());

        let collector = OpenwrtUpstreamCollector::new("openwrt".into(), "24.10".into());
        let triples = collector.collect(&mut writer, &identity_map, &parsed_meta, &parent_map).unwrap();

        assert!(triples > 0, "Should emit upstream triples");

        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should have UpstreamProject
        assert!(content.contains("UpstreamProject"), "Should emit UpstreamProject type");

        // Should have projectName (SHACL required)
        assert!(content.contains("projectName"), "Should emit projectName");
        assert!(content.contains("\"foo\""), "Should use parent name for projectName");

        // Should have hasUpstreamProject link
        assert!(content.contains("hasUpstreamProject"), "Should link via hasUpstreamProject");

        // Should link to VCS repository
        assert!(content.contains("projectRepository"), "Should link to projectRepository");

        // Both packages (parent and sub-package) should link to same UpstreamProject
        let upstream_count = content.matches("hasUpstreamProject").count();
        assert_eq!(upstream_count, 2, "Both foo and foo-utils should link to same UpstreamProject");
    }

    #[test]
    fn test_upstream_project_distro_scoped_uri() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut identity_map = HashMap::new();
        let pkg_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/openssl/1.0";
        identity_map.insert("openssl".to_string(), pkg_uri.to_string());

        let mut parsed_meta = HashMap::new();
        parsed_meta.insert("openssl".to_string(), OpenWrtPackageMeta {
            source_url: Some("https://www.openssl.org/source/openssl-1.0.tar.gz".to_string()),
            source_proto: Some("default".to_string()),
            source_hash: Some("def456".to_string()),
        });

        let parent_map = HashMap::new();

        let collector = OpenwrtUpstreamCollector::new("openwrt".into(), "24.10".into());
        collector.collect(&mut writer, &identity_map, &parsed_meta, &parent_map).unwrap();

        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // UpstreamProject URI should be distro-scoped (openwrt/openssl, not global openssl)
        assert!(content.contains("/upstream/openwrt%2Fopenssl"), "UpstreamProject URI should be distro-scoped");
    }
}
