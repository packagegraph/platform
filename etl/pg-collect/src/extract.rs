use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
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
        let content = std::fs::read_to_string(path).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("Failed to read config {}: {}", path.display(), e),
            )
        })?;
        toml::from_str(&content).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("Failed to parse config {}: {}", path.display(), e),
            )
        })
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

    eprintln!(
        "Phase 1: Resolved {}/{} seed packages across {} graphs",
        resolved_count,
        seed_names.len(),
        graph_uris.len()
    );

    Ok(graph_uris)
}

/// Predicates to follow during BFS expansion.
///
/// Capability edges are in this list because `extract_triples` drops any
/// triple whose object URI was not visited. A predicate that BFS does not
/// follow therefore does not merely go unexpanded -- the edge itself is
/// filtered out of the extracted corpus. When the RPM collector stopped
/// emitting `directlyProvides` in favour of `providesCapability`, that made
/// provision relationships vanish from test corpora entirely, leaving the
/// provider with a `packageName` and nothing else.
///
/// `requiresCapability` is listed for the same reason ahead of the requires
/// half of the same change, so the gap cannot reopen on the other side.
const BFS_PREDICATES: &[&str] = &[
    "https://purl.org/packagegraph/ontology/core#directlyDependsOn",
    "https://purl.org/packagegraph/ontology/core#buildDependsOn",
    "https://purl.org/packagegraph/ontology/core#isDirectDependencyOf",
    "https://purl.org/packagegraph/ontology/core#hasVersion",
    "https://purl.org/packagegraph/ontology/core#versionOf",
    "https://purl.org/packagegraph/ontology/core#isVersionOf",
    "https://purl.org/packagegraph/ontology/core#provides",
    "https://purl.org/packagegraph/ontology/core#directlyProvides",
    "https://purl.org/packagegraph/ontology/core#providesCapability",
    "https://purl.org/packagegraph/ontology/core#requiresCapability",
    "https://purl.org/packagegraph/ontology/core#conflicts",
    "https://purl.org/packagegraph/ontology/core#partOfDistribution",
    "https://purl.org/packagegraph/ontology/core#partOfRelease",
    "https://purl.org/packagegraph/ontology/core#maintainedBy",
    "https://purl.org/packagegraph/ontology/core#hasUpstreamProject",
    "https://purl.org/packagegraph/ontology/core#builtFromSource",
    "https://purl.org/packagegraph/ontology/core#memberOfPackageSet",
];

/// How many triples one selected URI is worth, for the estimate that stops
/// expansion before `max_triples`.
///
/// Measured, not assumed: a full `test-corpus.toml` run against the live
/// endpoint on 2026-09-27 selected 64,358 URIs and extracted 10,608,101
/// triples, or 165 apiece. The previous figure of 50 dated from when the
/// object filter deleted 83.6% of every record, and it let that run finish
/// 3.5 times over its own 3,000,000 ceiling.
///
/// No single figure can bound the corpus. A one-seed, one-hop run over the
/// same endpoint came out at 5,328 triples per URI, because a small selection
/// is all hubs: `openssl` alone carries thousands of version nodes. This
/// constant is a better average, not a guarantee, which is why
/// `Manifest::exceeded_max_triples` records what actually happened.
const TRIPLES_PER_URI: usize = 165;

/// What a BFS expansion had to leave out.
///
/// The fan-out cap is a silent truncation: a subject with more neighbors than
/// `fan_out` on one predicate contributes a subset, and nothing in the
/// extracted files records that the rest exist. Counting the cuts is what lets
/// the manifest call a selection partial instead of implying it is the whole
/// neighborhood.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SelectionReport {
    /// Neighbors discovered but dropped because a (subject, predicate) pair
    /// held more than `fan_out` of them.
    pub fan_out_cuts: usize,
    /// How many (subject, predicate) pairs were truncated.
    pub capped_pairs: usize,
    /// Hops actually walked before the frontier emptied.
    pub hops_walked: usize,
}

