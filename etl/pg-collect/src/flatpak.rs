use crate::npm::read_seed_file;
use crate::ntriples::NTriplesWriter;
use crate::uris::*;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

pub struct FlatpakCollector {
    distro_name: String,
    release_name: String,
    client: Client,
    pub graph_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FlatpakAppResponse {
    id: String,
    name: String,
    description: Option<String>,
    bundle: Option<FlatpakBundle>,
}

#[derive(Debug, Deserialize)]
struct FlatpakBundle {
    runtime: Option<String>,
}

impl FlatpakCollector {
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

    /// Collect from a seed file of app IDs.
    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let app_ids = read_seed_file(packages_file)?;
        eprintln!("Loaded {} Flatpak app IDs from seed file", app_ids.len());
        self.collect_apps(&app_ids, output_path)
    }

    /// Discover all apps from Flathub and collect them (no seed file needed).
    pub fn collect_discover(&self, output_path: &str) -> Result<(usize, usize)> {
        let app_ids = self.discover_apps()?;
        eprintln!("Discovered {} Flatpak apps from Flathub", app_ids.len());
        self.collect_apps(&app_ids, output_path)
    }

    /// Query the Flathub appstream index for all app IDs.
    fn discover_apps(&self) -> Result<Vec<String>> {
        eprintln!("Discovering Flatpak apps from Flathub...");
        let url = "https://flathub.org/api/v2/appstream";
        let response = self
            .client
            .get(url)
            .send()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        if !response.status().is_success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Flathub API returned {}", response.status()),
            ));
        }

        let app_ids: Vec<String> = response
            .json()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        Ok(app_ids)
    }

    fn collect_apps(&self, app_ids: &[String], output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());
        self.emit_distribution_metadata(&mut writer)?;

        let mut total_apps = 0;
        let mut total_triples = 0;

        for (idx, app_id) in app_ids.iter().enumerate() {
            if (idx + 1) % 50 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, app_ids.len());
            }

            match self.fetch_app_metadata(app_id) {
                Ok(app) => {
                    total_triples += self.emit_app_triples(&mut writer, &app)?;
                    total_apps += 1;
                }
                Err(e) => eprintln!("  Error fetching {}: {}", app_id, e),
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        writer.flush()?;
        Ok((total_apps, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri(&self.distro_name);
        let rel_uri = release_uri(&self.distro_name, &self.release_name);
        let mut triples = 0;
        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Flathub")?;
        triples += 2;
        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "flathub")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;
        Ok(triples)
    }

    fn fetch_app_metadata(&self, app_id: &str) -> std::result::Result<FlatpakAppResponse, String> {
        let url = format!("https://flathub.org/api/v2/appstream/{}", app_id);
        let response = self.client.get(&url).send().map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("HTTP {}: {}", response.status(), app_id));
        }

        let text = response.text().map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn emit_app_triples(
        &self,
        writer: &mut NTriplesWriter,
        app: &FlatpakAppResponse,
    ) -> Result<usize> {
        // Use app ID as version since Flatpak apps don't expose versions in the API
        let version = "latest";
        let pkg_uri = package_uri(
            &self.distro_name,
            &self.release_name,
            "any",
            &app.id,
            version,
        );
        let identity_uri =
            package_identity_uri(&self.distro_name, &self.release_name, "any", &app.id);
        let mut triples = 0;

        // Dual typing: pkg:Package + flatpak:FlatpakApp
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{FLATPAK}FlatpakApp"))?;
        triples += 2;

        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &app.id)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &app.id)?;
        triples += 1;

        let ver_uri = version_uri(&self.distro_name, &self.release_name, &app.id, version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        let dist_uri = distro_uri(&self.distro_name);
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        writer.write_literal(&pkg_uri, &format!("{PKG}displayName"), &app.name)?;
        triples += 1;

        if let Some(desc) = &app.description {
            // Strip HTML tags from description
            let plain_desc = desc
                .replace("<p>", "")
                .replace("</p>", " ")
                .replace("\n", " ")
                .trim()
                .to_string();
            if !plain_desc.is_empty() {
                writer.write_literal(&pkg_uri, &format!("{PKG}description"), &plain_desc)?;
                triples += 1;
            }
        }

        // Runtime association
        if let Some(bundle) = &app.bundle {
            if let Some(runtime) = &bundle.runtime {
                // Parse runtime string like "org.gnome.Platform/x86_64/50"
                if let Some(runtime_name) = runtime.split('/').next() {
                    let runtime_uri = package_identity_uri(
                        &self.distro_name,
                        &self.release_name,
                        "any",
                        runtime_name,
                    );
                    writer.write_triple(&pkg_uri, &format!("{FLATPAK}runtime"), &runtime_uri)?;
                    triples += 1;

                    // Extract runtime version if present
                    let parts: Vec<&str> = runtime.split('/').collect();
                    if parts.len() >= 3 {
                        writer.write_literal(
                            &pkg_uri,
                            &format!("{FLATPAK}runtimeVersion"),
                            parts[2],
                        )?;
                        triples += 1;
                    }
                }
            }
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
    fn test_flatpak_app_deserialization() {
        let json = r#"{
            "id": "org.gnome.gedit",
            "name": "gedit",
            "description": "<p>A text editor</p>",
            "bundle": {
                "runtime": "org.gnome.Platform/x86_64/50"
            }
        }"#;

        let app: FlatpakAppResponse = serde_json::from_str(json).unwrap();
        assert_eq!(app.id, "org.gnome.gedit");
        assert_eq!(app.name, "gedit");
        assert!(app.bundle.is_some());
    }

    #[test]
    fn test_emit_flatpak_app_dual_typing() {
        let collector = FlatpakCollector::new("flatpak".into(), "flathub".into());
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let app = FlatpakAppResponse {
            id: "org.gnome.gedit".into(),
            name: "gedit".into(),
            description: Some("<p>A text editor</p>".into()),
            bundle: Some(FlatpakBundle {
                runtime: Some("org.gnome.Platform/x86_64/50".into()),
            }),
        };

        let triples = collector.emit_app_triples(&mut writer, &app).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file
            .reopen()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("flatpak#FlatpakApp"));
        assert!(content.contains("\"org.gnome.gedit\""));
        assert!(content.contains("flatpak#runtime"));
        assert!(content.contains("flatpak#runtimeVersion"));
        assert!(content.contains("\"50\""));
        assert!(triples > 10);
    }
}
