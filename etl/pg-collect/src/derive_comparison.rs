//! Deriver: assess each rebuild source name's newest build against upstream
//! (presence/fidelity/drift) and emit reified pkg:RebuildAssessment nodes into
//! a derived graph. Implements rebuild-norm/v1-nostream -- see
//! docs/superpowers/specs/2026-09-04-rebuild-assessment-model-design.md.
use crate::ntriples::NTriplesWriter;
use crate::rebuild_classify::{assess, Assessment, Build};
use crate::rpmver::evr_cmp;
use crate::sparql::SparqlClient;
use crate::uris::{encode, DATA, PKG, RDFS_LABEL, RDF_TYPE, XSD};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Result;

pub struct Pair {
    pub rebuild_graph: String,
    pub rhel_graph: String,
}

/// This deriver's versioned method id. Never "rebuild-norm/v1": that name is
/// reserved for a producer that verifies module-stream membership on the
/// modular-equivalent tier, which this pipeline's collected data cannot do.
pub const ASSESSMENT_METHOD: &str = "rebuild-norm/v1-nostream";

#[derive(Debug)]
pub struct RebuildReport {
    pub pairs: usize,
    /// Total pkg:RebuildAssessment nodes emitted (one per assessed rebuild name).
    pub assessments: usize,
    pub presence_true: usize,
    pub presence_false: usize,
    /// Only for presence_true assessments. Sum == presence_true.
    pub fidelity_counts: BTreeMap<String, usize>,
    /// Only for presence_true assessments. Sum == presence_true.
    pub drift_counts: BTreeMap<String, usize>,
    /// Count of assessments with >=1 pkg:ambiguousCandidate. Subset of
    /// fidelity_counts["fidelity-unknown"] (the other subset is "no candidates
    /// at any tier", which is also fidelity-unknown but not ambiguous).
    pub ambiguous_count: usize,
    /// Distinct pkg:derivedFromDistribution triples emitted.
    pub distribution_count: usize,
    /// The exact set of pkg:assessmentOf TARGET URIs (the rebuild source nodes
    /// assessed this run). Staging validation requires the staging graph's
    /// assessmentOf targets to equal this set exactly.
    pub assessment_subjects: BTreeSet<String>,
    /// The exact set of pkg:DataSnapshot IRIs minted this run (one per distinct
    /// upstream graph). Staging validation requires every
    /// pkg:assessedAgainstSnapshot value to be a member of this set.
    pub snapshot_subjects: BTreeSet<String>,
    pub triples: usize,
}

