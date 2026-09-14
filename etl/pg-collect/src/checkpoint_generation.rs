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
//! never checks age). The generation is the only thing bounding reuse.
//!
//! **A generation is only retired by an explicit `pg-collect checkpoint
//! commit`**, which the rpm-full wrappers run after a successful upload. A
//! deployment that enables checkpointing without that call leaves a generation
//! active forever, and its fragments replay across every later scheduled run.
//! `deploy/quadlet/collectors/scripts/test-wrapper-checkpoint-contract.sh`
//! is what keeps the two in step.

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
                return Ok(AcquiredGeneration { id: s.id, reused: true });
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
