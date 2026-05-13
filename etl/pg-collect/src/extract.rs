use crate::sparql::SparqlClient;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{Error, ErrorKind, Result};
use std::path::Path;

/// Top-level TOML configuration for test corpus extraction.
#[derive(Debug, Deserialize)]
pub struct ExtractConfig {
    pub global: GlobalConfig,
    pub seeds: SeedConfig,
}

#[derive(Debug, Deserialize)]
pub struct GlobalConfig {
    pub max_triples: usize,
    pub depth: usize,
    pub fan_out: usize,
}

/// Seeds organized by category. Each category is a table of ecosystem -> package list.
#[derive(Debug, Deserialize)]
pub struct SeedConfig {
    pub linux_distro: Option<LinuxDistroSeeds>,
    pub language_ecosystem: Option<HashMap<String, Vec<String>>>,
    pub app_store: Option<HashMap<String, Vec<String>>>,
    pub embedded: Option<HashMap<String, Vec<String>>>,
    pub system: Option<HashMap<String, Vec<String>>>,
}

#[derive(Debug, Deserialize)]
pub struct LinuxDistroSeeds {
    pub packages: Vec<String>,
}

impl ExtractConfig {
    /// Load config from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to read config {}: {}", path.display(), e)))?;
        toml::from_str(&content)
            .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to parse config {}: {}", path.display(), e)))
    }

    /// Collect all seed package names into a flat deduplicated list.
    pub fn all_seed_names(&self) -> Vec<String> {
        let mut names = Vec::new();

        if let Some(ref distro) = self.seeds.linux_distro {
            names.extend(distro.packages.iter().cloned());
        }

        let tables = [
            &self.seeds.language_ecosystem,
            &self.seeds.app_store,
            &self.seeds.embedded,
            &self.seeds.system,
        ];
        for table in tables {
            if let Some(ref map) = table {
                for packages in map.values() {
                    names.extend(packages.iter().cloned());
                }
            }
        }

        names.sort();
        names.dedup();
        names
    }
}

