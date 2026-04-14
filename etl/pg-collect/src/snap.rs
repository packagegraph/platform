use crate::ntriples::NTriplesWriter;
use crate::npm::read_seed_file;
use crate::uris::*;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct SnapCollector {
    client: Client,
}

#[derive(Debug, Deserialize)]
struct SnapSearchResponse {
    results: Vec<SnapResult>,
}

#[derive(Debug, Deserialize)]
struct SnapResult {
    snap: SnapInfo,
}

#[derive(Debug, Deserialize)]
struct SnapInfo {
    name: String,
    title: Option<String>,
    summary: Option<String>,
    license: Option<String>,
    publisher: Option<SnapPublisher>,
    #[serde(rename = "snap-id")]
    snap_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SnapPublisher {
    #[serde(rename = "display-name")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SnapInfoResponse {
    name: String,
    #[serde(rename = "snap-id")]
    snap_id: Option<String>,
    summary: Option<String>,
    #[serde(rename = "default-track")]
    default_track: Option<String>,
    #[serde(rename = "channel-map")]
    channel_map: Option<Vec<SnapChannel>>,
}

#[derive(Debug, Deserialize)]
struct SnapChannel {
    channel: SnapChannelInfo,
    version: Option<String>,
    confinement: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SnapChannelInfo {
    name: Option<String>,
    risk: Option<String>,
    track: Option<String>,
}

impl SnapCollector {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to create HTTP client");
        Self { client }
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);
        self.emit_distribution_metadata(&mut writer)?;

        let names = read_seed_file(packages_file)?;
        eprintln!("Loaded {} snap names from seed file", names.len());

        let mut total_packages = 0;
        let mut total_triples = 0;

        for (idx, name) in names.iter().enumerate() {
            if (idx + 1) % 50 == 0 { eprintln!("Progress: {}/{}", idx + 1, names.len()); }

            match self.fetch_snap_info(name) {
                Ok(info) => {
                    total_triples += self.emit_snap_triples(&mut writer, &info)?;
                    total_packages += 1;
                }
                Err(e) => eprintln!("  Error fetching {}: {}", name, e),
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("snap");
        let rel_uri = release_uri("snap", "store");
        let mut triples = 0;
        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Snap Store")?;
        triples += 2;
        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "store")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;
        Ok(triples)
    }

    fn fetch_snap_info(&self, name: &str) -> std::result::Result<SnapInfoResponse, String> {
        let url = format!("https://api.snapcraft.io/v2/snaps/info/{}", name);
        let response = self.client.get(&url)
            .header("Snap-Device-Series", "16")
            .send()
            .map_err(|e| e.to_string())?;

        if response.status() == StatusCode::NOT_FOUND {
            return Err(format!("404: {}", name));
        }

        let text = response.text().map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn emit_snap_triples(&self, writer: &mut NTriplesWriter, info: &SnapInfoResponse) -> Result<usize> {
        // Get version from stable channel
        let version = info.channel_map.as_ref()
            .and_then(|channels| channels.iter()
                .find(|c| c.channel.risk.as_deref() == Some("stable"))
                .and_then(|c| c.version.as_deref()))
            .unwrap_or("latest");

        let confinement = info.channel_map.as_ref()
            .and_then(|channels| channels.iter()
                .find(|c| c.channel.risk.as_deref() == Some("stable"))
                .and_then(|c| c.confinement.as_deref()));

        let pkg_uri = package_uri("snap", "store", "any", &info.name, version);
        let identity_uri = package_identity_uri("snap", "store", "any", &info.name);
        let mut triples = 0;

        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{SNAP}SnapPackage"))?;
        triples += 2;

        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &info.name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &info.name)?;
        triples += 1;

        let ver_uri = version_uri("snap", "store", &info.name, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        let dist_uri = distro_uri("snap");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        if let Some(summary) = &info.summary {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), summary)?;
            triples += 1;
        }

        if let Some(snap_id) = &info.snap_id {
            writer.write_literal(&pkg_uri, &format!("{SNAP}snapId"), snap_id)?;
            triples += 1;
        }

        if let Some(conf) = confinement {
            writer.write_literal(&pkg_uri, &format!("{SNAP}confinement"), conf)?;
            triples += 1;
        }

        Ok(triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_snap_info_deserialization() {
        let json = r#"{
            "name": "firefox",
            "snap-id": "3wdHCAVyZEmYsCMFDE9qt92UV8rC8Wdk",
            "summary": "Mozilla Firefox web browser",
            "default-track": "latest",
            "channel-map": [{
                "channel": {"name": "stable", "risk": "stable", "track": "latest"},
                "version": "125.0",
                "confinement": "strict"
            }]
        }"#;

        let info: SnapInfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(info.name, "firefox");
        assert_eq!(info.channel_map.as_ref().unwrap()[0].version.as_deref(), Some("125.0"));
    }

    #[test]
    fn test_emit_snap_triples_dual_typing() {
        let collector = SnapCollector::new();
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let info = SnapInfoResponse {
            name: "firefox".into(),
            snap_id: Some("3wdHCAVyZEmYsCMFDE9qt92UV8rC8Wdk".into()),
            summary: Some("Web browser".into()),
            default_track: Some("latest".into()),
            channel_map: Some(vec![SnapChannel {
                channel: SnapChannelInfo { name: Some("stable".into()), risk: Some("stable".into()), track: Some("latest".into()) },
                version: Some("125.0".into()),
                confinement: Some("strict".into()),
            }]),
        };

        let triples = collector.emit_snap_triples(&mut writer, &info).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("snap#SnapPackage"));
        assert!(content.contains("\"firefox\""));
        assert!(content.contains("snap#confinement"));
        assert!(content.contains("\"strict\""));
        assert!(content.contains("snap#snapId"));
        assert!(triples > 10);
    }
}
