//! Run-generation lifecycle for output checkpoints.
//!
//! A generation scopes checkpoints to one in-flight run: retries of an
//! interrupted run reuse it, and a successful publication retires it. Without
//! this, checkpoints would be reused across every future scheduled run,
//! permanently bypassing spec re-fetch and signatures that appear after a
//! build was first observed.
//!
//! Note that the Koji `FileCache`'s nominal 30-day TTL is not a second line of
//! defence here: it is not enforced for Minio-backed entries (`cache.rs:295`
//! never checks age).
//!
//! **A generation is only retired by an explicit `pg-collect checkpoint
//! commit`**, which the rpm-full wrappers run after a successful upload. A
//! deployment that enables checkpointing without that call leaves a generation
//! active forever, and its fragments replay across every later scheduled run.
//! Repository tests validate the wrapper contract; they do not update the
//! host-mounted scripts when an image changes. Use the coordinated, pinned
//! rollout in `deploy/quadlet/collectors/checkpoint-cutover.md`.
//!
//! `MAX_GENERATION_AGE_DAYS` is the backstop for when that rollout is not
//! followed. It does not prevent the mispairing -- only the cutover does --
//! but it bounds the damage to one stale cycle and logs the cause, instead of
//! replaying silently forever.

use serde::{Deserialize, Serialize};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// `deny_unknown_fields` is load-bearing, not stylistic: the design requires
/// state we cannot structurally authenticate to be discarded rather than
/// trusted, and serde accepts unknown fields by default.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    id: String,
    status: String,
}

/// The generation this run should use, and how we came by it.
pub struct AcquiredGeneration {
    pub id: String,
    /// True when we resumed an interrupted run's generation rather than
    /// minting. Logged, so an operator can tell a resume from a fresh start.
    pub reused: bool,
}

/// Longest a generation may be reused before it is treated as stuck.
///
/// Not a retry budget: rpm-full collectors run weekly, so a legitimate resume
/// of a run that hit its 8h timeout lands 7 days later, and a second at 14.
/// The bound exists because an active generation that is never committed
/// replays its fragments forever -- silently bypassing spec re-fetch and
/// signatures that appear after a build was first observed. 30 days leaves
/// room for several real resume attempts while capping how stale a replayed
/// fragment can be at the Koji `FileCache`'s nominal TTL, so no derived
/// output outlives the source it was derived from.
pub const MAX_GENERATION_AGE_DAYS: i64 = 30;

pub struct Generation;

impl Generation {
    /// Ids are `<UTC YYYYMMDDTHHMMSSZ>-<8 hex>`. The timestamp is for human
    /// legibility only; uniqueness comes from the random suffix plus the
    /// exclusive directory create in `acquire`.
    ///
    /// Delegates to `output_cache`, which owns the format because it must
    /// reject an unsafe path component without depending on this module.
    pub fn is_valid_id(id: &str) -> bool {
        crate::output_cache::is_valid_generation_id(id)
    }

    fn output_dir(cache_dir: &Path) -> PathBuf {
        cache_dir.join("output")
    }

    fn state_path(cache_dir: &Path) -> PathBuf {
        Self::output_dir(cache_dir).join("GENERATION")
    }