/// Resolve seed package names to URIs across all named graphs.
///
/// Issues one SELECT query per seed name. Returns a map of graph URI -> set of package URIs.
/// Unresolved names are logged as warnings.
pub fn resolve_seeds(
    client: &SparqlClient,
    seed_names: &[String],
) -> Result<HashMap<String, HashSet<String>>> {
    let mut graph_uris: HashMap<String, HashSet<String>> = HashMap::new();
    let mut resolved_count = 0;

    for name in seed_names {
        let sparql = format!(
            "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
             SELECT DISTINCT ?pkg ?g WHERE {{\n\
               GRAPH ?g {{\n\
                 ?pkg pkg:packageName \"{}\" .\n\
               }}\n\
             }}",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        );

        match client.query(&sparql) {
            Ok(bindings) => {
                if bindings.is_empty() {
                    eprintln!("  Warning: seed \"{}\" not found in any graph", name);
                } else {
                    resolved_count += 1;
                    for binding in &bindings {
                        if let (Some(pkg), Some(g)) = (binding.get("pkg"), binding.get("g")) {
                            graph_uris.entry(g.clone()).or_default().insert(pkg.clone());
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("  Warning: failed to resolve seed \"{}\": {}", name, e);
            }
        }
    }

    eprintln!("Phase 1: Resolved {}/{} seed packages across {} graphs",
        resolved_count, seed_names.len(), graph_uris.len());

    Ok(graph_uris)
}

/// Predicates to follow during BFS expansion.
const BFS_PREDICATES: &[&str] = &[
    "https://purl.org/packagegraph/ontology/core#directlyDependsOn",
    "https://purl.org/packagegraph/ontology/core#buildDependsOn",
    "https://purl.org/packagegraph/ontology/core#isDirectDependencyOf",
    "https://purl.org/packagegraph/ontology/core#hasVersion",
    "https://purl.org/packagegraph/ontology/core#versionOf",
    "https://purl.org/packagegraph/ontology/core#isVersionOf",
    "https://purl.org/packagegraph/ontology/core#provides",
    "https://purl.org/packagegraph/ontology/core#directlyProvides",
    "https://purl.org/packagegraph/ontology/core#conflicts",
    "https://purl.org/packagegraph/ontology/core#partOfDistribution",
    "https://purl.org/packagegraph/ontology/core#partOfRelease",
    "https://purl.org/packagegraph/ontology/core#maintainedBy",
    "https://purl.org/packagegraph/ontology/core#hasUpstreamProject",
    "https://purl.org/packagegraph/ontology/core#builtFromSource",
    "https://purl.org/packagegraph/ontology/core#memberOfPackageSet",
];

/// BFS-expand a seed set within a single named graph.
///
/// Walks outward from `seeds` along `BFS_PREDICATES` for `depth` hops,
/// capping fan-out at `fan_out` neighbors per (seed, predicate) pair.
/// Returns the full set of discovered URIs (including the original seeds).
pub fn bfs_expand(
    client: &SparqlClient,
    graph_uri: &str,
    seeds: &HashSet<String>,
    depth: usize,
    fan_out: usize,
) -> Result<HashSet<String>> {
    let mut visited = seeds.clone();
    let mut frontier: Vec<String> = seeds.iter().cloned().collect();

    let predicates_values: String = BFS_PREDICATES.iter()
        .map(|p| format!("<{}>", p))
        .collect::<Vec<_>>()
        .join(" ");

    for hop in 0..depth {
        if frontier.is_empty() {
            break;
        }

        let mut next_frontier = Vec::new();

        for batch in frontier.chunks(50) {
            let uri_values: String = batch.iter()
                .map(|u| format!("<{}>", u))
                .collect::<Vec<_>>()
                .join(" ");

            let sparql = format!(
                "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>\n\
                 SELECT ?seed ?predicate ?neighbor WHERE {{\n\
                   GRAPH <{}> {{\n\
                     VALUES ?seed {{ {} }}\n\
                     VALUES ?predicate {{ {} }}\n\
                     ?seed ?predicate ?neighbor .\n\
                     FILTER(isIRI(?neighbor))\n\
                   }}\n\
                 }}",
                graph_uri, uri_values, predicates_values
            );

            let bindings = client.query(&sparql)?;

            // Group by (seed, predicate) and enforce fan-out cap
            let mut groups: HashMap<(String, String), Vec<String>> = HashMap::new();
            for binding in &bindings {
                if let (Some(seed), Some(pred), Some(neighbor)) = (
                    binding.get("seed"),
                    binding.get("predicate"),
                    binding.get("neighbor"),
                ) {
                    groups.entry((seed.clone(), pred.clone()))
                        .or_default()
                        .push(neighbor.clone());
                }
            }

            for ((_seed, _pred), neighbors) in &groups {
                for neighbor in neighbors.iter().take(fan_out) {
                    if visited.insert(neighbor.clone()) {
                        next_frontier.push(neighbor.clone());
                    }
                }
            }
        }

        eprintln!("  Hop {}: +{} URIs (total {})", hop + 1, next_frontier.len(), visited.len());
        frontier = next_frontier;
    }

    Ok(visited)
}

/// Extract all triples for a set of URIs from a named graph.
///
/// Issues batched CONSTRUCT queries where the subject is in the URI set.
/// Only includes triples where both subject and object (if a URI) are in the set,
/// preventing dangling references. Deduplicates across batches.
pub fn extract_triples(
    client: &SparqlClient,
    graph_uri: &str,
    uris: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut all_triples: HashSet<String> = HashSet::new();
    let uri_list: Vec<&String> = uris.iter().collect();

    // Extract triples where subject is in our set
    for batch in uri_list.chunks(50) {
        let values: String = batch.iter()
            .map(|u| format!("<{}>", u))
            .collect::<Vec<_>>()
            .join(" ");

        let sparql = format!(
            "CONSTRUCT {{ ?s ?p ?o }}\n\
             WHERE {{\n\
               GRAPH <{}> {{\n\
                 VALUES ?s {{ {} }}\n\
                 ?s ?p ?o .\n\
               }}\n\
             }}",
            graph_uri, values
        );

        let triples = client.query_construct(&sparql)?;
        for triple in triples {
            all_triples.insert(triple);
        }
    }

    // Filter: keep only triples where object URIs are also in our set
    // (literal objects are always kept)
    let filtered: Vec<String> = all_triples.into_iter().filter(|triple| {
        // If the object is a URI (starts with <, ends with > before the dot),
        // check it's in our extraction set
        if let Some(obj_start) = triple.rfind("> <") {
            // Object is a URI — extract it
            if let Some(obj) = triple.get(obj_start + 2..) {
                let obj = obj.trim_end_matches(" .").trim();
                if obj.starts_with('<') && obj.ends_with('>') {
                    let uri = &obj[1..obj.len()-1];
                    return uris.contains(uri);
                }
            }
        }
        // Literal objects or unparseable lines: keep them
        true
    }).collect();

    Ok(filtered)
}

/// Reference set of classes and predicates from the ontology.
#[derive(Debug)]
pub struct OntologyReferenceSet {
    pub classes: HashSet<String>,
    pub predicates: HashSet<String>,
    pub shacl_targets: HashSet<String>,
}

/// Coverage audit report.
#[derive(Debug, serde::Serialize)]
pub struct CoverageReport {
    pub generated_at: String,
    pub classes_total: usize,
    pub classes_covered: usize,
    pub classes_missing: Vec<String>,
    pub predicates_total: usize,
    pub predicates_covered: usize,
    pub predicates_missing: Vec<String>,
    pub shacl_total: usize,
    pub shacl_covered: usize,
    pub shacl_missing: Vec<String>,
    pub per_graph: HashMap<String, GraphStats>,
}

#[derive(Debug, serde::Serialize)]
pub struct GraphStats {
    pub triples: usize,
    pub types: usize,
    pub predicates: usize,
}

/// Parse ontology `.ttl` files to extract the reference set of classes and predicates.
///
/// Scans `ontology_dir` recursively for `*.ttl` files (excluding examples/test files).
/// Uses regex to find `owl:Class`, `owl:ObjectProperty`, `owl:DatatypeProperty` declarations
/// and `sh:targetClass` values. This is a lightweight parse, not a full Turtle parser.
pub fn parse_ontology_reference(ontology_dir: &Path) -> Result<OntologyReferenceSet> {
    let pattern = format!("{}/**/*.ttl", ontology_dir.display());
    let class_re = regex::Regex::new(r"^(\S+)\s+a\s+owl:Class").unwrap();
    let prop_re = regex::Regex::new(r"^(\S+)\s+a\s+owl:(ObjectProperty|DatatypeProperty)").unwrap();
    let shacl_re = regex::Regex::new(r"sh:targetClass\s+(\S+)").unwrap();
    let prefix_re = regex::Regex::new(r"^@prefix\s+(\w+):\s+<([^>]+)>").unwrap();

    let mut classes = HashSet::new();
    let mut predicates = HashSet::new();
    let mut shacl_targets = HashSet::new();

    let files: Vec<_> = glob::glob(&pattern)
        .map_err(|e| Error::new(ErrorKind::Other, format!("Glob error: {}", e)))?
        .filter_map(|r| r.ok())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_str().unwrap_or_default();
            !name.contains("examples") && !name.contains("test")
        })
        .collect();

    for file_path in &files {
        let content = std::fs::read_to_string(file_path)?;
        let mut prefixes: HashMap<String, String> = HashMap::new();

        for line in content.lines() {
            let line = line.trim();

            // Collect prefix declarations
            if let Some(caps) = prefix_re.captures(line) {
                prefixes.insert(caps[1].to_string(), caps[2].to_string());
            }

            // Match class declarations
            if let Some(caps) = class_re.captures(line) {
                let name = &caps[1];
                if let Some(expanded) = expand_prefixed(name, &prefixes) {
                    classes.insert(expanded);
                }
            }

            // Match property declarations
            if let Some(caps) = prop_re.captures(line) {
                let name = &caps[1];
                if let Some(expanded) = expand_prefixed(name, &prefixes) {
                    predicates.insert(expanded);
                }
            }

            // Match SHACL target classes
            if let Some(caps) = shacl_re.captures(line) {
                let name = &caps[1];
                if let Some(expanded) = expand_prefixed(name, &prefixes) {
                    shacl_targets.insert(expanded);
                }
            }
        }
    }

    eprintln!("Ontology: {} files, {} classes, {} predicates, {} SHACL targets",
        files.len(), classes.len(), predicates.len(), shacl_targets.len());

    Ok(OntologyReferenceSet { classes, predicates, shacl_targets })
}

/// Expand a prefixed name (e.g., "pkg:Package") to its full URI.
fn expand_prefixed(name: &str, prefixes: &HashMap<String, String>) -> Option<String> {
    if name.starts_with('<') && name.ends_with('>') {
        // Already a full URI
        Some(name[1..name.len()-1].to_string())
    } else if let Some(colon_pos) = name.find(':') {
        let prefix = &name[..colon_pos];
        let local = &name[colon_pos+1..];
        // Strip trailing semicolons/commas/periods from local name
        let local = local.trim_end_matches(|c| c == ';' || c == ',' || c == '.');
        prefixes.get(prefix).map(|base| format!("{}{}", base, local))
    } else {
        None
    }
}

/// Compute coverage of extracted triples against the ontology reference set.
pub fn compute_coverage(triples: &[String], ref_set: &OntologyReferenceSet) -> CoverageReport {
    let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    let mut found_classes = HashSet::new();
    let mut found_predicates = HashSet::new();

    for triple in triples {
        // Extract predicate (second URI in the triple)
        let parts: Vec<&str> = triple.splitn(3, ' ').collect();
        if parts.len() < 3 {
            continue;
        }
        let predicate = parts[1].trim_start_matches('<').trim_end_matches('>');
        found_predicates.insert(predicate.to_string());

        // If this is an rdf:type triple, extract the class
        if predicate == rdf_type {
            let object = parts[2].trim().trim_end_matches(" .");
            let class_uri = object.trim_start_matches('<').trim_end_matches('>');
            found_classes.insert(class_uri.to_string());
        }
    }

    let classes_missing: Vec<String> = ref_set.classes.difference(&found_classes)
        .cloned().collect();
    let predicates_missing: Vec<String> = ref_set.predicates.difference(&found_predicates)
        .cloned().collect();
    let shacl_missing: Vec<String> = ref_set.shacl_targets.difference(&found_classes)
        .cloned().collect();

    CoverageReport {
        generated_at: String::new(),
        classes_total: ref_set.classes.len(),
        classes_covered: ref_set.classes.len() - classes_missing.len(),
        classes_missing,
        predicates_total: ref_set.predicates.len(),
        predicates_covered: ref_set.predicates.len() - predicates_missing.len(),
        predicates_missing,
        shacl_total: ref_set.shacl_targets.len(),
        shacl_covered: ref_set.shacl_targets.len() - shacl_missing.len(),
        shacl_missing,
        per_graph: HashMap::new(),
    }
}

/// Manifest entry for one output file.
#[derive(Debug, serde::Serialize)]
pub struct ManifestEntry {
    pub path: String,
    pub graph: String,
    pub triples: usize,
}

/// Full manifest for the test corpus.
#[derive(Debug, serde::Serialize)]
pub struct Manifest {
    pub generated_at: String,
    pub endpoint: String,
    pub config: String,
    pub ontology_dir: String,
    pub depth: usize,
    pub fan_out: usize,
    pub total_triples: usize,
    pub files: Vec<ManifestEntry>,
}

/// Run the full test corpus extraction pipeline.
///
/// Orchestrates all four phases:
/// 1. Seed resolution
/// 2. BFS expansion
/// 3. Triple extraction
/// 4. Coverage audit and gap fill
pub fn run(
    endpoint: &str,
    config_path: &Path,
    ontology_dir: &Path,
    output_dir: &Path,
    max_triples_override: Option<usize>,
    depth_override: Option<usize>,
    fan_out_override: Option<usize>,
) -> Result<()> {
    eprintln!("=== PackageGraph Test Corpus Extraction ===");

    // Load config
    let config = ExtractConfig::load(config_path)?;
    let max_triples = max_triples_override.unwrap_or(config.global.max_triples);
    let depth = depth_override.unwrap_or(config.global.depth);
    let fan_out = fan_out_override.unwrap_or(config.global.fan_out);

    eprintln!("Config: {}", config_path.display());
    eprintln!("Endpoint: {}", endpoint);
    eprintln!("Max triples: {}", max_triples);

    let client = SparqlClient::new(endpoint);

    // Parse ontology reference set
    let ref_set = parse_ontology_reference(ontology_dir)?;

    // Phase 1: Seed resolution
    eprintln!("\n--- Phase 1: Seed Resolution ---");
    let seed_names = config.all_seed_names();
    let graph_seeds = resolve_seeds(&client, &seed_names)?;

    // Phase 2: BFS expansion per graph
    eprintln!("\n--- Phase 2: BFS Expansion ---");
    let mut graph_uris: HashMap<String, HashSet<String>> = HashMap::new();
    let mut total_uri_count = 0;

    for (graph, seeds) in &graph_seeds {
        eprintln!("Graph <{}>: {} seeds", graph, seeds.len());
        let expanded = bfs_expand(&client, graph, seeds, depth, fan_out)?;
        total_uri_count += expanded.len();
        graph_uris.insert(graph.clone(), expanded);

        // Size check
        let estimated = total_uri_count * 50;
        if estimated > max_triples {
            eprintln!("  Size estimate ({}) exceeds max_triples ({}), stopping expansion",
                estimated, max_triples);
            break;
        }
    }
    eprintln!("Phase 2: {} total URIs across {} graphs", total_uri_count, graph_uris.len());

    // Phase 3: Triple extraction
    eprintln!("\n--- Phase 3: Triple Extraction ---");
    std::fs::create_dir_all(output_dir.join("collector"))?;
    std::fs::create_dir_all(output_dir.join("enrichment"))?;

    let mut all_triples: Vec<String> = Vec::new();
    let mut manifest_entries: Vec<ManifestEntry> = Vec::new();

    for (graph, uris) in &graph_uris {
        let triples = extract_triples(&client, graph, uris)?;
        let count = triples.len();

        // Determine output file path from graph URI
        let file_name = graph_uri_to_filename(graph);
        let subdir = if graph.contains("/enrichment/") { "enrichment" } else { "collector" };
        let rel_path = format!("{}/{}", subdir, file_name);
        let full_path = output_dir.join(&rel_path);

        // Write triples to file
        let mut file = std::fs::File::create(&full_path)?;
        use std::io::Write;
        for triple in &triples {
            writeln!(file, "{}", triple)?;
        }

        eprintln!("  {} — {} triples", rel_path, count);

        manifest_entries.push(ManifestEntry {
            path: rel_path,
            graph: graph.clone(),
            triples: count,
        });

        all_triples.extend(triples);
    }

    // Phase 4: Coverage audit
    eprintln!("\n--- Phase 4: Coverage Audit ---");
    let mut report = compute_coverage(&all_triples, &ref_set);

    // Gap fill for missing classes
    let gap_fill_count = gap_fill(&client, &ref_set, &report, &mut all_triples, output_dir, &mut manifest_entries)?;
    if gap_fill_count > 0 {
        // Recompute coverage after gap fill
        report = compute_coverage(&all_triples, &ref_set);
    }

    let total_triples = all_triples.len();
    eprintln!("Coverage: {}/{} classes, {}/{} predicates, {}/{} SHACL shapes",
        report.classes_covered, report.classes_total,
        report.predicates_covered, report.predicates_total,
        report.shacl_covered, report.shacl_total);
    if !report.classes_missing.is_empty() {
        eprintln!("  Missing classes: {:?}", report.classes_missing);
    }
    if !report.predicates_missing.is_empty() {
        eprintln!("  Predicates with no instances: {:?}", report.predicates_missing);
    }

    // Write coverage report
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    report.generated_at = timestamp.clone();

    let report_json = serde_json::to_string_pretty(&report)
        .map_err(|e| Error::new(ErrorKind::Other, format!("JSON serialize error: {}", e)))?;
    std::fs::write(output_dir.join("coverage-report.json"), report_json)?;

    // Write manifest
    let manifest = Manifest {
        generated_at: timestamp,
        endpoint: endpoint.to_string(),
        config: config_path.display().to_string(),
        ontology_dir: ontology_dir.display().to_string(),
        depth,
        fan_out,
        total_triples,
        files: manifest_entries,
    };
    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| Error::new(ErrorKind::Other, format!("JSON serialize error: {}", e)))?;
    std::fs::write(output_dir.join("manifest.json"), manifest_json)?;

    eprintln!("\nFinal: {} triples", total_triples);
    eprintln!("Output: {}", output_dir.display());

    Ok(())
}

/// Convert a graph URI to a filename.
/// e.g., "https://packagegraph.github.io/graph/fedora/43" -> "fedora-43.nt"
fn graph_uri_to_filename(graph_uri: &str) -> String {
    let path = graph_uri
        .trim_end_matches('/')
        .rsplit_once("/graph/")
        .map(|(_, rest)| rest)
        .unwrap_or(graph_uri);
    format!("{}.nt", path.replace('/', "-"))
}

/// Attempt to fill coverage gaps by finding packages that use missing classes.
fn gap_fill(
    client: &SparqlClient,
    _ref_set: &OntologyReferenceSet,
    report: &CoverageReport,
    all_triples: &mut Vec<String>,
    output_dir: &Path,
    manifest_entries: &mut Vec<ManifestEntry>,
) -> Result<usize> {
    if report.classes_missing.is_empty() {
        return Ok(0);
    }

    eprintln!("  Gap fill: {} missing classes", report.classes_missing.len());
    let mut gap_triples = 0;

    for missing_class in &report.classes_missing {
        let sparql = format!(
            "SELECT ?pkg ?g WHERE {{\n\
               GRAPH ?g {{ ?pkg a <{}> . }}\n\
             }} LIMIT 1",
            missing_class
        );

        match client.query(&sparql) {
            Ok(bindings) if !bindings.is_empty() => {
                if let (Some(pkg), Some(g)) = (bindings[0].get("pkg"), bindings[0].get("g")) {
                    // Mini BFS depth 1 around this package
                    let mut mini_seeds = HashSet::new();
                    mini_seeds.insert(pkg.clone());
                    let expanded = bfs_expand(client, g, &mini_seeds, 1, 5)?;
                    let triples = extract_triples(client, g, &expanded)?;
                    let count = triples.len();

                    // Append to gap-fill file
                    let gap_path = output_dir.join("collector/gap-fill.nt");
                    let mut file = std::fs::OpenOptions::new()
                        .create(true).append(true).open(&gap_path)?;
                    use std::io::Write;
                    for triple in &triples {
                        writeln!(file, "{}", triple)?;
                    }

                    all_triples.extend(triples);
                    gap_triples += count;
                    eprintln!("    {} — found in <{}>, +{} triples", missing_class, g, count);
                }
            }
            _ => {
                eprintln!("    {} — not found in any graph (no instances exist)", missing_class);
            }
        }
    }

    if gap_triples > 0 {
        manifest_entries.push(ManifestEntry {
            path: "collector/gap-fill.nt".to_string(),
            graph: "gap-fill".to_string(),
            triples: gap_triples,
        });
    }

    Ok(gap_triples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_parse_config() {
        let toml_content = r#"
[global]
max_triples = 1_000_000
depth = 2
fan_out = 10

[seeds.linux_distro]
packages = ["openssl", "curl"]

[seeds.language_ecosystem]
npm = ["express", "lodash"]
pypi = ["requests"]

[seeds.embedded]
yocto = ["busybox"]
"#;
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        write!(temp, "{}", toml_content).unwrap();
        temp.flush().unwrap();

        let config = ExtractConfig::load(temp.path()).unwrap();
        assert_eq!(config.global.max_triples, 1_000_000);
        assert_eq!(config.global.depth, 2);
        assert_eq!(config.global.fan_out, 10);

        let distro = config.seeds.linux_distro.as_ref().unwrap();
        assert_eq!(distro.packages, vec!["openssl", "curl"]);

        let lang = config.seeds.language_ecosystem.as_ref().unwrap();
        assert_eq!(lang["npm"], vec!["express", "lodash"]);
        assert_eq!(lang["pypi"], vec!["requests"]);
    }

    #[test]
    fn test_all_seed_names_deduplicates() {
        let toml_content = r#"
[global]
max_triples = 1_000_000
depth = 2
fan_out = 10

[seeds.linux_distro]
packages = ["openssl", "curl"]

[seeds.system]
alpine = ["openssl", "busybox"]
"#;
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        write!(temp, "{}", toml_content).unwrap();
        temp.flush().unwrap();

        let config = ExtractConfig::load(temp.path()).unwrap();
        let names = config.all_seed_names();

        // "openssl" appears in both linux_distro and system.alpine but should be deduped
        assert_eq!(names.iter().filter(|n| *n == "openssl").count(), 1);
        assert!(names.contains(&"curl".to_string()));
        assert!(names.contains(&"busybox".to_string()));
    }

    #[test]
    fn test_resolve_seeds_parses_sparql_results() {
        let mut server = mockito::Server::new();

        // Mock: "openssl" resolves to two graphs
        let mock = server.mock("POST", "/sparql")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(r#"{
                "results": {
                    "bindings": [
                        {"pkg": {"type": "uri", "value": "http://example.org/pkg/fedora/openssl"}, "g": {"type": "uri", "value": "http://example.org/graph/fedora"}},
                        {"pkg": {"type": "uri", "value": "http://example.org/pkg/debian/openssl"}, "g": {"type": "uri", "value": "http://example.org/graph/debian"}}
                    ]
                }
            }"#)
            .expect(1)
            .create();

        let client = crate::sparql::SparqlClient::new(&server.url());
        let seed_names = vec!["openssl".to_string()];
        let result = resolve_seeds(&client, &seed_names);

        mock.assert();
        let resolved = result.unwrap();
        assert_eq!(resolved.len(), 2); // two graphs
        assert!(resolved.contains_key("http://example.org/graph/fedora"));
        assert!(resolved.contains_key("http://example.org/graph/debian"));
        assert!(resolved["http://example.org/graph/fedora"].contains("http://example.org/pkg/fedora/openssl"));
    }

    #[test]
    fn test_bfs_expand_depth_1() {
        let mut server = mockito::Server::new();

        // BFS query returns two neighbors for the seed
        let mock = server.mock("POST", "/sparql")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(r#"{
                "results": {
                    "bindings": [
                        {"seed": {"type": "uri", "value": "http://ex/pkg1"}, "predicate": {"type": "uri", "value": "http://ex/dep"}, "neighbor": {"type": "uri", "value": "http://ex/pkg2"}},
                        {"seed": {"type": "uri", "value": "http://ex/pkg1"}, "predicate": {"type": "uri", "value": "http://ex/dep"}, "neighbor": {"type": "uri", "value": "http://ex/pkg3"}}
                    ]
                }
            }"#)
            .expect_at_least(1)
            .create();

        let client = crate::sparql::SparqlClient::new(&server.url());
        let mut seeds = HashSet::new();
        seeds.insert("http://ex/pkg1".to_string());

        let result = bfs_expand(&client, "http://ex/graph/test", &seeds, 1, 20);

        mock.assert();
        let expanded = result.unwrap();
        assert!(expanded.contains("http://ex/pkg1"));
        assert!(expanded.contains("http://ex/pkg2"));
        assert!(expanded.contains("http://ex/pkg3"));
        assert_eq!(expanded.len(), 3);
    }

    #[test]
    fn test_bfs_expand_fan_out_cap() {
        let mut server = mockito::Server::new();

        // Return 5 neighbors, but fan_out is 2
        let bindings: Vec<String> = (0..5).map(|i| format!(
            r#"{{"seed": {{"type": "uri", "value": "http://ex/pkg1"}}, "predicate": {{"type": "uri", "value": "http://ex/dep"}}, "neighbor": {{"type": "uri", "value": "http://ex/n{i}"}}}}"#
        )).collect();
        let body = format!(r#"{{"results": {{"bindings": [{}]}}}}"#, bindings.join(","));

        let _mock = server.mock("POST", "/sparql")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(&body)
            .expect_at_least(1)
            .create();

        let client = crate::sparql::SparqlClient::new(&server.url());
        let mut seeds = HashSet::new();
        seeds.insert("http://ex/pkg1".to_string());

        let result = bfs_expand(&client, "http://ex/graph/test", &seeds, 1, 2);
        let expanded = result.unwrap();

        // seed + at most 2 neighbors (fan_out cap)
        assert!(expanded.contains("http://ex/pkg1"));
        // Total should be seed (1) + capped neighbors (2) = 3
        assert!(expanded.len() <= 3);
    }

    #[test]
    fn test_extract_triples_for_uris() {
        let mut server = mockito::Server::new();

        let _mock = server.mock("POST", "/sparql")
            .match_header("accept", "application/n-triples")
            .with_status(200)
            .with_header("content-type", "application/n-triples")
            .with_body("<http://ex/pkg1> <http://ex/name> \"openssl\" .\n<http://ex/pkg1> <http://ex/dep> <http://ex/pkg2> .\n")
            .expect_at_least(1)
            .create();

        let client = crate::sparql::SparqlClient::new(&server.url());
        let mut uris = HashSet::new();
        uris.insert("http://ex/pkg1".to_string());
        uris.insert("http://ex/pkg2".to_string());

        let triples = extract_triples(&client, "http://ex/graph/test", &uris).unwrap();

        assert!(!triples.is_empty());
        assert!(triples.contains(&"<http://ex/pkg1> <http://ex/name> \"openssl\" .".to_string()));
    }

    #[test]
    fn test_parse_ontology_reference_set() {
        let ttl_content = r#"
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix pkg: <https://purl.org/packagegraph/ontology/core#> .

pkg:Package a owl:Class .
pkg:Version a owl:Class .
pkg:packageName a owl:DatatypeProperty .
pkg:hasVersion a owl:ObjectProperty .
"#;
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("core.ttl");
        std::fs::write(&file_path, ttl_content).unwrap();

        let ref_set = parse_ontology_reference(dir.path()).unwrap();

        assert!(ref_set.classes.contains("https://purl.org/packagegraph/ontology/core#Package"));
        assert!(ref_set.classes.contains("https://purl.org/packagegraph/ontology/core#Version"));
        assert!(ref_set.predicates.contains("https://purl.org/packagegraph/ontology/core#packageName"));
        assert!(ref_set.predicates.contains("https://purl.org/packagegraph/ontology/core#hasVersion"));
    }

    #[test]
    fn test_coverage_audit() {
        let triples = vec![
            "<http://ex/p1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/ClassA> .".to_string(),
            "<http://ex/p1> <http://ex/pred1> \"value\" .".to_string(),
        ];

        let ref_set = OntologyReferenceSet {
            classes: vec!["http://ex/ClassA".to_string(), "http://ex/ClassB".to_string()].into_iter().collect(),
            predicates: vec!["http://ex/pred1".to_string(), "http://ex/pred2".to_string()].into_iter().collect(),
            shacl_targets: HashSet::new(),
        };

        let report = compute_coverage(&triples, &ref_set);

        assert_eq!(report.classes_covered, 1);
        assert_eq!(report.classes_total, 2);
        assert!(report.classes_missing.contains(&"http://ex/ClassB".to_string()));
        assert_eq!(report.predicates_covered, 1);
        assert_eq!(report.predicates_total, 2);
        assert!(report.predicates_missing.contains(&"http://ex/pred2".to_string()));
    }

    #[test]
    fn test_graph_uri_to_filename() {
        assert_eq!(
            graph_uri_to_filename("https://packagegraph.github.io/graph/fedora/43"),
            "fedora-43.nt"
        );
        assert_eq!(
            graph_uri_to_filename("https://packagegraph.github.io/graph/debian/trixie"),
            "debian-trixie.nt"
        );
        assert_eq!(
            graph_uri_to_filename("https://packagegraph.github.io/graph/enrichment/security"),
            "enrichment-security.nt"
        );
    }

    #[test]
    fn test_expand_prefixed() {
        let mut prefixes = HashMap::new();
        prefixes.insert("pkg".to_string(), "https://purl.org/packagegraph/ontology/core#".to_string());

        assert_eq!(
            expand_prefixed("pkg:Package", &prefixes),
            Some("https://purl.org/packagegraph/ontology/core#Package".to_string())
        );
        assert_eq!(
            expand_prefixed("pkg:Package;", &prefixes),
            Some("https://purl.org/packagegraph/ontology/core#Package".to_string())
        );
        assert_eq!(
            expand_prefixed("<http://full/uri>", &prefixes),
            Some("http://full/uri".to_string())
        );
        assert_eq!(expand_prefixed("unknown:Foo", &prefixes), None);
    }
}
