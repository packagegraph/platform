//! Links advisories to the packages this corpus actually holds.
//!
//! The bulk OSV collector (`osv.rs`) publishes the advisories themselves --
//! summary, CVSS, CWE, dates -- for every ecosystem it pulls. What it cannot
//! publish is which of OUR packages each advisory touches, because it does
//! not know what we collected. That join is this enricher's whole job, and
//! its only output is `sec:affectsPackage`.
//!
//! ## Why it no longer calls the OSV API (#59)
//!
//! It used to ask `api.osv.dev` about every package in the corpus, one
//! request at a time, at the 500ms pacing this repo applies to that host.
//! Measured: 0.26s per GET, ~0.65s effective per advisory, against 347,563
//! advisories across the ecosystems it looped -- roughly 63 hours, against
//! an 8 hour timeout it had already been SIGKILL'd by.
//!
//! Most of that was redundant: the bulk collector had already downloaded the
//! same records that morning, as ZIPs, for twelve of those ecosystems. So the
//! API is gone. This reads the same archives -- 72 MB for Debian and Alpine,
//! about 108 seconds -- and spends its time on the join instead.
//!
//! The important property is not that it got faster. It is that the cost now
//! tracks how many advisories OSV publishes, not how many packages we hold.
//! Every per-package path gets SLOWER as collection completes; this one does
//! not move at all.
//!
//! ## Why the archives rather than the graph
//!
//! The affected-package data is in the archives but not in the graph:
//! `emit_vulnerability_triples` drops the whole `affected[]` block for distro
//! ecosystems, because `ecosystem_mapping` has no entry for them.
//!
//! Putting it there would mean editing a function twelve working ecosystems
//! depend on, and would still lose the release. OSV names distro ecosystems
//! with it -- `Debian:13`, `Alpine:v3.20` -- while `ecosystem_uri` is used
//! everywhere else for bare names, so carrying `Debian:13` into the graph
//! would need a new ontology property for a fact that is only ever an INPUT
//! to this join. Reading the archive keeps the release precise and changes
//! nothing anyone else depends on.

use crate::ntriples::NTriplesWriter;
use crate::osv::{download_archive, for_each_vulnerability, vulnerability_subject_uri};
use crate::sparql::{make_sparql_client, SparqlAuth, SparqlBackend, SparqlClient};
use crate::uris::SEC;
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Result;

/// Where a collected distro's packages live, and which OSV archive covers it.
struct DistroTarget {
    /// The OSV archive to read, e.g. "Debian".
    archive: &'static str,
    /// Graph URI prefix for the graphs holding these packages. No trailing
    /// slash, so `.../graph/debian/trixie` also covers the per-architecture
    /// graphs published beside it.
    graph_prefix: &'static str,
    /// The RDF type the packages carry in those graphs.
    rdf_type: &'static str,
}

const DEB: &str = "https://purl.org/packagegraph/ontology/deb#BinaryPackage";
const APK: &str = "https://purl.org/packagegraph/ontology/apk#ApkPackage";

pub struct SecurityEnricher {
    sparql: SparqlClient,
    transport: crate::http_transport::HttpTransport,
    pub graph_uri: Option<String>,
}

impl SecurityEnricher {
    pub fn new(endpoint: &str, auth: SparqlAuth, backend: SparqlBackend) -> Self {
        // 300s, matching the collector: a run pulls whole ecosystem archives.
        let client = crate::enricher::http_client_builder()
            .timeout(std::time::Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            sparql: make_sparql_client(endpoint, &auth, backend),
            transport: crate::http_transport::HttpTransport::with_client(client),
            graph_uri: None,
        }
    }

    /// Set the graph URI for N-Quads output.
    pub fn with_graph(mut self, graph_uri: Option<String>) -> Self {
        self.graph_uri = graph_uri;
        self
    }

