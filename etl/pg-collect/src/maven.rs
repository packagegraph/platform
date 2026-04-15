use crate::ntriples::{bnode_id, NTriplesWriter};
use crate::uris::*;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::time::Duration;

pub struct MavenCollector {
    client: Client,
    search_base: String,
    repo_base: String,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    response: SearchResponseBody,
}

#[derive(Debug, Deserialize)]
struct SearchResponseBody {
    #[serde(default)]
    docs: Vec<SearchDoc>,
}

#[derive(Debug, Deserialize)]
struct SearchDoc {
    #[serde(rename = "latestVersion")]
    latest_version: String,
}

#[derive(Debug, Default)]
struct PomMetadata {
    group_id: String,
    artifact_id: String,
    version: String,
    description: Option<String>,
    url: Option<String>,
    licenses: Vec<String>,
    dependencies: Vec<PomDependency>,
}

#[derive(Debug, Clone)]
struct PomDependency {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
    scope: Option<String>,
    optional: bool,
}

impl MavenCollector {
    pub fn new(search_base: String, repo_base: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            search_base,
            repo_base,
        }
    }

    pub fn collect(&self, packages_file: &str, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new(file);

        self.emit_distribution_metadata(&mut writer)?;

        let coordinates = read_maven_seed_file(packages_file)?;
        eprintln!("Loaded {} Maven coordinates from seed file", coordinates.len());

        let mut total_packages = 0;
        let mut total_triples = 0;
        let mut base_delay_ms = 200;

        for (idx, coord) in coordinates.iter().enumerate() {
            if (idx + 1) % 100 == 0 {
                eprintln!("Progress: {}/{}", idx + 1, coordinates.len());
            }

            let (group_id, artifact_id) = coord;
            match self.fetch_artifact_with_retry(group_id, artifact_id, &mut base_delay_ms) {
                Ok(pom) => {
                    total_triples += self.emit_artifact_triples(&mut writer, &pom)?;
                    total_packages += 1;
                }
                Err(e) => eprintln!("  Error fetching {}:{}: {}", group_id, artifact_id, e),
            }

            std::thread::sleep(Duration::from_millis(base_delay_ms));
        }

        writer.flush()?;
        Ok((total_packages, total_triples))
    }

    fn emit_distribution_metadata(&self, writer: &mut NTriplesWriter) -> Result<usize> {
        let dist_uri = distro_uri("maven");
        let rel_uri = release_uri("maven", "central");
        let mut triples = 0;

        writer.write_triple(&dist_uri, RDF_TYPE, &format!("{PKG}Distribution"))?;
        writer.write_literal(&dist_uri, &format!("{PKG}projectName"), "Maven Central")?;
        triples += 2;

        writer.write_triple(&rel_uri, RDF_TYPE, &format!("{PKG}DistributionRelease"))?;
        writer.write_literal(&rel_uri, &format!("{PKG}releaseCodename"), "central")?;
        writer.write_triple(&rel_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 3;

        Ok(triples)
    }

    fn fetch_artifact_with_retry(
        &self,
        group_id: &str,
        artifact_id: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<PomMetadata, String> {
        // First, get latest version from search API
        let version = self.get_latest_version(group_id, artifact_id, base_delay_ms)?;

        // Then fetch POM
        let pom_url = self.build_pom_url(group_id, artifact_id, &version);
        let max_attempts = 5;

        for attempt in 0..max_attempts {
            match self.client.get(&pom_url).send() {
                Ok(response) => {
                    if response.status() == StatusCode::TOO_MANY_REQUESTS {
                        let retry_after_secs = response
                            .headers()
                            .get("retry-after")
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or_else(|| 2u64.pow(attempt as u32));

                        eprintln!("  Rate limited, waiting {}s...", retry_after_secs);
                        std::thread::sleep(Duration::from_secs(retry_after_secs));
                        *base_delay_ms = (*base_delay_ms * 2).min(5000);
                        continue;
                    }

                    if response.status() == StatusCode::NOT_FOUND {
                        return Err(format!("404: {}:{}", group_id, artifact_id));
                    }

                    let xml = response.text().map_err(|e| e.to_string())?;
                    return self.parse_pom(&xml, group_id, artifact_id, &version);
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        let delay = Duration::from_secs(2u64.pow(attempt as u32));
                        eprintln!("  Network error, retrying in {:?}...", delay);
                        std::thread::sleep(delay);
                        continue;
                    }
                    return Err(e.to_string());
                }
            }
        }

        Err(format!("Max retries exceeded for {}:{}", group_id, artifact_id))
    }

    fn get_latest_version(
        &self,
        group_id: &str,
        artifact_id: &str,
        base_delay_ms: &mut u64,
    ) -> std::result::Result<String, String> {
        let query = format!("g:{}+AND+a:{}", group_id, artifact_id);
        let url = format!("{}/solrsearch/select?q={}&rows=1&wt=json", self.search_base, query);
        let max_attempts = 3;

        for attempt in 0..max_attempts {
            match self.client.get(&url).send() {
                Ok(response) => {
                    if response.status() == StatusCode::TOO_MANY_REQUESTS {
                        let retry_secs = 2u64.pow(attempt as u32);
                        std::thread::sleep(Duration::from_secs(retry_secs));
                        *base_delay_ms = (*base_delay_ms * 2).min(5000);
                        continue;
                    }

                    let text = response.text().map_err(|e| e.to_string())?;
                    let search_resp: SearchResponse = serde_json::from_str(&text)
                        .map_err(|e| format!("Failed to parse search response: {}", e))?;

                    if let Some(doc) = search_resp.response.docs.first() {
                        return Ok(doc.latest_version.clone());
                    } else {
                        return Err(format!("No version found for {}:{}", group_id, artifact_id));
                    }
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        std::thread::sleep(Duration::from_secs(2u64.pow(attempt as u32)));
                        continue;
                    }
                    return Err(e.to_string());
                }
            }
        }

        Err(format!("Failed to get version for {}:{}", group_id, artifact_id))
    }

    fn build_pom_url(&self, group_id: &str, artifact_id: &str, version: &str) -> String {
        let group_path = group_id.replace('.', "/");
        format!(
            "{}/{}/{}/{}/{}-{}.pom",
            self.repo_base, group_path, artifact_id, version, artifact_id, version
        )
    }

    fn parse_pom(
        &self,
        xml: &str,
        group_id: &str,
        artifact_id: &str,
        version: &str,
    ) -> std::result::Result<PomMetadata, String> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut pom = PomMetadata {
            group_id: group_id.to_string(),
            artifact_id: artifact_id.to_string(),
            version: version.to_string(),
            ..Default::default()
        };

        let mut buf = Vec::new();
        let mut current_element = String::new();
        let mut in_dependencies = false;
        let mut in_licenses = false;
        let mut current_dep = PomDependency {
            group_id: String::new(),
            artifact_id: String::new(),
            version: None,
            scope: None,
            optional: false,
        };

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    current_element = name.clone();
                    if name == "dependencies" {
                        in_dependencies = true;
                    } else if name == "licenses" {
                        in_licenses = true;
                    } else if in_dependencies && name == "dependency" {
                        current_dep = PomDependency {
                            group_id: String::new(),
                            artifact_id: String::new(),
                            version: None,
                            scope: None,
                            optional: false,
                        };
                    }
                }
                Ok(Event::End(e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "dependencies" {
                        in_dependencies = false;
                    } else if name == "licenses" {
                        in_licenses = false;
                    } else if in_dependencies && name == "dependency" {
                        if !current_dep.group_id.is_empty() && !current_dep.artifact_id.is_empty() {
                            pom.dependencies.push(current_dep.clone());
                        }
                    }
                    current_element.clear();
                }
                Ok(Event::Text(e)) => {
                    let text = e.unescape().unwrap_or_default().trim().to_string();
                    if text.is_empty() {
                        continue;
                    }

                    match current_element.as_str() {
                        "description" if !in_dependencies => pom.description = Some(text),
                        "url" if !in_dependencies && !in_licenses => pom.url = Some(text),
                        "name" if in_licenses => {
                            if !pom.licenses.contains(&text) {
                                pom.licenses.push(text);
                            }
                        }
                        "groupId" if in_dependencies => current_dep.group_id = text,
                        "artifactId" if in_dependencies => current_dep.artifact_id = text,
                        "version" if in_dependencies => current_dep.version = Some(text),
                        "scope" if in_dependencies => current_dep.scope = Some(text),
                        "optional" if in_dependencies => current_dep.optional = text == "true",
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(format!("XML parse error: {}", e)),
                _ => {}
            }
            buf.clear();
        }

        Ok(pom)
    }

    fn emit_artifact_triples(&self, writer: &mut NTriplesWriter, pom: &PomMetadata) -> Result<usize> {
        let name = format!("{}/{}", pom.group_id, pom.artifact_id);
        let pkg_uri = package_uri("maven", "central", "any", &name, &pom.version);
        let identity_uri = package_identity_uri("maven", "central", "any", &name);
        let mut triples = 0;

        // Dual typing
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{PKG}Package"))?;
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{MAVEN}MavenArtifact"))?;
        triples += 2;

        // Identity
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), &name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), &name)?;
        triples += 1;

        // Maven-specific coordinates
        writer.write_literal(&pkg_uri, &format!("{MAVEN}groupId"), &pom.group_id)?;
        writer.write_literal(&pkg_uri, &format!("{MAVEN}artifactId"), &pom.artifact_id)?;
        triples += 2;

        // Version
        let ver_uri = version_uri("maven", "central", &name, &pom.version);
        writer.write_triple(&ver_uri, RDF_TYPE, &format!("{PKG}Version"))?;
        writer.write_literal(&ver_uri, &format!("{PKG}versionString"), &pom.version)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}hasVersion"), &ver_uri)?;
        triples += 3;

        // Distribution
        let dist_uri = distro_uri("maven");
        writer.write_triple(&pkg_uri, &format!("{PKG}partOfDistribution"), &dist_uri)?;
        triples += 1;

        // Optional properties
        if let Some(desc) = &pom.description {
            writer.write_literal(&pkg_uri, &format!("{PKG}description"), desc)?;
            triples += 1;
        }
        if let Some(url) = &pom.url {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), url)?;
            triples += 1;
        }
        for license in &pom.licenses {
            writer.write_literal(&pkg_uri, &format!("{PKG}licenseName"), license)?;
            triples += 1;
        }

        // Dependencies
        for dep in &pom.dependencies {
            triples += self.emit_maven_dependency(writer, &pkg_uri, dep)?;
        }

        Ok(triples)
    }

    fn emit_maven_dependency(
        &self,
        writer: &mut NTriplesWriter,
        pkg_uri: &str,
        dep: &PomDependency,
    ) -> Result<usize> {
        let dep_name = format!("{}/{}", dep.group_id, dep.artifact_id);
        let target_uri = package_identity_uri("maven", "central", "any", &dep_name);
        let mut triples = 0;

        writer.write_triple(pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
        triples += 1;

        let scope = dep.scope.as_deref().unwrap_or("compile");
        let bnode = bnode_id(scope, &format!("{}-{}", pkg_uri, &dep_name));
        writer.write_bnode_object(pkg_uri, &format!("{PKG}hasDependency"), &bnode)?;
        writer.write_bnode_subject(&bnode, RDF_TYPE, &format!("{PKG}Dependency"))?;
        writer.write_bnode_subject(&bnode, &format!("{PKG}dependencyTarget"), &target_uri)?;
        writer.write_bnode_literal(&bnode, &format!("{PKG}dependencyType"), scope)?;
        writer.write_bnode_literal(&bnode, &format!("{MAVEN}scope"), scope)?;
        triples += 5;

        if dep.optional {
            writer.write_bnode_literal(&bnode, &format!("{MAVEN}optional"), "true")?;
            triples += 1;
        }

        if let Some(version_constraint) = &dep.version {
            let cb = bnode_id("constraint", &format!("{}-{}", pkg_uri, &dep_name));
            writer.write_bnode_object(&bnode, &format!("{PKG}hasVersionConstraint"), &cb)?;
            writer.write_bnode_subject(&cb, RDF_TYPE, &format!("{PKG}VersionConstraint"))?;
            writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintOperator"), "maven")?;
            writer.write_bnode_literal(&cb, &format!("{PKG}versionConstraintValue"), version_constraint)?;
            triples += 4;
        }

        Ok(triples)
    }
}