pub fn parse_pair(s: &str) -> Result<Pair> {
    let (rb, rh) = s
        .split_once('=')
        .ok_or_else(|| err(format!("--pair must be rebuild=rhel, got: {s}")))?;
    for iri in [rb, rh] {
        if !(iri.starts_with("http://") || iri.starts_with("https://")) {
            return Err(err(format!("--pair IRIs must be absolute, got: {iri}")));
        }
        if iri.chars().any(|c| {
            matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`')
                || c.is_ascii_whitespace()
        }) {
            return Err(err(format!(
                "--pair IRIs must not contain <, >, \", {{, }}, |, \\, ^, backtick, or whitespace, got: {iri}"
            )));
        }
    }
    Ok(Pair {
        rebuild_graph: rb.to_string(),
        rhel_graph: rh.to_string(),
    })
}

fn err(m: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, m)
}

/// Collapse the one-row-per-(source, binary) shape from `query_source_builds`
/// into one `Build` per distinct source `node_uri`, taking the MAX epoch across
/// its rows. `version`/`release` are constant per source, so first-seen wins.
pub(crate) fn aggregate_source_rows(
    rows: Vec<(String, String, i64, String, String)>,
) -> Vec<(String, Build)> {
    let mut by_uri: HashMap<String, (String, Build)> = HashMap::new();
    for (name, uri, epoch, version, release) in rows {
        by_uri
            .entry(uri.clone())
            .and_modify(|(_, b)| {
                if epoch > b.epoch {
                    b.epoch = epoch;
                }
            })
            .or_insert((
                name,
                Build {
                    node_uri: uri,
                    epoch,
                    version,
                    release,
                },
            ));
    }
    by_uri.into_values().collect()
}

/// This run's deterministic `RebuildAssessment` IRI for a rebuild source node.
/// Run-scoped (includes `run_token`), NOT stable across runs: `RebuildAssessment`
/// is documented as a timestamped, point-in-time observation, and the whole
/// derived graph is atomically replaced every run, so a stable IRI would only
/// let a later run silently overwrite the meaning of an earlier one.
pub fn assessment_iri(node_uri: &str, run_token: &str) -> String {
    format!("{node_uri}/assessment/{}", encode(run_token))
}

/// This run's deterministic `DataSnapshot` IRI for an upstream graph. Keyed on
/// `(upstream_graph, run_token)`, NOT `upstream_graph` alone: `assessedAgainstSnapshot`
/// exists so results are reproducible against a specific state, and a graph-only
/// key would let a later run against changed upstream data silently reuse the
/// same IRI for a different state. Reused across every pair in THIS run that
/// cites the same upstream graph.
pub fn snapshot_iri(upstream_graph: &str, run_token: &str) -> String {
    format!(
        "{DATA}snapshot/{ASSESSMENT_METHOD}/{}/{}",
        encode(upstream_graph),
        encode(run_token)
    )
}

/// Per-pair inputs assembled by `derive` (all SPARQL I/O done by the caller) and
/// consumed by the pure `build_report`.
pub(crate) struct PairData {
    pub rebuild_dist: String,
    pub rhel_dist: String,
    /// The upstream graph IRI itself (e.g. ".../graph/rhel/9") -- needed to key
    /// the DataSnapshot, distinct from `rhel_dist` (the Distribution resource).
    pub rhel_graph: String,
    pub rebuild_builds: Vec<(String, Build)>,
    pub rhel_builds: Vec<(String, Build)>,
}

/// Emit one `RebuildAssessment` node's triples. Returns the triple count.
fn emit_assessment<W: std::io::Write>(
    w: &mut NTriplesWriter<W>,
    rb_node_uri: &str,
    snapshot: &str,
    assessed_at: &str,
    run_token: &str,
    a: &Assessment,
) -> Result<usize> {
    let assessment = assessment_iri(rb_node_uri, run_token);
    let mut n = 0;
    w.write_triple(&assessment, RDF_TYPE, &format!("{PKG}RebuildAssessment"))?;
    n += 1;
    w.write_triple(&assessment, &format!("{PKG}assessmentOf"), rb_node_uri)?;
    n += 1;
    w.write_datetime(&assessment, &format!("{PKG}assessedAt"), assessed_at)?;
    n += 1;
    w.write_literal(&assessment, &format!("{PKG}assessmentMethod"), ASSESSMENT_METHOD)?;
    n += 1;
    w.write_triple(&assessment, &format!("{PKG}assessedAgainstSnapshot"), snapshot)?;
    n += 1;
    w.write_boolean(&assessment, &format!("{PKG}hasUpstreamCounterpart"), a.has_upstream_counterpart)?;
    n += 1;
    w.write_boolean(&assessment, &format!("{PKG}lineageConfirmed"), false)?;
    n += 1;

    if let Some(fidelity) = &a.fidelity {
        w.write_triple(&assessment, &format!("{PKG}rebuildFidelity"), &format!("{PKG}{}", fidelity.concept))?;
        n += 1;
        w.write_typed_literal(&assessment, &format!("{PKG}assessmentConfidence"), fidelity.confidence, &format!("{XSD}decimal"))?;
        n += 1;
        if let Some(baseline) = &fidelity.baseline {
            w.write_triple(&assessment, &format!("{PKG}fidelityBaseline"), baseline)?;
            n += 1;
        }
        for cand in &fidelity.ambiguous_candidates {
            w.write_triple(&assessment, &format!("{PKG}ambiguousCandidate"), cand)?;
            n += 1;
        }
    }
    if let Some(drift) = &a.drift {
        w.write_triple(&assessment, &format!("{PKG}rebuildDrift"), &format!("{PKG}{}", drift.concept))?;
        n += 1;
        w.write_triple(&assessment, &format!("{PKG}comparedAgainst"), &drift.compared_against)?;
        n += 1;
    }
    Ok(n)
}

/// Pure orchestration core: given per-pair data (no SPARQL I/O) plus this run's
/// `run_token` and `assessed_at` timestamp, render the derived N-Triples and
/// tally the report. Deterministic; does distribution dedup, snapshot minting,
/// group-by-name, newest-per-name selection, assessment, and emission.
pub(crate) fn build_report(
    pairs_data: &[PairData],
    run_token: &str,
    assessed_at: &str,
) -> Result<(String, RebuildReport)> {
    let mut report = RebuildReport {
        pairs: pairs_data.len(),
        assessments: 0,
        presence_true: 0,
        presence_false: 0,
        fidelity_counts: BTreeMap::new(),
        drift_counts: BTreeMap::new(),
        ambiguous_count: 0,
        distribution_count: 0,
        assessment_subjects: BTreeSet::new(),
        snapshot_subjects: BTreeSet::new(),
        triples: 0,
    };
    let mut w = NTriplesWriter::new(Vec::<u8>::new());
    let mut emitted_dist: HashSet<(String, String)> = HashSet::new();

    for pd in pairs_data {
        // distribution-level lineage (deduped across release pairs) -- unchanged
        if emitted_dist.insert((pd.rebuild_dist.clone(), pd.rhel_dist.clone())) {
            w.write_triple(&pd.rebuild_dist, &format!("{PKG}derivedFromDistribution"), &pd.rhel_dist)?;
            report.triples += 1;
            report.distribution_count += 1;
        }

        // DataSnapshot for this pair's upstream graph: deterministic IRI, so
        // repeated references across pairs sharing an upstream graph collapse
        // via write_*_once regardless of call order.
        let snapshot = snapshot_iri(&pd.rhel_graph, run_token);
        report.snapshot_subjects.insert(snapshot.clone());
        if w.write_triple_once(&snapshot, RDF_TYPE, &format!("{PKG}DataSnapshot"))? {
            report.triples += 1;
        }
        let label = format!(
            "Upstream snapshot for {ASSESSMENT_METHOD}, graph {}, run {run_token}",
            pd.rhel_graph
        );
        if w.write_literal_once(&snapshot, RDFS_LABEL, &label)? {
            report.triples += 1;
        }
        // Pin the snapshot per ontology design-decisions.md (normative guidance,
        // not a SHACL requirement -- DataSnapshotShape only requires rdfs:label):
        // a mutable graph-name-only reference isn't enough to prove a later
        // query saw the same data, so record the timestamp this run observed
        // it at and which upstream graph it was read from. Deduped the same
        // way as the type/label triples above (pure function of the snapshot
        // IRI, so repeated pairs citing the same upstream graph collapse).
        if w.write_datetime_once(&snapshot, &format!("{PKG}snapshotTimestamp"), assessed_at)? {
            report.triples += 1;
        }
        if w.write_literal_once(&snapshot, &format!("{PKG}snapshotSource"), &pd.rhel_graph)? {
            report.triples += 1;
        }

        let mut up: HashMap<String, Vec<Build>> = HashMap::new();
        for (name, build) in &pd.rhel_builds {
            up.entry(name.clone()).or_default().push(build.clone());
        }
        let mut rb: HashMap<String, Vec<Build>> = HashMap::new();
        for (name, build) in &pd.rebuild_builds {
            rb.entry(name.clone()).or_default().push(build.clone());
        }

        for (name, mut builds) in rb {
            builds.sort_by(|a, b| {
                evr_cmp(b.epoch, &b.version, &b.release, a.epoch, &a.version, &a.release)
                    .then_with(|| a.node_uri.cmp(&b.node_uri))
            });
            let newest = &builds[0];
            let upstream = up.get(&name).map(|v| v.as_slice()).unwrap_or(&[]);
            let a = assess(newest, upstream);

            let n = emit_assessment(&mut w, &newest.node_uri, &snapshot, assessed_at, run_token, &a)?;
            report.triples += n;
            report.assessments += 1;
            report.assessment_subjects.insert(newest.node_uri.clone());

            if a.has_upstream_counterpart {
                report.presence_true += 1;
                if let Some(f) = &a.fidelity {
                    *report.fidelity_counts.entry(f.concept.to_string()).or_insert(0) += 1;
                    if !f.ambiguous_candidates.is_empty() {
                        report.ambiguous_count += 1;
                    }
                }
                if let Some(d) = &a.drift {
                    *report.drift_counts.entry(d.concept.to_string()).or_insert(0) += 1;
                }
            } else {
                report.presence_false += 1;
            }
        }
    }
    let ntriples = w.into_string()?;
    Ok((ntriples, report))
}

/// Rebuild-vocabulary terms that must be declared in the loaded TBox before
/// the deriver may run. Deliberately references ONLY the reified model's terms
/// -- not the old flat rebuildTrackingStatus/RebuildTrackingScheme, which no
/// longer exist in the ontology, so gating on them would always fail closed
/// (safe, but with a useless error message).
pub(crate) const REBUILD_TERMS: [&str; 11] = [
    "RebuildAssessment",
    "assessmentOf",
    "hasUpstreamCounterpart",
    "rebuildFidelity",
    "rebuildDrift",
    "fidelityBaseline",
    "comparedAgainst",
    "assessedAgainstSnapshot",
    "lineageConfirmed",
    "RebuildFidelityScheme",
    "RebuildDriftScheme",
];

/// Per-term existence probe. `SparqlClient::query` is built for SELECT bindings,
/// so we use `SELECT ?p WHERE { <term> a ?p } LIMIT 1` (NOT ASK) and require
/// each term to return >= 1 row (works uniformly for properties and classes:
/// a class has its own rdf:type, e.g. owl:Class).
pub(crate) fn term_type_query(term: &str) -> String {
    format!("SELECT ?p WHERE {{ <{PKG}{term}> a ?p }} LIMIT 1")
}

/// Compose the single-request, single-transaction swap that atomically replaces
/// `prod` with the contents of `staging`. COPY = DROP target then INSERT all
/// source triples; the trailing DROP removes staging.
pub(crate) fn atomic_swap_update(prod: &str, staging: &str) -> String {
    format!("DROP SILENT GRAPH <{prod}> ;\nCOPY <{staging}> TO <{prod}> ;\nDROP SILENT GRAPH <{staging}>")
}

/// A run token is interpolated into a `GRAPH <...>` staging IRI, so it must be
/// tightly constrained: `^[A-Za-z0-9._-]+$` (non-empty, no whitespace/injection).
pub(crate) fn valid_run_token(t: &str) -> bool {
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The four `pkg:fidelity-*` concepts of RebuildFidelityScheme. Every emitted
/// `pkg:rebuildFidelity` object MUST be one of these.
const FIDELITY_CONCEPTS: [&str; 4] = [
    "fidelity-exact",
    "fidelity-vendor-patched",
    "fidelity-modular-equivalent",
    "fidelity-unknown",
];

/// The four `pkg:drift-*` concepts of RebuildDriftScheme. Every emitted
/// `pkg:rebuildDrift` object MUST be one of these.
const DRIFT_CONCEPTS: [&str; 4] = [
    "drift-even",
    "drift-ahead",
    "drift-behind",
    "drift-version-equivalent",
];

/// Matched (non-unknown) fidelity tiers: these MUST carry a fidelityBaseline.
const MATCHED_FIDELITY_CONCEPTS: [&str; 3] = [
    "fidelity-exact",
    "fidelity-vendor-patched",
    "fidelity-modular-equivalent",
];

fn concept_list(concepts: &[&str]) -> String {
    concepts.iter().map(|c| format!("pkg:{c}")).collect::<Vec<_>>().join(", ")
}

/// A single fail-closed staging check: `query` is a `SELECT (COUNT(...) AS ?c)`
/// against the staging graph, and it must always evaluate to zero. (The two
/// coverage counts that need a caller-supplied expectation instead of a fixed
/// zero -- assessment count and distribution count -- are compared directly
/// in `validate_staging`, not through this mechanism.)
pub(crate) struct StagingCheck {
    pub description: String,
    pub query: String,
}

/// One assessment must carry exactly one `pkg:{predicate}` (required fields:
/// assessmentOf, assessedAt, assessmentMethod, assessedAgainstSnapshot,
/// hasUpstreamCounterpart, lineageConfirmed). Catches both missing (n=0) and
/// duplicate (n>1) in one query via GROUP BY ... HAVING.
pub(crate) fn exactly_one_check(staging: &str, predicate: &str) -> (String, String) {
    (
        format!("assessment(s) not carrying exactly one pkg:{predicate}"),
        format!(
            "PREFIX pkg: <{PKG}>\nSELECT (COUNT(?a) AS ?c) WHERE {{ SELECT ?a (COUNT(?v) AS ?n) WHERE {{ GRAPH <{staging}> {{ ?a a pkg:RebuildAssessment . OPTIONAL {{ ?a pkg:{predicate} ?v }} }} }} GROUP BY ?a HAVING(?n != 1) }}"
        ),
    )
}

/// An assessment may carry AT MOST one `pkg:{predicate}` (optional fields:
/// rebuildFidelity, rebuildDrift, fidelityBaseline, comparedAgainst,
/// assessmentConfidence, lineageEvidence).
pub(crate) fn at_most_one_check(staging: &str, predicate: &str) -> (String, String) {
    (
        format!("assessment(s) carrying more than one pkg:{predicate}"),
        format!(
            "PREFIX pkg: <{PKG}>\nSELECT (COUNT(?a) AS ?c) WHERE {{ SELECT ?a (COUNT(?v) AS ?n) WHERE {{ GRAPH <{staging}> {{ ?a a pkg:RebuildAssessment . OPTIONAL {{ ?a pkg:{predicate} ?v }} }} }} GROUP BY ?a HAVING(?n > 1) }}"
        ),
    )
}

/// Every present `pkg:{predicate}` value must be typed `expected_datatype`.
pub(crate) fn datatype_check(staging: &str, predicate: &str, expected_datatype: &str) -> (String, String) {
    (
        format!("pkg:{predicate} value(s) not typed as the required datatype"),
        format!(
            "PREFIX pkg: <{PKG}>\nSELECT (COUNT(?v) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:{predicate} ?v }} FILTER(!isLiteral(?v) || datatype(?v) != <{expected_datatype}>) }}"
        ),
    )
}

/// Build the full fail-closed staging check list. `rhel_graphs` is the distinct
/// set of upstream graphs configured for this run (used by the "unexpected
/// baseline/candidate object" check to verify every referenced baseline is a
/// real SourcePackage that actually exists in one of them).
pub(crate) fn staging_checks(staging: &str, rhel_graphs: &[String]) -> Vec<StagingCheck> {
    // xsd: is declared even though most checks reference full datatype IRIs
    // directly (`<{XSD}dateTime>`) -- it's needed for the confidence-range
    // check's numeric-literal FILTER and is harmless (an unused PREFIX) elsewhere.
    let prefix = format!("PREFIX pkg: <{PKG}>\nPREFIX xsd: <{XSD}>\n");
    let mut checks = Vec::new();
    let push = |checks: &mut Vec<StagingCheck>, description: &str, query: String| {
        checks.push(StagingCheck { description: description.to_string(), query });
    };

    // Required-field cardinality (exactly 1 each).
    for predicate in [
        "assessmentOf", "assessedAt", "assessmentMethod",
        "assessedAgainstSnapshot", "hasUpstreamCounterpart", "lineageConfirmed",
    ] {
        let (description, query) = exactly_one_check(staging, predicate);
        checks.push(StagingCheck { description, query });
    }
    // Optional-field max-cardinality (<=1 each).
    for predicate in [
        "rebuildFidelity", "rebuildDrift", "fidelityBaseline",
        "comparedAgainst", "assessmentConfidence", "lineageEvidence",
    ] {
        let (description, query) = at_most_one_check(staging, predicate);
        checks.push(StagingCheck { description, query });
    }
    // Datatype correctness.
    for (predicate, dt) in [
        ("assessedAt", format!("{XSD}dateTime")),
        ("assessmentMethod", format!("{XSD}string")),
        ("lineageEvidence", format!("{XSD}string")),
        ("hasUpstreamCounterpart", format!("{XSD}boolean")),
        ("lineageConfirmed", format!("{XSD}boolean")),
        ("assessmentConfidence", format!("{XSD}decimal")),
    ] {
        let (description, query) = datatype_check(staging, predicate, &dt);
        checks.push(StagingCheck { description, query });
    }

    // Direct numeric FILTER comparison on an xsd:decimal-typed literal applies
    // SPARQL's built-in numeric type promotion -- no explicit xsd:decimal(?v)
    // cast function is needed (or correct to add: casting an already-decimal
    // literal is redundant, and the datatype_check above already rejects any
    // non-decimal value before this comparison would even run against real data).
    push(&mut checks, "confidence value(s) outside [0.0, 1.0]",
        format!("{prefix}SELECT (COUNT(?v) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessmentConfidence ?v }} FILTER(?v < 0.0 || ?v > 1.0) }}"));
    push(&mut checks, "rebuildFidelity concept outside RebuildFidelityScheme",
        format!("{prefix}SELECT (COUNT(?v) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildFidelity ?v }} FILTER(?v NOT IN ({})) }}", concept_list(&FIDELITY_CONCEPTS)));
    push(&mut checks, "rebuildDrift concept outside RebuildDriftScheme",
        format!("{prefix}SELECT (COUNT(?v) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildDrift ?v }} FILTER(?v NOT IN ({})) }}", concept_list(&DRIFT_CONCEPTS)));
    push(&mut checks, "hasUpstreamCounterpart=false assessment(s) also carrying fidelity/drift/baselines/ambiguousCandidate",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:hasUpstreamCounterpart false . {{ ?a pkg:rebuildFidelity ?x }} UNION {{ ?a pkg:rebuildDrift ?x }} UNION {{ ?a pkg:fidelityBaseline ?x }} UNION {{ ?a pkg:comparedAgainst ?x }} UNION {{ ?a pkg:ambiguousCandidate ?x }} }} }}"));
    push(&mut checks, "hasUpstreamCounterpart=true assessment(s) missing rebuildFidelity or rebuildDrift",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:hasUpstreamCounterpart true . FILTER(NOT EXISTS {{ ?a pkg:rebuildFidelity ?f }} || NOT EXISTS {{ ?a pkg:rebuildDrift ?d }}) }} }}"));
    push(&mut checks, "rebuildDrift present without a comparedAgainst baseline",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildDrift ?d . FILTER NOT EXISTS {{ ?a pkg:comparedAgainst ?c }} }} }}"));
    push(&mut checks, "matched fidelity (exact/vendor-patched/modular-equivalent) without a fidelityBaseline",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildFidelity ?f FILTER(?f IN ({})) . FILTER NOT EXISTS {{ ?a pkg:fidelityBaseline ?b }} }} }}", concept_list(&MATCHED_FIDELITY_CONCEPTS)));
    push(&mut checks, "fidelity-unknown assessment(s) carrying a fidelityBaseline",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:rebuildFidelity pkg:fidelity-unknown ; pkg:fidelityBaseline ?b }} }}"));
    push(&mut checks, "ambiguousCandidate present without rebuildFidelity = fidelity-unknown",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:ambiguousCandidate ?c . FILTER NOT EXISTS {{ ?a pkg:rebuildFidelity pkg:fidelity-unknown }} }} }}"));
    push(&mut checks, "ambiguousCandidate present with fewer than two distinct candidates",
        format!("{prefix}SELECT (COUNT(?a) AS ?c) WHERE {{ SELECT ?a (COUNT(DISTINCT ?c) AS ?n) WHERE {{ GRAPH <{staging}> {{ ?a pkg:ambiguousCandidate ?c }} }} GROUP BY ?a HAVING(?n < 2) }}"));
    push(&mut checks, "self-baseline: assessmentOf target equals its own fidelityBaseline or comparedAgainst",
        format!("{prefix}SELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessmentOf ?p . {{ ?a pkg:fidelityBaseline ?p }} UNION {{ ?a pkg:comparedAgainst ?p }} }} }}"));

    // Unexpected baseline/candidate objects: every fidelityBaseline/
    // comparedAgainst/ambiguousCandidate must be a real SourcePackage that
    // exists in one of this run's configured upstream graphs.
    let graph_membership = rhel_graphs
        .iter()
        .map(|g| format!("{{ GRAPH <{g}> {{ ?b a pkg:SourcePackage }} }}"))
        .collect::<Vec<_>>()
        .join(" UNION ");
    push(&mut checks, "unexpected baseline/candidate object not found as a SourcePackage in a configured upstream graph",
        format!("{prefix}SELECT (COUNT(DISTINCT ?b) AS ?c) WHERE {{ GRAPH <{staging}> {{ {{ ?a pkg:fidelityBaseline ?b }} UNION {{ ?a pkg:comparedAgainst ?b }} UNION {{ ?a pkg:ambiguousCandidate ?b }} }} FILTER NOT EXISTS {{ {graph_membership} }} }}"));

    push(&mut checks, "assessedAgainstSnapshot value missing rdf:type pkg:DataSnapshot",
        format!("{prefix}SELECT (COUNT(DISTINCT ?s) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessedAgainstSnapshot ?s }} FILTER NOT EXISTS {{ GRAPH <{staging}> {{ ?s a pkg:DataSnapshot }} }} }}"));
    push(&mut checks, "assessedAgainstSnapshot value missing a non-empty rdfs:label",
        format!("PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>\n{prefix}SELECT (COUNT(DISTINCT ?s) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessedAgainstSnapshot ?s }} FILTER NOT EXISTS {{ GRAPH <{staging}> {{ ?s rdfs:label ?l }} FILTER(STRLEN(REPLACE(STR(?l), \"^\\\\s+|\\\\s+$\", \"\")) > 0) }} }}"));

    push(&mut checks, "pkg:rebuildOf triple present (this deriver never promotes lineage)",
        format!("{prefix}SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?s pkg:rebuildOf ?o }} }}"));
    push(&mut checks, "lineageConfirmed=true assessment(s) (this deriver never promotes lineage)",
        format!("{prefix}SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:lineageConfirmed true }} }}"));
    push(&mut checks, "lineageEvidence triple present (this deriver never promotes lineage)",
        format!("{prefix}SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:lineageEvidence ?e }} }}"));

    checks
}

/// The exact set of pkg:RebuildAssessment count and set-equality queries that
/// need a caller-supplied expectation rather than a fixed zero.
pub(crate) fn assessment_count_query(staging: &str) -> String {
    format!("PREFIX pkg: <{PKG}>\nSELECT (COUNT(DISTINCT ?a) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a a pkg:RebuildAssessment }} }}")
}

pub(crate) fn distribution_count_query(staging: &str) -> String {
    format!("PREFIX pkg: <{PKG}>\nSELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{staging}> {{ ?a pkg:derivedFromDistribution ?b }} }}")
}

pub(crate) fn assessment_subjects_query(staging: &str) -> String {
    format!("PREFIX pkg: <{PKG}>\nSELECT DISTINCT ?s WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessmentOf ?s }} }}")
}

pub(crate) fn snapshot_subjects_query(staging: &str) -> String {
    format!("PREFIX pkg: <{PKG}>\nSELECT DISTINCT ?s WHERE {{ GRAPH <{staging}> {{ ?a pkg:assessedAgainstSnapshot ?s }} }}")
}

/// Everything `load_atomic` needs to know about what THIS run's `derive()`
/// produced, so staging can be checked against it before the swap. Bundled
/// into a struct because the list of expectations has grown past what reads
/// cleanly as positional arguments.
pub struct LoadExpectations<'a> {
    pub assessments: usize,
    pub distribution_count: usize,
    pub subjects: &'a BTreeSet<String>,
    pub snapshots: &'a BTreeSet<String>,
    pub rhel_graphs: &'a [String],
}

pub struct RebuildComparisonDeriver {
    sparql: SparqlClient,
}

impl RebuildComparisonDeriver {
    pub fn new(endpoint: &str) -> Self {
        Self {
            sparql: SparqlClient::new(endpoint),
        }
    }

    /// Run a `SELECT (COUNT(...) AS ?c)` query and return the count.
    fn staging_count(&self, query: &str) -> Result<usize> {
        let rows = self.sparql.query(query)?;
        rows.first()
            .and_then(|r| r.get("c"))
            .and_then(|v| v.parse::<usize>().ok())
            .ok_or_else(|| err("staging validation query returned no numeric ?c binding".into()))
    }

    fn staging_subjects(&self, query: &str) -> Result<BTreeSet<String>> {
        let rows = self.sparql.query(query)?;
        Ok(rows.iter().filter_map(|r| r.get("s").cloned()).collect())
    }

    /// Fail-closed structural validation of the STAGING graph, run BEFORE the
    /// swap. Not a substitute for running the ontology's actual SHACL shapes (out
    /// of scope for this deriver) -- a targeted, enumerated set of checks against
    /// the highest-risk classes of staging corruption: truncation, malformed
    /// literals, axis-coupling violations, contamination/substitution, and stray
    /// snapshot/baseline references.
    fn validate_staging(&self, staging: &str, expect: &LoadExpectations) -> Result<()> {
        let assessments = self.staging_count(&assessment_count_query(staging))?;
        if assessments != expect.assessments {
            return Err(err(format!(
                "staging graph has {assessments} pkg:RebuildAssessment nodes but derive emitted {} (truncation/partial-load guard); aborting, prod untouched",
                expect.assessments
            )));
        }
        let distribution = self.staging_count(&distribution_count_query(staging))?;
        if distribution != expect.distribution_count {
            return Err(err(format!(
                "staging graph has {distribution} pkg:derivedFromDistribution triples but derive emitted {} (distribution-coverage guard); aborting, prod untouched",
                expect.distribution_count
            )));
        }

        for check in staging_checks(staging, expect.rhel_graphs) {
            let n = self.staging_count(&check.query)?;
            if n != 0 {
                return Err(err(format!(
                    "staging graph failed check ({n}): {}; aborting, prod untouched",
                    check.description
                )));
            }
        }

        let actual_subjects = self.staging_subjects(&assessment_subjects_query(staging))?;
        if actual_subjects != *expect.subjects {
            let unexpected: Vec<&String> = actual_subjects.difference(expect.subjects).take(5).collect();
            let missing: Vec<&String> = expect.subjects.difference(&actual_subjects).take(5).collect();
            return Err(err(format!(
                "staging assessmentOf subjects do not match the set derive emitted ({} in staging vs {} expected); unexpected: {:?}; missing: {:?}; aborting, prod untouched",
                actual_subjects.len(), expect.subjects.len(), unexpected, missing
            )));
        }

        let actual_snapshots = self.staging_subjects(&snapshot_subjects_query(staging))?;
        if actual_snapshots != *expect.snapshots {
            let unexpected: Vec<&String> = actual_snapshots.difference(expect.snapshots).take(5).collect();
            let missing: Vec<&String> = expect.snapshots.difference(&actual_snapshots).take(5).collect();
            return Err(err(format!(
                "staging assessedAgainstSnapshot values do not match the set derive emitted ({} in staging vs {} expected); unexpected: {:?}; missing: {:?}; aborting, prod untouched",
                actual_snapshots.len(), expect.snapshots.len(), unexpected, missing
            )));
        }

        Ok(())
    }

    /// Load `nt_path` into a per-run staging graph, run fail-closed structural
    /// validation against the STAGING graph, then atomically swap it into
    /// `prod_graph`. Prod is left untouched unless staging passed every check and
    /// the swap succeeded; staging is dropped on every failure path (no orphans).
    pub fn load_atomic(
        &self,
        nt_path: &str,
        prod_graph: &str,
        run_token: &str,
        expect: LoadExpectations,
    ) -> Result<()> {
        if !valid_run_token(run_token) {
            return Err(err(format!(
                "run token {run_token:?} is invalid; must match ^[A-Za-z0-9._-]+$"
            )));
        }
        let staging = format!("{prod_graph}-staging-{run_token}");
        self.sparql.drop_graph(&staging)?;
        if let Err(e) = self.sparql.load_file(nt_path, &staging, 10_000) {
            self.sparql.drop_graph(&staging).ok();
            return Err(e);
        }
        if let Err(e) = self.validate_staging(&staging, &expect) {
            self.sparql.drop_graph(&staging).ok();
            return Err(e);
        }
        // The swap itself must NOT go through the retrying `update()`: DROP+COPY+DROP
        // is not idempotent, and retrying after a successful-but-unconfirmed attempt
        // would re-drop already-correct prod data (see `update_no_retry`'s doc).
        match self.sparql.update_no_retry(&atomic_swap_update(prod_graph, &staging)) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.sparql.drop_graph(&staging).ok();
                Err(e)
            }
        }
    }

    /// Fail closed unless every rebuild-vocabulary term is declared in the store.
    /// Ruling: use a per-term SELECT (not ASK), require each non-empty.
    pub fn check_ontology_terms(&self) -> Result<()> {
        for term in REBUILD_TERMS {
            let rows = self.sparql.query(&term_type_query(term))?;
            if rows.is_empty() {
                return Err(err(format!(
                    "rebuild-assessment vocabulary term pkg:{term} is not declared. Sync ontology PR #5 (v0.13.0-reified) and load the TBox before running this deriver."
                )));
            }
        }
        Ok(())
    }

    /// Refuse to derive from empty/truncated graphs: every distinct graph must
    /// carry at least `min_sources` source builds and resolve exactly one
    /// distribution IRI.
    pub fn check_readiness(&self, pairs: &[Pair], min_sources: usize) -> Result<()> {
        let mut graphs: Vec<&String> = Vec::new();
        for p in pairs {
            graphs.push(&p.rebuild_graph);
            graphs.push(&p.rhel_graph);
        }
        graphs.sort();
        graphs.dedup();
        for g in graphs {
            // query_source_builds yields one row per (source, binary); collapse to
            // distinct sources so the floor means distinct source packages.
            let n = aggregate_source_rows(self.sparql.query_source_builds(g)?).len();
            if n < min_sources {
                return Err(err(format!(
                    "graph {g} has {n} source builds (< min {min_sources}); refusing to derive from an empty/truncated graph"
                )));
            }
            self.sparql.resolve_distribution(g)?; // errors on 0 or >1
        }
        Ok(())
    }

    pub fn derive(
        &self,
        output_path: &str,
        pairs: &[Pair],
        run_token: &str,
        assessed_at: &str,
    ) -> Result<RebuildReport> {
        // A rebuild graph must map to exactly one upstream: reject duplicates so a
        // single rebuild graph cannot be compared against two different RHEL graphs.
        let mut seen: HashSet<&str> = HashSet::new();
        for p in pairs {
            if !seen.insert(p.rebuild_graph.as_str()) {
                return Err(err(format!(
                    "rebuild graph {} appears in more than one pair; a rebuild graph must map to exactly one upstream",
                    p.rebuild_graph
                )));
            }
        }
        // A rebuild graph must never be paired with itself: it would make a
        // package its own fidelity/drift baseline, which RebuildAssessmentSelfBaselineShape
        // forbids -- reject it structurally before any I/O, not just via the
        // staging self-baseline check (belt and suspenders).
        for p in pairs {
            if p.rebuild_graph == p.rhel_graph {
                return Err(err(format!(
                    "pair rebuild_graph and rhel_graph are the same graph ({}); a rebuild graph cannot be its own upstream",
                    p.rebuild_graph
                )));
            }
        }

        let mut pairs_data: Vec<PairData> = Vec::with_capacity(pairs.len());
        for pair in pairs {
            let rebuild_dist = self.sparql.resolve_distribution(&pair.rebuild_graph)?;
            let rhel_dist = self.sparql.resolve_distribution(&pair.rhel_graph)?;
            let rebuild_builds = aggregate_source_rows(self.sparql.query_source_builds(&pair.rebuild_graph)?);
            let rhel_builds = aggregate_source_rows(self.sparql.query_source_builds(&pair.rhel_graph)?);
            pairs_data.push(PairData {
                rebuild_dist,
                rhel_dist,
                rhel_graph: pair.rhel_graph.clone(),
                rebuild_builds,
                rhel_builds,
            });
        }

        let (ntriples, report) = build_report(&pairs_data, run_token, assessed_at)?;
        std::fs::write(output_path, ntriples)?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pair_requires_absolute_iris() {
        let p = parse_pair("https://x/graph/almalinux/9=https://x/graph/rhel/9").unwrap();
        assert!(p.rebuild_graph.ends_with("almalinux/9"));
        assert!(parse_pair("almalinux/9=rhel/9").is_err()); // not absolute IRIs
        assert!(parse_pair("https://x/a").is_err());        // missing '='
    }

    #[test]
    fn parse_pair_rejects_iri_injection_chars() {
        // A '>' in either operand would prematurely close a `GRAPH <...>` clause.
        assert!(parse_pair("https://x/graph/a>evil=https://x/graph/rhel/9").is_err());
        assert!(parse_pair("https://x/graph/a=https://x/graph/rhel>9").is_err());
        // ASCII whitespace (space) is likewise rejected.
        assert!(parse_pair("https://x/graph/a b=https://x/graph/rhel/9").is_err());
        // Sanity: a clean pair with none of these still parses.
        assert!(parse_pair("https://x/graph/almalinux/9=https://x/graph/rhel/9").is_ok());
    }

    #[test]
    fn atomic_swap_update_replaces_prod_from_staging() {
        let u = atomic_swap_update(
            "https://packagegraph.github.io/graph/derived/rhel-rebuilds",
            "https://packagegraph.github.io/graph/derived/rhel-rebuilds-staging-RUN1");
        assert!(u.contains("DROP SILENT GRAPH <https://packagegraph.github.io/graph/derived/rhel-rebuilds>"));
        assert!(u.contains("COPY <https://packagegraph.github.io/graph/derived/rhel-rebuilds-staging-RUN1> TO <https://packagegraph.github.io/graph/derived/rhel-rebuilds>"));
        assert!(u.contains("DROP SILENT GRAPH <https://packagegraph.github.io/graph/derived/rhel-rebuilds-staging-RUN1>"));
    }

    #[test]
    fn ontology_gate_checks_the_reified_terms() {
        for term in REBUILD_TERMS {
            let q = term_type_query(term);
            assert!(q.contains(term), "gate query for {term} must reference it");
        }
        assert!(REBUILD_TERMS.contains(&"RebuildAssessment"));
        assert!(REBUILD_TERMS.contains(&"assessedAgainstSnapshot"));
        assert!(REBUILD_TERMS.contains(&"RebuildFidelityScheme"));
        assert!(REBUILD_TERMS.contains(&"RebuildDriftScheme"));
        assert!(!REBUILD_TERMS.contains(&"rebuildTrackingStatus")); // old term, gone
    }

    #[test]
    fn derive_rejects_a_pair_whose_rebuild_and_upstream_graph_are_the_same() {
        // Constructed directly against a fake endpoint URL; check_readiness/
        // check_ontology_terms are not called by derive() itself, so this only
        // exercises the guard, no live Fuseki needed.
        let deriver = RebuildComparisonDeriver::new("http://127.0.0.1:1/unused");
        let pairs = vec![Pair {
            rebuild_graph: "https://x/graph/rhel/9".into(),
            rhel_graph: "https://x/graph/rhel/9".into(),
        }];
        let err = deriver.derive("/tmp/unused-output.nt", &pairs, "run-1", "2026-09-04T00:00:00Z").unwrap_err();
        assert!(err.to_string().contains("cannot be its own upstream"));
    }

    #[test]
    fn derive_rejects_duplicate_rebuild_graphs() {
        // The duplicate guard runs before any SPARQL I/O, so a dummy endpoint is fine.
        let deriver = RebuildComparisonDeriver::new("http://127.0.0.1:0/none");
        let pairs = vec![
            Pair { rebuild_graph: "https://x/graph/alma/9".into(), rhel_graph: "https://x/graph/rhel/9".into() },
            Pair { rebuild_graph: "https://x/graph/alma/9".into(), rhel_graph: "https://x/graph/rhel/10".into() },
        ];
        let out = tempfile::NamedTempFile::new().unwrap();
        let err = deriver.derive(out.path().to_str().unwrap(), &pairs, "run-1", "2026-09-04T00:00:00Z").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("https://x/graph/alma/9"), "error must name the duplicate: {err}");
    }

    #[test]
    fn valid_run_token_rejects_whitespace_and_injection() {
        // Space or '>' would break/inject the interpolated `GRAPH <...>` staging IRI.
        assert!(!valid_run_token("has space"));
        assert!(!valid_run_token("evil>graph"));
        assert!(!valid_run_token("")); // empty is rejected by the +
        assert!(!valid_run_token("bad/slash"));
        assert!(!valid_run_token("bad{brace"));
        // A realistic token ("$(date +%s)-$HOSTNAME" expansion) is accepted.
        assert!(valid_run_token("1735689600-deriver-abc123"));
        assert!(valid_run_token("RUN.1_v2-final"));
    }

    #[test]
    fn exactly_one_and_at_most_one_checks_reference_the_predicate() {
        let (_, q1) = exactly_one_check("urn:staging", "assessmentOf");
        assert!(q1.contains("pkg:assessmentOf"));
        assert!(q1.contains("HAVING(?n != 1)"));
        let (_, q2) = at_most_one_check("urn:staging", "rebuildFidelity");
        assert!(q2.contains("pkg:rebuildFidelity"));
        assert!(q2.contains("HAVING(?n > 1)"));
    }

    #[test]
    fn datatype_check_references_predicate_and_type() {
        let (_, q) = datatype_check("urn:staging", "assessedAt", &format!("{XSD}dateTime"));
        assert!(q.contains("pkg:assessedAt"));
        assert!(q.contains("xsd") || q.contains("XMLSchema"));
    }

    #[test]
    fn staging_checks_cover_the_new_shape() {
        let checks = staging_checks("urn:staging", &["urn:rhel9".to_string()]);
        let descs: Vec<&str> = checks.iter().map(|c| c.description.as_str()).collect();
        for expected in [
            "concept outside RebuildFidelityScheme",
            "concept outside RebuildDriftScheme",
            "hasUpstreamCounterpart=false",
            "hasUpstreamCounterpart=true",
            "rebuildDrift present",
            "matched fidelity",
            "fidelity-unknown",
            "ambiguousCandidate",
            "self-baseline",
            "unexpected",
            "pkg:DataSnapshot",
            "rdfs:label",
            "pkg:rebuildOf",
            "lineageConfirmed",
            "lineageEvidence",
        ] {
            assert!(
                descs.iter().any(|d| d.contains(expected)),
                "missing a staging check whose description mentions {expected:?}; got {descs:?}"
            );
        }
    }

    #[test]
    fn snapshot_label_check_rejects_empty_and_whitespace_labels_not_just_absence() {
        // Design spec §6.1 item 16 requires a NON-EMPTY rdfs:label, not merely a
        // present one. A query that only tests `FILTER NOT EXISTS { ?s rdfs:label ?l }`
        // would let `rdfs:label ""` (or a whitespace-only label) pass, which is what
        // this test guards against: the query must filter on label *content*, not
        // just binding presence.
        let checks = staging_checks("urn:staging", &["urn:rhel9".to_string()]);
        let label_check = checks
            .iter()
            .find(|c| c.description.contains("rdfs:label"))
            .expect("a staging check must reference rdfs:label");
        assert!(
            label_check.query.contains("STRLEN") || label_check.query.contains("strlen"),
            "the rdfs:label check must filter on label length/content, not just presence; got: {}",
            label_check.query
        );
    }

    #[test]
    fn snapshot_type_check_scopes_not_exists_to_the_staging_graph() {
        // Confirmed via live testing against a PLAIN (non-union) Fuseki dataset:
        // `FILTER NOT EXISTS { ?s a pkg:DataSnapshot }`, unwrapped, searches the
        // DEFAULT graph, not the enclosing GRAPH <staging> block -- it does NOT
        // inherit the outer GRAPH scope the way one might assume. This project's
        // deployed Fuseki config happens to set tdb2:unionDefaultGraph true, which
        // incidentally masks the bug by unioning every named graph into the default
        // graph -- but that is a deployment-specific accident, not a SPARQL
        // guarantee, and validate_staging must be correct independent of it: a
        // differently-configured (or default-configured) Fuseki would see this
        // check flag EVERY valid DataSnapshot as missing, rejecting every run.
        let checks = staging_checks("urn:staging", &["urn:rhel9".to_string()]);
        let type_check = checks
            .iter()
            .find(|c| c.description.contains("pkg:DataSnapshot"))
            .expect("a staging check must reference pkg:DataSnapshot");
        assert!(
            type_check.query.contains("GRAPH <urn:staging> { ?s a pkg:DataSnapshot }")
                || type_check.query.matches("GRAPH <urn:staging>").count() >= 2,
            "the pkg:DataSnapshot type check's FILTER NOT EXISTS must explicitly \
             re-scope to GRAPH <staging>, not rely on outer-scope inheritance \
             (which does not hold on a non-union dataset); got: {}",
            type_check.query
        );
    }

    #[test]
    fn epoch_aggregation_collapses_rows_taking_max_epoch() {
        // query_source_builds yields one row per (source, binary); two rows for the
        // same source URI with epochs 0 and 1 must collapse to one Build w/ epoch 1.
        let rows = vec![
            ("openssl".to_string(), "src:openssl".to_string(), 0, "3.5.5".to_string(), "6.el9".to_string()),
            ("openssl".to_string(), "src:openssl".to_string(), 1, "3.5.5".to_string(), "6.el9".to_string()),
        ];
        let out = aggregate_source_rows(rows);
        assert_eq!(out.len(), 1, "same source URI must collapse to one Build");
        assert_eq!(out[0].0, "openssl");
        assert_eq!(out[0].1.epoch, 1, "epoch must be the MAX across rows");
        assert_eq!(out[0].1.version, "3.5.5");
        assert_eq!(out[0].1.release, "6.el9");
    }
}

#[cfg(test)]
mod emission_tests {
    use super::*;
    use crate::rebuild_classify::Build;

    #[test]
    fn iris_are_scoped_to_run_token() {
        let a1 = assessment_iri("https://x/src/rhel/9/foo/1.0-1", "run-1");
        let a2 = assessment_iri("https://x/src/rhel/9/foo/1.0-1", "run-2");
        assert_ne!(a1, a2);
        assert!(a1.starts_with("https://x/src/rhel/9/foo/1.0-1/assessment/"));

        let s1 = snapshot_iri("https://x/graph/rhel/9", "run-1");
        let s2 = snapshot_iri("https://x/graph/rhel/9", "run-2");
        assert_ne!(s1, s2);
        // same (graph, run) => same snapshot IRI (reused within a run)
        assert_eq!(snapshot_iri("https://x/graph/rhel/9", "run-1"), s1);
    }

    #[test]
    fn build_report_end_to_end_case_coverage() {
        // One pair: almalinux/9 -> rhel/9. Names: "openssl" (exact match, drift-even),
        // "foo" (no upstream candidates -> presence=false), "bar" (behind).
        let pairs_data = vec![PairData {
            rebuild_dist: "https://x/d/distro/almalinux".into(),
            rhel_dist: "https://x/d/distro/rhel".into(),
            rhel_graph: "https://x/graph/rhel/9".into(),
            rebuild_builds: vec![
                ("openssl".into(), Build { node_uri: "alma:openssl".into(), epoch: 0, version: "3.5.5".into(), release: "6.el9_8".into() }),
                ("bar".into(), Build { node_uri: "alma:bar".into(), epoch: 0, version: "1.0".into(), release: "1.el9".into() }),
                ("rocky-logos".into(), Build { node_uri: "alma:rocky-logos".into(), epoch: 0, version: "90".into(), release: "1.el9".into() }),
            ],
            rhel_builds: vec![
                ("openssl".into(), Build { node_uri: "rhel:openssl".into(), epoch: 0, version: "3.5.5".into(), release: "6.el9_8".into() }),
                ("bar".into(), Build { node_uri: "rhel:bar".into(), epoch: 0, version: "1.2".into(), release: "1.el9".into() }),
            ],
        }];

        let (nt, report) = build_report(&pairs_data, "run-abc", "2026-09-04T00:00:00Z").unwrap();

        assert_eq!(report.pairs, 1);
        assert_eq!(report.assessments, 3);
        assert_eq!(report.presence_true, 2); // openssl, bar
        assert_eq!(report.presence_false, 1); // rocky-logos
        assert_eq!(report.assessment_subjects.len(), 3);
        assert_eq!(report.assessment_subjects.len(), report.assessments);
        assert_eq!(report.fidelity_counts.values().sum::<usize>(), report.presence_true);
        assert_eq!(report.drift_counts.values().sum::<usize>(), report.presence_true);
        assert_eq!(report.distribution_count, 1);
        assert_eq!(report.snapshot_subjects.len(), 1); // one upstream graph this run

        // Exactly one DataSnapshot definition emitted despite 3 references to it.
        // NOTE: N-Triples has no "a" (rdf:type) abbreviation -- NTriplesWriter always
        // emits the full rdf:type predicate IRI -- so match on that, not "a <...>".
        assert_eq!(nt.matches("<http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#DataSnapshot>").count(), 1);
        // Snapshot is pinned per design-decisions.md: timestamp + source graph.
        assert!(nt.contains("#snapshotTimestamp> \"2026-09-04T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"));
        assert!(nt.contains("#snapshotSource> \"https://x/graph/rhel/9\""));
        // Never emit rebuildOf, never lineageConfirmed=true, never lineageEvidence.
        assert!(!nt.contains("#rebuildOf>"));
        assert!(!nt.contains("lineageConfirmed> \"true\""));
        assert!(!nt.contains("#lineageEvidence>"));
        // Method id is the -nostream variant everywhere.
        assert!(nt.contains("\"rebuild-norm/v1-nostream\""));
        assert!(!nt.contains("\"rebuild-norm/v1\""));
        // openssl: exact fidelity, drift-even, confidence 1.0.
        assert!(nt.contains("#fidelityBaseline> <rhel:openssl>"));
        assert!(nt.contains("#fidelity-exact>"));
        assert!(nt.contains("#drift-even>"));
        // rocky-logos: hasUpstreamCounterpart false, no fidelity/drift lines for it.
        let rocky_assessment_prefix = "alma:rocky-logos/assessment/";
        assert!(nt.lines().any(|l| l.contains(rocky_assessment_prefix) && l.contains("hasUpstreamCounterpart> \"false\"")));
    }

    #[test]
    fn dedup_writer_collapses_repeated_snapshot_definition() {
        // Two pairs citing the SAME upstream graph must share one DataSnapshot node.
        let make_pair = |rb_dist: &str| PairData {
            rebuild_dist: rb_dist.into(),
            rhel_dist: "https://x/d/distro/rhel".into(),
            rhel_graph: "https://x/graph/rhel/9".into(),
            rebuild_builds: vec![("foo".into(), Build { node_uri: format!("{rb_dist}:foo"), epoch: 0, version: "1.0".into(), release: "1.el9".into() })],
            rhel_builds: vec![("foo".into(), Build { node_uri: "rhel:foo".into(), epoch: 0, version: "1.0".into(), release: "1.el9".into() })],
        };
        let pairs_data = vec![make_pair("https://x/d/distro/almalinux"), make_pair("https://x/d/distro/rocky")];
        let (nt, report) = build_report(&pairs_data, "run-1", "2026-09-04T00:00:00Z").unwrap();
        assert_eq!(report.snapshot_subjects.len(), 1);
        assert_eq!(nt.matches("<http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#DataSnapshot>").count(), 1);
        assert_eq!(nt.matches("http://www.w3.org/2000/01/rdf-schema#label>").count(), 1);
        // snapshotTimestamp/snapshotSource dedup exactly like type/label above:
        // two pairs share the same upstream graph -> each emitted exactly once.
        assert_eq!(nt.matches("#snapshotTimestamp>").count(), 1);
        assert_eq!(nt.matches("#snapshotSource>").count(), 1);
        assert!(nt.contains("#snapshotSource> \"https://x/graph/rhel/9\""));
    }

    #[test]
    fn ambiguous_tie_emits_multiple_candidates_and_no_baseline() {
        // Two upstream builds tied at the SAME fidelity tier (identical
        // epoch/version/release, different node_uri) for one rebuild name:
        // drives assess()'s exact-tier ambiguous branch (resolve_tier's
        // candidates.len() != 1 case) end-to-end through build_report.
        let pairs_data = vec![PairData {
            rebuild_dist: "https://x/d/distro/almalinux".into(),
            rhel_dist: "https://x/d/distro/rhel".into(),
            rhel_graph: "https://x/graph/rhel/9".into(),
            rebuild_builds: vec![(
                "tied-pkg".into(),
                Build { node_uri: "alma:tied-pkg".into(), epoch: 0, version: "1.0".into(), release: "1.el9".into() },
            )],
            rhel_builds: vec![
                (
                    "tied-pkg".into(),
                    Build { node_uri: "rhel:tied-pkg-a".into(), epoch: 0, version: "1.0".into(), release: "1.el9".into() },
                ),
                (
                    "tied-pkg".into(),
                    Build { node_uri: "rhel:tied-pkg-b".into(), epoch: 0, version: "1.0".into(), release: "1.el9".into() },
                ),
            ],
        }];

        let (nt, report) = build_report(&pairs_data, "run-token", "2026-09-08T00:00:00Z").unwrap();

        assert_eq!(report.ambiguous_count, 1);

        let assessment = assessment_iri("alma:tied-pkg", "run-token");
        let subject_tag = format!("<{assessment}>");
        let lines: Vec<&str> = nt.lines().filter(|l| l.starts_with(&subject_tag)).collect();
        assert!(!lines.is_empty());

        assert_eq!(lines.iter().filter(|l| l.contains("#rebuildFidelity>")).count(), 1);
        assert!(lines.iter().any(|l| l.contains("#rebuildFidelity> <https://purl.org/packagegraph/ontology/core#fidelity-unknown>")));

        assert_eq!(lines.iter().filter(|l| l.contains("#assessmentConfidence>")).count(), 1);
        assert!(lines.iter().any(|l| l.contains("#assessmentConfidence> \"0.5\"^^<http://www.w3.org/2001/XMLSchema#decimal>")));

        assert_eq!(lines.iter().filter(|l| l.contains("#ambiguousCandidate>")).count(), 2);
        assert!(lines.iter().any(|l| l.contains("#ambiguousCandidate> <rhel:tied-pkg-a>")));
        assert!(lines.iter().any(|l| l.contains("#ambiguousCandidate> <rhel:tied-pkg-b>")));

        assert!(!lines.iter().any(|l| l.contains("#fidelityBaseline>")));
    }
}
