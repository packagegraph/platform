//! Per-item derived-output checkpoints, so an interrupted collector run
//! resumes instead of restarting. Distinct from `source_cache.rs` (raw
//! upstream artifacts) and `cache.rs` (TTL'd API responses): this stores the
//! *triples we derived*, keyed by item + declared context + run generation.
//!
//! Local-only by design -- never synced to Minio. See the design spec at
//! docs/superpowers/specs/2026-09-12-collector-output-cache-resumability-design.md

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Serialization format of the on-disk envelope. Distinct from the stages'
/// RDF schema versions: bump this when the envelope layout changes, so a
/// serialization change does not force every adopter to bump its own
/// schema version. An unknown value is treated as a cache miss.
pub const OUTPUT_CACHE_FORMAT_VERSION: u32 = 1;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generation ids are `<UTC YYYYMMDDTHHMMSSZ>-<8 lowercase hex>`.
///
/// Defined here rather than in `checkpoint_generation` because this module is
/// the lower-level primitive: it must be able to reject an unsafe path
/// component without depending on the module that mints them.
/// `Generation::is_valid_id` delegates to this.
pub fn is_valid_generation_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.len() != 25 {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(|c| c.is_ascii_digit());
    digits(0..8)
        && b[8] == b'T'
        && digits(9..15)
        && b[15] == b'Z'
        && b[16] == b'-'
        && b[17..].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

/// One item's derived output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedOutput {
    /// The caller's logical tally. NOT derivable from `text`: write_triple
    /// auto-emits inverse statements, so output lines exceed logical writes
    /// by `auto_inverses` (see ntriples.rs).
    pub logical_triples: usize,
    /// Parent-writer counters, restored on replay rather than recomputed.
    pub skipped_invalid_iri: usize,
    pub auto_inverses: usize,
    /// Canonical N-Triples, no graph term. A String, so UTF-8 validity is
    /// settled once at capture rather than on every replay.
    pub text: String,
}

/// Whether a computation may be checkpointed. Never inferred from `Err`, and
/// never from whether a DQ issue was emitted -- a successful ecosystem
/// detection deliberately emits one (collect_spec.rs), so DQ emission says
/// nothing about transience.
pub enum ComputeOutcome {
    /// Deterministic result from a valid upstream response. Persist and replay.
    Complete(CachedOutput),
    /// Operationally inconclusive (transport error, API fault, malformed
    /// response). Use for this run's output, but never persist.
    Retryable(CachedOutput),
}

/// A stage's declared cache-context inputs. The stage owns *what* goes in;
/// this type owns *how* it is encoded, so two stages cannot disagree.
///
/// Encoding is length-prefixed per field (`<u32 len><bytes>` for both name and
/// value), which makes it injective: ("ab","c") and ("a","bc") cannot collide.
#[derive(Debug, Default, Clone)]
pub struct CanonicalContext {
    buf: Vec<u8>,
}

impl CanonicalContext {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// `tag` domain-separates the variants: without it `field("x", "0")` and
    /// `flag("x", false)` would encode identically, so swapping one for the
    /// other during a refactor would silently reuse the other's checkpoints.
    fn push(mut self, tag: u8, name: &str, value: &str) -> Self {
        self.buf.push(tag);
        self.buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(value.as_bytes());
        self
    }

    pub fn field(self, name: &str, value: &str) -> Self {
        self.push(b'F', name, value)
    }

    /// Booleans encode as "0"/"1" explicitly, never via Display -- a future
    /// type change must not be able to alter the encoding silently.
    pub fn flag(self, name: &str, value: bool) -> Self {
        self.push(b'B', name, if value { "1" } else { "0" })
    }

    /// Sorted before hashing, so element order is not significant, and
    /// length-prefixed element-wise so contents still are.
    pub fn list(mut self, name: &str, values: &[String]) -> Self {
        let mut sorted: Vec<&String> = values.iter().collect();
        sorted.sort();
        self = self.push(b'L', name, "");
        self.buf.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
        for v in sorted {
            self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(v.as_bytes());
        }
        self
    }

