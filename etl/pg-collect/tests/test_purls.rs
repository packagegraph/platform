//! PURL contracts for the collectors with supported package coordinates.
use packageurl::PackageUrl;
use pg_collect::debian::DebianCollector;
use pg_collect::ntriples::{format_purl, NTriplesWriter};
use pg_collect::rpm::{RpmCollector, RpmPackageData};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use tempfile::NamedTempFile;

fn purls(nt: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut result = BTreeMap::<_, BTreeSet<_>>::new();
    for line in nt.lines().filter(|line| line.contains("/core#purl>")) {
        let (subject, rest) = line.split_once(' ').unwrap();
        let value = rest.split('"').nth(1).unwrap();
        assert!(line.ends_with("\"^^<http://www.w3.org/2001/XMLSchema#anyURI> ."));
        let parsed = PackageUrl::from_str(value).expect("collector PURL must parse");
        assert_eq!(
            parsed.to_string(),
            value,
            "collector PURL must be canonical"
        );
        result
            .entry(subject.to_string())
            .or_default()
            .insert(value.to_string());
    }
    result
}

fn save_fixture(name: &str, nt: &str) {
    if let Some(dir) = std::env::var_os("PURL_FIXTURE_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(std::path::Path::new(&dir).join(name), nt).unwrap();
    }
}

#[test]
fn formatter_canonicalizes_components_and_qualifier_order() {
    let cases = [
        (
            "RPM",
            Some("Fedora"),
            "LibDemo",
            Some("1.0-2.fc43"),
            vec![("Epoch", "2"), ("ARCH", "x86_64")],
            "pkg:rpm/fedora/LibDemo@1.0-2.fc43?arch=x86_64&epoch=2",
        ),
        (
            "deb",
            Some("Debian"),
            "LibDemo++",
            Some("2:1.0-1+b1"),
            vec![("arch", "amd64")],
            "pkg:deb/debian/libdemo%2B%2B@2:1.0-1%2Bb1?arch=amd64",
        ),
        (
            "maven",
            Some("org.Example"),
            "Café+lib",
            Some("1.0+build@next"),
            vec![],
            "pkg:maven/org.Example/Caf%C3%A9%2Blib@1.0%2Bbuild%40next",
        ),
        (
            "generic",
            Some("team/components"),
            "demo",
            None,
            vec![("classifier", "native+debug&x=1"), ("arch", "any")],
            "pkg:generic/team/components/demo?arch=any&classifier=native%2Bdebug%26x%3D1",
        ),
    ];
    for (kind, namespace, name, version, qualifiers, expected) in cases {
        let actual = format_purl(kind, namespace, name, version, &qualifiers);
        assert_eq!(actual, expected);
        let parsed = PackageUrl::from_str(&actual).unwrap();
        assert_eq!(parsed.to_string(), actual);
        assert_eq!(parsed.version(), version);
    }
}

#[test]
fn rpm_two_versions_share_one_identity_purl_and_keep_source_coordinates() {
    let collector = RpmCollector::new("https://example.org".into(), "fedora".into(), "43".into());
    let temp = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp.reopen().unwrap());
    let mut emitted = HashSet::new();
    let mut triples = 0;
    for (version, epoch, source) in [
        ("1.0", "1", "demo-1.0-1.fc43.src.rpm"),
        ("2.0", "2", "demo-2.0-1.fc43.src.rpm"),
    ] {
        let fields = HashMap::from([
            ("name", "LibDemo"),
            ("arch", "x86_64"),
            ("ver", version),
            ("rel", "1.fc43"),
            ("epoch", epoch),
            ("sourcerpm", source),
        ])
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        triples += collector
            .emit_package_triples(
                &mut writer,
                &RpmPackageData {
                    fields,
                    deps: vec![],
                },
                None,
                &mut emitted,
            )
            .unwrap()
            .0;
    }
    writer.flush().unwrap();
    let nt = std::fs::read_to_string(temp.path()).unwrap();
    save_fixture("rpm-two-versions.nt", &nt);
    assert_eq!(nt.lines().count(), triples + writer.auto_inverses);
    let actual = purls(&nt);
    let expected = BTreeMap::from([
        (
            "<https://packagegraph.github.io/d/pkg/fedora/43/x86_64/LibDemo>",
            vec!["pkg:rpm/fedora/LibDemo?arch=x86_64"],
        ),
        (
            "<https://packagegraph.github.io/d/pkg/fedora/43/x86_64/LibDemo/1.0-1.fc43.x86_64>",
            vec!["pkg:rpm/fedora/LibDemo@1.0-1.fc43?arch=x86_64&epoch=1"],
        ),
        (
            "<https://packagegraph.github.io/d/pkg/fedora/43/x86_64/LibDemo/2.0-1.fc43.x86_64>",
            vec!["pkg:rpm/fedora/LibDemo@2.0-1.fc43?arch=x86_64&epoch=2"],
        ),
        (
            "<https://packagegraph.github.io/d/src/fedora/43/demo/1.0-1.fc43>",
            vec!["pkg:rpm/fedora/demo@1.0-1.fc43?arch=src"],
        ),
        (
            "<https://packagegraph.github.io/d/src/fedora/43/demo/2.0-1.fc43>",
            vec!["pkg:rpm/fedora/demo@2.0-1.fc43?arch=src"],
        ),
    ])
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.into_iter().map(String::from).collect()))
    .collect();
    assert_eq!(actual, expected);
}

