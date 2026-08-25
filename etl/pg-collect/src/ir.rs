//! Collector-agnostic intermediate representation (IR) for package metadata.
//!
//! The IR represents normalized package facts WITHOUT ontology-specific semantics.
//! No `pkg:` prefixes, no RDF type references, no precomputed URIs.
//!
//! This module defines:
//! - Core IR structs (PackageIr, MaintainerIr, DependencyIr, etc.)
//! - IrWriter for writing `.jsonl.zst` shards with manifests
//! - IrReader for streaming IR records from `.jsonl.zst`

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use zstd::stream::{read::Decoder, write::Encoder};

/// IR schema version for invalidation tracking.
pub const IR_SCHEMA_VERSION: u32 = 1;

/// Scope identifier for an IR shard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeIr {
    pub collector: String,
    pub distro: String,
    pub release: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub arch: String,
}

/// Maintainer information (ontology-agnostic).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaintainerIr {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_hint: Option<String>,
}

/// Package dependency (ontology-agnostic).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyIr {
    pub name: String,
    pub dep_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_constraint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<String>,
}

/// Source package reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourcePackageRef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
}

/// Package metadata (ontology-agnostic).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageMetadataIr {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

/// Core package IR record (ontology-agnostic).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageIr {
    pub ir_schema: u32,
    pub scope: ScopeIr,
    pub source_artifacts: BTreeMap<String, String>,
    pub package: PackageInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_package: Option<SourcePackageRef>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub maintainers: Vec<MaintainerIr>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub dependencies: Vec<DependencyIr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<PackageMetadataIr>,
    /// Collector-specific extension data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collector_specific: Option<serde_json::Value>,
}

/// Package identification fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageInfo {
    pub kind: String,
    pub name: String,
    pub epoch: u32,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    pub full_version: String,
    pub arch: String,
}

/// IR shard manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrManifest {
    pub schema: String,
    pub collector: String,
    pub collector_version: String,
    pub ir_schema_version: u32,
    pub scope: ScopeIr,
    pub source_artifact_hashes: Vec<String>,
    pub record_count: usize,
    pub generated_at: String,
    pub path: String,
}

/// Writer for IR shards (.jsonl.zst).
pub struct IrWriter {
    encoder: Encoder<'static, BufWriter<File>>,
    record_count: usize,
}

impl IrWriter {
    /// Create a new IR writer for the given file path.
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        let buf_writer = BufWriter::new(file);
        let encoder = Encoder::new(buf_writer, 3)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        Ok(Self {
            encoder,
            record_count: 0,
        })
    }

    /// Write a single PackageIr record as a JSON line.
    pub fn write(&mut self, record: &PackageIr) -> io::Result<()> {
        let json = serde_json::to_string(record)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        writeln!(self.encoder, "{}", json)?;
        self.record_count += 1;
        Ok(())
    }

    /// Finish writing and return the record count.
    pub fn finish(mut self) -> io::Result<usize> {
        self.encoder
            .finish()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(self.record_count)
    }
}

/// Reader for IR shards (.jsonl.zst).
pub struct IrReader<R: std::io::Read> {
    buf_reader: BufReader<R>,
}

impl IrReader<Decoder<'static, BufReader<File>>> {
    /// Open an IR shard file for streaming reads.
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let decoder =
            Decoder::new(file).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let buf_reader = BufReader::new(decoder);

        Ok(Self { buf_reader })
    }
}

impl<R: std::io::Read> IrReader<R> {
    /// Read the next IR record. Returns None at EOF.
    pub fn read_next(&mut self) -> io::Result<Option<PackageIr>> {
        let mut line = String::new();
        let bytes_read = self.buf_reader.read_line(&mut line)?;
        if bytes_read == 0 {
            return Ok(None);
        }

        let record: PackageIr = serde_json::from_str(line.trim())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(Some(record))
    }

    /// Iterate over all records.
    pub fn records(self) -> IrRecordIterator<R> {
        IrRecordIterator { reader: self }
    }
}

/// Iterator over IR records.
pub struct IrRecordIterator<R: std::io::Read> {
    reader: IrReader<R>,
}

impl<R: std::io::Read> Iterator for IrRecordIterator<R> {
    type Item = io::Result<PackageIr>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.reader.read_next() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Write an IR manifest file.
pub fn write_manifest(path: &Path, manifest: &IrManifest) -> io::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let content = serde_json::to_string_pretty(manifest)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::write(path, content)?;
    Ok(())
}

