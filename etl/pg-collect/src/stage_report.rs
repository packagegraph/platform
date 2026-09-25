//! What a run's stages actually managed to do, recorded alongside the graph.
//!
//! Optional enrichment stages are allowed to fail item by item without failing
//! the run. That is a deliberate availability trade: one unreachable Koji hub
//! should not cost a distribution's entire package graph for the night. The
//! cost of the trade is that a published graph can be a knowingly partial
//! snapshot, and until now nothing downstream could tell -- a graph enriched
//! for 1,200 of 1,200 builds and one enriched for 400 of 1,200 arrive
//! identical, both green (#70).
//!
//! So a run writes a sidecar next to its N-Triples output, and `upload-nt.sh`
//! carries it into the graph's commit manifest as `quality`
//! (docs/GRAPH-PUBLICATION.md). Two rules make it worth having:
//!
//!   * **`complete` is never inferred.** A graph with no sidecar has unknown
//!     completeness, which is what every graph published before this was.
//!     Absence must not read as "complete" -- that is the status quo lie this
//!     exists to stop telling.
//!   * **A limited run is not a complete one.** `--limit` reduces
//!     `attempted` and `completed` together, so the obvious check passes
//!     trivially. `eligible` records the untruncated population.
//!   * **A stage with any retryable or failed item is not complete**, even
//!     when the run exits 0 and the graph publishes. Those are the same run.
//!
//! Required stages do not appear here as a softer signal: they still abort the
//! run. They are recorded so a reader can see that they were attempted and
//! that their failure would have been fatal.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// The sidecar format. Bumped only for a change that would make an existing
/// reader wrong; new optional fields do not bump it.
pub const QUALITY_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StageReport {
    /// Stage name as the operator sees it in the log: `rpm`, `spec`, `koji`.
    pub stage: String,
    /// A required stage's failure aborts the run, so it can never be the
    /// reason a published graph is partial. Recorded to make that visible
    /// rather than implied.
    pub required: bool,
    /// Items this stage took responsibility for, after de-duplication and
    /// after any `--limit` was applied. A limited run is not a complete one.
    pub attempted: u64,
    /// Items that reached a conclusive answer. "The build does not exist" is
    /// conclusive and counts here; only an inconclusive answer does not.
    pub completed: u64,
    /// Items whose upstream answer was inconclusive -- a transport failure, a
    /// fault, a 5xx. Deliberately not checkpointed, so the next run retries
    /// them.
    pub retryable: u64,
    /// Items that errored outright. These used to be printed and then
    /// forgotten; a run could lose a hundred of them and still report success
    /// with no trace in its stage totals.
    pub failed: u64,
    /// How many items this stage was eligible to process, before `--limit`
    /// truncated it. `None` means no limit was in play and `attempted` is the
    /// whole population.
    ///
    /// Without this a limited run is indistinguishable from a complete one:
    /// `--limit` reduces `attempted` and `completed` together, so
    /// `attempted == completed` holds trivially and the run publishes
    /// claiming to have covered everything. Any caller that truncates its
    /// work MUST set this, at the same place it applies the limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligible: Option<u64>,
    /// Set when the stage stopped short of its population but cannot say by
    /// how much.
    ///
    /// `eligible` is the better signal and should be preferred wherever the
    /// untruncated count is knowable. It is not always: the RPM stage counts
    /// architecture URLs while `--limit` truncates *packages*, and the number
    /// of packages an arch would have yielded is not known without collecting
    /// them. This says "not exhaustive" without inventing a denominator.
    ///
    /// Deliberately conservative -- a limit larger than the population sets
    /// this too, so an exhaustive run can read as partial. That direction is
    /// safe; the reverse is the bug this exists to prevent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl StageReport {
    pub fn new(stage: &str, required: bool) -> Self {
        StageReport {
            stage: stage.to_string(),
            required,
            ..Default::default()
        }
    }

    /// Every attempted item reached a conclusive answer and none errored.
    ///
    /// `attempted == completed` is checked as well as the two failure counters
    /// being zero: an item that falls out of the loop without being classified
    /// at all is a gap in the accounting, and a gap must read as incomplete
    /// rather than as success.
    pub fn is_complete(&self) -> bool {
        self.retryable == 0
            && self.failed == 0
            && self.attempted == self.completed
            && self.eligible.is_none_or(|e| e == self.attempted)
            && !self.truncated
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunQuality {
    pub schema: u32,
    /// True only when every stage is complete. A run with no stages at all is
    /// not evidence of completeness, so this is false for an empty report.
    pub complete: bool,
    pub stages: Vec<StageReport>,
}

impl RunQuality {
    pub fn new(stages: Vec<StageReport>) -> Self {
        let complete = !stages.is_empty() && stages.iter().all(StageReport::is_complete);
        RunQuality {
            schema: QUALITY_SCHEMA,
            complete,
            stages,
        }
    }

    /// `<output>.quality.json`, beside the N-Triples the run just produced.
    ///
    /// A sidecar rather than a flag on the upload call because the collector
    /// and the uploader are separate processes joined by a file path, and
    /// because the counts have to survive a wrapper that does nothing but move
    /// that path around.
    pub fn sidecar_path(output: &Path) -> PathBuf {
        let mut name = output.as_os_str().to_os_string();
        name.push(".quality.json");
        PathBuf::from(name)
    }

    /// Written to a temp file in the same directory and renamed, so a reader
    /// never sees a half-written report. The uploader refuses a sidecar it
    /// cannot parse rather than publishing the graph with its completeness
    /// silently unrecorded, which makes a torn write a failed upload; it must
    /// not be possible to produce one.
    pub fn write_sidecar(&self, output: &Path) -> io::Result<PathBuf> {
        let path = Self::sidecar_path(output);
        let temp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(&body)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, &path)?;
        Ok(path)
    }

    /// One line per stage, so the counts an operator reads in the journal are
    /// the same ones the manifest will carry.
    pub fn report(&self) {
        eprintln!(
            "\n=== Stage completeness: {} ===",
            if self.complete { "complete" } else { "PARTIAL" }
        );
        for stage in &self.stages {
            eprintln!(
                "  {:<8} {:>8} attempted, {:>8} completed, {:>6} retryable, {:>6} failed{}{}",
                stage.stage,
                stage.attempted,
                stage.completed,
                stage.retryable,
                stage.failed,
                if stage.required { " [required]" } else { "" },
                if stage.is_complete() {
                    ""
                } else {
                    "  <- partial"
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(attempted: u64, completed: u64, retryable: u64, failed: u64) -> StageReport {
        StageReport {
            stage: "koji".to_string(),
            required: false,
            attempted,
            completed,
            retryable,
            failed,
            eligible: None,
            truncated: false,
        }
    }

    fn limited(attempted: u64, eligible: u64) -> StageReport {
        StageReport {
            eligible: Some(eligible),
            ..stage(attempted, attempted, 0, 0)
        }
    }

    #[test]
    fn a_stage_that_answered_every_item_is_complete() {
        assert!(stage(10, 10, 0, 0).is_complete());
    }

    #[test]
    fn one_retryable_item_makes_a_stage_partial() {
        // The availability trade: this run still publishes. It must not
        // publish claiming to be complete.
        assert!(!stage(10, 9, 1, 0).is_complete());
    }

    #[test]
    fn one_failed_item_makes_a_stage_partial() {
        assert!(!stage(10, 9, 0, 1).is_complete());
    }

    #[test]
    fn an_unaccounted_item_makes_a_stage_partial() {
        // 10 attempted, 9 classified. The missing one is a hole in the
        // accounting, and a hole must not read as success.
        assert!(!stage(10, 9, 0, 0).is_complete());
    }

    #[test]
    fn a_limited_run_is_not_complete() {
        // The whole point. --limit truncates attempted and completed
        // together, so attempted == completed holds trivially and every
        // other check passes. Without `eligible` this reports success for a
        // run that touched 10 of 363,822 items.
        assert!(!limited(10, 363_822).is_complete());
    }

    #[test]
    fn a_truncated_stage_is_not_complete_even_with_matching_counts() {
        // The RPM shape: every architecture URL it attempted also completed,
        // so attempted == completed holds and there is no denominator to
        // compare against -- the limit cut packages, not arches.
        let mut st = stage(3, 3, 0, 0);
        assert!(st.is_complete(), "precondition: counts alone look clean");
        st.truncated = true;
        assert!(!st.is_complete());
    }

    #[test]
    fn truncated_is_omitted_from_the_sidecar_when_false() {
        let json = serde_json::to_string(&stage(3, 3, 0, 0)).unwrap();
        assert!(!json.contains("truncated"), "{json}");
    }

    #[test]
    fn a_limit_larger_than_the_population_is_not_a_limit() {
        // --limit 1000 against 10 eligible items processed all of them.
        // Recording eligible must not make an exhaustive run look partial.
        assert!(limited(10, 10).is_complete());
    }

    #[test]
    fn eligible_is_omitted_from_the_sidecar_when_unset() {
        // Schema 1 readers must keep working: an unlimited run's sidecar is
        // byte-identical to what it was before this field existed.
        let json = serde_json::to_string(&stage(10, 10, 0, 0)).unwrap();
        assert!(!json.contains("eligible"), "{json}");
    }

    #[test]
    fn a_sidecar_without_eligible_still_parses() {
        let old = r#"{"stage":"koji","required":false,"attempted":10,
                      "completed":10,"retryable":0,"failed":0}"#;
        let parsed: StageReport = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.eligible, None);
        assert!(parsed.is_complete());
    }

    #[test]
    fn a_limited_stage_makes_the_whole_run_partial() {
        let run = RunQuality::new(vec![stage(5, 5, 0, 0), limited(10, 999)]);
        assert!(!run.complete);
    }

    #[test]
    fn a_stage_that_attempted_nothing_is_complete() {
        // Vacuous, but correct: it was asked for nothing and delivered it.
        // The run-level check below is what refuses to call a report with no
        // stages at all a complete one.
        assert!(stage(0, 0, 0, 0).is_complete());
    }

    #[test]
    fn a_run_is_complete_only_when_every_stage_is() {
        let good = StageReport {
            stage: "rpm".into(),
            required: true,
            attempted: 5,
            completed: 5,
            ..Default::default()
        };
        assert!(RunQuality::new(vec![good.clone()]).complete);
        assert!(!RunQuality::new(vec![good, stage(10, 9, 1, 0)]).complete);
    }

    #[test]
    fn a_run_with_no_stages_is_not_complete() {
        // Otherwise a collector that recorded nothing would publish a graph
        // asserting perfect enrichment.
        assert!(!RunQuality::new(vec![]).complete);
    }

    #[test]
    fn the_sidecar_sits_beside_the_output_keeping_its_full_name() {
        // `.with_extension` would turn fedora-44.nt into fedora-44.quality.json
        // and, worse, collide across two outputs differing only by extension.
        assert_eq!(
            RunQuality::sidecar_path(Path::new("/run/fedora-44.nt")),
            Path::new("/run/fedora-44.nt.quality.json")
        );
    }

    #[test]
    fn a_written_sidecar_round_trips_and_leaves_no_temp_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let output = dir.path().join("fedora-44.nt");
        std::fs::write(&output, b"").unwrap();

        let quality = RunQuality::new(vec![stage(10, 9, 1, 0)]);
        let written = quality.write_sidecar(&output).unwrap();

        let parsed: RunQuality = serde_json::from_slice(&std::fs::read(&written).unwrap()).unwrap();
        assert_eq!(parsed, quality);
        assert!(!parsed.complete);
        assert_eq!(parsed.schema, QUALITY_SCHEMA);

        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files survived: {:?}", strays);
    }
}
