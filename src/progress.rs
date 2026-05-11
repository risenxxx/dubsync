//! Pipeline-phase progress events and reporters.
//!
//! [`run_pipeline`](crate::run_pipeline) takes a `&dyn ProgressReporter` and emits a
//! [`PipelineEvent`] at every phase boundary plus throttled `PhaseProgress` updates
//! inside long-running phases (correlation, fps-stretch, splice, remux). Two
//! reporter implementations ship: [`StderrReporter`] for the CLI binary and (under
//! the `gui` feature) [`ChannelReporter`] that fans events into a `crossbeam_channel`
//! for the GUI thread to render as cards.
//!
//! `tracing::info!/warn!/error!` calls inside phases continue to flow through the
//! existing tracing infra — those are *sub-event* details (per-segment splice
//! strategy, ffmpeg invocation, etc.). The two systems are deliberately disjoint:
//! reporter for phase-level structure, tracing for free-form line logs.

use crate::report::RunSummary;
use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PhaseId {
    Workspace,
    ExtractAnchors,
    ExtractDubs,
    FpsNormalize,
    Correlate,
    Splice,
    Remux,
}

impl PhaseId {
    /// 1-based step number, used for `[3/7]` prefixes in the CLI reporter and
    /// for ordering cards in the GUI.
    pub fn ordinal(self) -> usize {
        match self {
            PhaseId::Workspace => 1,
            PhaseId::ExtractAnchors => 2,
            PhaseId::ExtractDubs => 3,
            PhaseId::FpsNormalize => 4,
            PhaseId::Correlate => 5,
            PhaseId::Splice => 6,
            PhaseId::Remux => 7,
        }
    }

    pub const TOTAL: usize = 7;
}

#[derive(Clone, Debug)]
pub enum PipelineEvent {
    PhaseStarted {
        id: PhaseId,
        title: String,
        eta_hint: Option<Duration>,
        detail: Option<String>,
    },
    PhaseProgress {
        id: PhaseId,
        fraction: f32,
        detail: Option<String>,
    },
    PhaseFinished {
        id: PhaseId,
        summary: Option<String>,
    },
    PhaseFailed {
        id: PhaseId,
        error: String,
    },
    /// Emitted exactly once at the end of a successful pipeline run with the full
    /// summary stats (anchor counts, jump magnitudes, splice strategy breakdown,
    /// elapsed time, output path). Reporters render this as a multi-line block /
    /// summary card. Not emitted on failure — `PhaseFailed` carries that case.
    RunSummary(Box<RunSummary>),
}

pub trait ProgressReporter: Send + Sync {
    fn emit(&self, ev: PipelineEvent);
}

/// No-op reporter for tests and any embedding that doesn't care about progress.
pub struct NullReporter;

impl ProgressReporter for NullReporter {
    fn emit(&self, _ev: PipelineEvent) {}
}

/// Stderr reporter for the CLI. Writes one `[N/7] Title…` line per phase. When
/// stderr is a terminal, in-phase progress overwrites the line via `\r`; when piped
/// to a file/CI log, progress lines are suppressed and only Started/Finished/Failed
/// are emitted (keeps log files small and grep-friendly).
pub struct StderrReporter {
    inner: Mutex<StderrState>,
}

struct StderrState {
    is_tty: bool,
    /// Last fraction printed for the active phase (so we can throttle to ≥1% deltas).
    last_fraction_pct: i32,
    /// True if the last write left a `\r` line that needs a `\n` before the next
    /// phase starts.
    pending_newline: bool,
}

impl StderrReporter {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(StderrState {
                is_tty: std::io::stderr().is_terminal(),
                last_fraction_pct: -1,
                pending_newline: false,
            }),
        }
    }
}

