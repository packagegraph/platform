use crate::ntriples::NTriplesWriter;
use crate::source_cache::SourceCache;
use crate::uris::*;
use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Result};

/// Collects binary package metadata from opkg Packages.gz or apk APKINDEX.tar.gz
pub struct OpkgIndexCollector {
    distro_name: String,
    release_name: String,
    release_url: String,
    arch: String,
}

impl OpkgIndexCollector {
    pub fn new(distro_name: String, release_name: String, release_url: String, arch: String) -> Self {
        Self { distro_name, release_name, release_url, arch }
    }

    pub fn collect(
        &self,
        writer: &mut NTriplesWriter,
        identity_map: &HashMap<String, String>,
        cache: Option<&SourceCache>,
    ) -> Result<(usize, HashMap<String, String>)> {
        let mut digest_map = HashMap::new();
        let mut total_packages = 0;

        // Collect unique feed names from identity_map (derive from source URIs if needed)
        // For now, use known feeds: packages, luci, routing, telephony, base
        let feeds = vec!["packages", "luci", "routing", "telephony", "base"];

        for feed in feeds {
            let url = format!("{}/packages/{}/{}/Packages.gz", self.release_url, self.arch, feed);

            match self.fetch_and_parse_packages_gz(&url, feed, writer, identity_map, &mut digest_map, cache) {
                Ok(count) => {
                    total_packages += count;
                    eprintln!("  {} feed: {} packages", feed, count);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // 404 is expected for some feeds (not all releases have all feeds)
                    eprintln!("  {} feed: not found (OK)", feed);
                }
                Err(e) => {
                    // Other errors (5xx, timeout, corrupt data) are failures
                    return Err(e);
                }
            }
        }

        Ok((total_packages, digest_map))
    }