/// Read Maven coordinates from seed file (one "groupId:artifactId" per line).
pub fn read_maven_seed_file(path: &str) -> Result<Vec<(String, String)>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut coords = Vec::new();

    for line_result in reader.lines() {
        let line = line_result?;
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some((group_id, artifact_id)) = trimmed.split_once(':') {
            coords.push((group_id.to_string(), artifact_id.to_string()));
        }
    }

    coords.sort();
    coords.dedup();

    Ok(coords)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_maven_seed_file() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "# Comment").unwrap();
        writeln!(temp, "org.apache.commons:commons-lang3").unwrap();
        writeln!(temp, "").unwrap();
        writeln!(temp, "com.google.guava:guava").unwrap();
        writeln!(temp, "org.apache.commons:commons-lang3").unwrap();
        temp.flush().unwrap();

        let coords = read_maven_seed_file(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(coords.len(), 2);
        assert_eq!(coords[0], ("com.google.guava".to_string(), "guava".to_string()));
        assert_eq!(coords[1], ("org.apache.commons".to_string(), "commons-lang3".to_string()));
    }

    #[test]
    fn test_parse_simple_pom() {
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>org.example</groupId>
  <artifactId>my-lib</artifactId>
  <version>1.0.0</version>
  <description>Example library</description>
  <url>https://example.org</url>
  <licenses>
    <license>
      <name>Apache-2.0</name>
    </license>
  </licenses>
  <dependencies>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
      <version>4.13.2</version>
      <scope>test</scope>
    </dependency>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>32.0.0-jre</version>
      <optional>true</optional>
    </dependency>
  </dependencies>
</project>"#;

        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        let pom = collector.parse_pom(pom_xml, "org.example", "my-lib", "1.0.0").unwrap();

        assert_eq!(pom.group_id, "org.example");
        assert_eq!(pom.artifact_id, "my-lib");
        assert_eq!(pom.version, "1.0.0");
        assert_eq!(pom.description.unwrap(), "Example library");
        assert_eq!(pom.url.unwrap(), "https://example.org");
        assert_eq!(pom.licenses.len(), 1);
        assert_eq!(pom.licenses[0], "Apache-2.0");
        assert_eq!(pom.dependencies.len(), 2);
        assert_eq!(pom.dependencies[0].group_id, "junit");
        assert_eq!(pom.dependencies[0].scope.as_deref(), Some("test"));
        assert!(pom.dependencies[1].optional);
    }

    #[test]
    fn test_emit_maven_artifact_with_coordinates() {
        let collector = MavenCollector::new(
            "https://search.maven.org".into(),
            "https://repo1.maven.org/maven2".into(),
        );
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        let pom = PomMetadata {
            group_id: "org.apache.commons".to_string(),
            artifact_id: "commons-lang3".to_string(),
            version: "3.14.0".to_string(),
            description: Some("Apache Commons Lang".to_string()),
            url: Some("https://commons.apache.org/proper/commons-lang/".to_string()),
            licenses: vec!["Apache-2.0".to_string()],
            dependencies: vec![],
        };

        let triples = collector.emit_artifact_triples(&mut writer, &pom).unwrap();
        writer.flush().unwrap();

        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        assert!(content.contains("core#Package"));
        assert!(content.contains("maven#MavenArtifact"));
        assert!(content.contains("maven#groupId"));
        assert!(content.contains("maven#artifactId"));
        assert!(content.contains("\"org.apache.commons\""));
        assert!(content.contains("\"commons-lang3\""));
        assert!(content.contains("\"3.14.0\""));
        assert!(triples > 10);
    }
}
