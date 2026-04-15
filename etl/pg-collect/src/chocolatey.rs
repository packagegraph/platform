use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::blocking::Client;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct ChocolateyCollector {
    client: Client,
    api_url: String,
}

#[derive(Debug, Default, Clone)]
struct ChocolateyPackage {
    id: String,
    version: String,
    title: Option<String>,
    description: Option<String>,
    authors: Option<String>,
    package_hash: Option<String>,
    package_hash_algorithm: Option<String>,
    download_count: Option<i64>,
    version_download_count: Option<i64>,
    is_prerelease: bool,
}

impl ChocolateyCollector {
    pub fn new(api_url: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, api_url }
    }

    pub fn collect(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut skip = 0;
        let page_size = 100;

        loop {
            eprintln!("Fetching packages {} to {}", skip, skip + page_size);

            let packages = self.fetch_page(skip, page_size)?;
            if packages.is_empty() {
                break;
            }

            for pkg in &packages {
                total_triples += self.emit_package_triples(&mut writer, pkg)?;
                total_packages += 1;
            }

            skip += packages.len();
            std::thread::sleep(Duration::from_millis(200));
        }

        eprintln!("Collected {} Chocolatey packages", total_packages);

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("chocolatey");
        let rel_uri = release_uri("chocolatey", "community");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Chocolatey")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "community")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_page(&self, skip: usize, top: usize) -> Result<Vec<ChocolateyPackage>> {
        let url = format!(
            "{}/Packages()?$filter=IsLatestVersion eq true&$top={}&$skip={}&$orderby=Id",
            self.api_url.trim_end_matches('/'),
            top,
            skip
        );

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

        let xml = response
            .text()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        self.parse_odata_feed(&xml)
    }

    fn parse_odata_feed(&self, xml: &str) -> Result<Vec<ChocolateyPackage>> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut packages = Vec::new();
        let mut current = ChocolateyPackage::default();
        let mut in_entry = false;
        let mut in_properties = false;
        let mut current_element = String::new();
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    current_element = name.clone();

                    if name == "entry" {
                        in_entry = true;
                        current = ChocolateyPackage::default();
                    } else if name == "m:properties" {
                        in_properties = true;
                    }
                }
                Ok(Event::End(e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "entry" {
                        if !current.id.is_empty() {
                            packages.push(current.clone());
                        }
                        in_entry = false;
                        current = ChocolateyPackage::default();
                    } else if name == "m:properties" {
                        in_properties = false;
                    }
                    current_element.clear();
                }
                Ok(Event::Text(e)) => {
                    if !in_properties {
                        continue;
                    }

                    let text = e.unescape().unwrap_or_default().trim().to_string();
                    if text.is_empty() {
                        continue;
                    }

                    match current_element.as_str() {
                        "d:Id" => current.id = text,
                        "d:Version" => current.version = text,
                        "d:Title" => current.title = Some(text),
                        "d:Description" => current.description = Some(text),
                        "d:Authors" => current.authors = Some(text),
                        "d:PackageHash" => current.package_hash = Some(text),
                        "d:PackageHashAlgorithm" => current.package_hash_algorithm = Some(text),
                        "d:DownloadCount" => current.download_count = text.parse().ok(),
                        "d:VersionDownloadCount" => current.version_download_count = text.parse().ok(),
                        "d:IsPrerelease" => current.is_prerelease = text == "true",
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    eprintln!("XML parse error: {}", e);
                    break;
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(packages)
    }

    fn emit_package_triples(&self, writer: &mut NTriplesWriter, pkg: &ChocolateyPackage) -> Result<usize> {
        let pkg_uri = package_uri("chocolatey", "community", "windows", &pkg.id, &pkg.version);
        let identity_uri = package_identity_uri("chocolatey", "community", "windows", &pkg.id);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{CHOCO}ChocolateyPackage"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &pkg.id)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &pkg.id)?;
        triples += 1;

        // Version
        let ver_uri = version_uri("chocolatey", "community", &pkg.id, &pkg.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &pkg.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("chocolatey");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(title) = &pkg.title {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), title)?;
            triples += 1;
        }
        if let Some(authors) = &pkg.authors {
            writer.write_literal(&pkg_uri, &format!("{CHOCO}authors"), authors)?;
            triples += 1;
        }
        if let Some(hash) = &pkg.package_hash {
            writer.write_literal(&pkg_uri, &format!("{PKG}checksum"), hash)?;
            triples += 1;
        }
        if let Some(download_count) = pkg.download_count {
            writer.write_integer(&pkg_uri, &format!("{CHOCO}downloadCount"), download_count)?;
            triples += 1;
        }
        if pkg.is_prerelease {
            writer.write_boolean(&pkg_uri, &format!("{CHOCO}isPrerelease"), true)?;
            triples += 1;
        }

        Ok(triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_odata_feed() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:d="http://schemas.microsoft.com/ado/2007/08/dataservices" xmlns:m="http://schemas.microsoft.com/ado/2007/08/dataservices/metadata">
  <entry>
    <id>https://community.chocolatey.org/api/v2/Packages(Id='7zip',Version='23.01')</id>
    <content type="application/zip" src="https://community.chocolatey.org/api/v2/package/7zip/23.01"/>
    <m:properties>
      <d:Id>7zip</d:Id>
      <d:Version>23.01</d:Version>
      <d:Title>7-Zip</d:Title>
      <d:Description>File archiver</d:Description>
      <d:Authors>Igor Pavlov</d:Authors>
      <d:DownloadCount m:type="Edm.Int32">1000000</d:DownloadCount>
      <d:IsPrerelease m:type="Edm.Boolean">false</d:IsPrerelease>
    </m:properties>
  </entry>
</feed>"#;

        let collector = ChocolateyCollector::new("https://community.chocolatey.org/api/v2".into());
        let packages = collector.parse_odata_feed(xml).unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].id, "7zip");
        assert_eq!(packages[0].version, "23.01");
        assert_eq!(packages[0].title, Some("7-Zip".to_string()));
        assert_eq!(packages[0].download_count, Some(1000000));
        assert!(!packages[0].is_prerelease);
    }

    #[test]
    fn test_emit_chocolatey_package() {
        use std::io::{Read, Write};
        use tempfile::NamedTempFile;

        let collector = ChocolateyCollector::new("https://community.chocolatey.org/api/v2".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pkg = ChocolateyPackage {
            id: "7zip".to_string(),
            version: "23.01".to_string(),
            title: Some("7-Zip".to_string()),
            description: Some("File archiver".to_string()),
            authors: Some("Igor Pavlov".to_string()),
            package_hash: Some("abc123".to_string()),
            package_hash_algorithm: Some("SHA256".to_string()),
            download_count: Some(1000000),
            version_download_count: Some(50000),
            is_prerelease: false,
        };

        let triples = collector.emit_package_triples(&mut writer, &pkg).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("chocolatey#ChocolateyPackage"));
        assert!(content.contains("\"7zip\""));
        assert!(content.contains("chocolatey#downloadCount"));
        assert!(triples > 10);
    }
}
