//! Golden dataset test — verifies that IR → N-Triples emission produces
//! correct, deterministic output for known inputs.
//!
//! This test creates IR fixtures in memory, emits N-Triples via the shared
//! emitter, and verifies the output contains expected triples.

use pg_collect::emit::rdf::{emit_distribution_metadata, emit_rdf, EmitPolicy};
use pg_collect::ir::*;
use pg_collect::ntriples::NTriplesWriter;
use std::collections::BTreeMap;
use std::io::Read;
use tempfile::NamedTempFile;

fn fedora_package_ir() -> PackageIr {
    PackageIr {
        ir_schema: IR_SCHEMA_VERSION,
        scope: ScopeIr {
            collector: "rpm".to_string(),
            distro: "fedora".to_string(),
            release: "43".to_string(),
            repo: Some("fedora".to_string()),
            arch: "x86_64".to_string(),
        },
        source_artifacts: {
            let mut m = BTreeMap::new();
            m.insert("primary".to_string(), "sha256:abc".to_string());
            m
        },
        package: PackageInfo {
            kind: "binary".to_string(),
            name: "openssl".to_string(),
            epoch: 1,
            version: "3.2.1".to_string(),
            release: Some("1.fc43".to_string()),
            full_version: "3.2.1-1.fc43.x86_64".to_string(),
            arch: "x86_64".to_string(),
        },
        source_package: Some(SourcePackageRef {
            name: "openssl".to_string(),
            version: Some("3.2.1".to_string()),
            release: Some("1.fc43".to_string()),
        }),
        maintainers: vec![MaintainerIr {
            name: "Tomáš Mráz".to_string(),
            email: Some("tmraz@fedoraproject.org".to_string()),
            role_hint: Some("maintainer".to_string()),
        }],
        dependencies: vec![
            DependencyIr {
                name: "ca-certificates".to_string(),
                dep_type: "requires".to_string(),
                version_constraint: None,
                flags: None,
            },
            DependencyIr {
                name: "openssl-libs".to_string(),
                dep_type: "requires".to_string(),
                version_constraint: Some("= 3.2.1-1.fc43".to_string()),
                flags: Some("EQ".to_string()),
            },
        ],
        metadata: Some(PackageMetadataIr {
            summary: Some("TLS toolkit and library".to_string()),
            description: Some("OpenSSL is a toolkit for TLS/SSL protocols".to_string()),
            homepage: Some("https://github.com/openssl/openssl".to_string()),
            license: Some("Apache-2.0".to_string()),
            checksum: None,
            size_bytes: Some(2345678),
        }),
        collector_specific: Some(serde_json::json!({
            "source_rpm": "openssl-3.2.1-1.fc43.src.rpm",
            "rpm_epoch": 1
        })),
    }
}

fn debian_package_ir() -> PackageIr {
    PackageIr {
        ir_schema: IR_SCHEMA_VERSION,
        scope: ScopeIr {
            collector: "debian".to_string(),
            distro: "debian".to_string(),
            release: "trixie".to_string(),
            repo: Some("main".to_string()),
            arch: "amd64".to_string(),
        },
        source_artifacts: BTreeMap::new(),
        package: PackageInfo {
            kind: "binary".to_string(),
            name: "libc6".to_string(),
            epoch: 0,
            version: "2.36-9+deb13u1".to_string(),
            release: None,
            full_version: "2.36-9+deb13u1".to_string(),
            arch: "amd64".to_string(),
        },
        source_package: Some(SourcePackageRef {
            name: "glibc".to_string(),
            version: Some("2.36-9+deb13u1".to_string()),
            release: None,
        }),
        maintainers: vec![MaintainerIr {
            name: "GNU Libc Maintainers".to_string(),
            email: Some("debian-glibc@lists.debian.org".to_string()),
            role_hint: Some("maintainer".to_string()),
        }],
        dependencies: vec![DependencyIr {
            name: "libgcc-s1".to_string(),
            dep_type: "requires".to_string(),
            version_constraint: None,
            flags: None,
        }],
        metadata: Some(PackageMetadataIr {
            summary: Some("GNU C Library: Shared libraries".to_string()),
            description: None,
            homepage: None,
            license: None,
            checksum: None,
            size_bytes: None,
        }),
        collector_specific: Some(serde_json::json!({
            "suite": "testing",
            "component": "main"
        })),
    }
}

