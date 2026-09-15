//! Guards a Maven root seed list, when one is available to check.
//!
//! Seed lists are **not** in this repository -- they are curated sets drawn
//! from private sources, installed on the collector host and bind-mounted at
//! `/seeds` (see `deploy/quadlet/collectors/seeds/README.md`). So these tests
//! are opt-in: point `PG_COLLECT_MAVEN_SEEDS` at a list and they run; leave it
//! unset, as in CI, and they skip.
//!
//! That makes them a maintainer's tool rather than a build gate, which is the
//! honest trade for keeping the list out of a public repository. Run them
//! before installing an edited list.
//!
//! What they catch is the silent failures. `MavenSeed::parse` returns `None`
//! for a malformed line and `read_maven_seed_file` simply skips it, so a typo
//! raises nothing -- it just shrinks the published `graph/maven`. A pinned root
//! skips version resolution entirely, freezing that coordinate. Neither shows
//! up as an error at collection time.

use pg_collect::maven::read_maven_seed_file;
use std::collections::HashSet;

/// The list under test, or `None` when `PG_COLLECT_MAVEN_SEEDS` is unset.
///
/// Returning `None` rather than defaulting to a repository path is deliberate:
/// there is no in-repo list to fall back to, and a default would turn "no list
/// configured" into a confusing missing-file failure.
fn seed_path() -> Option<String> {
    match std::env::var("PG_COLLECT_MAVEN_SEEDS") {
        Ok(p) if !p.trim().is_empty() => Some(p),
        _ => {
            eprintln!(
                "SKIP: set PG_COLLECT_MAVEN_SEEDS=<path to a seed list> to run \
                 the Maven seed-list checks"
            );
            None
        }
    }
}

/// Lines `read_maven_seed_file` is expected to turn into a seed -- everything
/// that is not blank and not a `#` comment.
fn significant_lines(path: &str) -> Vec<String> {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read seed file {}: {}", path, e));
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Every non-comment line must parse. `read_maven_seed_file` drops unparseable
/// lines silently, so comparing counts is the only way a typo ever surfaces.
#[test]
fn seed_file_has_no_silently_dropped_lines() {
    let Some(path) = seed_path() else { return };
    let lines = significant_lines(&path);
    let seeds = read_maven_seed_file(&path).expect("seed file should be readable");

    // read_maven_seed_file dedups, so compare against the deduped line set
    // rather than the raw count -- otherwise a duplicate line would be
    // misreported here as a parse failure.
    let unique_lines: HashSet<&String> = lines.iter().collect();
    assert_eq!(
        seeds.len(),
        unique_lines.len(),
        "{} of {} unique seed lines failed to parse; \
         every line must be `groupId:artifactId`",
        unique_lines.len().saturating_sub(seeds.len()),
        unique_lines.len()
    );
}

/// The general Maven ecosystem view resolves each root to its newest release
/// and then walks that release's declared dependencies. A pinned root skips
/// version resolution entirely (see `MavenSeed::parse`), which would freeze
/// that coordinate at whatever version was written down and, for
/// vendor-suffixed builds such as `...redhat-00001`, 404 outright -- those are
/// published to vendor repositories, never to Maven Central.
#[test]
fn seed_roots_are_all_unpinned() {
    let Some(path) = seed_path() else { return };
    let seeds = read_maven_seed_file(&path).expect("seed file should be readable");

    let pinned: Vec<String> = seeds
        .iter()
        .filter(|s| s.version.is_some())
        .map(|s| format!("{}:{}:{}", s.group_id, s.artifact_id, s.version.as_ref().unwrap()))
        .collect();

    assert!(
        pinned.is_empty(),
        "seed roots must be unpinned `groupId:artifactId` so each run resolves \
         the newest release; found {} pinned: {:?}",
        pinned.len(),
        pinned
    );
}

/// An emptied seed list would still "succeed" -- the collector would publish an
/// almost-empty `graph/maven` over the real one. No specific size is asserted:
/// the expected count is a property of a private list, not of this repository.
#[test]
fn seed_file_is_not_empty() {
    let Some(path) = seed_path() else { return };
    let seeds = read_maven_seed_file(&path).expect("seed file should be readable");
    assert!(!seeds.is_empty(), "seed list parsed to zero roots: {}", path);
}

/// Duplicates are harmless at runtime (`read_maven_seed_file` dedups) but
/// signal an uncurated edit, and they mask the drop-detection above.
#[test]
fn seed_file_has_no_duplicate_lines() {
    let Some(path) = seed_path() else { return };
    let lines = significant_lines(&path);
    let mut seen = HashSet::new();
    let dupes: Vec<&String> = lines.iter().filter(|l| !seen.insert(*l)).collect();
    assert!(dupes.is_empty(), "duplicate seed lines: {:?}", dupes);
}

/// The list must stay sorted so hand edits produce reviewable diffs.
#[test]
fn seed_file_is_sorted() {
    let Some(path) = seed_path() else { return };
    let lines = significant_lines(&path);
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(lines, sorted, "seed file must be sorted");
}