impl SelectionReport {
    /// Fold another graph's expansion into a corpus-wide total.
    pub fn absorb(&mut self, other: &SelectionReport) {
        self.fan_out_cuts += other.fan_out_cuts;
        self.capped_pairs += other.capped_pairs;
        self.hops_walked = self.hops_walked.max(other.hops_walked);
    }
}

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
    Ok(bfs_expand_reporting(client, graph_uri, seeds, depth, fan_out)?.0)
}

/// BFS-expand a seed set, reporting what the fan-out cap discarded.
///
/// Neighbors are sorted before the cap applies, so which ones survive is a
/// function of the data rather than of the order the endpoint happened to
/// return rows in. Two runs over an unchanged graph select the same subset,
/// and the count of what was cut travels with the corpus.
pub fn bfs_expand_reporting(
    client: &SparqlClient,
    graph_uri: &str,
    seeds: &HashSet<String>,
    depth: usize,
    fan_out: usize,
) -> Result<(HashSet<String>, SelectionReport)> {
    let mut visited = seeds.clone();
    let mut frontier: Vec<String> = seeds.iter().cloned().collect();
    frontier.sort();
    let mut report = SelectionReport::default();

    let predicates_values: String = BFS_PREDICATES
        .iter()
        .map(|p| format!("<{}>", p))
        .collect::<Vec<_>>()
        .join(" ");

    for hop in 0..depth {
        if frontier.is_empty() {
            break;
        }

        let mut next_frontier = Vec::new();

        for batch in frontier.chunks(50) {
            let uri_values: String = batch
                .iter()
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
            let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
            for binding in &bindings {
                if let (Some(seed), Some(pred), Some(neighbor)) = (
                    binding.get("seed"),
                    binding.get("predicate"),
                    binding.get("neighbor"),
                ) {
                    groups
                        .entry((seed.clone(), pred.clone()))
                        .or_default()
                        .push(neighbor.clone());
                }
            }

            for (_pair, mut neighbors) in groups {
                neighbors.sort();
                neighbors.dedup();
                if neighbors.len() > fan_out {
                    report.fan_out_cuts += neighbors.len() - fan_out;
                    report.capped_pairs += 1;
                }
                for neighbor in neighbors.into_iter().take(fan_out) {
                    if visited.insert(neighbor.clone()) {
                        next_frontier.push(neighbor);
                    }
                }
            }
        }

        next_frontier.sort();
        report.hops_walked = hop + 1;
        eprintln!(
            "  Hop {}: +{} URIs (total {}){}",
            hop + 1,
            next_frontier.len(),
            visited.len(),
            if report.fan_out_cuts > 0 {
                format!(", {} neighbors cut by fan_out", report.fan_out_cuts)
            } else {
                String::new()
            }
        );
        frontier = next_frontier;
    }

    Ok((visited, report))
}

/// What an extracted graph points at but does not contain.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ExtractionReport {
    /// Triples written for this graph.
    pub triples: usize,
    /// Distinct data URIs referenced as objects that are not themselves in the
    /// selection: the edges that leave the corpus.
    pub dangling_targets: usize,
}

/// Extract the complete description of a set of URIs from a named graph.
///
/// Every triple whose subject is selected is retained, `rdf:type` included.
/// Objects outside the selection stay as references and are counted in the
/// returned report rather than deleted.
///
/// This replaces an object filter that searched each returned N-Triples line
/// for `"> <"`, took the tail as the object, and dropped the triple unless
/// that text named a selected URI. Two things went wrong with it. A class IRI
/// is never a selected instance, so it deleted every `rdf:type` assertion, and
/// a corpus without types validates vacuously: `sh:targetClass` selects no
/// focus node, so every shape conforms over nothing. It also turned a
/// predicate's absence from `BFS_PREDICATES` into deletion of the edge rather
/// than into leaving it unexpanded, which is how `providesCapability`
/// relationships vanished from test corpora once the RPM collector emitted
/// them.
///
/// Membership is not pushed into the query as a `VALUES` block. Measured on
/// the live endpoint with `test-corpus.toml`, the largest per-graph selection
/// is 9,662 URIs; at the ~90 bytes a rendered term costs, that is around
/// 870 KB against the SPARQL proxy's 1 MB request-body limit, and it would
/// have to be repeated in every subject batch.
pub fn extract_triples(
    client: &SparqlClient,
    graph_uri: &str,
    uris: &HashSet<String>,
) -> Result<Vec<String>> {
    Ok(extract_triples_reporting(client, graph_uri, uris)?.0)
}