    pub fn digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(&self.buf);
        h.finalize().into()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub retryable: u64,
    pub write_failures: u64,
    pub integrity_failures: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: u32,
    logical_triples: usize,
    skipped_invalid_iri: usize,
    auto_inverses: usize,
    /// Hex SHA-256 over *every* field of the CachedOutput, not just `text`.
    /// The counters are restored into the parent writer and drive reported
    /// totals, so a readable corruption of one of them must invalidate the
    /// entry too. `fs::read` succeeding does not mean these are the bytes we
    /// wrote.
    digest: String,
    len: usize,
    text: String,
}

/// Canonical digest of a whole `CachedOutput`, length-prefixed so the
/// counters cannot be confused with each other or with the text.
fn output_digest(o: &CachedOutput) -> String {
    let mut h = Sha256::new();
    h.update((o.logical_triples as u64).to_le_bytes());
    h.update((o.skipped_invalid_iri as u64).to_le_bytes());
    h.update((o.auto_inverses as u64).to_le_bytes());
    h.update((o.text.len() as u64).to_le_bytes());
    h.update(o.text.as_bytes());
    hex(&h.finalize())
}

pub struct OutputCache {
    /// `None` when disabled (no --cache-dir): compute-through, no filesystem.
    base_dir: Option<PathBuf>,
    hits: Cell<u64>,
    misses: Cell<u64>,
    retryable: Cell<u64>,
    write_failures: Cell<u64>,
    integrity_failures: Cell<u64>,
}