    fn fetch_and_parse_packages_gz(
        &self,
        url: &str,
        feed: &str,
        writer: &mut NTriplesWriter,
        identity_map: &HashMap<String, String>,
        digest_map: &mut HashMap<String, String>,
        cache: Option<&SourceCache>,
    ) -> Result<usize> {
        // Fetch Packages.gz
        let resp = reqwest::blocking::get(url)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        // Handle 404 gracefully (feed may not exist for this arch)
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Packages.gz not found for {} feed", feed),
            ));
        }

        let response = resp
            .bytes()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
            .to_vec();

        // Decompress and parse
        let decoder = GzDecoder::new(&response[..]);
        let reader = BufReader::new(decoder);
        self.parse_packages_from_reader(reader, writer, identity_map, digest_map)
    }

    fn parse_packages_from_reader<R: BufRead>(
        &self,
        reader: R,
        writer: &mut NTriplesWriter,
        identity_map: &HashMap<String, String>,
        digest_map: &mut HashMap<String, String>,
    ) -> Result<usize> {
        let mut count = 0;
        let mut current_pkg: HashMap<String, String> = HashMap::new();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();

            if trimmed.is_empty() {
                // End of stanza - emit package
                if !current_pkg.is_empty() {
                    self.emit_binary_package(writer, &current_pkg, identity_map, digest_map)?;
                    count += 1;
                    current_pkg.clear();
                }
            } else if let Some(colon_pos) = line.find(':') {
                let field = &line[..colon_pos];
                let value = line[colon_pos + 1..].trim();
                current_pkg.insert(field.to_string(), value.to_string());
            }
        }

        // Last stanza (if file doesn't end with blank line)
        if !current_pkg.is_empty() {
            self.emit_binary_package(writer, &current_pkg, identity_map, digest_map)?;
            count += 1;
        }

        Ok(count)
    }

    fn emit_binary_package(
        &self,
        writer: &mut NTriplesWriter,
        pkg: &HashMap<String, String>,
        identity_map: &HashMap<String, String>,
        digest_map: &mut HashMap<String, String>,
    ) -> Result<usize> {
        let name = pkg.get("Package").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing Package field")
        })?;

        let version = pkg.get("Version").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing Version field")
        })?;

        // Get actual architecture from index (may differ from CLI --arch for "all" packages)
        let arch = pkg.get("Architecture").unwrap_or(&self.arch);

        // Binary package URI (uses actual arch from index, not CLI arch)
        let pkg_uri = package_uri(&self.distro_name, &self.release_name, arch, name, version);
        let identity_uri = package_identity_uri(&self.distro_name, &self.release_name, arch, name);

        let mut triples = 0;

        // Type as BinaryIPK (opkg format)
        writer.write_triple(&pkg_uri, RDF_TYPE, &format!("{OPENWRT}BinaryIPK"))?;
        triples += 1;

        // Package name
        writer.write_literal(&pkg_uri, &format!("{PKG}packageName"), name)?;
        triples += 1;

        // Link to binary identity (isVersionOf)
        writer.write_triple(&identity_uri, RDF_TYPE, &format!("{PKG}PackageIdentity"))?;
        writer.write_literal(&identity_uri, &format!("{PKG}packageName"), name)?;
        writer.write_triple(&pkg_uri, &format!("{PKG}isVersionOf"), &identity_uri)?;
        triples += 3;

        // Source linkage via builtFromSource (if in identity_map)
        if let Some(source_uri) = identity_map.get(name) {
            writer.write_triple(&pkg_uri, &format!("{PKG}builtFromSource"), source_uri)?;
            triples += 1;
        } else {
            // Standalone binary (base/ or other unmatched)
            eprintln!("  Creating standalone binary node for {} (not in feed identity map)", name);
        }

        // Binary metadata - targetArchitecture
        if let Some(arch) = pkg.get("Architecture") {
            let arch_uri = arch_uri(arch);
            writer.write_triple(&pkg_uri, &format!("{PKG}targetArchitecture"), &arch_uri)?;
            writer.write_triple(&arch_uri, RDF_TYPE, &format!("{PKG}Architecture"))?;
            writer.write_literal(&arch_uri, &format!("{PKG}architectureName"), arch)?;
            triples += 3;
        }

        // hasChecksum
        if let Some(sha256) = pkg.get("SHA256sum") {
            let checksum_uri = format!("{}#sha256", pkg_uri);
            writer.write_triple(&pkg_uri, &format!("{PKG}hasChecksum"), &checksum_uri)?;
            writer.write_triple(&checksum_uri, RDF_TYPE, &format!("{PKG}Checksum"))?;
            writer.write_literal(&checksum_uri, &format!("{PKG}checksumAlgorithm"), "SHA256")?;
            writer.write_literal(&checksum_uri, &format!("{PKG}checksumValue"), sha256)?;
            triples += 4;

            // Build digest_map for attestation stage
            let digest_key = format!("sha256:{}", sha256);
            digest_map.insert(digest_key, pkg_uri.clone());
        }

        // installedSize
        if let Some(size_str) = pkg.get("Installed-Size") {
            if let Ok(size) = size_str.parse::<i64>() {
                writer.write_typed_literal(&pkg_uri, &format!("{OPENWRT}installedSize"), &size.to_string(), &format!("{XSD}integer"))?;
                triples += 1;
            }
        }

        // packageSize (download size)
        if let Some(size_str) = pkg.get("Size") {
            if let Ok(size) = size_str.parse::<i64>() {
                writer.write_typed_literal(&pkg_uri, &format!("{PKG}packageSize"), &size.to_string(), &format!("{XSD}long"))?;
                triples += 1;
            }
        }

        // opkgFilename
        if let Some(filename) = pkg.get("Filename") {
            writer.write_literal(&pkg_uri, &format!("{OPENWRT}opkgFilename"), filename)?;
            triples += 1;
        }

        // Dependencies (opkg format: space-separated, +dep required, -dep conflict)
        if let Some(depends_str) = pkg.get("Depends") {
            for dep_raw in depends_str.split(',') {
                let dep = dep_raw.trim().trim_start_matches('+');
                if dep.is_empty() || dep.starts_with('-') {
                    continue; // Skip conflicts for now
                }

                // Strip version constraints (e.g., "libc (>= 2.0)" → "libc")
                let dep_name = if let Some(paren_pos) = dep.find('(') {
                    dep[..paren_pos].trim()
                } else {
                    dep
                };

                let target_uri = package_identity_uri(&self.distro_name, &self.release_name, arch, dep_name);
                writer.write_triple(&pkg_uri, &format!("{PKG}directlyDependsOn"), &target_uri)?;
                triples += 1;
            }
        }

        Ok(triples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufReader, Read};
    use tempfile::NamedTempFile;

    #[test]
    fn test_opkg_packages_gz_parse() {
        // Synthetic opkg Packages data
        let packages_data = "Package: testpkg
Version: 1.0-1
Depends: libc
Architecture: mips_24kc
Installed-Size: 10240
Filename: testpkg_1.0-1_mips_24kc.ipk
Size: 5000
SHA256sum: abc123
Description: Test package

Package: anotherpkg
Version: 2.0-1
Architecture: mips_24kc
Installed-Size: 20480
Filename: anotherpkg_2.0-1_mips_24kc.ipk
Size: 8000
SHA256sum: def456
Description: Another test
";

        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

        // identity_map: testpkg exists (matched to source), anotherpkg doesn't (standalone)
        let mut identity_map = HashMap::new();
        let source_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/testpkg/1.0";
        identity_map.insert("testpkg".to_string(), source_uri.to_string());

        let collector = OpkgIndexCollector::new(
            "openwrt".into(),
            "24.10".into(),
            "https://downloads.openwrt.org/releases/24.10.0".into(),
            "mips_24kc".into(),
        );

        let mut digest_map = HashMap::new();
        let reader = BufReader::new(packages_data.as_bytes());
        let count = collector.parse_packages_from_reader(reader, &mut writer, &identity_map, &mut digest_map).unwrap();

        assert_eq!(count, 2, "Should parse 2 packages");

        // Verify digest_map built
        assert!(digest_map.contains_key("sha256:abc123"), "Should have testpkg digest");
        assert!(digest_map.contains_key("sha256:def456"), "Should have anotherpkg digest");

        writer.flush().unwrap();

        // Verify output
        let mut content = String::new();
        temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

        // Should have BinaryIPK types
        assert!(content.contains("opkg#BinaryIPK"), "Should emit BinaryIPK nodes");

        // testpkg should have builtFromSource link
        assert!(content.contains("builtFromSource"), "Should link to source");
        assert!(content.contains(source_uri), "Should link testpkg to its source URI");

        // Both should have binary metadata
        assert!(content.contains("installedSize"), "Should have installed size");
        assert!(content.contains("opkgFilename"), "Should have filename");
        assert!(content.contains("hasChecksum"), "Should have checksum");
    }
}