    /// Reads state only if it is structurally authenticatable. Anything else --
    /// unparseable, unknown status, an id that fails validation -- returns None
    /// so the caller mints fresh rather than trusting it.
    fn read_state(cache_dir: &Path) -> Option<State> {
        let path = Self::state_path(cache_dir);
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None, // first run
            Err(e) => {
                eprintln!("Warning: cannot read checkpoint state {}: {}", path.display(), e);
                return None;
            }
        };
        // Every invalidation is logged: a silently discarded state looks
        // identical to a first run, which would hide a recurring corruption.
        let s: State = match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Warning: ignoring unparseable checkpoint state (or unknown fields): {e}");
                return None;
            }
        };
        if !Self::is_valid_id(&s.id) {
            eprintln!("Warning: ignoring checkpoint state with invalid generation id");
            return None;
        }
        if s.status != "active" && s.status != "complete" {
            eprintln!("Warning: ignoring checkpoint state with unknown status {:?}", s.status);
            return None;
        }
        Some(s)
    }

    fn write_state(cache_dir: &Path, state: &State) -> io::Result<()> {
        let dir = Self::output_dir(cache_dir);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".GENERATION.{}.tmp", std::process::id()));
        let body = serde_json::to_vec(state)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, Self::state_path(cache_dir))
    }

    /// Absolute distance between the instant named by `id`'s timestamp prefix
    /// and now. `None` when the prefix names no real instant -- `is_valid_id`
    /// is purely structural, so a shape-valid id can still be unparseable, and
    /// an unparseable age cannot be bounded.
    ///
    /// Returns a duration rather than a day count because callers must compare
    /// against the bound *before* truncating: `num_days` rounds toward zero, so
    /// a stamp 31 days away is reported as 30 once sub-second clock drift eats
    /// into it, and an over-bound generation slips through as in-bound.
    fn age(id: &str) -> Option<chrono::TimeDelta> {
        let stamp = id.get(..16)?;
        let naive = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ").ok()?;
        Some((chrono::Utc::now() - naive.and_utc()).abs())
    }

    fn mint_id() -> String {
        let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        // The crate has no `rand` dependency by design; seed from RandomState
        // the same way http_transport.rs does for its jitter.
        let mut h = RandomState::new().build_hasher();
        h.write_u64(std::process::id() as u64);
        format!("{}-{:08x}", now, (h.finish() & 0xffff_ffff) as u32)
    }

    /// Returns the generation id this run should use.
    ///
    /// Reuses an `active` generation (a retry of an interrupted run); otherwise
    /// mints a new one and prunes every other generation directory. Pruning at
    /// mint time, not at commit, means a crash between publication and commit
    /// leaves the prior generation intact -- costing one redundant
    /// re-collection rather than losing data.
    pub fn acquire(cache_dir: &Path) -> io::Result<AcquiredGeneration> {
        if let Some(s) = Self::read_state(cache_dir) {
            if s.status == "active" && Self::output_dir(cache_dir).join(&s.id).is_dir() {
                // Falling through to mint is the safe direction: it costs one
                // redundant re-collection, where reusing indefinitely costs
                // correctness with no signal that anything is wrong.
                let bound = chrono::TimeDelta::days(MAX_GENERATION_AGE_DAYS);
                match Self::age(&s.id) {
                    Some(age) if age <= bound => {
                        return Ok(AcquiredGeneration { id: s.id, reused: true });
                    }
                    Some(age) => eprintln!(
                        "Warning: checkpoint generation {} is {} days old (bound is {}); \
                         re-deriving instead of replaying. An uncommitted generation this \
                         old usually means the wrappers are not running `checkpoint commit` \
                         -- see deploy/quadlet/collectors/checkpoint-cutover.md",
                        s.id, age.num_days(), MAX_GENERATION_AGE_DAYS
                    ),
                    None => eprintln!(
                        "Warning: checkpoint generation {} has an unparseable timestamp; \
                         re-deriving instead of replaying",
                        s.id
                    ),
                }
            }
        }

        // Exclusive create is the uniqueness authority: if the directory
        // already exists we lost a race (or collided), so re-mint.
        let dir = Self::output_dir(cache_dir);
        std::fs::create_dir_all(&dir)?;
        let mut id = String::new();
        for _ in 0..64 {
            let candidate = Self::mint_id();
            match std::fs::create_dir(dir.join(&candidate)) {
                Ok(()) => {
                    id = candidate;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        if id.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "could not mint a unique checkpoint generation id",
            ));
        }

        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_name() != std::ffi::OsStr::new(&id) && entry.path().is_dir() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }

        Self::write_state(cache_dir, &State { id: id.clone(), status: "active".into() })?;
        Ok(AcquiredGeneration { id, reused: false })
    }

    /// Marks the active generation complete. Called only after a successful
    /// publication, so a run that collected everything but failed to publish
    /// stays resumable.
    pub fn commit(cache_dir: &Path) -> io::Result<()> {
        let s = Self::read_state(cache_dir).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no valid checkpoint generation to commit")
        })?;
        Self::write_state(cache_dir, &State { id: s.id, status: "complete".into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fresh_dir_mints_a_valid_generation() {
        let d = TempDir::new().unwrap();
        let g = Generation::acquire(d.path()).unwrap();
        assert!(Generation::is_valid_id(&g.id), "minted id must be valid: {}", g.id);
        assert!(d.path().join("output").join(&g.id).is_dir());
    }

    #[test]
    fn an_active_generation_is_reused() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        let b = Generation::acquire(d.path()).unwrap().id;
        assert_eq!(a, b, "an interrupted run must resume its own generation");
    }

    #[test]
    fn commit_then_acquire_mints_a_new_generation_and_prunes_the_old() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        std::fs::write(d.path().join("output").join(&a).join("leftover"), b"x").unwrap();
        Generation::commit(d.path()).unwrap();
        let b = Generation::acquire(d.path()).unwrap().id;
        assert_ne!(a, b);
        assert!(!d.path().join("output").join(&a).exists(), "old generation must be pruned");
        assert!(d.path().join("output").join(&b).is_dir());
    }

    #[test]
    fn two_mints_in_the_same_second_do_not_collide() {
        // The collision a bare one-second timestamp would allow: a completing
        // run and a new invocation would share a directory, and pruning would
        // preserve the old fragments as if they were the new run's.
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        Generation::commit(d.path()).unwrap();
        let b = Generation::acquire(d.path()).unwrap().id;
        Generation::commit(d.path()).unwrap();
        let c = Generation::acquire(d.path()).unwrap().id;
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn malformed_state_mints_fresh_instead_of_reusing() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        std::fs::write(d.path().join("output").join("GENERATION"), b"not json at all").unwrap();
        let b = Generation::acquire(d.path()).unwrap();
        assert_ne!(a, b.id, "unauthenticatable state must not be reused");
        assert!(Generation::is_valid_id(&b.id));
    }

    #[test]
    fn a_traversal_id_in_state_is_rejected() {
        let d = TempDir::new().unwrap();
        Generation::acquire(d.path()).unwrap();
        std::fs::write(
            d.path().join("output").join("GENERATION"),
            br#"{"id":"../../etc","status":"active"}"#,
        ).unwrap();
        let g = Generation::acquire(d.path()).unwrap();
        assert!(Generation::is_valid_id(&g.id));
        assert!(!g.id.contains(".."), "id is used as a path component");
    }

    #[test]
    fn id_validation_rejects_dangerous_and_malformed_values() {
        assert!(Generation::is_valid_id("20260912T010203Z-abcdef01"));
        for bad in ["", ".", "..", "../x", "a/b", "20260912T010203Z", "20260912T010203Z-ABCDEF01",
                    "20260912T010203Z-abcdef0", "20260912T010203Z-abcdef012"] {
            assert!(!Generation::is_valid_id(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn commit_without_a_generation_is_an_error_not_a_panic() {
        let d = TempDir::new().unwrap();
        assert!(Generation::commit(d.path()).is_err());
    }

    #[test]
    fn unknown_fields_in_state_invalidate_it() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        std::fs::write(
            d.path().join("output").join("GENERATION"),
            format!(r#"{{"id":"{a}","status":"active","injected":"surprise"}}"#),
        ).unwrap();
        let b = Generation::acquire(d.path()).unwrap();
        assert_ne!(a, b.id, "state with unknown fields must not be trusted");
    }

    #[test]
    fn unknown_status_invalidates_state() {
        let d = TempDir::new().unwrap();
        let a = Generation::acquire(d.path()).unwrap().id;
        std::fs::write(
            d.path().join("output").join("GENERATION"),
            format!(r#"{{"id":"{a}","status":"halfway"}}"#),
        ).unwrap();
        let b = Generation::acquire(d.path()).unwrap();
        assert_ne!(a, b.id);
    }

    /// Plants an `active` state whose generation directory exists, dated
    /// `days_ago` before now, and returns its id.
    fn plant_aged_generation(dir: &Path, days_ago: i64) -> String {
        let stamp = (chrono::Utc::now() - chrono::Duration::days(days_ago))
            .format("%Y%m%dT%H%M%SZ");
        let id = format!("{stamp}-abcdef01");
        std::fs::create_dir_all(dir.join("output").join(&id)).unwrap();
        std::fs::write(
            dir.join("output").join("GENERATION"),
            format!(r#"{{"id":"{id}","status":"active"}}"#),
        ).unwrap();
        id
    }

    #[test]
    fn a_legitimate_weekly_resume_chain_still_reuses_its_generation() {
        // rpm-full collectors run weekly, so the first resume of a run that
        // hit its 8h timeout lands a full 7 days later, and a second lands at
        // 14. A staleness bound that rejected those would defeat the feature.
        let d = TempDir::new().unwrap();
        for days_ago in [7, 14, 21] {
            let id = plant_aged_generation(d.path(), days_ago);
            let g = Generation::acquire(d.path()).unwrap();
            assert_eq!(g.id, id, "a {days_ago}-day-old resume must still be reused");
            assert!(g.reused);
        }
    }

    #[test]
    fn a_generation_older_than_the_staleness_bound_is_not_reused() {
        // The failure this bounds: a host whose image auto-updated onto
        // wrappers that never run `checkpoint commit`. The generation stays
        // active forever and replays its fragments on every later run, so
        // spec re-fetch and late-appearing signatures are bypassed silently
        // and indefinitely. Minting instead costs one re-collection.
        let d = TempDir::new().unwrap();
        let stale = plant_aged_generation(d.path(), MAX_GENERATION_AGE_DAYS + 1);
        let g = Generation::acquire(d.path()).unwrap();
        assert_ne!(g.id, stale, "a generation past the bound must not be reused");
        assert!(!g.reused);
        assert!(!d.path().join("output").join(&stale).exists(), "and must be pruned");
    }

    #[test]
    fn a_shape_valid_but_impossible_timestamp_is_not_reused() {
        // is_valid_id is purely structural, so this id passes it while naming
        // no real instant. Unparseable means unbounded, so it must not reuse.
        let d = TempDir::new().unwrap();
        let id = "20269999T999999Z-abcdef01";
        assert!(Generation::is_valid_id(id));
        std::fs::create_dir_all(d.path().join("output").join(id)).unwrap();
        std::fs::write(
            d.path().join("output").join("GENERATION"),
            format!(r#"{{"id":"{id}","status":"active"}}"#),
        ).unwrap();
        let g = Generation::acquire(d.path()).unwrap();
        assert_ne!(g.id, id);
        assert!(!g.reused);
    }

    #[test]
    fn a_far_future_generation_is_not_reused_either() {
        // Clock skew or a tampered state file. Age is measured absolutely so
        // a future stamp cannot buy unbounded reuse.
        let d = TempDir::new().unwrap();
        let future = plant_aged_generation(d.path(), -(MAX_GENERATION_AGE_DAYS + 1));
        let g = Generation::acquire(d.path()).unwrap();
        assert_ne!(g.id, future);
        assert!(!g.reused);
    }

    #[test]
    fn acquire_reports_whether_it_reused_or_minted() {
        let d = TempDir::new().unwrap();
        let first = Generation::acquire(d.path()).unwrap();
        assert!(!first.reused, "a fresh directory mints");
        let second = Generation::acquire(d.path()).unwrap();
        assert!(second.reused, "an active generation is reused");
        Generation::commit(d.path()).unwrap();
        let third = Generation::acquire(d.path()).unwrap();
        assert!(!third.reused, "a committed generation is retired, not reused");
    }
}
