//! Memory-bound regression tests for the RPM repodata parse path.
//!
//! `pg-collect rpm-full` was OOM-killed collecting RHEL 9 against a 4 GB
//! container cap. The repodata parse path held three whole-corpus copies at
//! once: the compressed `primary.xml.gz` bytes, the fully decompressed XML,
//! and a `Vec<RpmPackageData>` of every package in the repo. RHEL's repodata
//! keeps growing, so correctness tests alone would not have caught it — these
//! tests pin the *shape* of the memory use instead.
//!
//! The two properties asserted here are:
//!
//! 1. Decoding a compressed artifact never materializes the decompressed
//!    bytes (`decoding_reader`).
//! 2. Parsing `primary.xml` never retains more than one package at a time,
//!    so peak memory does not scale with the package count
//!    (`stream_primary_packages`).
//!
//! This whole file is a single `#[test]` on purpose: the tracking allocator
//! below is process-wide, so concurrent tests in the same binary would race on
//! the peak counter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use pg_collect::rpm::{decoding_reader, stream_primary_packages};

// ---------------------------------------------------------------------------
// Tracking allocator: records currently-live bytes and the high-water mark.
// ---------------------------------------------------------------------------

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct TrackingAlloc;

fn record_growth(n: usize) {
    let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            record_growth(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() {
            record_growth(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            if new_size > layout.size() {
                record_growth(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOC: TrackingAlloc = TrackingAlloc;

/// Run `body`, returning its value and the peak *additional* bytes live at any
/// point during it, relative to the live bytes on entry.
fn peak_extra_bytes<T>(body: impl FnOnce() -> T) -> (T, usize) {
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let out = body();
    let peak = PEAK.load(Ordering::Relaxed);
    (out, peak.saturating_sub(baseline))
}

const MIB: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Synthetic primary.xml, generated on the fly so the *input* is not held in
// memory either (a Vec<u8> of test input would swamp what we are measuring).
// ---------------------------------------------------------------------------

const DEPS_PER_PACKAGE: usize = 20;

struct PrimaryXmlGen {
    total: usize,
    next_pkg: usize,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl PrimaryXmlGen {
    fn new(total: usize) -> Self {
        let mut g = Self {
            total,
            next_pkg: 0,
            buf: Vec::with_capacity(8192),
            pos: 0,
            done: false,
        };
        g.buf.extend_from_slice(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm">
"#,
        );
        g
    }

    /// Bytes one package occupies in the generated document.
    fn refill(&mut self) {
        self.buf.clear();
        self.pos = 0;
        if self.next_pkg >= self.total {
            if !self.done {
                self.buf.extend_from_slice(b"</metadata>\n");
                self.done = true;
            }
            return;
        }
        let i = self.next_pkg;
        self.next_pkg += 1;
        let mut s = String::with_capacity(4096);
        s.push_str("<package type=\"rpm\">\n");
        s.push_str(&format!("  <name>synthpkg-{:06}</name>\n", i));
        s.push_str("  <arch>x86_64</arch>\n");
        s.push_str(&format!(
            "  <version epoch=\"0\" ver=\"1.{}.0\" rel=\"{}.el9\"/>\n",
            i % 50,
            i % 7 + 1
        ));
        s.push_str(
            "  <checksum type=\"sha256\" pkgid=\"YES\">\
             0000000000000000000000000000000000000000000000000000000000000000</checksum>\n",
        );
        s.push_str(&format!("  <summary>Synthetic package {}</summary>\n", i));
        s.push_str(&format!(
            "  <description>A synthetic package used to exercise the repodata parser, \
             number {} of {}.</description>\n",
            i, self.total
        ));
        s.push_str("  <packager>Synthetic Packager &lt;packager@example.invalid&gt;</packager>\n");
        s.push_str(&format!(
            "  <url>https://example.invalid/synthpkg-{:06}</url>\n",
            i
        ));
        s.push_str("  <time file=\"1700000000\" build=\"1700000000\"/>\n");
        s.push_str("  <size package=\"123456\" installed=\"234567\" archive=\"345678\"/>\n");
        s.push_str(&format!(
            "  <location href=\"Packages/s/synthpkg-{:06}-1.0.0-1.el9.x86_64.rpm\"/>\n",
            i
        ));
        s.push_str("  <format>\n");
        s.push_str("    <rpm:license>MIT</rpm:license>\n");
        s.push_str("    <rpm:vendor>Synthetic Vendor</rpm:vendor>\n");
        s.push_str("    <rpm:group>Unspecified</rpm:group>\n");
        s.push_str(&format!(
            "    <rpm:sourcerpm>synthsrc-{:06}-1.0.0-1.el9.src.rpm</rpm:sourcerpm>\n",
            i
        ));
        s.push_str("    <rpm:provides>\n");
        s.push_str(&format!(
            "      <rpm:entry name=\"synthpkg-{:06}\" flags=\"EQ\" epoch=\"0\" ver=\"1.0.0\" rel=\"1.el9\"/>\n",
            i
        ));
        s.push_str("    </rpm:provides>\n");
        s.push_str("    <rpm:requires>\n");
        for d in 0..DEPS_PER_PACKAGE {
            s.push_str(&format!(
                "      <rpm:entry name=\"libsynth{}.so.{}()(64bit)\"/>\n",
                d,
                d % 5
            ));
        }
        s.push_str("    </rpm:requires>\n");
        s.push_str("  </format>\n");
        s.push_str("</package>\n");
        self.buf.extend_from_slice(s.as_bytes());
    }
}

impl Read for PrimaryXmlGen {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            if self.done {
                return Ok(0);
            }
            self.refill();
            if self.buf.is_empty() {
                return Ok(0);
            }
        }
        let n = std::cmp::min(out.len(), self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Wraps a reader, counting bytes actually pulled from it.
struct ByteCountingReader<R> {
    inner: R,
    bytes: usize,
}

impl<R: Read> Read for ByteCountingReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(out)?;
        self.bytes += n;
        Ok(n)
    }
}

fn generated_len(packages: usize) -> usize {
    let mut g = PrimaryXmlGen::new(packages);
    let mut sink = [0u8; 64 * 1024];
    let mut total = 0;
    loop {
        match g.read(&mut sink).unwrap() {
            0 => break,
            n => total += n,
        }
    }
    total
}

#[test]
fn repodata_parse_path_is_streaming_and_memory_bounded() {
    // ---------------------------------------------------------------
    // Part 1: decoding a .gz artifact must not materialize the plaintext.
    // ---------------------------------------------------------------
    let dir = tempfile::tempdir().unwrap();
    let gz_path = dir.path().join("synthetic-primary.xml.gz");

    // ~25 MiB of highly compressible XML on disk.
    let plaintext_len = {
        let f = std::fs::File::create(&gz_path).unwrap();
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut g = PrimaryXmlGen::new(12_000);
        let mut chunk = [0u8; 64 * 1024];
        let mut written = 0usize;
        loop {
            match g.read(&mut chunk).unwrap() {
                0 => break,
                n => {
                    enc.write_all(&chunk[..n]).unwrap();
                    written += n;
                }
            }
        }
        enc.finish().unwrap().sync_all().unwrap();
        written
    };
    let gz_len = std::fs::metadata(&gz_path).unwrap().len() as usize;
    assert!(
        plaintext_len > 16 * MIB,
        "test fixture too small to be meaningful: {} bytes",
        plaintext_len
    );

    let ((decoded_len, digest), decode_peak) = peak_extra_bytes(|| {
        let mut r = decoding_reader(&gz_path).unwrap();
        let mut chunk = [0u8; 64 * 1024];
        let mut len = 0usize;
        // Cheap order-sensitive checksum: proves we really read the plaintext.
        let mut digest: u64 = 0;
        loop {
            match r.read(&mut chunk).unwrap() {
                0 => break,
                n => {
                    for b in &chunk[..n] {
                        digest = digest.wrapping_mul(31).wrapping_add(*b as u64);
                    }
                    len += n;
                }
            }
        }
        (len, digest)
    });

    assert_eq!(
        decoded_len, plaintext_len,
        "decoding_reader must yield the whole decompressed artifact"
    );
    assert_ne!(digest, 0, "decoded content must be non-trivial");

    // The decoder is allowed a bounded I/O buffer, nothing proportional to the
    // artifact. Anything that reads the file or the plaintext whole blows this.
    let decode_budget = 4 * MIB;
    assert!(
        decode_peak < decode_budget,
        "decoding_reader materialized the artifact: peak {} KiB for a {} KiB gz / {} KiB plaintext \
         (budget {} KiB). It must decode as a stream, not read the file into a Vec.",
        decode_peak / 1024,
        gz_len / 1024,
        plaintext_len / 1024,
        decode_budget / 1024,
    );

    // ---------------------------------------------------------------
    // Part 2: the primary.xml parse must not retain packages.
    // ---------------------------------------------------------------

    // Correctness first — a memory bound is worthless if the parse is wrong.
    let mut seen = Vec::new();
    let parsed = stream_primary_packages(std::io::BufReader::new(PrimaryXmlGen::new(3)), |pkg| {
        seen.push(pkg);
        Ok(())
    })
    .unwrap();
    assert_eq!(parsed, 3, "callback must fire once per <package>");
    assert_eq!(seen.len(), 3);
    assert_eq!(
        seen[1].fields.get("name").map(String::as_str),
        Some("synthpkg-000001")
    );
    assert_eq!(
        seen[1].fields.get("arch").map(String::as_str),
        Some("x86_64")
    );
    assert_eq!(seen[1].fields.get("ver").map(String::as_str), Some("1.1.0"));
    assert_eq!(
        seen[1].fields.get("rpm:sourcerpm").map(String::as_str),
        Some("synthsrc-000001-1.0.0-1.el9.src.rpm")
    );
    assert_eq!(
        seen[1]
            .deps
            .iter()
            .filter(|d| d.dep_type == "requires")
            .count(),
        DEPS_PER_PACKAGE
    );
    assert_eq!(
        seen[1]
            .deps
            .iter()
            .filter(|d| d.dep_type == "provides")
            .count(),
        1
    );
    drop(seen);

    // A callback error must abort the stream immediately, not run to EOF.
    // `rpm-full --limit N` depends on this to avoid reading a 1.3 GB
    // primary.xml when the caller asked for ten packages.
    //
    // Counting callback invocations alone would not prove this -- a parser
    // that buffered everything and only then drained into the callback would
    // still "stop" at the right call. So measure how much input was actually
    // consumed.
    let total_len = generated_len(5_000);
    let counting = ByteCountingReader {
        inner: PrimaryXmlGen::new(5_000),
        bytes: 0,
    };
    let mut calls = 0usize;
    let stop_after = 5usize;
    let (err, consumed) = {
        let mut r = std::io::BufReader::with_capacity(8 * 1024, counting);
        let err = stream_primary_packages(&mut r, |_pkg| {
            calls += 1;
            if calls == stop_after {
                return Err(std::io::Error::other("stop here"));
            }
            Ok(())
        })
        .expect_err("callback error must propagate");
        let consumed = r.into_inner().bytes;
        (err, consumed)
    };
    assert_eq!(err.to_string(), "stop here");
    assert_eq!(
        calls, stop_after,
        "callback must fire per package, in order"
    );
    assert!(
        consumed < total_len / 10,
        "stream kept reading after the callback failed: consumed {} of {} bytes. \
         The parser must abort on callback error, not buffer the document first.",
        consumed,
        total_len
    );

    // Now the streaming property. Parse at two sizes 4x apart; a parser that
    // retains packages shows peak memory scaling with the package count, a
    // streaming one does not.
    let small_n = 4_000usize;
    let large_n = 16_000usize;

    let mut counted = 0usize;
    let (small_count, small_peak) = peak_extra_bytes(|| {
        stream_primary_packages(
            std::io::BufReader::new(PrimaryXmlGen::new(small_n)),
            |pkg| {
                // Touch the data so nothing can be optimized away, but retain none.
                counted += pkg.fields.len() + pkg.deps.len();
                Ok(())
            },
        )
        .unwrap()
    });
    assert_eq!(small_count, small_n);

    let (large_count, large_peak) = peak_extra_bytes(|| {
        stream_primary_packages(
            std::io::BufReader::new(PrimaryXmlGen::new(large_n)),
            |pkg| {
                counted += pkg.fields.len() + pkg.deps.len();
                Ok(())
            },
        )
        .unwrap()
    });
    assert_eq!(large_count, large_n);
    assert!(counted > 0);

    let large_xml_bytes = generated_len(large_n);

    // Absolute bound: one package in flight plus quick-xml's buffers. Nothing
    // here should be within an order of magnitude of the document size.
    let parse_budget = 4 * MIB;
    assert!(
        large_peak < parse_budget,
        "primary.xml parse retained the corpus: peak {} KiB parsing {} packages \
         ({} KiB of XML), budget {} KiB. Emit packages as they are parsed instead \
         of collecting them into a Vec.",
        large_peak / 1024,
        large_n,
        large_xml_bytes / 1024,
        parse_budget / 1024,
    );

    // Scaling bound: this is the assertion that actually encodes "streaming".
    // It holds regardless of how large RHEL's repodata grows.
    let scaling_slack = MIB;
    assert!(
        large_peak <= small_peak + scaling_slack,
        "primary.xml parse peak memory scales with package count: {} KiB at {} packages \
         vs {} KiB at {} packages (4x the input). Peak must be O(1) in the package count.",
        large_peak / 1024,
        large_n,
        small_peak / 1024,
        small_n,
    );
}
