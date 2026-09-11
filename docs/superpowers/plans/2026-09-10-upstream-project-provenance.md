# Cross-Ecosystem Upstream Project Provenance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Recognize Bitbucket as a forge everywhere, extend the existing `UpstreamProject` hub pattern to every collector that emits `pkg:upstreamRepository` (including a keying migration for OpenWrt), and give every graph published through the Minio+QLever replacement pipeline a `pkg:DataSnapshot` recording when and from what source it was collected.

**Architecture:** Two Rust crates worth of changes in `etl/pg-collect/src/` (forge.rs gains a shared matcher and hub-linking helper; uris.rs stops duplicating forge-matching logic and delegates to forge.rs; three direct-writer collectors and one existing OpenWrt collector get small, targeted edits), plus a shell-script change (`upload-nt.sh` mints `DataSnapshot` triples at upload time, since that's the only place in the pipeline that reliably has the graph URI) and a mechanical batch update across the 41 deployed collector scripts.

**Tech Stack:** Rust (std, no new dependencies), POSIX shell (`/bin/sh`, matching existing script style), N-Triples/SPARQL.

**Spec:** `docs/superpowers/specs/2026-09-10-upstream-project-provenance-design.md`

**Post-execution correction:** a whole-branch review (after all 7 tasks
below were implemented) found that `emit_upstream_project_link` as
specified here wrote `pkg:hasUpstreamProject` on every `identity_uri` it
was called with -- but that predicate is `rdfs:domain :SourcePackage`, and
every call site below passes a `pkg:PackageIdentity`, not a
`pkg:SourcePackage`. The shipped code (and the spec, which this plan
argues from) was corrected: the function was renamed to
`emit_upstream_project`, dropped its identity/package parameter entirely,
and never asserts `hasUpstreamProject` -- the hub is discovered via the
`upstreamRepository`/`projectRepository` join instead. See the spec's
"Revision note" (round three) for the full finding. Every
`emit_upstream_project_link(writer, identity_uri, ...)` call shown in the
task text below reflects the plan as originally written and executed, not
the corrected final shape -- read it as history, not as the current
contract.

## Global Constraints

- No ontology version bump, no new RDF class or property. Every term used
  (`pkg:UpstreamProject`, `pkg:hasUpstreamProject`, `pkg:projectRepository`,
  `pkg:projectName`, `pkg:DataSnapshot`, `pkg:snapshotGraph`,
  `pkg:snapshotTimestamp`, `pkg:snapshotSource`) already exists in the pinned
  v0.13.0 ontology.
- No existing collector's emitted `pkg:upstreamRepository` triple changes
  *value* for any input that already resolves today. Internal code shape may
  change (Task 4); the object written must not.
- `pkg:snapshotGraph` MUST be written as `"<uri>"^^<http://www.w3.org/2001/XMLSchema#anyURI>`
  — a typed literal, never an IRI reference (`<uri>`). Getting this backwards
  makes the join query in the spec's §5 silently return zero rows.
- `DataSnapshot` is minted ONLY in `upload-nt.sh` (Task 6), never inside a
  Rust collector. Production invocations never pass `--graph` to `pg-collect`
  (verified: `grep -rn -- '--graph' deploy/quadlet/collectors/scripts/*.sh`
  returns nothing across all 41 scripts) — the Rust process cannot know its
  own destination graph in the deployed pipeline.
- `collect_openwrt_upstream.rs`'s archive-source branch (`meta.source_proto
  != Some("git")`) must remain byte-for-byte behaviorally unchanged — same
  keying, same triples. Only the git-source-with-resolvable-forge-URL case
  changes.
- `uris::normalize_forge_url` and the new `uris::normalize_forge_url_canonical`
  must not contain independently-maintained forge-matching logic — both
  delegate to `forge::extract_forge_url`. This is the fix for how the
  Bitbucket gap and the GitLab nested-group truncation bug both happened
  (two hand-synced matchers drifting).
- Run `cargo test --lib` (from `etl/pg-collect/`; 775 tests before this plan
  starts) after every task that touches Rust — it must stay green throughout.
  Run it from `etl/pg-collect/` (the crate root), not the repo root.

---

### Task 1: Recognize Bitbucket in `forge.rs`

**Files:**
- Modify: `etl/pg-collect/src/forge.rs:40` (`FORGE_HOSTS`), `~line 233`
  (`normalize_direct_forge`, immediately after the GitHub case)
- Test: same file, `mod tests` (existing module starting around line 935)

**Interfaces:**
- Produces: `forge::extract_forge_url("https://bitbucket.org/owner/repo")`
  now returns `Some(ForgeExtraction { repo_url: "https://bitbucket.org/owner/repo", confidence: High, .. })`.
  This is the sole recognition point every later task depends on (Task 2
  delegates to it; Task 3's `emit_upstream_project_link` is keyed on
  whatever `extract_forge_url` resolves).

- [ ] **Step 1: Write the failing tests**

Add to `forge.rs`'s existing `mod tests`, next to `test_extract_github_direct`
and `test_owner_repo_github_dotted` (matching their exact style):

```rust
#[test]
fn test_extract_bitbucket_direct() {
    let result = extract_forge_url("https://bitbucket.org/owner/repo").unwrap();
    assert_eq!(result.repo_url, "https://bitbucket.org/owner/repo");
    assert_eq!(result.confidence, Confidence::High);
}

#[test]
fn test_extract_bitbucket_trailing_slash() {
    let result = extract_forge_url("https://bitbucket.org/owner/repo/").unwrap();
    assert_eq!(result.repo_url, "https://bitbucket.org/owner/repo");
}

#[test]
fn test_extract_bitbucket_git_suffix() {
    let result = extract_forge_url("https://bitbucket.org/owner/repo.git").unwrap();
    assert_eq!(result.repo_url, "https://bitbucket.org/owner/repo");
}

#[test]
fn test_extract_bitbucket_with_subpath() {
    let result = extract_forge_url("https://bitbucket.org/owner/repo/src/master/README.md").unwrap();
    assert_eq!(result.repo_url, "https://bitbucket.org/owner/repo");
}

#[test]
fn test_extract_bitbucket_org_only_rejected() {
    // No repo segment -- must not match, same as test_extract_github_org_only_rejected
    assert!(extract_forge_url("https://bitbucket.org/owner").is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib bitbucket -- --nocapture` (from `etl/pg-collect/`)
Expected: FAIL — `extract_forge_url` returns `None` for all Bitbucket URLs
(Bitbucket isn't in `FORGE_HOSTS` yet).

- [ ] **Step 3: Add Bitbucket to `FORGE_HOSTS`**

In `forge.rs`, change:

```rust
const FORGE_HOSTS: &[&str] = &["github.com", "codeberg.org", "sr.ht"];
```

to:

```rust
const FORGE_HOSTS: &[&str] = &["github.com", "codeberg.org", "sr.ht", "bitbucket.org"];
```

- [ ] **Step 4: Add the Bitbucket case to `normalize_direct_forge`**

Immediately after the GitHub block (the one ending `if !owner.is_empty() &&
!repo.is_empty() { return Some(format!("https://github.com/{}/{}", owner,
repo)); } }`, around line 233), add:

```rust
// Bitbucket: bitbucket.org/{owner}/{repo}
if path.starts_with("bitbucket.org/") {
    let rest = path.strip_prefix("bitbucket.org/")?;
    let caps = FORGE_OWNER_REPO_RE.captures(rest)?;
    let owner = caps.get(1)?.as_str();
    let repo = caps.get(2)?.as_str();
    if !owner.is_empty() && !repo.is_empty() {
        return Some(format!("https://bitbucket.org/{}/{}", owner, repo));
    }
}
```

Also update `normalize_direct_forge`'s doc comment (`~line 219`, "Handles all
known forge patterns: GitHub, GitLab instances, Codeberg, Pagure, Fedora
dist-git, Savannah, Sourceware, kernel.org, Gitea/Forgejo.") to add
Bitbucket to the list.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib bitbucket -- --nocapture`
Expected: PASS, all 5 new tests.

Run: `cargo test --lib` (from `etl/pg-collect/`)
Expected: PASS, 780 tests (775 existing + 5 new), 0 failures.

- [ ] **Step 6: Commit**

```bash
git add etl/pg-collect/src/forge.rs
git commit -m "feat(pg-collect): recognize Bitbucket as a known forge host"
```

---

### Task 2: Retire `uris.rs`'s independent forge matcher; delegate to `forge::extract_forge_url`

**Files:**
- Modify: `etl/pg-collect/src/uris.rs:258-389` (replace `normalize_forge_url`'s
  body; add `normalize_forge_url_canonical`)
- Test: same file, `mod tests` (starting line 689)

**Interfaces:**
- Consumes: `forge::extract_forge_url` (Task 1, already exists; Task 1 only
  added a host, didn't change its signature: `pub fn extract_forge_url(url:
  &str) -> Option<ForgeExtraction>`, `ForgeExtraction.repo_url: String`).
- Produces: `uris::normalize_forge_url_canonical(url: &str) -> Option<String>`
  (new, returns the plain canonical URL) and `uris::normalize_forge_url(url:
  &str) -> Option<String>` (existing signature, now a thin wrapper —
  Task 4's direct writers consume both of these).

There are no existing unit tests for `normalize_forge_url`'s matching logic
in `uris.rs` today (verified: `grep -n -i forge src/uris.rs` shows zero test
functions referencing it) — this task adds the first ones, so there's
nothing pre-existing to preserve or migrate.

- [ ] **Step 1: Write the failing parity/regression tests**

Add a new test module section in `uris.rs`'s existing `mod tests` (after
`test_repo_uri`, ~line 793):

```rust
#[test]
fn test_normalize_forge_url_canonical_gitlab_nested_group() {
    // Round-two finding 3: uris.rs's old hand-rolled GitLab matcher
    // truncated nested groups to two segments. Delegating to
    // forge::extract_forge_url must preserve the full path.
    assert_eq!(
        normalize_forge_url_canonical("https://gitlab.com/group/subgroup/project"),
        Some("https://gitlab.com/group/subgroup/project".to_string())
    );
    assert_eq!(
        normalize_forge_url_canonical("https://gitlab.freedesktop.org/mesa/mesa"),
        Some("https://gitlab.freedesktop.org/mesa/mesa".to_string())
    );
}

#[test]
fn test_normalize_forge_url_canonical_strips_fragment_and_query() {
    assert_eq!(
        normalize_forge_url_canonical("https://github.com/owner/repo#readme"),
        Some("https://github.com/owner/repo".to_string())
    );
    assert_eq!(
        normalize_forge_url_canonical("https://github.com/owner/repo?tab=readme"),
        Some("https://github.com/owner/repo".to_string())
    );
}

#[test]
fn test_normalize_forge_url_canonical_strips_git_suffix() {
    assert_eq!(
        normalize_forge_url_canonical("https://codeberg.org/owner/repo.git"),
        Some("https://codeberg.org/owner/repo".to_string())
    );
}

#[test]
fn test_normalize_forge_url_canonical_forge_specific_paths() {
    assert_eq!(
        normalize_forge_url_canonical("https://salsa.debian.org/team/repo"),
        Some("https://salsa.debian.org/team/repo".to_string())
    );
    assert_eq!(
        normalize_forge_url_canonical("https://pagure.io/some-repo"),
        Some("https://pagure.io/some-repo".to_string())
    );
    assert_eq!(
        normalize_forge_url_canonical("https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git"),
        Some("https://git.kernel.org/linux/kernel/git/torvalds/linux".to_string())
    );
}

#[test]
fn test_normalize_forge_url_canonical_bitbucket() {
    // Confirms uris.rs inherits Task 1's Bitbucket recognition through
    // delegation, with no separate uris.rs-side patch needed.
    assert_eq!(
        normalize_forge_url_canonical("https://bitbucket.org/owner/repo"),
        Some("https://bitbucket.org/owner/repo".to_string())
    );
}

#[test]
fn test_normalize_forge_url_canonical_loose_gitlab_substring_no_longer_matches() {
    // Intentional narrowing (round two, finding 3): the old uris.rs matcher
    // matched any URL containing "gitlab." as a substring anywhere, not
    // just a real GitLab host -- a false-positive risk. forge.rs's finite
    // GITLAB_HOSTS list does not have this problem. If this test starts
    // failing, something reintroduced the loose match.
    assert_eq!(
        normalize_forge_url_canonical("https://blog.example.com/tags/gitlab.html"),
        None
    );
}

#[test]
fn test_normalize_forge_url_still_wraps_with_repo_uri() {
    // normalize_forge_url's existing public contract: same recognized
    // input, but wrapped as a repo_uri() PackageGraph node IRI.
    assert_eq!(
        normalize_forge_url("https://github.com/owner/repo"),
        Some(repo_uri("https://github.com/owner/repo"))
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib normalize_forge_url_canonical -- --nocapture` (from
`etl/pg-collect/`)
Expected: FAIL with "cannot find function `normalize_forge_url_canonical`
in this scope" (compile error) — the function doesn't exist yet.

- [ ] **Step 3: Replace `normalize_forge_url`'s body and add the canonical variant**

In `uris.rs`, delete the entire existing body of `normalize_forge_url`
(lines 266-389 — every `if path.starts_with(...) { ... return Some(repo_uri(...)) ... }`
branch) and replace the whole function plus its doc comment with:

```rust
/// Match a URL against known forge patterns and return its canonical form
/// (e.g. "https://github.com/owner/repo") -- the plain URL, not a
/// PackageGraph node IRI. Delegates to forge::extract_forge_url so this
/// and every forge.rs-routed collector share exactly one matcher -- see
/// round-two finding 3 in the design spec for why two independently
/// maintained copies drifted (a GitLab nested-group truncation bug).
pub fn normalize_forge_url_canonical(url: &str) -> Option<String> {
    crate::forge::extract_forge_url(url).map(|extraction| extraction.repo_url)
}

/// Try to normalize a URL into a canonical forge repository URI.
/// Returns Some(repo_uri) if the URL matches a known forge pattern, None otherwise.
pub fn normalize_forge_url(url: &str) -> Option<String> {
    normalize_forge_url_canonical(url).map(|canonical| repo_uri(&canonical))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib` (from `etl/pg-collect/`)
Expected: PASS, 787 tests (780 from Task 1 + 7 new), 0 failures. This also
confirms no other existing test anywhere in the crate depended on the old
`normalize_forge_url` matching behavior (e.g. the loose GitLab substring
match) — if something else breaks here, stop and investigate before
continuing; don't proceed to Task 3 with a red suite.

- [ ] **Step 5: Commit**

```bash
git add etl/pg-collect/src/uris.rs
git commit -m "refactor(pg-collect): delegate normalize_forge_url to forge::extract_forge_url

Two independently-maintained forge matchers is how the Bitbucket gap and a
GitLab nested-group truncation bug both happened. uris.rs no longer has its
own matching logic."
```

---

### Task 3: Add the `UpstreamProject` hub-linking helper, centralized in `forge::emit_upstream_repo`

**Files:**
- Modify: `etl/pg-collect/src/uris.rs` (add `project_name_from_repo_url`,
  near `upstream_uri` at line 230)
- Modify: `etl/pg-collect/src/forge.rs` (add `emit_upstream_project_link`;
  add one call inside `emit_upstream_repo`, `forge.rs:906-928`)
- Test: both files' existing `mod tests`

**Interfaces:**
- Consumes: `uris::upstream_uri(name: &str) -> String` (existing,
  `uris.rs:230-233`), `uris::repo_uri(url: &str) -> String` (existing,
  `uris.rs:248-255`), `NTriplesWriter::write_triple_once`/`write_literal_once`
  (existing, `ntriples.rs:81`/`92`, both `-> std::io::Result<bool>`).
- Produces: `uris::project_name_from_repo_url(repo_url: &str) -> String`
  and `forge::emit_upstream_project_link(writer: &mut NTriplesWriter,
  identity_uri: &str, repo_url: &str) -> std::io::Result<usize>` — Task 4
  and Task 5 both call this helper directly.

- [ ] **Step 1: Write the failing tests**

In `uris.rs`'s `mod tests`, after the new Task 2 tests:

```rust
#[test]
fn test_project_name_from_repo_url_github() {
    assert_eq!(
        project_name_from_repo_url("https://github.com/FasterXML/jackson-databind"),
        "FasterXML/jackson-databind"
    );
}

#[test]
fn test_project_name_from_repo_url_gitlab_nested() {
    assert_eq!(
        project_name_from_repo_url("https://gitlab.com/group/subgroup/project"),
        "group/subgroup/project"
    );
}

#[test]
fn test_project_name_from_repo_url_bitbucket() {
    assert_eq!(
        project_name_from_repo_url("https://bitbucket.org/owner/repo"),
        "owner/repo"
    );
}
```

In `forge.rs`'s `mod tests`, after the existing `emit_upstream_repo`-adjacent
tests (search for where `emit_upstream_repo` itself is tested, or add near
the end of the module if it isn't tested directly yet):

```rust
#[test]
fn test_emit_upstream_project_link_writes_hub_triples() {
    use crate::ntriples::NTriplesWriter;
    use std::io::Read;
    use tempfile::NamedTempFile;

    let temp_file = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

    let triples = emit_upstream_project_link(
        &mut writer,
        "https://packagegraph.github.io/d/pkg-identity/example",
        "https://github.com/owner/repo",
    ).unwrap();
    writer.flush().unwrap();

    assert_eq!(triples, 4); // type, projectName, projectRepository, hasUpstreamProject

    let mut content = String::new();
    temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

    assert!(content.contains("UpstreamProject"));
    assert!(content.contains("\"owner/repo\""));
    assert!(content.contains("projectRepository"));
    assert!(content.contains("hasUpstreamProject"));
}

#[test]
fn test_emit_upstream_project_link_dedupes_across_calls() {
    use crate::ntriples::NTriplesWriter;
    use std::io::Read;
    use tempfile::NamedTempFile;

    let temp_file = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

    // Two different packages, same upstream repo.
    emit_upstream_project_link(&mut writer, "https://.../identity/a", "https://github.com/owner/repo").unwrap();
    emit_upstream_project_link(&mut writer, "https://.../identity/b", "https://github.com/owner/repo").unwrap();
    writer.flush().unwrap();

    let mut content = String::new();
    temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

    // Hub minted once (write_*_once), but linked from both identities.
    assert_eq!(content.matches("UpstreamProject>").count(), 1);
    assert_eq!(content.matches("hasUpstreamProject").count(), 2);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib project_name_from_repo_url` and `cargo test --lib
emit_upstream_project_link` (from `etl/pg-collect/`)
Expected: FAIL — compile error, neither function exists yet.

- [ ] **Step 3: Add `project_name_from_repo_url` to `uris.rs`**

Near `upstream_uri` (`uris.rs:230-233`):

```rust
/// Derive a human-readable UpstreamProject name from its canonical repo
/// URL -- the owner/repo slug, e.g. "FasterXML/jackson-databind". Used as
/// pkg:projectName, which UpstreamProject requires (OWL cardinality 1 on
/// :projectName, core.ttl:1319).
pub fn project_name_from_repo_url(repo_url: &str) -> String {
    let path = repo_url
        .strip_prefix("https://")
        .or_else(|| repo_url.strip_prefix("http://"))
        .unwrap_or(repo_url);
    path.splitn(2, '/')
        .nth(1)
        .unwrap_or(path)
        .to_string()
}
```

- [ ] **Step 4: Add `emit_upstream_project_link` to `forge.rs`**

Add near `emit_upstream_repo` (`forge.rs:906-928`):

```rust
/// Link a PackageIdentity to its UpstreamProject hub, minting the hub node
/// (idempotently) if this is the first time any collector run has seen
/// this canonical repo. Keyed on the same repo_url normalize_forge_url /
/// extract_forge_url already produced -- no new identity scheme, reuses
/// the existing uris::upstream_uri helper.
pub fn emit_upstream_project_link(
    writer: &mut NTriplesWriter,
    identity_uri: &str,
    repo_url: &str,
) -> Result<usize> {
    let project_uri = crate::uris::upstream_uri(repo_url);
    let mut triples = 0;

    if writer.write_triple_once(&project_uri, RDF_TYPE, &format!("{PKG}UpstreamProject"))? {
        triples += 1;
    }
    if writer.write_literal_once(
        &project_uri,
        &format!("{PKG}projectName"),
        &crate::uris::project_name_from_repo_url(repo_url),
    )? {
        triples += 1;
    }
    if writer.write_triple_once(
        &project_uri,
        &format!("{PKG}projectRepository"),
        &crate::uris::repo_uri(repo_url),
    )? {
        triples += 1;
    }
    writer.write_triple(identity_uri, &format!("{PKG}hasUpstreamProject"), &project_uri)?;
    triples += 1;

    Ok(triples)
}
```

- [ ] **Step 5: Wire it into `emit_upstream_repo`**

In `emit_upstream_repo` (`forge.rs:906-928`), immediately after the existing
`triples += emit_forge_triples(writer, &r_uri, repo_url)?;` line and before
`Ok(triples)`, add:

```rust
    triples += emit_upstream_project_link(writer, identity_uri, repo_url)?;
```

This single addition covers `debian.rs`, `gentoo.rs`, `collect_salsa.rs`,
`collect_spec.rs`, `cargo_collect.rs`, `yocto.rs`, and `openwrt.rs` — every
caller of `emit_upstream_repo` — with no changes to those seven files.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib` (from `etl/pg-collect/`)
Expected: PASS, 792 tests (787 from Task 2 + 5 new), 0 failures. Any
existing test that asserts an exact triple *count* for a collector routed
through `emit_upstream_repo` (e.g. `debian.rs`, `gentoo.rs`,
`collect_salsa.rs`, `collect_spec.rs`, `cargo_collect.rs`, `yocto.rs`,
`openwrt.rs`) will now see up to 4 more triples per resolved forge URL —
if the suite goes red here, find the specific assertion(s) with `cargo
test --lib 2>&1 | grep FAILED` and update the expected counts to match the
new, correct behavior (don't loosen the assertion to stop checking counts
at all).

- [ ] **Step 7: Commit**

```bash
git add etl/pg-collect/src/uris.rs etl/pg-collect/src/forge.rs
git commit -m "feat(pg-collect): link identities to their UpstreamProject hub in emit_upstream_repo

Covers debian, gentoo, salsa, spec, cargo, yocto, and openwrt collectors in
one change, since they all route through emit_upstream_repo already."
```

---

### Task 4: Wire the hub link into the three direct writers (`rpm.rs`, `maven.rs`, `emit/rdf.rs`)

**Files:**
- Modify: `etl/pg-collect/src/rpm.rs:875-886` (inside `emit_package_triples`,
  `rpm.rs:780`)
- Modify: `etl/pg-collect/src/maven.rs:890-899` (inside
  `emit_package_metadata`, `maven.rs:899`)
- Modify: `etl/pg-collect/src/emit/rdf.rs:105-119`
- Test: each file's existing `mod tests`

**Interfaces:**
- Consumes: `uris::normalize_forge_url_canonical` (Task 2),
  `forge::emit_upstream_project_link` (Task 3).

All three sites emit a second triple today — `&repo_iri, RDF_TYPE,
VCS Repository` — typing the repository node. This must be preserved
exactly; it's easy to drop by accident when refactoring the block, since
it's easy to mistake for something `emit_upstream_project_link` now
covers (it doesn't — that helper types the *hub* node as
`pkg:UpstreamProject`, not the repository node as `vcs:Repository`; both
triples are needed and are about different subjects).

- [ ] **Step 1: Write the failing test for `rpm.rs`**

`rpm.rs` has no existing test for `emit_package_triples`'s
`upstreamRepository` block specifically (verified: `grep -n
"upstreamRepository\|fn test_" src/rpm.rs` shows no test near it). Add a
new one to `rpm.rs`'s `mod tests` (starts line 1763, `use super::*;` at
1764 — brings in `RpmCollector`, `RpmPackageData`, `NTriplesWriter`,
`HashMap`, `HashSet` already used elsewhere in the file):

```rust
#[test]
fn test_rpm_upstream_repo_links_to_hub() {
    use std::io::Read;
    use tempfile::NamedTempFile;

    let collector = RpmCollector::new(
        "https://example.com/repo".to_string(),
        "testdistro".to_string(),
        "1".to_string(),
    );

    let mut fields = HashMap::new();
    fields.insert("name".to_string(), "example-pkg".to_string());
    fields.insert("arch".to_string(), "x86_64".to_string());
    fields.insert("ver".to_string(), "1.0".to_string());
    fields.insert("rel".to_string(), "1".to_string());
    fields.insert("url".to_string(), "https://github.com/owner/repo".to_string());

    let pkg_data = RpmPackageData {
        fields,
        deps: Vec::new(),
    };

    let temp_file = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
    let mut emitted_packages = HashSet::new();

    collector
        .emit_package_triples(&mut writer, &pkg_data, None, &mut emitted_packages)
        .unwrap();
    writer.flush().unwrap();

    let mut content = String::new();
    temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

    assert!(content.contains("upstreamRepository"), "existing triple must still be emitted");
    assert!(content.contains(&format!("{VCS}Repository")), "existing repo typing must be preserved");
    assert!(content.contains("UpstreamProject"), "new hub triple");
    assert!(content.contains("hasUpstreamProject"), "new hub link");
    assert!(
        content.contains("\"owner/repo\""),
        "projectName should be derived from the repo URL"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib test_rpm_upstream_repo_links_to_hub -- --nocapture`
(from `etl/pg-collect/`)
Expected: FAIL — no `UpstreamProject`/`hasUpstreamProject` content yet
(the `upstreamRepository`/`Repository`-typing assertions already pass,
since that part isn't changing).

- [ ] **Step 3: Update `rpm.rs:875-886`**

Change:

```rust
        // Upstream repository (from Homepage/URL if it matches a forge)
        if let Some(url) = fields.get("url") {
            if let Some(upstream_uri) = normalize_forge_url(url) {
                writer.write_triple(
                    &identity_uri,
                    &format!("{PKG}upstreamRepository"),
                    &upstream_uri,
                )?;
                writer.write_triple(&upstream_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
            }
        }
```

to:

```rust
        // Upstream repository (from Homepage/URL if it matches a forge)
        if let Some(url) = fields.get("url") {
            if let Some(canonical_url) = normalize_forge_url_canonical(url) {
                let upstream_repo_iri = repo_uri(&canonical_url);
                writer.write_triple(
                    &identity_uri,
                    &format!("{PKG}upstreamRepository"),
                    &upstream_repo_iri,
                )?;
                writer.write_triple(&upstream_repo_iri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
                triples += crate::forge::emit_upstream_project_link(writer, &identity_uri, &canonical_url)?;
            }
        }
```

(`normalize_forge_url_canonical` and `repo_uri` resolve unqualified here
because `rpm.rs` already has `use crate::uris::*;` at its top, the same way
the original `normalize_forge_url` call did — only `forge::` needs an
explicit `crate::` path, since `rpm.rs` doesn't glob-import it.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib rpm` (from `etl/pg-collect/`)
Expected: PASS, including the new test and all pre-existing `rpm.rs` tests.

- [ ] **Step 5: Repeat for `maven.rs:890-899`**

`maven.rs` also has no existing test isolating this block. Add, near the
end of its `mod tests`:

```rust
#[test]
fn test_maven_upstream_repo_links_to_hub() {
    use std::io::Read;
    use tempfile::NamedTempFile;

    let collector = MavenCollector::new(
        "https://search.maven.org".to_string(),
        "https://repo1.maven.org/maven2".to_string(),
    );
    let pom = PomMetadata {
        group_id: "com.example".to_string(),
        artifact_id: "example-lib".to_string(),
        version: "1.0.0".to_string(),
        scm_url: Some("https://github.com/owner/repo".to_string()),
        ..Default::default() // PomMetadata derives Default (maven.rs:78)
    };

    let temp_file = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());
    collector.emit_package_metadata(&mut writer, &pom).unwrap();
    writer.flush().unwrap();

    let mut content = String::new();
    temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

    assert!(content.contains("upstreamRepository"));
    assert!(content.contains(&format!("{VCS}Repository")));
    assert!(content.contains("UpstreamProject"));
    assert!(content.contains("hasUpstreamProject"));
    assert!(content.contains("\"owner/repo\""));
}
```

(If `PomMetadata` doesn't derive `Default`, or `MavenCollector::new`'s
signature differs from this guess, check `maven.rs`'s existing test module
for how it already constructs a `PomMetadata`/`MavenCollector` for other
tests — e.g. around line 2228 where `scm_url: None,` already appears in a
test fixture per an earlier grep — and match that construction style
instead of the sketch above.)

Change `maven.rs:890-899` from:

```rust
        if let Some(scm_url) = &pom.scm_url {
            if let Some(repo_uri) = crate::uris::normalize_forge_url(scm_url) {
                writer.write_triple(
                    &identity_uri,
                    &format!("{PKG}upstreamRepository"),
                    &repo_uri,
                )?;
                writer.write_triple(&repo_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
            }
        }
```

to:

```rust
        if let Some(scm_url) = &pom.scm_url {
            if let Some(canonical_url) = crate::uris::normalize_forge_url_canonical(scm_url) {
                let upstream_repo_iri = crate::uris::repo_uri(&canonical_url);
                writer.write_triple(
                    &identity_uri,
                    &format!("{PKG}upstreamRepository"),
                    &upstream_repo_iri,
                )?;
                writer.write_triple(&upstream_repo_iri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
                triples += crate::forge::emit_upstream_project_link(writer, &identity_uri, &canonical_url)?;
            }
        }
```

Run `cargo test --lib maven`, verify pass (new test plus all pre-existing
`maven.rs` tests).

- [ ] **Step 6: Repeat for `emit/rdf.rs:105-119`**

Change:

```rust
        if let Some(ref homepage) = meta.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
            // Upstream repository from homepage (if forge URL)
            if let Some(upstream_uri) = normalize_forge_url(homepage) {
                writer.write_triple(
                    &identity_uri,
                    &format!("{PKG}upstreamRepository"),
                    &upstream_uri,
                )?;
                writer.write_triple(&upstream_uri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
            }
        }
```

to:

```rust
        if let Some(ref homepage) = meta.homepage {
            writer.write_literal(&pkg_uri, &format!("{PKG}homepage"), homepage)?;
            triples += 1;
            // Upstream repository from homepage (if forge URL)
            if let Some(canonical_url) = normalize_forge_url_canonical(homepage) {
                let upstream_repo_iri = repo_uri(&canonical_url);
                writer.write_triple(
                    &identity_uri,
                    &format!("{PKG}upstreamRepository"),
                    &upstream_repo_iri,
                )?;
                writer.write_triple(&upstream_repo_iri, RDF_TYPE, &format!("{VCS}Repository"))?;
                triples += 2;
                triples += crate::forge::emit_upstream_project_link(writer, &identity_uri, &canonical_url)?;
            }
        }
```

Find this file's existing tests for the homepage/upstream-repository block
(`grep -n "fn test_" src/emit/rdf.rs`) and add a sibling test following the
same construction pattern used there, asserting the same four things as
the `rpm.rs`/`maven.rs` tests above (`upstreamRepository` unchanged,
`vcs:Repository` typing preserved, `UpstreamProject`/`hasUpstreamProject`
new, `projectName` derived from the URL). Run `cargo test --lib rdf`
(check the actual module path with `cargo test --lib 2>&1 | grep -i rdf`
if that doesn't match), verify pass.

Note `emit/rdf.rs` threads a running `triples` counter (unlike `rpm.rs`/
`maven.rs`) — `emit_upstream_project_link`'s return value must be added to
it, not discarded, to keep this file's triple-count bookkeeping correct.

Add `test_emit_rdf_upstream_repo_links_to_hub`, run `cargo test --lib
emit::rdf` (or whatever the module path resolves to — check with `cargo
test --lib rdf` if the former doesn't match), verify pass.

- [ ] **Step 7: Run full suite**

Run: `cargo test --lib` (from `etl/pg-collect/`)
Expected: PASS, 792 + 3 new tests, 0 failures.

- [ ] **Step 8: Commit**

```bash
git add etl/pg-collect/src/rpm.rs etl/pg-collect/src/maven.rs etl/pg-collect/src/emit/rdf.rs
git commit -m "feat(pg-collect): link rpm, maven, and generic-IR upstreamRepository writes to the hub

These three collectors write upstreamRepository directly instead of going
through forge::emit_upstream_repo, so they need their own hub-link call."
```

---

### Task 5: Migrate `collect_openwrt_upstream.rs`'s git-source branch to repo-URI keying

**Files:**
- Modify: `etl/pg-collect/src/collect_openwrt_upstream.rs:38-96`
- Test: same file, `mod tests` (lines 103-225)

**Interfaces:**
- Consumes: `forge::extract_forge_url` (existing), `uris::upstream_uri`
  (existing), `uris::repo_uri` (existing), `uris::project_name_from_repo_url`
  (Task 3).

- [ ] **Step 1: Update the existing `test_upstream_project_with_subpackages` assertion**

This test (lines 111-182) uses a git source
(`https://github.com/example/foo.git`), which resolves through
`forge::extract_forge_url` to canonical `https://github.com/example/foo`.
After this migration, `projectName` for this case becomes
`project_name_from_repo_url("https://github.com/example/foo")` =
`"example/foo"`, not the current `effective_name` (`"foo"`). Change:

```rust
// Before:
assert!(content.contains("\"foo\""), "Should use parent name for projectName");

// After:
assert!(
    content.contains("\"example/foo\""),
    "Should derive projectName from the resolved repo URL's owner/repo slug"
);
```

`test_upstream_project_distro_scoped_uri` (lines 185-224, a non-git/
`"default"`-proto source) is **not touched** — it exercises the archive
branch, which this task leaves unchanged.

- [ ] **Step 2: Add a new parity test**

After `test_upstream_project_distro_scoped_uri`, add:

```rust
#[test]
fn test_upstream_project_git_source_keyed_by_repo_not_name() {
    // Confirms the migration: a git source with a resolvable forge URL
    // is keyed the same way any other collector's hub link would be --
    // by canonical repo URL, not by "openwrt/{name}".
    let temp_file = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

    let mut identity_map = HashMap::new();
    let pkg_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/bar/2.0";
    identity_map.insert("bar".to_string(), pkg_uri.to_string());

    let mut parsed_meta = HashMap::new();
    parsed_meta.insert(
        "bar".to_string(),
        OpenWrtPackageMeta {
            source_url: Some("https://github.com/example/bar.git".to_string()),
            source_proto: Some("git".to_string()),
            source_hash: Some("abc123".to_string()),
        },
    );

    let collector = OpenwrtUpstreamCollector::new("openwrt".into(), "24.10".into());
    collector
        .collect(&mut writer, &identity_map, &parsed_meta, &HashMap::new())
        .unwrap();
    writer.flush().unwrap();

    let mut content = String::new();
    temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

    let expected_uri = crate::uris::upstream_uri("https://github.com/example/bar");
    assert!(
        content.contains(&expected_uri),
        "UpstreamProject should be keyed by canonical repo URL, not openwrt/{{name}}"
    );
    assert!(
        !content.contains("upstream/openwrt%2Fbar"),
        "Should NOT use the old per-name key when a forge URL resolves"
    );
}

#[test]
fn test_upstream_project_git_source_unresolvable_keeps_name_keying() {
    // A git source whose URL doesn't match any known forge: falls back to
    // the existing per-name key, exactly as it did before this migration
    // (no projectRepository triple either -- there's nothing to link to).
    let temp_file = NamedTempFile::new().unwrap();
    let mut writer = NTriplesWriter::new(temp_file.reopen().unwrap());

    let mut identity_map = HashMap::new();
    let pkg_uri = "https://packagegraph.github.io/d/pkg/openwrt/24.10/any/baz/1.0";
    identity_map.insert("baz".to_string(), pkg_uri.to_string());

    let mut parsed_meta = HashMap::new();
    parsed_meta.insert(
        "baz".to_string(),
        OpenWrtPackageMeta {
            source_url: Some("https://example-vcs.internal/baz.git".to_string()),
            source_proto: Some("git".to_string()),
            source_hash: Some("def456".to_string()),
        },
    );

    let collector = OpenwrtUpstreamCollector::new("openwrt".into(), "24.10".into());
    collector
        .collect(&mut writer, &identity_map, &parsed_meta, &HashMap::new())
        .unwrap();
    writer.flush().unwrap();

    let mut content = String::new();
    temp_file.reopen().unwrap().read_to_string(&mut content).unwrap();

    assert!(content.contains("upstream/openwrt%2Fbaz"));
    assert!(!content.contains("projectRepository"));
}
```

- [ ] **Step 3: Run tests to verify the new/updated ones fail**

Run: `cargo test --lib collect_openwrt_upstream -- --nocapture` (from
`etl/pg-collect/`)
Expected: `test_upstream_project_with_subpackages` FAILS (still asserts
`"foo"`, current code still emits `"foo"`); the two new tests FAIL to
compile or fail assertions (current code always uses `openwrt/{name}`
keying, so `test_upstream_project_git_source_keyed_by_repo_not_name`'s
"NOT openwrt%2Fbar" assertion currently passes trivially but its "contains
expected_uri" assertion fails).

- [ ] **Step 4: Migrate the collector's `collect()` method**

Replace lines 38-96 (from `if let Some(ref source_url) = meta.source_url {`
through the closing of that block) with:

```rust
if let Some(ref source_url) = meta.source_url {
    // Check if we already created the UpstreamProject for this parent
    let upstream_uri = if let Some(existing_uri) = emitted_upstream.get(effective_name) {
        // Reuse existing UpstreamProject URI
        existing_uri.clone()
    } else {
        // Only git sources can resolve to a canonical repo URL; archive
        // sources have nothing to key on and keep the per-name fallback
        // (this design's explicit non-goal -- see spec §1).
        let forge_extraction = if meta.source_proto.as_deref() == Some("git") {
            crate::forge::extract_forge_url(source_url)
        } else {
            None
        };

        let upstream_uri = match &forge_extraction {
            Some(extraction) => crate::uris::upstream_uri(&extraction.repo_url),
            None => upstream_uri(&format!("openwrt/{}", effective_name)),
        };

        writer.write_triple(
            &upstream_uri,
            RDF_TYPE,
            &format!("{PKG}UpstreamProject"),
        )?;
        total_triples += 1;

        // pkg:projectName (SHACL required)
        let project_name = match &forge_extraction {
            Some(extraction) => crate::uris::project_name_from_repo_url(&extraction.repo_url),
            None => effective_name.clone(),
        };
        writer.write_literal(
            &upstream_uri,
            &format!("{PKG}projectName"),
            &project_name,
        )?;
        total_triples += 1;

        if meta.source_proto.as_deref() == Some("git") {
            // Git source: link to VCS repository if a forge URL resolved.
            // If it didn't, emit nothing further here -- unchanged from
            // pre-migration behavior (no projectRepository, no projectUrl).
            if let Some(extraction) = &forge_extraction {
                let repo_uri = crate::uris::repo_uri(&extraction.repo_url);
                writer.write_triple(
                    &upstream_uri,
                    &format!("{PKG}projectRepository"),
                    &repo_uri,
                )?;
                total_triples += 1;
            }
        } else {
            // Archive sources: emit download URL as projectUrl (unchanged).
            writer.write_literal(
                &upstream_uri,
                &format!("{PKG}projectUrl"),
                source_url,
            )?;
            total_triples += 1;
        }

        emitted_upstream.insert(effective_name.clone(), upstream_uri.clone());
        upstream_uri
    };

    // Link THIS package (parent or sub-package) to the UpstreamProject
    writer.write_triple(
        source_pkg_uri,
        &format!("{PKG}hasUpstreamProject"),
        &upstream_uri,
    )?;
    total_triples += 1;
}
```

(This nests inside the existing `if let Some(meta) = parsed_meta.get(effective_name) {`
block exactly as the original code did — only the content of the inner
`if let Some(ref source_url) = ...` block changes.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib collect_openwrt_upstream -- --nocapture`
Expected: PASS — all 4 tests (2 existing, one updated in Step 1, 2 new from
Step 2).

Run: `cargo test --lib` (from `etl/pg-collect/`)
Expected: PASS, all tests green.

- [ ] **Step 6: Commit**

```bash
git add etl/pg-collect/src/collect_openwrt_upstream.rs
git commit -m "feat(pg-collect): key OpenWrt's git-sourced UpstreamProject nodes by repo URL

Converges OpenWrt with every other collector's hub for the same real
project. Archive-source packages (no resolvable repo URL) keep the
existing openwrt/{name} keying -- there's nothing else to key on."
```

---

### Task 6: Mint `DataSnapshot` in `upload-nt.sh`

**Files:**
- Modify: `etl/scripts/upload-nt.sh` (currently 61 lines, full file already
  read in the spec process)
- Test: manual/shell verification (no existing shell test framework in this
  repo for these scripts — verify by direct invocation)

**Interfaces:**
- Produces: `upload-nt.sh <local-file.nt> <graph-uri> [source-url]` — the
  third argument is new and optional; existing two-argument invocations
  keep working unchanged (Task 7 adds the third argument to scripts that
  have a single canonical source).

- [ ] **Step 1: Write a throwaway verification script**

Before editing `upload-nt.sh`, capture its current behavior so the "after"
can be diffed against it. Create a scratch test file (not committed) to
exercise the change once it's written:

```bash
# Run from etl/scripts/ after Step 2 below, not before.
printf '<https://example.org/s> <https://example.org/p> "o" .\n' > /tmp/pg-test-upload.nt
MINIO_ENDPOINT=unused MINIO_ACCESS_KEY=unused MINIO_SECRET_KEY=unused MINIO_BUCKET=unused \
  bash -c '
    LOCAL_FILE=/tmp/pg-test-upload.nt
    GRAPH_URI="https://packagegraph.github.io/graph/test/example"
    SOURCE_URL="https://example.org/upstream"
    GRAPH_SLUG=$(echo "$GRAPH_URI" | sed "s|https://packagegraph.github.io/graph/||; s|https://packagegraph.github.io/||" | tr "/" "-")
    NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    SNAPSHOT_TIMESTAMP_COMPACT=$(echo "$NOW" | tr -d ":-")
    SNAPSHOT_IRI="https://packagegraph.github.io/d/snapshot/collector/${GRAPH_SLUG}/${SNAPSHOT_TIMESTAMP_COMPACT}"
    {
      printf "<%s> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#DataSnapshot> .\n" "$SNAPSHOT_IRI"
      printf "<%s> <https://purl.org/packagegraph/ontology/core#snapshotGraph> \"%s\"^^<http://www.w3.org/2001/XMLSchema#anyURI> .\n" "$SNAPSHOT_IRI" "$GRAPH_URI"
      printf "<%s> <https://purl.org/packagegraph/ontology/core#snapshotTimestamp> \"%s\"^^<http://www.w3.org/2001/XMLSchema#dateTime> .\n" "$SNAPSHOT_IRI" "$NOW"
      printf "<%s> <https://purl.org/packagegraph/ontology/core#snapshotSource> \"%s\" .\n" "$SNAPSHOT_IRI" "$SOURCE_URL"
    } >> "$LOCAL_FILE"
    cat "$LOCAL_FILE"
  '
```

Run this now, before touching `upload-nt.sh`, purely to confirm the N-Triples
syntax is well-formed shell-side (correct quoting, no unescaped characters)
before it's embedded in the real script. Expected output: the original
triple plus 4 well-formed N-Triples lines appended, no shell syntax errors.
Delete `/tmp/pg-test-upload.nt` after.

- [ ] **Step 2: Update `upload-nt.sh`**

Change the usage comment (lines 4-9) to:

```bash
# Upload an N-Triples file to Minio and register it in the graph manifest.
# Also mints a pkg:DataSnapshot describing this upload, appended directly
# into the file before it's gzipped, so it lands in the same named graph
# as the data it describes.
#
# Usage: upload-nt.sh <local-file.nt> <graph-uri> [source-url]
#
# source-url is optional: omit it for collectors with no single canonical
# upstream source (registry/API-based collectors, or multi-mirror
# collectors with several equally-canonical source URLs).
#
# Example:
#   upload-nt.sh /tmp/packages.nt "https://packagegraph.github.io/graph/debian/trixie" "http://deb.debian.org/debian"
#
# Uploads to: pgraph/${MINIO_BUCKET}/nt-output/debian-trixie.nt.gz
# Creates: pgraph/${MINIO_BUCKET}/nt-output/debian-trixie.nt.gz.graph (sidecar)
```

Change the argument check (lines 14-20) from:

```bash
if [ $# -ne 2 ]; then
    echo "Usage: upload-nt.sh <local-file.nt> <graph-uri>" >&2
    exit 1
fi

LOCAL_FILE="$1"
GRAPH_URI="$2"
```

to:

```bash
if [ $# -lt 2 ] || [ $# -gt 3 ]; then
    echo "Usage: upload-nt.sh <local-file.nt> <graph-uri> [source-url]" >&2
    exit 1
fi

LOCAL_FILE="$1"
GRAPH_URI="$2"
SOURCE_URL="${3:-}"
```

After the existing `GRAPH_SLUG=...` line (line 31) and before the `echo
"=== Uploading N-Triples to Minio ==="` block, insert:

```bash
# Append a DataSnapshot describing this graph, before gzip/upload, so it
# lands in the same named graph as the rest of this file's content --
# whichever named graph this .nt file's triples are loaded into is the
# same graph these triples describe.
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
SNAPSHOT_TIMESTAMP_COMPACT=$(echo "$NOW" | tr -d ':-')
SNAPSHOT_IRI="https://packagegraph.github.io/d/snapshot/collector/${GRAPH_SLUG}/${SNAPSHOT_TIMESTAMP_COMPACT}"
{
  printf '<%s> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://purl.org/packagegraph/ontology/core#DataSnapshot> .\n' "$SNAPSHOT_IRI"
  # snapshotGraph is owl:DatatypeProperty, range xsd:anyURI (core.ttl:823-829)
  # -- a typed literal, NOT an IRI reference. Getting this backwards makes
  # the join query in the design spec's §5 silently return zero rows (an
  # IRI and a literal never test equal in SPARQL, even with identical
  # string content).
  printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotGraph> "%s"^^<http://www.w3.org/2001/XMLSchema#anyURI> .\n' "$SNAPSHOT_IRI" "$GRAPH_URI"
  printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotTimestamp> "%s"^^<http://www.w3.org/2001/XMLSchema#dateTime> .\n' "$SNAPSHOT_IRI" "$NOW"
  if [ -n "$SOURCE_URL" ]; then
    printf '<%s> <https://purl.org/packagegraph/ontology/core#snapshotSource> "%s" .\n' "$SNAPSHOT_IRI" "$SOURCE_URL"
  fi
} >> "$LOCAL_FILE"
```

- [ ] **Step 3: Manually verify the updated script**

Run (from the repo root, does not require real Minio credentials since it
fails at the `mc alias set` step, which is fine — the snapshot-append logic
runs before that in the file order, but to test it in isolation, extract
just the new block or run the whole script and inspect the file before it
errors out on the Minio call):

```bash
cp /home/bharring/scratch-pg-collect/example.nt /tmp/pg-verify.nt 2>/dev/null || printf '<https://example.org/s> <https://example.org/p> "o" .\n' > /tmp/pg-verify.nt
MINIO_ENDPOINT=unused MINIO_ACCESS_KEY=unused MINIO_SECRET_KEY=unused MINIO_BUCKET=unused \
  bash etl/scripts/upload-nt.sh /tmp/pg-verify.nt "https://packagegraph.github.io/graph/test/example" "https://example.org/upstream" 2>&1 | head -20
cat /tmp/pg-verify.nt
rm -f /tmp/pg-verify.nt /tmp/pg-verify.nt.gz
```

Expected: the script prints its normal "Uploading N-Triples to Minio"
banner, then fails at the `mc alias set` step (no real Minio) — that
failure is expected and fine. `/tmp/pg-verify.nt` must contain the original
triple plus exactly 4 well-formed appended triples: `rdf:type
DataSnapshot`, a quoted `xsd:anyURI`-typed `snapshotGraph` literal (not an
`<...>` IRI), a quoted `xsd:dateTime`-typed `snapshotTimestamp`, and a
quoted plain-literal `snapshotSource`.

Re-run once more omitting the third argument (two-argument form) and
confirm the output has only 3 appended triples (no `snapshotSource` line,
and no empty-string literal in its place).

- [ ] **Step 4: Commit**

```bash
git add etl/scripts/upload-nt.sh
git commit -m "feat(etl): mint a pkg:DataSnapshot for every graph published via upload-nt.sh

DataSnapshot minting has to happen here rather than in the Rust collectors:
production invocations never pass --graph to pg-collect (verified across
all 41 deployed collector scripts), so upload-nt.sh is the only place in
the pipeline that reliably has the graph URI."
```

---

### Task 7: Pass a source URL to `upload-nt.sh` from collector scripts that have one

**Files:**
- Modify: `deploy/quadlet/collectors/scripts/conda.sh`,
  `alpine-v320.sh`, `alpine-v320-aarch64.sh`, `alpine-edge-riscv64.sh`,
  `archarm-aarch64.sh`, `gentoo.sh`, `debian-trixie-full.sh`,
  `debian-trixie-arm64.sh`, `debian-trixie-riscv64.sh`, `ubuntu-noble.sh`,
  `ubuntu-noble-arm64.sh`, `ubuntu-noble-riscv64.sh`, `void.sh` (verify each
  against the rule below — some may turn out multi-source on inspection)
- Verify (no change expected, confirm and move on): `npm.sh`, `cargo.sh`,
  `cpan.sh`, `hackage.sh`, `pypi.sh`, `nuget.sh`, `hex.sh`, `rubygems.sh`,
  `gomod.sh`, `maven.sh`, `chocolatey.sh`, `homebrew.sh`, `cran.sh`,
  `flatpak.sh`, `nix.sh`, `snap.sh`, `freebsd.sh`, `arch.sh`, `osv.sh`,
  `alma-9-full.sh`, `alma-10-full.sh`, `rocky-9-full.sh`, `rocky-10-full.sh`,
  `centos-stream-9-full.sh`, `centos-stream-10-full.sh`, `fedora-43-full.sh`,
  `fedora-44-full.sh`, `openwrt-2410.sh`

**Interfaces:**
- Consumes: `upload-nt.sh`'s new optional third argument (Task 6).

**The rule** (apply to each of the 41 scripts in
`deploy/quadlet/collectors/scripts/`):

1. Read the script's `pg-collect ...` invocation.
2. If it passes exactly **one** external, non-PackageGraph URL via a flag
   (`--mirror`, `--channel-url`, a single `--url`, `--repo` used once) —
   or the script does a `curl`/similar fetch of a single tarball/archive
   URL before invoking `pg-collect` with a local path — pass that same URL
   string as `upload-nt.sh`'s new third argument.
3. If it passes `--endpoint "$FUSEKI_ENDPOINT"` (PackageGraph's own SPARQL
   endpoint — not an upstream source) or no URL-shaped flag at all
   (registry collectors with a hardcoded default inside the Rust binary):
   omit the third argument. There is no single external source to record.
4. If it passes **two or more** URL/mirror-style values (e.g. one `--url`
   per architecture): omit the third argument. Per the design spec, this is
   a real, accepted gap — don't invent an artificial "primary" mirror.

**Already-verified examples** (apply directly, no further investigation
needed for these 5):

| Script | Rule | Source URL to add |
|---|---|---|
| `conda.sh` | 1 (single `--channel-url`) | `https://conda.anaconda.org/conda-forge` |
| `alpine-v320.sh` | 1 (single `--mirror`) | `https://dl-cdn.alpinelinux.org/alpine` |
| `archarm-aarch64.sh` | 1 (single `--mirror`) | `http://fl.us.mirror.archlinuxarm.org/aarch64` |
| `gentoo.sh` | 1 (single `curl` tarball before `--repo-path`) | `https://github.com/gentoo/gentoo/archive/refs/heads/master.tar.gz` |
| `debian-trixie-full.sh` | 1 (single `--repo`, two `--arch` flags but one repo) | `http://deb.debian.org/debian` |

**Already-verified omissions** (confirm no third argument needed; these 20
don't need editing, just checking off — already read during plan
preparation):

`npm.sh`, `cargo.sh`, `cpan.sh`, `hackage.sh`, `pypi.sh`, `nuget.sh`,
`hex.sh`, `rubygems.sh`, `gomod.sh`, `maven.sh` (all `--endpoint
"$FUSEKI_ENDPOINT"`); `chocolatey.sh`, `homebrew.sh`, `cran.sh`,
`flatpak.sh`, `nix.sh`, `snap.sh`, `freebsd.sh`, `arch.sh` (no URL flag);
`osv.sh` (explicitly multi-ecosystem/multi-source, documented in its own
comments); `alma-9-full.sh` (two `--url` values, one per arch).

**Not yet checked** — apply the rule fresh to each (don't assume family
membership from the name; `debian-trixie-full.sh` and `alma-9-full.sh`
looked like they'd be the same shape and turned out different — one
`--repo` argument shared across `--arch` flags vs. one `--url` per arch):
`alpine-v320-aarch64.sh`, `alpine-edge-riscv64.sh`, `debian-trixie-arm64.sh`,
`debian-trixie-riscv64.sh`, `ubuntu-noble.sh`, `ubuntu-noble-arm64.sh`,
`ubuntu-noble-riscv64.sh`, `void.sh`, `alma-10-full.sh`, `rocky-9-full.sh`,
`rocky-10-full.sh`, `centos-stream-9-full.sh`, `centos-stream-10-full.sh`,
`fedora-43-full.sh`, `fedora-44-full.sh`, `openwrt-2410.sh`.

- [ ] **Step 1: Apply the rule to each of the 16 not-yet-checked scripts**

For each, `grep -n "pg-collect\|upload-nt.sh"` the file, classify per the
rule above, and if it qualifies, change its `upload-nt.sh` call from:

```bash
/app/scripts/upload-nt.sh /tmp/collection/whatever.nt "$GRAPH_URI"
```

to:

```bash
/app/scripts/upload-nt.sh /tmp/collection/whatever.nt "$GRAPH_URI" "<the-source-url>"
```

- [ ] **Step 2: Apply the rule to the 5 already-verified single-source scripts**

Make the same one-line change for `conda.sh`, `alpine-v320.sh`,
`archarm-aarch64.sh`, `gentoo.sh`, and `debian-trixie-full.sh`, using the
exact URLs from the table above.

- [ ] **Step 3: Spot-check with `shellcheck` or `sh -n`**

For every script touched in Steps 1-2:

```bash
sh -n deploy/quadlet/collectors/scripts/<script>.sh
```

Expected: no syntax errors (this only parses the script, doesn't run it —
safe to run against all of them, including ones with real credentials
expected in the environment).

- [ ] **Step 4: Commit**

```bash
git add deploy/quadlet/collectors/scripts/
git commit -m "feat(quadlet): pass upstream source URL to upload-nt.sh where one exists

Populates pkg:snapshotSource for collectors with a single canonical
upstream endpoint. Collectors with no single source (registry/API-based,
or multiple mirrors) are left as-is -- upload-nt.sh treats the argument as
optional for exactly this reason."
```

---

## Final check

- [ ] Run `cargo test --lib` from `etl/pg-collect/` one more time — confirm
  the full suite is green (775 original + ~15 new tests from Tasks 1-5).
- [ ] Run `cargo clippy --lib` from `etl/pg-collect/` — confirm no new
  warnings introduced by Tasks 1-5's changes.
- [ ] Confirm no file under `etl/ontology/` was touched (this design uses
  zero new ontology terms — if `git diff` shows anything there, that's a
  scope violation, not an artifact of this plan).

**Not a task in this plan — a follow-up once deployed:** the spec's §8
calls for live verification after a real `qlever-rebuild-index` cycle picks
up graphs published with these changes: run the join query from spec §5
against production QLever for a project known to be packaged in two
different ecosystems, confirming both resolve to one `UpstreamProject` node
with correctly-typed, distinctly-timestamped `DataSnapshot`s per graph. This
can't happen until Tasks 1-7 are merged, deployed, and at least one
scheduled collector run has published through the updated pipeline.