/// Extract a graph's selected description and report what it references but
/// does not carry.
pub fn extract_triples_reporting(
    client: &SparqlClient,
    graph_uri: &str,
    uris: &HashSet<String>,
) -> Result<(Vec<String>, ExtractionReport)> {
    let mut all_triples: HashSet<String> = HashSet::new();
    let mut dangling: HashSet<String> = HashSet::new();
    let mut uri_list: Vec<&String> = uris.iter().collect();
    uri_list.sort();

    for batch in uri_list.chunks(50) {
        let values: String = batch
            .iter()
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

        for triple in client.query_construct(&sparql)? {
            all_triples.insert(triple);
        }

        // Ask the endpoint for the object terms instead of recovering them
        // from the serialised output. A binding's value is the IRI itself, so
        // there is nothing to parse and no typed literal whose datatype can be
        // mistaken for an object.
        let objects = format!(
            "SELECT DISTINCT ?o\n\
             WHERE {{\n\
               GRAPH <{}> {{\n\
                 VALUES ?s {{ {} }}\n\
                 ?s ?p ?o .\n\
               }}\n\
               FILTER(isIRI(?o) && STRSTARTS(STR(?o), \"{}\"))\n\
             }}",
            graph_uri,
            values,
            crate::uris::DATA
        );
        for binding in client.query(&objects)? {
            if let Some(object) = binding.get("o") {
                if !uris.contains(object) {
                    dangling.insert(object.clone());
                }
            }
        }
    }

    // Sorting makes the written file a function of the selection rather than
    // of hash iteration order, so two runs over an unchanged graph produce
    // byte-identical output.
    let mut triples: Vec<String> = all_triples.into_iter().collect();
    triples.sort();

    let report = ExtractionReport {
        triples: triples.len(),
        dangling_targets: dangling.len(),
    };
    Ok((triples, report))
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
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_str()
                .unwrap_or_default();
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

    eprintln!(
        "Ontology: {} files, {} classes, {} predicates, {} SHACL targets",
        files.len(),
        classes.len(),
        predicates.len(),
        shacl_targets.len()
    );

    Ok(OntologyReferenceSet {
        classes,
        predicates,
        shacl_targets,
    })
}