#[test]
fn debian_two_versions_share_one_identity_purl_and_keep_source_version() {
    let collector = DebianCollector::new(
        "https://example.org".into(),
        "debian".into(),
        "stable".into(),
        "main".into(),
    );
    let temp = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp.reopen().unwrap());
    let mut triples = 0;
    for (version, source) in [
        ("2:1.0-1+b1", "demo (2:1.0-1)"),
        ("2:2.0-1+b2", "demo (2:2.0-1)"),
    ] {
        let fields = HashMap::from([
            ("Package", "libdemo"),
            ("Version", version),
            ("Source", source),
        ])
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        triples += collector
            .emit_package_triples(&mut writer, &fields, "trixie", "stable", "amd64")
            .unwrap();
    }
    writer.flush().unwrap();
    let nt = std::fs::read_to_string(temp.path()).unwrap();
    save_fixture("debian-two-versions.nt", &nt);
    assert_eq!(nt.lines().count(), triples + writer.auto_inverses);
    let actual = purls(&nt);
    let expected = BTreeMap::from([
        (
            "<https://packagegraph.github.io/d/pkg/debian/trixie/amd64/libdemo>",
            vec!["pkg:deb/debian/libdemo?arch=amd64"],
        ),
        (
            "<https://packagegraph.github.io/d/pkg/debian/trixie/amd64/libdemo/2%3A1.0-1%2Bb1>",
            vec!["pkg:deb/debian/libdemo@2:1.0-1%2Bb1?arch=amd64"],
        ),
        (
            "<https://packagegraph.github.io/d/pkg/debian/trixie/amd64/libdemo/2%3A2.0-1%2Bb2>",
            vec!["pkg:deb/debian/libdemo@2:2.0-1%2Bb2?arch=amd64"],
        ),
        (
            "<https://packagegraph.github.io/d/src/debian/trixie/demo/2%3A1.0-1>",
            vec!["pkg:deb/debian/demo@2:1.0-1?arch=source"],
        ),
        (
            "<https://packagegraph.github.io/d/src/debian/trixie/demo/2%3A2.0-1>",
            vec!["pkg:deb/debian/demo@2:2.0-1?arch=source"],
        ),
    ])
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.into_iter().map(String::from).collect()))
    .collect();
    assert_eq!(actual, expected);
}

#[test]
fn rpm_nosrc_purl_uses_source_architecture_and_omits_unknown_epoch() {
    let collector = RpmCollector::new("https://example.org".into(), "fedora".into(), "43".into());
    let temp = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp.reopen().unwrap());
    let fields = HashMap::from([
        ("name", "demo"),
        ("arch", "noarch"),
        ("ver", "1.0"),
        ("rel", "1.fc43"),
        ("epoch", "0"),
        ("sourcerpm", "demo-1.0-1.fc43.nosrc.nosrc.rpm"),
    ])
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    collector
        .emit_package_triples(
            &mut writer,
            &RpmPackageData {
                fields,
                deps: vec![],
            },
            None,
            &mut HashSet::new(),
        )
        .unwrap();
    writer.flush().unwrap();
    let nt = std::fs::read_to_string(temp.path()).unwrap();
    let actual = purls(&nt);
    let values: BTreeSet<_> = actual
        .values()
        .flat_map(|values| values.iter().map(String::as_str))
        .collect();
    assert_eq!(
        values,
        BTreeSet::from([
            "pkg:rpm/fedora/demo?arch=noarch",
            "pkg:rpm/fedora/demo@1.0-1.fc43?arch=noarch",
            "pkg:rpm/fedora/demo@1.0-1.fc43.nosrc?arch=nosrc",
        ])
    );
}

