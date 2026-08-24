use crate::ntriples::NTriplesWriter;
use crate::source_cache::{CacheResult, CacheScope, SourceCache};
use crate::uris::*;
use brotli::Decompressor;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Result};
use std::time::Duration;

pub struct NixCollector {
    distro_name: String,
    release_name: String,
    client: Client,
    channel_url: String,
    source_cache: Option<SourceCache>,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PackagesRoot {
    packages: HashMap<String, NixPackage>,
}

#[derive(Debug, Deserialize)]
struct NixPackage {
    pname: Option<String>,
    version: Option<String>,
    #[serde(rename = "meta")]
    metadata: Option<NixMeta>,
}

#[derive(Debug, Deserialize)]
struct NixMeta {
    description: Option<String>,
    homepage: Option<serde_json::Value>,
    license: Option<serde_json::Value>,
    platforms: Option<Vec<String>>,
    broken: Option<bool>,
}

impl NixCollector {
    pub fn new(distro_name: String, release_name: String, channel_url: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            distro_name,
            release_name,
            client,
            channel_url,
            source_cache: None,
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn with_cache(mut self, cache_dir: &str) -> Result<Self> {
        self.source_cache = Some(SourceCache::new(cache_dir, "nix")?);
        Ok(self)
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let url = format!(
            "{}/packages.json.br",
            self.channel_url.trim_end_matches('/')
        );
        eprintln!("Fetching packages.json.br from: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        if !response.status().is_success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("HTTP {}", response.status()),
            ));
        }

        let bytes = response
            .bytes()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        // Decompress brotli
        let mut decompressor = Decompressor::new(&bytes[..], 4096);
        let reader = BufReader::new(&mut decompressor);

        // Parse root object to get packages map
        let root: PackagesRoot = serde_json::from_reader(reader)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        eprintln!("Parsed {} Nix packages", root.packages.len());

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        self.emit_distribution_metadata(&mut writer)?;

        let mut total_triples = 0;
        let mut count = 0;

        for (attr_path, pkg) in &root.packages {
            if count % 5000 == 0 {
                eprintln!("Progress: {}/{}", count, root.packages.len());
            }

            if let (Some(pname), Some(version)) = (&pkg.pname, &pkg.version) {
                total_triples +=
                    self.emit_package_triples(&mut writer, pname, version, attr_path, pkg)?;
                count += 1;
            }
        }

        writer.flush()?;
        Ok((count, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.release_name);
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, RDFS_LABEL, "Nix")?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Nix")?;
        triples += 3;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "nixpkgs")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn emit_package_triples(
        &self,
        writer: &mut NTriplesWriter,
        pname: &str,
        version: &str,
        attr_path: &str,
        pkg: &NixPackage,
    ) -> Result<usize> {
        let pkg_uri = package_uri(&self.distro_name, &self.release_name, "any", pname, version);
        let identity_uri =
            package_identity_uri(&self.distro_name, &self.release_name, "any", pname);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{NIX}NixPackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), pname)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), pname)?;
        triples += 1;

        // Nix-specific: attribute path
        writer.write_literal(&pkg_uri, &format!("{NIX}attrPath"), attr_path)?;
        triples += 1;

        // Version
        let ver_uri = version_uri(&self.distro_name, &self.release_name, pname, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Metadata
        if let Some(meta) = &pkg.metadata {
            if let Some(desc) = &meta.description {
                writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
                triples += 1;
            }

            // Homepage can be string or array
            if let Some(homepage_val) = &meta.homepage {
                let homepage_str = match homepage_val {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Array(arr) => {
                        arr.first().and_then(|v| v.as_str()).map(|s| s.to_string())
                    }
                    _ => None,
                };
                if let Some(homepage) = homepage_str {
                    writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), &homepage)?;
                    triples += 1;
                }
            }

            // License can be various types
            if let Some(license_val) = &meta.license {
                let license_str = match license_val {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Object(obj) => obj
                        .get("shortName")
                        .or_else(|| obj.get("fullName"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    _ => None,
                };
                if let Some(license) = license_str {
                    writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), &license)?;
                    triples += 1;
                    // License entity (SPDX)
                    let license_uri = crate::uris::spdx_license_uri(&license);
                    writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
                    writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
                    triples += 2;
                }
            }

            if let Some(broken) = meta.broken {
                writer.write_boolean(&pkg_uri, &format!("{NIX}broken"), broken)?;
                triples += 1;
            }
        }

        Ok(triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nix_package_deserialization() {
        let json = r#"{"pname":"hello","version":"2.12.1","meta":{"description":"GNU Hello","homepage":"https://www.gnu.org/software/hello/","license":{"shortName":"GPL-3.0"},"platforms":["x86_64-linux"],"broken":false}}"#;

        let pkg: NixPackage = serde_json::from_str(json).unwrap();
        assert_eq!(pkg.pname, Some("hello".to_string()));
        assert_eq!(pkg.version, Some("2.12.1".to_string()));
        assert_eq!(pkg.metadata.as_ref().unwrap().broken, Some(false));
    }

    #[test]
    fn test_emit_nix_package() {
        use std::io::{Read, Write};
        use tempfile::NamedTempFile;

        let collector = NixCollector::new(
            "nix".into(),
            "nixpkgs".into(),
            "https://channels.nixos.org/nixos-24.05".into(),
        );
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg = NixPackage {
            pname: Some("hello".to_string()),
            version: Some("2.12.1".to_string()),
            metadata: Some(NixMeta {
                description: Some("GNU Hello".to_string()),
                homepage: Some(serde_json::Value::String(
                    "https://www.gnu.org/software/hello/".to_string(),
                )),
                license: None,
                platforms: None,
                broken: Some(false),
            }),
        };

        let triples = collector
            .emit_package_triples(&mut writer, "hello", "2.12.1", "nixpkgs.hello", &pkg)
            .unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("nix#NixPackage"));
        assert!(content.contains("\"hello\""));
        assert!(content.contains("nix#attrPath"));
        assert!(content.contains("nix#broken"));
        assert!(triples > 10);
    }
}
