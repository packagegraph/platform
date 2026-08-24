use crate::npm::read_seed_file;
use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct SnapCollector {
    distro_name: String,
    release_name: String,
    client: Client,
    pub graph_uri: Option<String>,
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
    snap: Option<SnapDetails>,
    #[serde(rename = "default-track")]
    default_track: Option<String>,
    #[serde(rename = "channel-map")]
    channel_map: Option<Vec<SnapChannel>>,
}

#[derive(Debug, Deserialize)]
struct SnapDetails {
    summary: Option<String>,
    license: Option<String>,
    #[serde(rename = "snap-id")]
    snap_id: Option<String>,
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
    pub fn new(distro_name: String, release_name: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to create HTTP client");
        Self {
            distro_name,
            release_name,
            client,
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    /// Collect from a seed file of snap names.
    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let names = read_seed_file(packages_file)?;
        eprintln!("Loaded {} snap names from seed file", names.len());
        self.collect_names(&names, output_path)
    }

    /// Discover all snaps from the Snap Store and collect them (no seed file needed).
    pub fn collect_discover(&self, output_path: &str) -> Result<(usize, usize)> {
        let names = self.discover_snaps()?;
        eprintln!("Discovered {} snaps from Snap Store", names.len());
        self.collect_names(&names, output_path)
    }

    /// Paginate the Snap Store v1 search API to discover all snap names.
    fn discover_snaps(&self) -> Result<Vec<String>> {
        eprintln!("Discovering snaps from Snap Store (v1 API)...");
        let mut names = Vec::new();
        let mut page = 1;
        let page_size = 500;

        loop {
            let url = format!(
                "https://api.snapcraft.io/api/v1/snaps/search?fields=package_name&page={}&size={}",
                page, page_size
            );
            let response = self
                .client
                .get(&url)
                .header("X-Ubuntu-Series", "16")
                .send()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

            if !response.status().is_success() {
                if response.status().as_u16() == 404 {
                    break;
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Snap Store API returned {}", response.status()),
                ));
            }

            let data: serde_json::Value = response
                .json()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

            let packages = match data["_embedded"]["clickindex:package"].as_array() {
                Some(p) if !p.is_empty() => p,
                _ => break,
            };

            let batch_count = packages.len();
            for pkg in packages {
                if let Some(name) = pkg["package_name"].as_str() {
                    names.push(name.to_string());
                }
            }

            eprintln!(
                "  Page {}: {} snaps (total: {})",
                page,
                batch_count,
                names.len()
            );

            // Check if there's a next page
            if data["_links"]["next"].is_null() || batch_count < page_size {
                break;
            }
            page += 1;
            std::thread::sleep(Duration::from_millis(200));
        }

        Ok(names)
    }

    fn collect_names(&self, names: &[String], output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());
        self.emit_distribution_metadata(&mut writer)?;

        let mut total_packages = 0;
        let mut total_triples = 0;