#[test]
fn test_golden_fedora_package_emission() {
    let temp_file = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

    let ir = fedora_package_ir();
    let policy = EmitPolicy::default();

    // Emit distribution metadata
    emit_distribution_metadata(&mut writer, "fedora", "43", "Fedora").unwrap();
    // Emit package
    let triples = emit_rdf(&ir, &mut writer, &policy).unwrap();
    writer.flush().unwrap();

    assert!(
        triples > 15,
        "Fedora package should emit at least 15 triples, got {}",
        triples
    );

    let mut content = String::new();
    temp_file
        .reopen()
        .unwrap()
        .read_to_string(&mut content)
        .unwrap();

    // Distribution metadata
    assert!(
        content.contains("core#Distribution"),
        "Missing Distribution type"
    );
    assert!(content.contains("\"Fedora\""), "Missing Fedora label");
    assert!(
        content.contains("core#releaseVersion"),
        "43 is numeric → releaseVersion"
    );
    assert!(
        content.contains("core#hasRelease"),
        "Missing hasRelease inverse"
    );

    // Package identity
    assert!(
        content.contains("core#BinaryPackage"),
        "Missing BinaryPackage type"
    );
    assert!(
        content.contains("core#PackageIdentity"),
        "Missing PackageIdentity"
    );
    assert!(content.contains("core#isVersionOf"), "Missing isVersionOf");
    assert!(content.contains("\"openssl\""), "Missing package name");

    // Maintainer (SD-3: Person, not Maintainer)
    assert!(
        content.contains("core#Person"),
        "Maintainer must be typed as Person"
    );
    assert!(
        !content.contains("core#Maintainer"),
        "Must NOT type as Maintainer"
    );
    assert!(content.contains("foaf/0.1/name"), "Missing foaf:name");

    // Dependencies
    assert!(
        content.contains("core#hasDependency"),
        "Missing dependency reification"
    );
    assert!(
        content.contains("core#directlyDependsOn"),
        "Missing direct dependency"
    );

    // Source package
    assert!(
        content.contains("core#SourcePackage"),
        "Missing SourcePackage"
    );
    assert!(
        content.contains("core#builtFromSource"),
        "Missing builtFromSource"
    );

    // Version
    assert!(content.contains("core#hasVersion"), "Missing hasVersion");
    assert!(content.contains("core#epoch"), "Missing epoch on version");

    // Upstream repo from homepage
    assert!(
        content.contains("core#upstreamRepository"),
        "Missing upstream repo from GitHub homepage"
    );
}

#[test]
fn test_golden_debian_package_emission() {
    let temp_file = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

    let ir = debian_package_ir();
    let policy = EmitPolicy::default();

    emit_distribution_metadata(&mut writer, "debian", "trixie", "Debian").unwrap();
    let triples = emit_rdf(&ir, &mut writer, &policy).unwrap();
    writer.flush().unwrap();

    assert!(triples > 10);

    let mut content = String::new();
    temp_file
        .reopen()
        .unwrap()
        .read_to_string(&mut content)
        .unwrap();

    // Debian uses codename, not version
    assert!(
        content.contains("core#releaseCodename"),
        "trixie is a codename"
    );
    assert!(content.contains("\"trixie\""), "Missing codename value");

    // Package basics
    assert!(content.contains("\"libc6\""), "Missing package name");
    assert!(content.contains("core#Person"), "Maintainer must be Person");
    assert!(
        content.contains("\"GNU Libc Maintainers\""),
        "Missing maintainer name"
    );
}

#[test]
fn test_ir_round_trip_then_emit() {
    // Write IR to disk, read it back, emit N-Triples
    let tmp = tempfile::TempDir::new().unwrap();
    let ir_path = tmp.path().join("golden.jsonl.zst");

    let ir1 = fedora_package_ir();
    let ir2 = debian_package_ir();

    // Write
    let mut ir_writer = IrWriter::new(&ir_path).unwrap();
    ir_writer.write(&ir1).unwrap();
    ir_writer.write(&ir2).unwrap();
    let count = ir_writer.finish().unwrap();
    assert_eq!(count, 2);

    // Read and emit
    let temp_file = NamedTempFile::new().unwrap();
    let mut nt_writer = NTriplesWriter::new(temp_file.reopen().unwrap());
    let policy = EmitPolicy::default();

    let reader = IrReader::new(&ir_path).unwrap();
    let mut total_triples = 0;
    for record in reader.records() {
        let ir = record.unwrap();
        total_triples += emit_rdf(&ir, &mut nt_writer, &policy).unwrap();
    }
    nt_writer.flush().unwrap();

    assert!(
        total_triples > 25,
        "Two packages should emit at least 25 triples, got {}",
        total_triples
    );

    let mut content = String::new();
    temp_file
        .reopen()
        .unwrap()
        .read_to_string(&mut content)
        .unwrap();

    // Both packages should be present
    assert!(content.contains("\"openssl\""), "Missing Fedora package");
    assert!(content.contains("\"libc6\""), "Missing Debian package");
}