impl Default for StderrReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressReporter for StderrReporter {
    fn emit(&self, ev: PipelineEvent) {
        let mut state = match self.inner.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut err = std::io::stderr().lock();
        // Always terminate any `\r`-style line before starting a fresh one.
        if state.pending_newline {
            let _ = writeln!(err);
            state.pending_newline = false;
            state.last_fraction_pct = -1;
        }
        match ev {
            PipelineEvent::PhaseStarted {
                id,
                title,
                eta_hint,
                detail,
            } => {
                let eta = eta_hint
                    .map(|d| format!(" (~{})", format_duration(d)))
                    .unwrap_or_default();
                let detail = detail.map(|d| format!(" — {d}")).unwrap_or_default();
                let _ = writeln!(
                    err,
                    "[{}/{}] {}{}{}",
                    id.ordinal(),
                    PhaseId::TOTAL,
                    title,
                    eta,
                    detail
                );
                state.last_fraction_pct = -1;
            }
            PipelineEvent::PhaseProgress {
                id,
                fraction,
                detail,
            } => {
                if !state.is_tty {
                    return;
                }
                let pct = (fraction * 100.0).clamp(0.0, 100.0) as i32;
                if pct == state.last_fraction_pct {
                    return;
                }
                state.last_fraction_pct = pct;
                let detail = detail.map(|d| format!(" — {d}")).unwrap_or_default();
                let _ = write!(
                    err,
                    "\r    [{}/{}] {pct:>3}%{}",
                    id.ordinal(),
                    PhaseId::TOTAL,
                    detail
                );
                let _ = err.flush();
                state.pending_newline = true;
            }
            PipelineEvent::PhaseFinished { id, summary } => {
                let summary = summary.map(|s| format!(" — {s}")).unwrap_or_default();
                let _ = writeln!(
                    err,
                    "    [{}/{}] done{}",
                    id.ordinal(),
                    PhaseId::TOTAL,
                    summary
                );
                state.last_fraction_pct = -1;
            }
            PipelineEvent::PhaseFailed { id, error } => {
                let _ = writeln!(
                    err,
                    "    [{}/{}] FAILED: {error}",
                    id.ordinal(),
                    PhaseId::TOTAL
                );
                state.last_fraction_pct = -1;
            }
            PipelineEvent::RunSummary(summary) => {
                let _ = writeln!(err, "─── run summary ───");
                for line in summary.human_lines() {
                    let _ = writeln!(err, "  {line}");
                }
                state.last_fraction_pct = -1;
            }
        }
    }
}

fn format_duration(d: Duration) -> String {
    let total_s = d.as_secs();
    if total_s >= 60 {
        format!("{}m{:02}s", total_s / 60, total_s % 60)
    } else {
        format!("{total_s}s")
    }
}

#[cfg(feature = "gui")]
pub use channel::ChannelReporter;

#[cfg(feature = "gui")]
mod channel {
    use super::{PipelineEvent, ProgressReporter};
    use crossbeam_channel::Sender;

    /// Channel-backed reporter for the GUI. The pipeline runs on a worker thread and
    /// pushes events through a bounded crossbeam channel that the UI thread drains
    /// every frame. `try_send` so a stalled UI never blocks the pipeline.
    pub struct ChannelReporter {
        tx: Sender<PipelineEvent>,
    }

    impl ChannelReporter {
        pub fn new(tx: Sender<PipelineEvent>) -> Self {
            Self { tx }
        }
    }

    impl ProgressReporter for ChannelReporter {
        fn emit(&self, ev: PipelineEvent) {
            let _ = self.tx.try_send(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_reporter_is_noop() {
        let r = NullReporter;
        r.emit(PipelineEvent::PhaseStarted {
            id: PhaseId::Workspace,
            title: "x".into(),
            eta_hint: None,
            detail: None,
        });
        // No assertion needed — just verify it doesn't panic and compiles.
    }

    #[test]
    fn phase_ordinals_are_unique_and_in_range() {
        let ids = [
            PhaseId::Workspace,
            PhaseId::ExtractAnchors,
            PhaseId::ExtractDubs,
            PhaseId::FpsNormalize,
            PhaseId::Correlate,
            PhaseId::Splice,
            PhaseId::Remux,
        ];
        let ords: Vec<usize> = ids.iter().map(|id| id.ordinal()).collect();
        for &o in &ords {
            assert!((1..=PhaseId::TOTAL).contains(&o));
        }
        let mut sorted = ords.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ordinals must be unique");
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
        assert_eq!(format_duration(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m05s");
    }

    #[cfg(feature = "gui")]
    #[test]
    fn channel_reporter_round_trips_events() {
        let (tx, rx) = crossbeam_channel::bounded(4);
        let r = ChannelReporter::new(tx);
        r.emit(PipelineEvent::PhaseStarted {
            id: PhaseId::Correlate,
            title: "Correlating".into(),
            eta_hint: None,
            detail: None,
        });
        r.emit(PipelineEvent::PhaseFinished {
            id: PhaseId::Correlate,
            summary: Some("3 segments".into()),
        });
        let n = rx.try_iter().count();
        assert_eq!(n, 2);
    }
}