#[test]
fn ir_replay_preserves_rpm_and_debian_purl_coordinates() {
    use pg_collect::emit::rdf::{emit_rdf, EmitPolicy};
    use pg_collect::ir::ScopeIr;
    use pg_collect::normalize::{debian::normalize_debian_package, rpm::normalize_rpm_package};

    let rpm_fields = HashMap::from([
        ("name", "demo"),
        ("arch", "noarch"),
        ("ver", "2.0"),
        ("rel", "3.fc43"),
        ("epoch", "2"),
        ("sourcerpm", "demo-1.0-1.fc43.nosrc.nosrc.rpm"),
    ])
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let rpm_scope = ScopeIr {
        collector: "rpm".into(),
        distro: "fedora".into(),
        release: "43".into(),
        repo: None,
        arch: "noarch".into(),
    };
    let rpm = normalize_rpm_package(
        &RpmPackageData {
            fields: rpm_fields,
            deps: vec![],
        },
        &rpm_scope,
        "fixture",
    )
    .unwrap();
    let deb_fields = HashMap::from([
        ("Package", "demo"),
        ("Architecture", "all"),
        ("Version", "2:2.0-1+b2"),
        ("Source", "demo-source (2:1.0-1)"),
        ("Depends", "virtual-package"),
    ])
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let deb_scope = ScopeIr {
        collector: "debian".into(),
        distro: "debian".into(),
        release: "trixie".into(),
        repo: Some("main".into()),
        arch: "amd64".into(),
    };
    let deb = normalize_debian_package(&deb_fields, &deb_scope, "fixture").unwrap();
    for (ir, expected) in [
        (rpm, BTreeMap::from([
            ("<https://packagegraph.github.io/d/pkg/fedora/43/noarch/demo>", "pkg:rpm/fedora/demo?arch=noarch"),
            ("<https://packagegraph.github.io/d/pkg/fedora/43/noarch/demo/2.0-3.fc43.noarch>", "pkg:rpm/fedora/demo@2.0-3.fc43?arch=noarch&epoch=2"),
            ("<https://packagegraph.github.io/d/src/fedora/43/demo/1.0>", "pkg:rpm/fedora/demo@1.0-1.fc43.nosrc?arch=nosrc"),
        ])),
        (deb, BTreeMap::from([
            ("<https://packagegraph.github.io/d/pkg/debian/trixie/all/demo>", "pkg:deb/debian/demo?arch=all"),
            ("<https://packagegraph.github.io/d/pkg/debian/trixie/all/demo/2%3A2.0-1%2Bb2>", "pkg:deb/debian/demo@2:2.0-1%2Bb2?arch=all"),
            ("<https://packagegraph.github.io/d/src/debian/trixie/demo-source/2%3A1.0-1>", "pkg:deb/debian/demo-source@2:1.0-1?arch=source"),
        ])),
    ] {
        let temp = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp.reopen().unwrap());
        let triples = emit_rdf(&ir, &mut writer, &EmitPolicy::default()).unwrap();
        writer.flush().unwrap();
        let nt = std::fs::read_to_string(temp.path()).unwrap();
        save_fixture(&format!("{}-ir.nt", ir.scope.collector), &nt);
        assert_eq!(nt.lines().count(), triples + writer.auto_inverses);
        let expected = expected.into_iter().map(|(subject, value)| (subject.to_string(), BTreeSet::from([value.to_string()]))).collect();
        assert_eq!(purls(&nt), expected);
    }
}

#[test]
fn purl_writer_rejects_distinct_values_but_allows_repeated_statements() {
    for graph in [None, Some("https://example.org/graph")] {
        let temp = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new_maybe_graph(temp.reopen().unwrap(), graph);
        let subject = "https://example.org/package";
        let predicate = "https://purl.org/packagegraph/ontology/core#purl";
        let datatype = "http://www.w3.org/2001/XMLSchema#anyURI";
        writer
            .write_typed_literal(subject, predicate, "pkg:rpm/fedora/demo@1", datatype)
            .unwrap();
        writer
            .write_typed_literal(subject, predicate, "pkg:rpm/fedora/demo@1", datatype)
            .unwrap();
        let error = writer
            .write_typed_literal(subject, predicate, "pkg:rpm/fedora/demo@2", datatype)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(subject));
        writer.flush().unwrap();
        let nt = std::fs::read_to_string(temp.path()).unwrap();
        assert_eq!(nt.lines().count(), 2);
        assert!(!nt.contains("demo@2"));
    }
}

#[test]
fn rpm_epoch_collisions_fail_direct_collection_and_ir_replay() {
    use pg_collect::emit::rdf::{emit_rdf, EmitPolicy};
    use pg_collect::ir::ScopeIr;
    use pg_collect::normalize::rpm::normalize_rpm_package;
    for replay in [false, true] {
        let collector =
            RpmCollector::new("https://example.org".into(), "fedora".into(), "43".into());
        let scope = ScopeIr {
            collector: "rpm".into(),
            distro: "fedora".into(),
            release: "43".into(),
            repo: None,
            arch: "x86_64".into(),
        };
        let temp = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp.reopen().unwrap());
        for epoch in ["1", "2"] {
            let fields = HashMap::from([
                ("name", "demo"),
                ("arch", "x86_64"),
                ("ver", "1.0"),
                ("rel", "1.fc43"),
                ("epoch", epoch),
            ])
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
            let data = RpmPackageData {
                fields,
                deps: vec![],
            };
            let result = if replay {
                emit_rdf(
                    &normalize_rpm_package(&data, &scope, "fixture").unwrap(),
                    &mut writer,
                    &EmitPolicy::default(),
                )
            } else {
                collector
                    .emit_package_triples(&mut writer, &data, None, &mut HashSet::new())
                    .map(|(triples, _ecosystem_from_provides)| triples)
            };
            if epoch == "1" {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                assert!(error.to_string().contains("demo/1.0-1.fc43.x86_64"));
            }
        }
    }
}