        for (idx, name) in names.iter().enumerate() {
            if (idx + 1) % 50 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, names.len());
            }

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
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.release_name);
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
        let response = self
            .client
            .get(&url)
            .header("Snap-Device-Series", "16")
            .send()
            .map_err(|e| e.to_string())?;

        if response.status() == StatusCode::NOT_FOUND {
            return Err(format!("404: {}", name));
        }

        let text = response.text().map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;

        // Parse manually to handle API response variations
        let snap_name = v["name"].as_str().unwrap_or(name).to_string();
        let snap_id = v["snap-id"].as_str().map(|s| s.to_string());
        let snap_details = v.get("snap").and_then(|s| {
            Some(SnapDetails {
                summary: s["summary"].as_str().map(|s| s.to_string()),
                license: s["license"].as_str().map(|s| s.to_string()),
                snap_id: s["snap-id"].as_str().map(|s| s.to_string()),
            })
        });

        let channel_map = v["channel-map"].as_array().map(|channels| {
            channels
                .iter()
                .filter_map(|ch| {
                    let channel_obj = ch.get("channel")?;
                    Some(SnapChannel {
                        channel: SnapChannelInfo {
                            name: channel_obj["name"].as_str().map(|s| s.to_string()),
                            risk: channel_obj["risk"].as_str().map(|s| s.to_string()),
                            track: channel_obj["track"].as_str().map(|s| s.to_string()),
                        },
                        version: ch["version"].as_str().map(|s| s.to_string()),
                        confinement: ch["confinement"].as_str().map(|s| s.to_string()),
                    })
                })
                .collect()
        });

        Ok(SnapInfoResponse {
            name: snap_name,
            snap_id,
            snap: snap_details,
            default_track: v["default-track"].as_str().map(|s| s.to_string()),
            channel_map,
        })
    }

    fn emit_snap_triples(
        &self,
        writer: &mut NTriplesWriter,
        info: &SnapInfoResponse,
    ) -> Result<usize> {
        // Get version from stable channel
        let version = info
            .channel_map
            .as_ref()
            .and_then(|channels| {
                channels
                    .iter()
                    .find(|c| c.channel.risk.as_deref() == Some("stable"))
                    .and_then(|c| c.version.as_deref())
            })
            .unwrap_or("latest");

        let confinement = info.channel_map.as_ref().and_then(|channels| {
            channels
                .iter()
                .find(|c| c.channel.risk.as_deref() == Some("stable"))
                .and_then(|c| c.confinement.as_deref())
        });

        let pkg_uri = package_uri(
            &self.distro_name,
            &self.release_name,
            "any",
            &info.name,
            version,
        );
        let identity_uri =
            package_identity_uri(&self.distro_name, &self.release_name, "any", &info.name);
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

        let ver_uri = version_uri(&self.distro_name, &self.release_name, &info.name, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Extract summary and license from snap sub-object
        if let Some(snap) = &info.snap {
            if let Some(summary) = &snap.summary {
                writer.write_literal(&pkg_uri, &format!("{PKG}description"), summary)?;
                triples += 1;
            }
            if let Some(license) = &snap.license {
                writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
                triples += 1;
                // License entity (SPDX)
                let license_uri = crate::uris::spdx_license_uri(license);
                writer.write_triple(&pkg_uri, &format!("{PKG}hasLicense"), &license_uri)?;
                writer.write_triple(&license_uri, RDF_TYPE, &format!("{PKG}License"))?;
                triples += 2;
            }
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
            "snap": {
                "summary": "Mozilla Firefox web browser",
                "license": "MPL-2.0",
                "snap-id": "3wdHCAVyZEmYsCMFDE9qt92UV8rC8Wdk"
            },
            "default-track": "latest",
            "channel-map": [{
                "channel": {"name": "stable", "risk": "stable", "track": "latest"},
                "version": "125.0",
                "confinement": "strict"
            }]
        }"#;

        let info: SnapInfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(info.name, "firefox");
        assert_eq!(
            info.snap.as_ref().unwrap().summary.as_deref(),
            Some("Mozilla Firefox web browser")
        );
        assert_eq!(
            info.channel_map.as_ref().unwrap()[0].version.as_deref(),
            Some("125.0")
        );
    }

    #[test]
    fn test_emit_snap_triples_dual_typing() {
        let collector = SnapCollector::new("snap".into(), "store".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let info = SnapInfoResponse {
            name: "firefox".into(),
            snap_id: Some("3wdHCAVyZEmYsCMFDE9qt92UV8rC8Wdk".into()),
            snap: Some(SnapDetails {
                summary: Some("Web browser".into()),
                license: Some("MPL-2.0".into()),
                snap_id: Some("3wdHCAVyZEmYsCMFDE9qt92UV8rC8Wdk".into()),
            }),
            default_track: Some("latest".into()),
            channel_map: Some(vec![SnapChannel {
                channel: SnapChannelInfo {
                    name: Some("stable".into()),
                    risk: Some("stable".into()),
                    track: Some("latest".into()),
                },
                version: Some("125.0".into()),
                confinement: Some("strict".into()),
            }]),
        };

        let triples = collector.emit_snap_triples(&mut writer, &info).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("snap#SnapPackage"));
        assert!(content.contains("\"firefox\""));
        assert!(content.contains("snap#confinement"));
        assert!(content.contains("\"strict\""));
        assert!(content.contains("snap#snapId"));
        assert!(content.contains("licenseName"));
        assert!(content.contains("\"MPL-2.0\""));
        assert!(triples > 10);
    }
}