/// Read an IR manifest file.
pub fn read_manifest(path: &Path) -> io::Result<IrManifest> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_package_ir() -> PackageIr {
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
                let mut map = BTreeMap::new();
                map.insert("primary".to_string(), "sha256:abc123".to_string());
                map
            },
            package: PackageInfo {
                kind: "binary".to_string(),
                name: "glibc".to_string(),
                epoch: 0,
                version: "2.39".to_string(),
                release: Some("17.fc43".to_string()),
                full_version: "2.39-17.fc43".to_string(),
                arch: "x86_64".to_string(),
            },
            source_package: Some(SourcePackageRef {
                name: "glibc".to_string(),
                version: Some("2.39".to_string()),
                release: Some("17.fc43".to_string()),
            }),
            maintainers: vec![MaintainerIr {
                name: "Fedora Project".to_string(),
                email: None,
                role_hint: Some("maintainer".to_string()),
            }],
            dependencies: vec![DependencyIr {
                name: "glibc-common".to_string(),
                dep_type: "requires".to_string(),
                version_constraint: Some("= 2.39-17.fc43".to_string()),
                flags: None,
            }],
            metadata: Some(PackageMetadataIr {
                summary: Some("GNU C Library".to_string()),
                description: Some("The GNU libc libraries".to_string()),
                homepage: None,
                license: Some("LGPL-2.1-or-later".to_string()),
                checksum: None,
                size_bytes: Some(123456),
            }),
            collector_specific: None,
        }
    }

    #[test]
    fn test_ir_round_trip() {
        let tmp = TempDir::new().unwrap();
        let ir_path = tmp.path().join("test.jsonl.zst");

        let record = sample_package_ir();

        // Write
        let mut writer = IrWriter::new(&ir_path).unwrap();
        writer.write(&record).unwrap();
        let count = writer.finish().unwrap();
        assert_eq!(count, 1);

        // Read
        let mut reader = IrReader::new(&ir_path).unwrap();
        let read_record = reader.read_next().unwrap().unwrap();
        assert_eq!(read_record, record);

        // EOF
        assert!(reader.read_next().unwrap().is_none());
    }

    #[test]
    fn test_ir_multiple_records() {
        let tmp = TempDir::new().unwrap();
        let ir_path = tmp.path().join("multi.jsonl.zst");

        let mut record1 = sample_package_ir();
        let mut record2 = sample_package_ir();
        record2.package.name = "gcc".to_string();

        let mut writer = IrWriter::new(&ir_path).unwrap();
        writer.write(&record1).unwrap();
        writer.write(&record2).unwrap();
        let count = writer.finish().unwrap();
        assert_eq!(count, 2);

        let reader = IrReader::new(&ir_path).unwrap();
        let records: Vec<PackageIr> = reader.records().collect::<io::Result<Vec<_>>>().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].package.name, "glibc");
        assert_eq!(records[1].package.name, "gcc");
    }

    #[test]
    fn test_ir_serialization_deterministic() {
        // Write same record twice, verify identical JSON output
        let tmp = TempDir::new().unwrap();

        let record = sample_package_ir();

        let json1 = serde_json::to_string(&record).unwrap();
        let json2 = serde_json::to_string(&record).unwrap();

        assert_eq!(
            json1, json2,
            "Identical records must produce identical JSON"
        );

        // Verify BTreeMap serialization is ordered
        assert!(json1.contains("\"source_artifacts\":{\"primary\":\"sha256:abc123\"}"));
    }

    #[test]
    fn test_manifest_round_trip() {
        let tmp = TempDir::new().unwrap();
        let manifest_path = tmp.path().join("manifest.json");

        let manifest = IrManifest {
            schema: "collector-ir/v1".to_string(),
            collector: "rpm".to_string(),
            collector_version: "0.8.0".to_string(),
            ir_schema_version: 1,
            scope: ScopeIr {
                collector: "rpm".to_string(),
                distro: "fedora".to_string(),
                release: "43".to_string(),
                repo: Some("fedora".to_string()),
                arch: "x86_64".to_string(),
            },
            source_artifact_hashes: vec!["sha256:abc".to_string(), "sha256:def".to_string()],
            record_count: 123456,
            generated_at: "2026-04-22T12:10:00Z".to_string(),
            path: "rpm/fedora/43/fedora/x86_64/ir.jsonl.zst".to_string(),
        };

        write_manifest(&manifest_path, &manifest).unwrap();
        let read_manifest = read_manifest(&manifest_path).unwrap();

        assert_eq!(read_manifest.collector, manifest.collector);
        assert_eq!(read_manifest.record_count, manifest.record_count);
        assert_eq!(read_manifest.source_artifact_hashes.len(), 2);
    }
}