/// Expand a prefixed name (e.g., "pkg:Package") to its full URI.
fn expand_prefixed(name: &str, prefixes: &HashMap<String, String>) -> Option<String> {
    if name.starts_with('<') && name.ends_with('>') {
        // Already a full URI
        Some(name[1..name.len() - 1].to_string())
    } else if let Some(colon_pos) = name.find(':') {
        let prefix = &name[..colon_pos];
        let local = &name[colon_pos + 1..];
        // Strip trailing semicolons/commas/periods from local name
        let local = local.trim_end_matches(|c| c == ';' || c == ',' || c == '.');
        prefixes
            .get(prefix)
            .map(|base| format!("{}{}", base, local))
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

    let classes_missing: Vec<String> = ref_set
        .classes
        .difference(&found_classes)
        .cloned()
        .collect();
    let predicates_missing: Vec<String> = ref_set
        .predicates
        .difference(&found_predicates)
        .cloned()
        .collect();
    let shacl_missing: Vec<String> = ref_set
        .shacl_targets
        .difference(&found_classes)
        .cloned()
        .collect();

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
    /// Distinct data URIs this file references but does not describe.
    pub dangling_targets: usize,
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
    /// The ceiling the run was given.
    pub max_triples: usize,
    /// Whether the corpus came out over that ceiling. The expansion stop is
    /// driven by an estimate made before any triple is fetched, so it can be
    /// wrong; the manifest records the outcome rather than the guess.
    pub exceeded_max_triples: bool,
    /// What the fan-out cap discarded, summed over every graph.
    pub selection: SelectionReport,
    /// Graphs whose expansion was abandoned because the running size estimate
    /// passed `max_triples`. A corpus that stopped early is not the corpus the
    /// seeds describe, and the manifest has to say so.
    pub graphs_unexpanded: usize,
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
    auth: SparqlAuth,
    backend: SparqlBackend,
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

    let client = make_sparql_client(endpoint, &auth, backend);

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
    let mut selection = SelectionReport::default();
    let mut graphs_unexpanded = 0;

    for (graph, seeds) in &graph_seeds {
        eprintln!("Graph <{}>: {} seeds", graph, seeds.len());
        let (expanded, graph_selection) =
            bfs_expand_reporting(&client, graph, seeds, depth, fan_out)?;
        selection.absorb(&graph_selection);
        total_uri_count += expanded.len();
        graph_uris.insert(graph.clone(), expanded);

        // Size check
        let estimated = total_uri_count * TRIPLES_PER_URI;
        if estimated > max_triples {
            graphs_unexpanded = graph_seeds.len() - graph_uris.len();
            eprintln!(
                "  Size estimate ({}) exceeds max_triples ({}), stopping expansion \
                 with {} graphs unexpanded",
                estimated, max_triples, graphs_unexpanded
            );
            break;
        }
    }
    eprintln!(
        "Phase 2: {} total URIs across {} graphs ({} neighbors cut by fan_out \
         across {} capped pairs)",
        total_uri_count,
        graph_uris.len(),
        selection.fan_out_cuts,
        selection.capped_pairs
    );

    // Phase 3: Triple extraction
    eprintln!("\n--- Phase 3: Triple Extraction ---");
    std::fs::create_dir_all(output_dir.join("collector"))?;
    std::fs::create_dir_all(output_dir.join("enrichment"))?;

    let mut all_triples: Vec<String> = Vec::new();
    let mut manifest_entries: Vec<ManifestEntry> = Vec::new();

    for (graph, uris) in &graph_uris {
        let (triples, extraction) = extract_triples_reporting(&client, graph, uris)?;
        let count = triples.len();

        // Determine output file path from graph URI
        let file_name = graph_uri_to_filename(graph);
        let subdir = if graph.contains("/enrichment/") {
            "enrichment"
        } else {
            "collector"
        };
        let rel_path = format!("{}/{}", subdir, file_name);
        let full_path = output_dir.join(&rel_path);

        // Write triples to file
        let mut file = std::fs::File::create(&full_path)?;
        use std::io::Write;
        for triple in &triples {
            writeln!(file, "{}", triple)?;
        }

        eprintln!(
            "  {} — {} triples, {} references leaving the corpus",
            rel_path, count, extraction.dangling_targets
        );

        manifest_entries.push(ManifestEntry {
            path: rel_path,
            graph: graph.clone(),
            triples: count,
            dangling_targets: extraction.dangling_targets,
        });

        all_triples.extend(triples);
    }

    // Phase 4: Coverage audit
    eprintln!("\n--- Phase 4: Coverage Audit ---");
    let mut report = compute_coverage(&all_triples, &ref_set);

    // Gap fill for missing classes
    let gap_fill_count = gap_fill(
        &client,
        &ref_set,
        &report,
        &mut all_triples,
        output_dir,
        &mut manifest_entries,
    )?;
    if gap_fill_count > 0 {
        // Recompute coverage after gap fill
        report = compute_coverage(&all_triples, &ref_set);
    }

    let total_triples = all_triples.len();
    let exceeded_max_triples = total_triples > max_triples;
    if exceeded_max_triples {
        eprintln!(
            "WARNING: extracted {} triples against a max_triples of {}. The \
             expansion stop uses a per-URI estimate made before extraction; \
             lower depth or fan_out to bring the corpus under the ceiling.",
            total_triples, max_triples
        );
    }
    eprintln!(
        "Coverage: {}/{} classes, {}/{} predicates, {}/{} SHACL shapes",
        report.classes_covered,
        report.classes_total,
        report.predicates_covered,
        report.predicates_total,
        report.shacl_covered,
        report.shacl_total
    );
    if !report.classes_missing.is_empty() {
        eprintln!("  Missing classes: {:?}", report.classes_missing);
    }
    if !report.predicates_missing.is_empty() {
        eprintln!(
            "  Predicates with no instances: {:?}",
            report.predicates_missing
        );
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
        max_triples,
        exceeded_max_triples,
        selection,
        graphs_unexpanded,
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

    eprintln!(
        "  Gap fill: {} missing classes",
        report.classes_missing.len()
    );
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
                        .create(true)
                        .append(true)
                        .open(&gap_path)?;
                    use std::io::Write;
                    for triple in &triples {
                        writeln!(file, "{}", triple)?;
                    }

                    all_triples.extend(triples);
                    gap_triples += count;
                    eprintln!(
                        "    {} — found in <{}>, +{} triples",
                        missing_class, g, count
                    );
                }
            }
            _ => {
                eprintln!(
                    "    {} — not found in any graph (no instances exist)",
                    missing_class
                );
            }
        }
    }

    if gap_triples > 0 {
        manifest_entries.push(ManifestEntry {
            path: "collector/gap-fill.nt".to_string(),
            graph: "gap-fill".to_string(),
            triples: gap_triples,
            dangling_targets: 0,
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
        assert!(resolved["http://example.org/graph/fedora"]
            .contains("http://example.org/pkg/fedora/openssl"));
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

        let _mock = server
            .mock("POST", "/sparql")
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
    fn bfs_follows_capability_edges() {
        // Regression: extract_triples drops any triple whose object URI was not
        // visited, so a predicate missing from BFS_PREDICATES removes the edge
        // from the extracted corpus rather than merely leaving it unexpanded.
        // When the RPM collector moved from directlyProvides to
        // providesCapability, provision relationships disappeared from test
        // corpora. This asserts the generated query asks for them: the mock
        // only matches when the predicate is present, so a regression fails the
        // request rather than silently returning nothing.
        let mut server = mockito::Server::new();

        let _mock = server
            .mock("POST", "/sparql")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("core%23providesCapability".to_string()),
                mockito::Matcher::Regex("core%23requiresCapability".to_string()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(
                r#"{"results": {"bindings": [{
                     "seed": {"type": "uri", "value": "http://ex/pkg1"},
                     "predicate": {"type": "uri", "value": "https://purl.org/packagegraph/ontology/core#providesCapability"},
                     "neighbor": {"type": "uri", "value": "http://ex/capability/libssl.so.3"}
                   }]}}"#,
            )
            .expect_at_least(1)
            .create();

        let client = crate::sparql::SparqlClient::new(&server.url());
        let mut seeds = HashSet::new();
        seeds.insert("http://ex/pkg1".to_string());

        let expanded = bfs_expand(&client, "http://ex/graph/test", &seeds, 1, 10).unwrap();

        assert!(
            expanded.contains("http://ex/capability/libssl.so.3"),
            "BFS must reach the capability node, or extract_triples will filter \
             the providesCapability edge out of the corpus"
        );
    }

    /// A capability description as the endpoint would serialise it. The two
    /// type assertions are the triples the old filter deleted.
    const CAPABILITY_DESCRIPTION: &str = concat!(
        "<https://packagegraph.github.io/d/pkg/f/openssl> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#Package> .\n",
        "<https://packagegraph.github.io/d/pkg/f/openssl> <https://purl.org/packagegraph/ontology/core#packageName> \"openssl\" .\n",
        "<https://packagegraph.github.io/d/pkg/f/openssl> <https://purl.org/packagegraph/ontology/core#providesCapability> <https://packagegraph.github.io/d/capability/libssl.so.3> .\n",
        "<https://packagegraph.github.io/d/capability/libssl.so.3> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#Capability> .\n",
        "<https://packagegraph.github.io/d/capability/libssl.so.3> <https://purl.org/packagegraph/ontology/core#capabilityName> \"libssl.so.3\" .\n",
        "<https://packagegraph.github.io/d/capability/libssl.so.3> <http://www.w3.org/2000/01/rdf-schema#label> \"libssl.so.3\" .\n",
    );

    /// Serve a fixed CONSTRUCT body and a fixed object list for the companion
    /// SELECT, so a test can state what the endpoint holds and assert what
    /// extraction keeps.
    fn extraction_server(
        construct_body: &str,
        object_bindings: &str,
    ) -> (mockito::ServerGuard, Vec<mockito::Mock>) {
        let mut server = mockito::Server::new();
        let construct = server
            .mock("POST", "/sparql")
            .match_header("accept", "application/n-triples")
            .with_status(200)
            .with_header("content-type", "application/n-triples")
            .with_body(construct_body)
            .expect_at_least(1)
            .create();
        let select = server
            .mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(format!(
                r#"{{"results": {{"bindings": [{}]}}}}"#,
                object_bindings
            ))
            .create();
        (server, vec![construct, select])
    }

    #[test]
    fn extraction_keeps_the_type_assertions_that_make_shapes_apply() {
        // The regression that let a corpus certify itself: the old object
        // filter compared every URI object against the visited set, and a
        // class IRI is never a visited instance, so `rdf:type` was deleted
        // from every record. With no typed node left, `sh:targetClass` selects
        // no focus node and every shape conforms over nothing.
        let (server, _mocks) = extraction_server(CAPABILITY_DESCRIPTION, "");
        let client = crate::sparql::SparqlClient::new(&server.url());

        let mut uris = HashSet::new();
        uris.insert("https://packagegraph.github.io/d/pkg/f/openssl".to_string());
        uris.insert("https://packagegraph.github.io/d/capability/libssl.so.3".to_string());

        let kept = extract_triples(&client, "http://ex/graph/test", &uris).unwrap();

        for expected in CAPABILITY_DESCRIPTION.lines() {
            assert!(
                kept.iter().any(|t| t == expected),
                "extraction dropped {expected}"
            );
        }
    }

    #[test]
    fn an_edge_leaving_the_corpus_is_kept_and_counted() {
        // Deleting the edge was how `providesCapability` relationships
        // disappeared when the RPM collector started emitting them: a
        // predicate missing from BFS_PREDICATES removed the fact rather than
        // leaving it unexpanded. An edge out of the selection is now retained
        // and reported, so a partial corpus says it is partial.
        let (server, _mocks) = extraction_server(
            CAPABILITY_DESCRIPTION,
            r#"{"o": {"type": "uri", "value": "https://packagegraph.github.io/d/capability/libssl.so.3"}}"#,
        );
        let client = crate::sparql::SparqlClient::new(&server.url());

        let mut uris = HashSet::new();
        uris.insert("https://packagegraph.github.io/d/pkg/f/openssl".to_string());

        let (kept, report) =
            extract_triples_reporting(&client, "http://ex/graph/test", &uris).unwrap();

        assert!(
            kept.iter().any(|t| t.contains("providesCapability")),
            "an edge out of the selection must survive, not be silently deleted"
        );
        assert_eq!(
            report.dangling_targets, 1,
            "and the corpus must record that it points somewhere it does not describe"
        );
    }

    #[test]
    fn extraction_output_does_not_depend_on_hash_iteration_order() {
        let (server, _mocks) = extraction_server(CAPABILITY_DESCRIPTION, "");
        let client = crate::sparql::SparqlClient::new(&server.url());

        let mut uris = HashSet::new();
        for n in 0..64 {
            uris.insert(format!("https://packagegraph.github.io/d/pkg/f/p{n}"));
        }
        uris.insert("https://packagegraph.github.io/d/pkg/f/openssl".to_string());

        let first = extract_triples(&client, "http://ex/graph/test", &uris).unwrap();
        let second = extract_triples(&client, "http://ex/graph/test", &uris).unwrap();
        assert_eq!(first, second, "two runs over one selection must agree");
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(first, sorted, "output must be ordered, not hash-ordered");
    }

    #[test]
    fn the_fan_out_cap_reports_and_repeats_its_cut() {
        // A capped neighbourhood is a partial record. Which neighbours survive
        // has to be a function of the data, and how many were dropped has to
        // reach the manifest, or the corpus implies a completeness it does not
        // have.
        let bindings: Vec<String> = (0..10)
            .map(|n| {
                format!(
                    r#"{{"seed": {{"type": "uri", "value": "http://ex/pkg1"}},
                        "predicate": {{"type": "uri", "value": "https://purl.org/packagegraph/ontology/core#providesCapability"}},
                        "neighbor": {{"type": "uri", "value": "http://ex/cap{n}"}}}}"#
                )
            })
            .collect();
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("POST", "/sparql")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(format!(
                r#"{{"results": {{"bindings": [{}]}}}}"#,
                bindings.join(",")
            ))
            .expect_at_least(1)
            .create();

        let client = crate::sparql::SparqlClient::new(&server.url());
        let mut seeds = HashSet::new();
        seeds.insert("http://ex/pkg1".to_string());

        let (first, report) =
            bfs_expand_reporting(&client, "http://ex/graph/test", &seeds, 1, 3).unwrap();

        assert_eq!(report.fan_out_cuts, 7, "ten neighbours capped at three");
        assert_eq!(report.capped_pairs, 1);
        assert_eq!(report.hops_walked, 1);

        let (second, _) =
            bfs_expand_reporting(&client, "http://ex/graph/test", &seeds, 1, 3).unwrap();
        assert_eq!(
            first, second,
            "the cap must choose the same three every run"
        );
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
        let _objects = server
            .mock("POST", "/sparql")
            .match_header("accept", "application/sparql-results+json")
            .with_status(200)
            .with_header("content-type", "application/sparql-results+json")
            .with_body(r#"{"results": {"bindings": []}}"#)
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

        assert!(ref_set
            .classes
            .contains("https://purl.org/packagegraph/ontology/core#Package"));
        assert!(ref_set
            .classes
            .contains("https://purl.org/packagegraph/ontology/core#Version"));
        assert!(ref_set
            .predicates
            .contains("https://purl.org/packagegraph/ontology/core#packageName"));
        assert!(ref_set
            .predicates
            .contains("https://purl.org/packagegraph/ontology/core#hasVersion"));
    }

    #[test]
    fn test_coverage_audit() {
        let triples = vec![
            "<http://ex/p1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/ClassA> ."
                .to_string(),
            "<http://ex/p1> <http://ex/pred1> \"value\" .".to_string(),
        ];

        let ref_set = OntologyReferenceSet {
            classes: vec![
                "http://ex/ClassA".to_string(),
                "http://ex/ClassB".to_string(),
            ]
            .into_iter()
            .collect(),
            predicates: vec!["http://ex/pred1".to_string(), "http://ex/pred2".to_string()]
                .into_iter()
                .collect(),
            shacl_targets: HashSet::new(),
        };

        let report = compute_coverage(&triples, &ref_set);

        assert_eq!(report.classes_covered, 1);
        assert_eq!(report.classes_total, 2);
        assert!(report
            .classes_missing
            .contains(&"http://ex/ClassB".to_string()));
        assert_eq!(report.predicates_covered, 1);
        assert_eq!(report.predicates_total, 2);
        assert!(report
            .predicates_missing
            .contains(&"http://ex/pred2".to_string()));
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
        prefixes.insert(
            "pkg".to_string(),
            "https://purl.org/packagegraph/ontology/core#".to_string(),
        );

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
