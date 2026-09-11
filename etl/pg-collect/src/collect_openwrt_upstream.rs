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
        Self {
            distro_name,
            release_name,
        }
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
                        // Only git sources can resolve to a canonical repo URL; archive
                        // sources have nothing to key on and keep the per-name fallback
                        // (this design's explicit non-goal -- see spec §1).
                        let forge_extraction = if meta.source_proto.as_deref() == Some("git") {
                            crate::forge::extract_forge_url(source_url)
                        } else {
                            None
                        };

                        let upstream_uri = match &forge_extraction {
                            Some(extraction) => crate::uris::upstream_uri(&extraction.repo_url),
                            None => upstream_uri(&format!("openwrt/{}", effective_name)),
                        };

                        writer.write_triple(
                            &upstream_uri,
                            RDF_TYPE,
                            &format!("{PKG}UpstreamProject"),
                        )?;
                        total_triples += 1;

                        // pkg:projectName (SHACL required)
                        let project_name = match &forge_extraction {
                            Some(extraction) => crate::uris::project_name_from_repo_url(&extraction.repo_url),
                            None => effective_name.clone(),
                        };
                        writer.write_literal(
                            &upstream_uri,
                            &format!("{PKG}projectName"),
                            &project_name,
                        )?;
                        total_triples += 1;

                        if meta.source_proto.as_deref() == Some("git") {
                            // Git source: link to VCS repository if a forge URL resolved.
                            // If it didn't, emit nothing further here -- unchanged from
                            // pre-migration behavior (no projectRepository, no projectUrl).
                            if let Some(extraction) = &forge_extraction {
                                let repo_uri = crate::uris::repo_uri(&extraction.repo_url);
                                writer.write_triple(
                                    &upstream_uri,
                                    &format!("{PKG}projectRepository"),
                                    &repo_uri,
                                )?;
                                total_triples += 1;
                            }
                        } else {
                            // Archive sources: emit download URL as projectUrl (unchanged).
                            writer.write_literal(
                                &upstream_uri,
                                &format!("{PKG}projectUrl"),
                                source_url,
                            )?;
                            total_triples += 1;
                        }

                        emitted_upstream.insert(effective_name.clone(), upstream_uri.clone());
                        upstream_uri
                    };

                    // Link THIS package (parent or sub-package) to the UpstreamProject
                    writer.write_triple(
                        source_pkg_uri,
                        &format!("{PKG}hasUpstreamProject"),
                        &upstream_uri,
                    )?;
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
        parsed_meta.insert(
            "foo".to_string(),
            OpenWrtPackageMeta {
                source_url: Some("https://github.com/example/foo.git".to_string()),
                source_proto: Some("git".to_string()),
                source_hash: Some("abc123".to_string()),
            },
        );

        let mut parent_map = HashMap::new();
        parent_map.insert("foo-utils".to_string(), "foo".to_string());

        let collector = OpenwrtUpstreamCollector::new("openwrt".into(), "24.10".into());
        let triples = collector
            .collect(&mut writer, &identity_map, &parsed_meta, &parent_map)
            .unwrap();

        assert!(triples > 0, "Should emit upstream triples");

        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // Should have UpstreamProject
        assert!(
            content.contains("UpstreamProject"),
            "Should emit UpstreamProject type"
        );

        // Should have projectName (SHACL required)
        assert!(content.contains("projectName"), "Should emit projectName");
        assert!(
            content.contains("\"example/foo\""),
            "Should derive projectName from the resolved repo URL's owner/repo slug"
        );

        // Should have hasUpstreamProject link
        assert!(
            content.contains("hasUpstreamProject"),
            "Should link via hasUpstreamProject"
        );

        // Should link to VCS repository
        assert!(
            content.contains("projectRepository"),
            "Should link to projectRepository"
        );

        // Both packages (parent and sub-package) should link to same UpstreamProject
        let upstream_count = content.matches("hasUpstreamProject").count();
        assert_eq!(
            upstream_count, 2,
            "Both foo and foo-utils should link to same UpstreamProject"
        );
    }

    #[test]
    fn test_upstream_project_distro_scoped_uri() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut identity_map = HashMap::new();
        let pkg_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/openssl/1.0";
        identity_map.insert("openssl".to_string(), pkg_uri.to_string());

        let mut parsed_meta = HashMap::new();
        parsed_meta.insert(
            "openssl".to_string(),
            OpenWrtPackageMeta {
                source_url: Some("https://www.openssl.org/source/openssl-1.0.tar.gz".to_string()),
                source_proto: Some("default".to_string()),
                source_hash: Some("def456".to_string()),
            },
        );

        let parent_map = HashMap::new();

        let collector = OpenwrtUpstreamCollector::new("openwrt".into(), "24.10".into());
        collector
            .collect(&mut writer, &identity_map, &parsed_meta, &parent_map)
            .unwrap();

        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // UpstreamProject URI should be distro-scoped (openwrt/openssl, not global openssl)
        assert!(
            content.contains("/upstream/openwrt%2Fopenssl"),
            "UpstreamProject URI should be distro-scoped"
        );
    }

    #[test]
    fn test_upstream_project_git_source_keyed_by_repo_not_name() {
        // Confirms the migration: a git source with a resolvable forge URL
        // is keyed the same way any other collector's hub link would be --
        // by canonical repo URL, not by "openwrt/{name}".
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut identity_map = HashMap::new();
        let pkg_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/bar/2.0";
        identity_map.insert("bar".to_string(), pkg_uri.to_string());

        let mut parsed_meta = HashMap::new();
        parsed_meta.insert(
            "bar".to_string(),
            OpenWrtPackageMeta {
                source_url: Some("https://github.com/example/bar.git".to_string()),
                source_proto: Some("git".to_string()),
                source_hash: Some("abc123".to_string()),
            },
        );

        let collector = OpenwrtUpstreamCollector::new("openwrt".into(), "24.10".into());
        collector
            .collect(&mut writer, &identity_map, &parsed_meta, &HashMap::new())
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        let expected_uri = crate::uris::upstream_uri("https://github.com/example/bar");
        assert!(
            content.contains(&expected_uri),
            "UpstreamProject should be keyed by canonical repo URL, not openwrt/{{name}}"
        );
        assert!(
            !content.contains("upstream/openwrt%2Fbar"),
            "Should NOT use the old per-name key when a forge URL resolves"
        );
    }

    #[test]
    fn test_upstream_project_git_source_unresolvable_keeps_name_keying() {
        // A git source whose URL doesn't match any known forge: falls back to
        // the existing per-name key, exactly as it did before this migration
        // (no projectRepository triple either -- there's nothing to link to).
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let mut identity_map = HashMap::new();
        let pkg_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/baz/1.0";
        identity_map.insert("baz".to_string(), pkg_uri.to_string());

        let mut parsed_meta = HashMap::new();
        parsed_meta.insert(
            "baz".to_string(),
            OpenWrtPackageMeta {
                source_url: Some("https://example-vcs.internal/baz.git".to_string()),
                source_proto: Some("git".to_string()),
                source_hash: Some("def456".to_string()),
            },
        );

        let collector = OpenwrtUpstreamCollector::new("openwrt".into(), "24.10".into());
        collector
            .collect(&mut writer, &identity_map, &parsed_meta, &HashMap::new())
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("upstream/openwrt%2Fbaz"));
        assert!(!content.contains("projectRepository"));
    }

    #[test]
    fn test_openwrt_and_forge_writer_converge_on_same_upstream_project_hub() {
        // This is the load-bearing test for the branch's entire cross-
        // ecosystem convergence claim (spec §8 gap): two different writers
        // -- OpenwrtUpstreamCollector (this module) and
        // forge::emit_upstream_project (the shared helper the three direct
        // writers + emit_upstream_repo's collectors route through) -- must
        // mint the *identical* pkg:UpstreamProject hub for the same
        // canonical repo URL, not just each in isolation.
        //
        // Reuses test_upstream_project_git_source_keyed_by_repo_not_name's
        // fixture: OpenWrt package "bar" with git source
        // "https://github.com/example/bar.git", which resolves to the
        // canonical repo URL "https://github.com/example/bar".

        // --- Writer 1: collect_openwrt_upstream.rs's collector ---
        let openwrt_file = NamedTempFile::new().unwrap();
        let mut openwrt_writer = NTriplesWriter::new(openwrt_file.reopen().unwrap());

        let mut identity_map = HashMap::new();
        let pkg_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/bar/2.0";
        identity_map.insert("bar".to_string(), pkg_uri.to_string());

        let mut parsed_meta = HashMap::new();
        parsed_meta.insert(
            "bar".to_string(),
            OpenWrtPackageMeta {
                source_url: Some("https://github.com/example/bar.git".to_string()),
                source_proto: Some("git".to_string()),
                source_hash: Some("abc123".to_string()),
            },
        );

        let collector = OpenwrtUpstreamCollector::new("openwrt".into(), "24.10".into());
        collector
            .collect(&mut openwrt_writer, &identity_map, &parsed_meta, &HashMap::new())
            .unwrap();
        openwrt_writer.flush().unwrap();

        let mut openwrt_content = String::new();
        openwrt_file
            .reopen()
            .unwrap()
            .read_to_string(&mut openwrt_content)
            .unwrap();

        // --- Writer 2: forge::emit_upstream_project, called directly with
        //     the same canonical repo URL the OpenWrt fixture resolves to
        //     (this function takes no identity parameter -- see forge.rs's
        //     doc comment on why it must not link any identity/package
        //     directly: hasUpstreamProject is rdfs:domain :SourcePackage,
        //     and a cross-ecosystem caller here would be a PackageIdentity). ---
        let forge_file = NamedTempFile::new().unwrap();
        let mut forge_writer = NTriplesWriter::new(forge_file.reopen().unwrap());

        let canonical_repo_url = "https://github.com/example/bar";
        crate::forge::emit_upstream_project(&mut forge_writer, canonical_repo_url).unwrap();
        forge_writer.flush().unwrap();

        let mut forge_content = String::new();
        forge_file
            .reopen()
            .unwrap()
            .read_to_string(&mut forge_content)
            .unwrap();

        // Both writers must agree on the exact same UpstreamProject subject
        // IRI -- computed independently here via the same upstream_uri()
        // helper both code paths use under the hood, so this assertion
        // fails if either writer's keying ever drifts.
        let expected_hub_uri = crate::uris::upstream_uri(canonical_repo_url);
        assert!(
            openwrt_content.contains(&expected_hub_uri),
            "OpenWrt writer's output should reference the shared hub IRI {expected_hub_uri}\ngot:\n{openwrt_content}"
        );
        assert!(
            forge_content.contains(&expected_hub_uri),
            "forge writer's output should reference the shared hub IRI {expected_hub_uri}\ngot:\n{forge_content}"
        );

        // And they must agree on the same pkg:projectName literal value --
        // the human-readable slug derived from the canonical repo URL.
        let expected_project_name_triple = format!(
            "<{expected_hub_uri}> <{PKG}projectName> \"example/bar\" .",
            PKG = "https://purl.org/packagegraph/ontology/core#"
        );
        assert!(
            openwrt_content.contains(&expected_project_name_triple),
            "OpenWrt writer should emit the expected projectName triple\ngot:\n{openwrt_content}"
        );
        assert!(
            forge_content.contains(&expected_project_name_triple),
            "forge writer should emit the identical projectName triple\ngot:\n{forge_content}"
        );

        // Sanity: the two writers' overall output is NOT byte-identical
        // (identity subjects differ) -- confirming this test compares only
        // the shared hub-node lines, not accidentally identical files.
        assert_ne!(
            openwrt_content, forge_content,
            "Full outputs should differ (different identity subjects) even though the hub converges"
        );

        // The OpenWrt writer's subject is a genuine SourcePackage (OpkgPackage
        // -> SourcePackage -> Package), so its own hasUpstreamProject edge is
        // domain-conformant and expected here. forge::emit_upstream_project
        // takes no identity/package argument at all and must never emit that
        // predicate -- it would be a PackageIdentity subject for every one of
        // its real callers, which violates hasUpstreamProject's declared
        // rdfs:domain :SourcePackage.
        assert!(openwrt_content.contains("hasUpstreamProject"));
        assert!(!forge_content.contains("hasUpstreamProject"));
    }
}
