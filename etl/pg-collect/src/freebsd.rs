use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;
use tar::Archive;
use xz2::read::XzDecoder;

pub struct FreebsdCollector {
    client: Client,
    mirror: String,
    release: String,
    arch: String,
}

#[derive(Debug, Deserialize)]
struct PackageSiteEntry {
    name: String,
    version: String,
    origin: Option<String>,
    comment: Option<String>,
    www: Option<String>,
    licenses: Option<Vec<String>>,
    #[serde(default)]
    deps: HashMap<String, DependencyInfo>,
    flatsize: Option<u64>,
    abi: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DependencyInfo {
    origin: Option<String>,
    version: Option<String>,
}

use std::collections::HashMap;

impl FreebsdCollector {
    pub fn new(mirror: String, release: String, arch: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            mirror,
            release,
            arch,
        }
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let url = format!(
            "{}/FreeBSD:{}:{}/latest/packagesite.txz",
            self.mirror.trim_end_matches('/'),
            self.release,
            self.arch
        );
        eprintln!("Fetching packagesite.txz from: {}", url);

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

        // Extract .tar.xz
        let xz_decoder = XzDecoder::new(&bytes[..]);
        let mut archive = Archive::new(xz_decoder);

        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let mut total_packages = 0;
        let mut total_triples = 0;

        // Extract and parse packagesite.yaml (actually NDJSON)
        for entry_result in archive.entries()? {
            let mut entry = entry_result?;
            let path = entry.path()?.to_string_lossy().to_string();

            if path.ends_with("packagesite.yaml") {
                eprintln!("Parsing {}", path);
                let reader = BufReader::new(&mut entry);

                for line_result in reader.lines() {
                    let line = line_result?;
                    if line.trim().is_empty() {
                        continue;
                    }

                    match serde_json::from_str::<PackageSiteEntry>(&line) {
                        Ok(pkg) => {
                            total_triples += self.emit_package_triples(&mut writer, &pkg)?;
                            total_packages += 1;

                            if total_packages % 1000 == 0 {
                                eprintln!("Progress: {} packages", total_packages);
                            }
                        }
                        Err(e) => eprintln!("  Parse error: {}", e),
                    }
                }
            }
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("freebsd");
        let rel_uri = release_uri("freebsd", &self.release);
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "FreeBSD")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), &self.release)?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn emit_package_triples(&self, writer: &mut NTriplesWriter, pkg: &PackageSiteEntry) -> Result<usize> {
        let pkg_uri = package_uri("freebsd", &self.release, &self.arch, &pkg.name, &pkg.version);
        let identity_uri = package_identity_uri("freebsd", &self.release, &self.arch, &pkg.name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{FREEBSD}BinaryPackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &pkg.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.name)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("freebsd", &self.release, &pkg.name, &pkg.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &pkg.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("freebsd");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // FreeBSD-specific properties
        if let Some(origin) = &pkg.origin {
            writer.write_literal(&pkg_uri, &format!("{FREEBSD}origin"), origin)?;
            triples += 1;
        }
        if let Some(comment) = &pkg.comment {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), comment)?;
            triples += 1;
        }
        if let Some(www) = &pkg.www {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), www)?;
            triples += 1;
        }
        if let Some(abi) = &pkg.abi {
            writer.write_literal(&pkg_uri, &format!("{FREEBSD}abi"), abi)?;
            triples += 1;
        }
        if let Some(flatsize) = pkg.flatsize {
            writer.write_integer(&pkg_uri, &format!("{FREEBSD}flatsize"), flatsize as i64)?;
            triples += 1;
        }
        if let Some(licenses) = &pkg.licenses {
            for license in licenses {
                writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
                triples += 1;
            }
        }

        // Dependencies
        for (dep_name, _info) in &pkg.deps {
            let target = package_identity_uri("freebsd", &self.release, &self.arch, dep_name);
            writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target)?;
            triples += 1;
        }

        Ok(triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packagesite_entry_deserialization() {
        let json = r#"{"name":"nginx","version":"1.24.0","origin":"www/nginx","comment":"Robust web server","www":"https://nginx.org","licenses":["BSD2CLAUSE"],"deps":{"pcre2":{"origin":"devel/pcre2","version":"10.42"}},"flatsize":895488,"abi":"FreeBSD:14:amd64"}"#;

        let entry: PackageSiteEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.name, "nginx");
        assert_eq!(entry.version, "1.24.0");
        assert_eq!(entry.origin, Some("www/nginx".to_string()));
        assert_eq!(entry.deps.len(), 1);
    }

    #[test]
    fn test_emit_freebsd_package() {
        use std::io::{Read, Write};
        use tempfile::NamedTempFile;

        let collector = FreebsdCollector::new(
            "https://pkg.freebsd.org".into(),
            "14".into(),
            "amd64".into(),
        );
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg = PackageSiteEntry {
            name: "nginx".to_string(),
            version: "1.24.0".to_string(),
            origin: Some("www/nginx".to_string()),
            comment: Some("Robust web server".to_string()),
            www: Some("https://nginx.org".to_string()),
            licenses: Some(vec!["BSD2CLAUSE".to_string()]),
            deps: HashMap::new(),
            flatsize: Some(895488),
            abi: Some("FreeBSD:14:amd64".to_string()),
        };

        let triples = collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("freebsd#BinaryPackage"));
        assert!(content.contains("\"nginx\""));
        assert!(content.contains("freebsd#origin"));
        assert!(content.contains("\"www/nginx\""));
        assert!(triples > 10);
    }
}