#[test]
fn rpm_ir_source_release_collisions_fail_instead_of_publishing_two_purls() {
    use pg_collect::emit::rdf::{emit_rdf, EmitPolicy};
    use pg_collect::ir::ScopeIr;
    use pg_collect::normalize::rpm::normalize_rpm_package;
    let scope = ScopeIr {
        collector: "rpm".into(),
        distro: "fedora".into(),
        release: "43".into(),
        repo: None,
        arch: "x86_64".into(),
    };
    let temp = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp.reopen().unwrap());
    for (release, source) in [
        ("1.fc43", "demo-1.0-1.fc43.src.rpm"),
        ("2.fc43", "demo-1.0-2.fc43.src.rpm"),
    ] {
        let fields = HashMap::from([
            ("name", "demo"),
            ("arch", "x86_64"),
            ("ver", "1.0"),
            ("rel", release),
            ("sourcerpm", source),
        ])
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let ir = normalize_rpm_package(
            &RpmPackageData {
                fields,
                deps: vec![],
            },
            &scope,
            "fixture",
        )
        .unwrap();
        let result = emit_rdf(&ir, &mut writer, &EmitPolicy::default());
        if release == "1.fc43" {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("src/fedora/43/demo/1.0"));
        }
    }
}

#[test]
fn debian_purls_use_declared_architecture_and_reject_identity_arch_changes() {
    let collector = DebianCollector::new(
        "https://example.org".into(),
        "debian".into(),
        "stable".into(),
        "main".into(),
    );
    let temp = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp.reopen().unwrap());
    let mut fields: HashMap<String, String> = HashMap::from([
        ("Package", "demo"),
        ("Version", "1.0"),
        ("Architecture", "all"),
    ])
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    collector
        .emit_package_triples(&mut writer, &fields, "trixie", "stable", "amd64")
        .unwrap();
    writer.flush().unwrap();
    let actual = purls(&std::fs::read_to_string(temp.path()).unwrap());
    assert_eq!(
        actual["<https://packagegraph.github.io/d/pkg/debian/trixie/amd64/demo>"],
        BTreeSet::from(["pkg:deb/debian/demo?arch=all".to_string()])
    );
    assert_eq!(
        actual["<https://packagegraph.github.io/d/pkg/debian/trixie/amd64/demo/1.0>"],
        BTreeSet::from(["pkg:deb/debian/demo@1.0?arch=all".to_string()])
    );
    fields.insert("Architecture".into(), "amd64".into());
    fields.insert("Version".into(), "2.0".into());
    let error = collector
        .emit_package_triples(&mut writer, &fields, "trixie", "stable", "amd64")
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn rpm_cli_exits_unsuccessfully_when_package_coordinates_conflict() {
    use std::io::Write;
    let mut server = mockito::Server::new();
    let _repomd = server.mock("GET", "/repodata/repomd.xml").with_status(200).with_body(
        r#"<repomd xmlns="http://linux.duke.edu/metadata/repo"><data type="primary"><location href="repodata/primary.xml.gz"/></data></repomd>"#,
    ).create();
    let primary = r#"<metadata xmlns="http://linux.duke.edu/metadata/common" packages="2">
        <package type="rpm"><name>demo</name><arch>x86_64</arch><version epoch="1" ver="1.0" rel="1.fc43"/></package>
        <package type="rpm"><name>demo</name><arch>x86_64</arch><version epoch="2" ver="1.0" rel="1.fc43"/></package>
        </metadata>"#;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(primary.as_bytes()).unwrap();
    let _primary = server
        .mock("GET", "/repodata/primary.xml.gz")
        .with_status(200)
        .with_body(gzip.finish().unwrap())
        .create();
    for (option, value) in [
        ("--repo", server.url()),
        ("--rpm-repo", format!("fedora:43:{}", server.url())),
    ] {
        let temp = NamedTempFile::new().unwrap();
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_pg-collect"))
            .args([
                "rpm",
                option,
                &value,
                "--output",
                temp.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            !result.status.success(),
            "{option} accepted conflicting PURLs"
        );
        assert!(String::from_utf8_lossy(&result.stderr).contains("conflicting PURLs"));
    }
}