    pub fn enrich(&self, output_path: &str) -> Result<(usize, usize)> {
        let file = File::create(output_path)?;
        let mut writer = NTriplesWriter::new_maybe_graph(file, self.graph_uri.as_deref());

        let mut linked_packages = 0;
        let mut total_triples = 0;

        for target in targets() {
            let corpus = self.corpus_index(&target)?;
            if corpus.is_empty() {
                // Nothing collected for this distro: reading its archive
                // could only produce links to packages we do not have.
                eprintln!(
                    "No packages under {} -- skipping the {} archive",
                    target.graph_prefix, target.archive
                );
                continue;
            }
            eprintln!(
                "{}: {} package names collected under {}",
                target.archive,
                corpus.len(),
                target.graph_prefix
            );

            let zip_bytes = download_archive(&self.transport, target.archive)?;
            let (_advisories, triples) = for_each_vulnerability(&zip_bytes, |vuln| {
                let subject = vulnerability_subject_uri(vuln);
                // Distinct, because one advisory carries an entry per
                // affected RELEASE and they all name the same package:
                // measured 23 identical entries for curl in a single Alpine
                // record. The link is release-independent -- it says which
                // identity is affected, not in which release -- so without
                // this the output is the same statement 23 times.
                let mut identities: BTreeSet<&str> = BTreeSet::new();
                for name in affected_names(vuln, target.archive) {
                    if let Some(found) = corpus.get(name) {
                        identities.extend(found.iter().map(|s| s.as_str()));
                    }
                }
                for identity in &identities {
                    writer.write_triple(&subject, &format!("{SEC}affectsPackage"), identity)?;
                }
                Ok(identities.len())
            })?;

            linked_packages += corpus.len();
            total_triples += triples;
            eprintln!("{}: {} links emitted", target.archive, triples);
        }

        writer.flush()?;
        Ok((linked_packages, total_triples))
    }

    /// Package name to the identities we hold for it.
    ///
    /// Several identities per name is the normal case, not an edge case:
    /// identities are arch-qualified and each architecture is collected into
    /// its own graph, so `curl` in trixie is three identities.
    fn corpus_index(&self, target: &DistroTarget) -> Result<HashMap<String, Vec<String>>> {
        let rows = self
            .sparql
            .query_identities_by_type_under(target.rdf_type, target.graph_prefix)?;

        let mut index: HashMap<String, Vec<String>> = HashMap::new();
        for (name, identity) in rows {
            index.entry(name).or_default().push(identity);
        }
        Ok(index)
    }
}

/// The distros this corpus collects that OSV also publishes, and whose
/// advisories this enricher links to our packages.
///
/// OSV has no Fedora ecosystem at all, so fedora/{43,44} cannot appear here
/// at any point (#97); #101 has where its advisories would come from.
///
/// AlmaLinux, Rocky Linux and Red Hat are absent for a different reason: the
/// OSV collector now pulls all three archives, so their advisory nodes are
/// published, but wiring them into THIS join is separate, unvalidated work.
/// `ecosystem_base` already handles their names, and the `affected[]` names
/// are binary RPM names rather than source ones, so the shape fits -- what
/// does not yet fit is the release. OSV qualifies these per product variant,
/// not per release (`Red Hat:enterprise_linux:9::appstream`; 917 distinct
/// strings across 23,285 records), and the corpus index for a prefix like
/// `.../graph/rhel/` would span rhel/9 and rhel/10 at once, so a name shared
/// by a release we hold and one we do not links anyway. That is tolerable
/// for two Debian releases and is not for six RHEL generations. Tracked
/// separately.
fn targets() -> Vec<DistroTarget> {
    vec![
        DistroTarget {
            archive: "Debian",
            graph_prefix: "https://packagegraph.github.io/graph/debian/trixie",
            rdf_type: DEB,
        },
        DistroTarget {
            archive: "Alpine",
            graph_prefix: "https://packagegraph.github.io/graph/alpine/",
            rdf_type: APK,
        },
    ]
}

/// The package names an advisory reports against `archive`.
///
/// OSV qualifies a distro ecosystem with its release -- `Debian:13`,
/// `Alpine:v3.20` -- and one advisory usually carries an entry per affected
/// release. Only entries whose ecosystem belongs to this archive count; the
/// release itself is not filtered here, because the corpus index is already
/// scoped to the releases we collected, so a name from a release we do not
/// hold simply will not match.
fn affected_names<'a>(
    vuln: &'a crate::osv::OsvVulnerability,
    archive: &str,
) -> impl Iterator<Item = &'a str> {
    let archive = archive.to_string();
    vuln.affected.iter().filter_map(move |affected| {
        let package = affected.package.as_ref()?;
        if ecosystem_base(&package.ecosystem) != archive {
            return None;
        }
        Some(package.name.as_str())
    })
}