impl OutputCache {
    pub fn new(
        cache_dir: &Path,
        generation: &str,
        stage: &str,
        schema_version: &str,
    ) -> io::Result<Self> {
        // This is a shared public primitive, so it enforces its own path
        // invariant rather than trusting every caller to have validated.
        if !is_valid_generation_id(generation) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid checkpoint generation id: {generation:?}"),
            ));
        }
        for (label, part) in [("stage", stage), ("schema_version", schema_version)] {
            let bad = part.is_empty()
                || part == "."
                || part == ".."
                || part.contains('/')
                || part.contains('\\');
            if bad {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{label} must be a single safe path component, got {part:?}"),
                ));
            }
        }

        let base = cache_dir
            .join("output")
            .join(generation)
            .join(stage)
            .join(schema_version);
        std::fs::create_dir_all(&base)?;
        Ok(Self {
            base_dir: Some(base),
            hits: Cell::new(0),
            misses: Cell::new(0),
            retryable: Cell::new(0),
            write_failures: Cell::new(0),
            integrity_failures: Cell::new(0),
        })
    }

    pub fn disabled() -> Self {
        Self {
            base_dir: None,
            hits: Cell::new(0),
            misses: Cell::new(0),
            retryable: Cell::new(0),
            write_failures: Cell::new(0),
            integrity_failures: Cell::new(0),
        }
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.get(),
            misses: self.misses.get(),
            retryable: self.retryable.get(),
            write_failures: self.write_failures.get(),
            integrity_failures: self.integrity_failures.get(),
        }
    }

    /// `sha256(sha256(context) || sha256(item_key))` over fixed-width binary
    /// digests -- field boundaries are unambiguous, unlike raw concatenation.
    pub fn entry_path(&self, item_key: &str, context: &CanonicalContext) -> Option<PathBuf> {
        let base = self.base_dir.as_ref()?;
        let mut item = Sha256::new();
        item.update(item_key.as_bytes());
        let item: [u8; 32] = item.finalize().into();

        let mut outer = Sha256::new();
        outer.update(context.digest());
        outer.update(item);
        let digest: [u8; 32] = outer.finalize().into();
        Some(base.join(hex(&digest)))
    }

    pub fn temp_path(&self) -> PathBuf {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".tmp.{}.{}", std::process::id(), n);
        match &self.base_dir {
            Some(b) => b.join(name),
            None => PathBuf::from(name),
        }
    }

    pub fn get_or_compute<F>(
        &self,
        item_key: &str,
        context: &CanonicalContext,
        compute: F,
    ) -> io::Result<CachedOutput>
    where
        F: FnOnce() -> io::Result<ComputeOutcome>,
    {
        let path = match self.entry_path(item_key, context) {
            Some(p) => p,
            // Disabled: compute-through, no filesystem, no counters beyond misses.
            None => {
                self.misses.set(self.misses.get() + 1);
                return Ok(match compute()? {
                    ComputeOutcome::Complete(o) | ComputeOutcome::Retryable(o) => o,
                });
            }
        };

        if let Some(hit) = self.read_entry(&path) {
            self.hits.set(self.hits.get() + 1);
            return Ok(hit);
        }
        self.misses.set(self.misses.get() + 1);

        match compute()? {
            ComputeOutcome::Retryable(o) => {
                self.retryable.set(self.retryable.get() + 1);
                Ok(o)
            }
            ComputeOutcome::Complete(o) => {
                // Best-effort: a persistence failure must never discard
                // derived triples or fail the run.
                if let Err(e) = self.write_entry(&path, &o) {
                    self.write_failures.set(self.write_failures.get() + 1);
                    eprintln!("Warning: checkpoint write failed for {}: {}", path.display(), e);
                }
                Ok(o)
            }
        }
    }

    /// `None` on any of: absent, unreadable, unparseable, unknown format
    /// version, length mismatch, digest mismatch. A corrupt entry is evicted
    /// so it cannot poison later lookups (mirrors source_cache.rs's
    /// self-healing manifest read).
    fn read_entry(&self, path: &Path) -> Option<CachedOutput> {
        let raw = std::fs::read_to_string(path).ok()?;
        let bad = |s: &Self| {
            s.integrity_failures.set(s.integrity_failures.get() + 1);
            let _ = std::fs::remove_file(path);
            None::<CachedOutput>
        };
        let env: Envelope = match serde_json::from_str(&raw) {
            Ok(e) => e,
            Err(_) => return bad(self),
        };
        if env.format != OUTPUT_CACHE_FORMAT_VERSION || env.len != env.text.len() {
            return bad(self);
        }
        let out = CachedOutput {
            logical_triples: env.logical_triples,
            skipped_invalid_iri: env.skipped_invalid_iri,
            auto_inverses: env.auto_inverses,
            text: env.text,
        };
        if output_digest(&out) != env.digest {
            return bad(self);
        }
        Some(out)
    }

    /// Temp file + fsync + rename, so an interrupted write can never leave a
    /// truncated entry that looks valid.
    fn write_entry(&self, path: &Path, o: &CachedOutput) -> io::Result<()> {
        let env = Envelope {
            format: OUTPUT_CACHE_FORMAT_VERSION,
            logical_triples: o.logical_triples,
            skipped_invalid_iri: o.skipped_invalid_iri,
            auto_inverses: o.auto_inverses,
            digest: output_digest(o),
            len: o.text.len(),
            text: o.text.clone(),
        };
        let body = serde_json::to_vec(&env)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let tmp = self.temp_path();
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn out(text: &str) -> CachedOutput {
        CachedOutput { logical_triples: 1, skipped_invalid_iri: 0, auto_inverses: 0, text: text.to_string() }
    }
    fn ctx() -> CanonicalContext {
        CanonicalContext::new().field("distro", "fedora").field("release", "44")
    }
    fn cache(dir: &TempDir) -> OutputCache {
        OutputCache::new(dir.path(), "20260912T010203Z-abcdef01", "spec", "spec-v1").unwrap()
    }

    #[test]
    fn miss_then_hit_skips_the_closure() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let r1 = c.get_or_compute("pkg", &ctx(), || Ok(ComputeOutcome::Complete(out("<a> <b> <c> .\n")))).unwrap();
        assert_eq!(r1.text, "<a> <b> <c> .\n");
        let r2 = c.get_or_compute("pkg", &ctx(), || panic!("closure must not run on a hit")).unwrap();
        assert_eq!(r2.text, "<a> <b> <c> .\n");
        assert_eq!(c.stats().hits, 1);
        assert_eq!(c.stats().misses, 1);
    }

    #[test]
    fn counters_survive_a_round_trip() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let o = CachedOutput { logical_triples: 7, skipped_invalid_iri: 2, auto_inverses: 3, text: "x".into() };
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(o))).unwrap();
        let hit = c.get_or_compute("k", &ctx(), || panic!("hit expected")).unwrap();
        assert_eq!((hit.logical_triples, hit.skipped_invalid_iri, hit.auto_inverses), (7, 2, 3));
    }

    #[test]
    fn retryable_is_returned_but_never_persisted() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let r = c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Retryable(out("tmp")))).unwrap();
        assert_eq!(r.text, "tmp");
        let mut ran = false;
        let r2 = c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("real"))) }).unwrap();
        assert!(ran, "a Retryable result must not have been checkpointed");
        assert_eq!(r2.text, "real");
        assert_eq!(c.stats().retryable, 1);
    }

    #[test]
    fn err_propagates_and_writes_nothing() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let e = c.get_or_compute("k", &ctx(), || Err(io::Error::new(io::ErrorKind::Other, "boom")));
        assert!(e.is_err());
        let mut ran = false;
        let _ = c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v"))) }).unwrap();
        assert!(ran, "an Err must not have been checkpointed");
    }

    #[test]
    fn disabled_mode_never_touches_disk() {
        let d = TempDir::new().unwrap();
        let c = OutputCache::disabled();
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        let mut ran = false;
        c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v"))) }).unwrap();
        assert!(ran, "disabled cache must always recompute");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn corrupt_entry_is_a_miss_and_is_evicted() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        let path = c.entry_path("k", &ctx()).unwrap();
        std::fs::write(&path, b"{ not a valid envelope").unwrap();
        let mut ran = false;
        c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v2"))) }).unwrap();
        assert!(ran, "corrupt entry must be treated as a miss");
        assert_eq!(c.stats().integrity_failures, 1);
    }

    #[test]
    fn digest_mismatch_is_a_miss() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        let path = c.entry_path("k", &ctx()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // Valid JSON envelope, but the payload no longer matches its recorded digest.
        std::fs::write(&path, raw.replace("\"text\":\"v\"", "\"text\":\"TAMPERED\"")).unwrap();
        let mut ran = false;
        c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v")))}).unwrap();
        assert!(ran, "payload not matching its digest must be a miss");
    }

    #[test]
    fn unknown_format_version_is_a_miss() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        let path = c.entry_path("k", &ctx()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replace("\"format\":1", "\"format\":9999")).unwrap();
        let mut ran = false;
        c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v")))}).unwrap();
        assert!(ran, "an unknown envelope format must be a miss");
    }

    #[test]
    fn context_encoding_is_injective() {
        // ("ab","c") and ("a","bc") must not collide -- the ambiguity that
        // plain concatenation would introduce.
        let a = CanonicalContext::new().field("x", "ab").field("y", "c");
        let b = CanonicalContext::new().field("x", "a").field("y", "bc");
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn every_context_field_changes_the_digest() {
        let base = CanonicalContext::new()
            .field("distro", "fedora").field("release", "44")
            .flag("buildrequires", false)
            .list("identities", &["a".into(), "b".into()]);
        let variants = [
            CanonicalContext::new().field("distro", "rhel").field("release", "44")
                .flag("buildrequires", false).list("identities", &["a".into(), "b".into()]),
            CanonicalContext::new().field("distro", "fedora").field("release", "43")
                .flag("buildrequires", false).list("identities", &["a".into(), "b".into()]),
            CanonicalContext::new().field("distro", "fedora").field("release", "44")
                .flag("buildrequires", true).list("identities", &["a".into(), "b".into()]),
            CanonicalContext::new().field("distro", "fedora").field("release", "44")
                .flag("buildrequires", false).list("identities", &["a".into(), "c".into()]),
        ];
        for v in &variants {
            assert_ne!(base.digest(), v.digest());
        }
    }

    #[test]
    fn context_variants_are_domain_separated() {
        // Without a type tag these all collapse onto the same encoding, so
        // swapping field for flag during a refactor would silently reuse the
        // other's checkpoints.
        let as_field = CanonicalContext::new().field("x", "0");
        let as_flag = CanonicalContext::new().flag("x", false);
        let as_list = CanonicalContext::new().list("x", &["0".into()]);
        assert_ne!(as_field.digest(), as_flag.digest());
        assert_ne!(as_field.digest(), as_list.digest());
        assert_ne!(as_flag.digest(), as_list.digest());
        assert_ne!(
            CanonicalContext::new().field("x", "1").digest(),
            CanonicalContext::new().flag("x", true).digest()
        );
    }

    #[test]
    fn list_order_does_not_matter_but_contents_do() {
        let a = CanonicalContext::new().list("l", &["b".into(), "a".into()]);
        let b = CanonicalContext::new().list("l", &["a".into(), "b".into()]);
        assert_eq!(a.digest(), b.digest(), "lists are sorted before hashing");
        let c = CanonicalContext::new().list("l", &["a".into()]);
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn item_key_and_context_are_separately_distinguished() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let p1 = c.entry_path("ab", &CanonicalContext::new().field("f", "c")).unwrap();
        let p2 = c.entry_path("a", &CanonicalContext::new().field("f", "bc")).unwrap();
        assert_ne!(p1, p2);
    }

    #[test]
    fn write_failure_still_returns_the_computed_output() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        // Replace the entry directory with a file so the atomic write cannot succeed.
        let dir = c.entry_path("k", &ctx()).unwrap().parent().unwrap().to_path_buf();
        std::fs::remove_dir_all(&dir).ok();
        std::fs::write(&dir, b"blocker").unwrap();
        let r = c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(out("v")))).unwrap();
        assert_eq!(r.text, "v", "a cache write failure must not discard derived output");
        assert_eq!(c.stats().write_failures, 1);
    }

    #[test]
    fn temp_files_are_unique_per_write() {
        let d = TempDir::new().unwrap();
        let c = cache(&d);
        let a = c.temp_path();
        let b = c.temp_path();
        assert_ne!(a, b);
    }

    #[test]
    fn tampering_with_any_counter_is_an_integrity_miss() {
        // The counters are restored into the parent writer and drive reported
        // totals, so they must be covered by the digest -- not just the RDF text.
        for field in ["logical_triples", "skipped_invalid_iri", "auto_inverses"] {
            let d = TempDir::new().unwrap();
            let c = cache(&d);
            c.get_or_compute("k", &ctx(), || Ok(ComputeOutcome::Complete(CachedOutput {
                logical_triples: 1, skipped_invalid_iri: 1, auto_inverses: 1, text: "v".into(),
            }))).unwrap();
            let path = c.entry_path("k", &ctx()).unwrap();
            let raw = std::fs::read_to_string(&path).unwrap();
            let tampered = raw.replace(&format!("\"{field}\":1"), &format!("\"{field}\":999"));
            assert_ne!(tampered, raw, "test must actually mutate {field}");
            std::fs::write(&path, tampered).unwrap();
            let mut ran = false;
            c.get_or_compute("k", &ctx(), || { ran = true; Ok(ComputeOutcome::Complete(out("v"))) }).unwrap();
            assert!(ran, "a tampered {field} must be an integrity miss");
        }
    }

    #[test]
    fn constructor_rejects_unsafe_path_components() {
        // OutputCache is a shared public primitive: it enforces its own
        // invariant rather than trusting every caller to have validated.
        let d = TempDir::new().unwrap();
        let good = "20260912T010203Z-abcdef01";
        assert!(OutputCache::new(d.path(), "../escape", "spec", "spec-v1").is_err());
        assert!(OutputCache::new(d.path(), good, "..", "spec-v1").is_err());
        assert!(OutputCache::new(d.path(), good, "spec", "../x").is_err());
        assert!(OutputCache::new(d.path(), good, "a/b", "spec-v1").is_err());
        assert!(OutputCache::new(d.path(), good, "", "spec-v1").is_err());
        assert!(OutputCache::new(d.path(), good, "spec", "").is_err());
        assert!(OutputCache::new(d.path(), good, "spec", "spec-v1").is_ok());
    }

    #[test]
    fn constructor_rejects_an_invalid_generation_id() {
        let d = TempDir::new().unwrap();
        assert!(OutputCache::new(d.path(), "not-a-generation", "spec", "spec-v1").is_err());
    }
}