/// `Debian:13` -> `Debian`. The part before the colon is the ecosystem;
/// anything after it is the release.
fn ecosystem_base(ecosystem: &str) -> &str {
    ecosystem.split(':').next().unwrap_or(ecosystem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osv::{OsvAffected, OsvPackage, OsvVulnerability};

    fn vuln(id: &str, aliases: &[&str], affected: &[(&str, &str)]) -> OsvVulnerability {
        OsvVulnerability {
            id: id.to_string(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            summary: None,
            details: None,
            published: None,
            modified: None,
            withdrawn: None,
            affected: affected
                .iter()
                .map(|(ecosystem, name)| OsvAffected {
                    package: Some(OsvPackage {
                        name: name.to_string(),
                        ecosystem: ecosystem.to_string(),
                        purl: None,
                    }),
                    versions: vec![],
                    ranges: vec![],
                    database_specific: None,
                })
                .collect(),
            severity: vec![],
            references: vec![],
            database_specific: None,
        }
    }

    #[test]
    fn an_osv_release_qualified_ecosystem_reduces_to_its_archive() {
        assert_eq!(ecosystem_base("Debian:13"), "Debian");
        assert_eq!(ecosystem_base("Alpine:v3.20"), "Alpine");
        // Language ecosystems carry no release and must survive unchanged.
        assert_eq!(ecosystem_base("crates.io"), "crates.io");
        assert_eq!(ecosystem_base("npm"), "npm");
    }

    #[test]
    fn only_the_archives_own_entries_are_read() {
        // One advisory routinely carries an entry per affected release, and
        // records reached through the Debian archive can still name other
        // ecosystems. Reading those would link an advisory to a package it
        // does not describe.
        let v = vuln(
            "DEBIAN-CVE-2023-31102",
            &["CVE-2023-31102"],
            &[
                ("Debian:12", "7zip"),
                ("Debian:13", "7zip"),
                ("Alpine:v3.20", "7zip"),
                ("npm", "7zip-bin"),
            ],
        );

        let debian: Vec<&str> = affected_names(&v, "Debian").collect();
        assert_eq!(debian, vec!["7zip", "7zip"], "both Debian releases");

        let alpine: Vec<&str> = affected_names(&v, "Alpine").collect();
        assert_eq!(alpine, vec!["7zip"]);
    }

    #[test]
    fn an_advisory_is_linked_under_the_uri_the_collector_published_it_as() {
        // The whole output is worthless if the two disagree: these links
        // have to land on the same subject the bulk collector minted.
        let with_cve = vuln("DEBIAN-CVE-2023-31102", &["CVE-2023-31102"], &[]);
        assert!(
            vulnerability_subject_uri(&with_cve).contains("CVE-2023-31102"),
            "a CVE alias keys the advisory, not its OSV id"
        );

        let without = vuln("OSV-2020-111", &[], &[]);
        assert!(vulnerability_subject_uri(&without).contains("OSV-2020-111"));
    }

    /// Measured against the real Alpine archive: one record carried 23
    /// entries for curl, one per affected Alpine release. The link says
    /// which identity is affected, not in which release, so emitting one
    /// per entry writes the same statement 23 times.
    #[test]
    fn one_advisory_links_an_identity_once_however_many_releases_name_it() {
        let releases = ["v3.17", "v3.18", "v3.19", "v3.20"];
        let v = vuln(
            "ALPINE-CVE-2017-7468",
            &["CVE-2017-7468"],
            &releases
                .iter()
                .map(|r| (format!("Alpine:{r}"), "curl".to_string()))
                .collect::<Vec<_>>()
                .iter()
                .map(|(e, n)| (e.as_str(), n.as_str()))
                .collect::<Vec<_>>(),
        );

        let mut corpus: HashMap<String, Vec<String>> = HashMap::new();
        corpus.insert(
            "curl".to_string(),
            vec!["urn:curl-amd64".to_string(), "urn:curl-arm64".to_string()],
        );

        let mut identities: BTreeSet<&str> = BTreeSet::new();
        for name in affected_names(&v, "Alpine") {
            if let Some(found) = corpus.get(name) {
                identities.extend(found.iter().map(|s| s.as_str()));
            }
        }

        assert_eq!(
            affected_names(&v, "Alpine").count(),
            4,
            "the record really does name curl once per release"
        );
        assert_eq!(
            identities.len(),
            2,
            "but it is two identities, once each -- not eight"
        );
    }

    #[test]
    fn a_name_we_do_not_hold_produces_no_link() {
        let v = vuln(
            "DEBIAN-CVE-1",
            &["CVE-1"],
            &[("Debian:13", "not-collected")],
        );
        let corpus: HashMap<String, Vec<String>> = HashMap::new();
        let matched: Vec<&str> = affected_names(&v, "Debian")
            .filter(|n| corpus.contains_key(*n))
            .collect();
        assert!(matched.is_empty());
    }
}
