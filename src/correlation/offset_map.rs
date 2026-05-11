use super::sliding::AnchorPoint;
use crate::error::Result;
use serde::Serialize;
use std::path::Path;
use thiserror::Error;

/// Failure modes for `OffsetMap::build`. Surfaced to the caller (instead of returning
/// a dummy `OffsetMap` with zero-length valid range) so the binary can fail with a
/// non-zero exit code when the inputs share no detectable content.
#[derive(Debug, Error)]
pub enum BuildError {
    #[error(
        "no anchors survived the confidence filter (kept 0 of {total}, min_confidence={min_confidence}); \
         the master and donor files may not share content, or the anchor track choice is wrong"
    )]
    NoUsableAnchors { total: usize, min_confidence: f32 },
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Segment {
    pub master_start_s: f32,
    pub master_end_s: f32,
    pub donor_offset_s: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct OffsetMap {
    pub segments: Vec<Segment>,
    pub master_duration_s: f32,
    pub anchor_count: usize,
    pub rejected_count: usize,
    /// Number of would-be segment boundaries that were suppressed because the
    /// proposed offset jump exceeded `max_jump_s`. These are typically GCC-PHAT
    /// false positives in low-information regions (end credits' repetitive music,
    /// long sustained tones, etc.) where the autocorrelation has multiple
    /// equally-plausible peaks.
    #[serde(default)]
    pub suppressed_jumps: usize,
    /// Master timestamps outside `[valid_master_start_s, valid_master_end_s]` had no
    /// confident correlation evidence and are rendered as silence in synced dubs to
    /// avoid playing extrapolated garbage (e.g. mismatched intros).
    pub valid_master_start_s: f32,
    pub valid_master_end_s: f32,
}

impl OffsetMap {
    /// Build a piecewise-constant map from raw anchors.
    ///
    /// Pipeline:
    ///   1. drop anchors with confidence below `min_confidence`
    ///   2. median-filter offsets in a 5-wide kernel to kill single-window outliers
    ///   3. detect jumps where successive offsets differ by more than one frame
    ///      (1 / `frame_rate`), emit a transition between the two windows
    ///   4. **suppress** any jump whose magnitude exceeds `max_jump_s` — those are
    ///      almost always GCC-PHAT false positives in low-information regions
    ///      (repetitive end-credits music, long tones) where the autocorrelation
    ///      has multiple equally-good peaks. The current offset extrapolates past
    ///      the suppressed point.
    ///   5. clamp the last segment to `master_duration_s`
    ///   6. record the master-time range covered by kept anchors as the "valid" range
    pub fn build(
        anchors: &[AnchorPoint],
        frame_rate: f32,
        master_duration_s: f32,
        correlation_window_s: f32,
        min_confidence: f32,
        max_jump_s: f32,
    ) -> std::result::Result<Self, BuildError> {
        let total = anchors.len();
        let kept: Vec<AnchorPoint> = anchors
            .iter()
            .copied()
            .filter(|a| a.confidence >= min_confidence)
            .collect();
        let rejected_count = total - kept.len();

        if kept.is_empty() {
            return Err(BuildError::NoUsableAnchors {
                total,
                min_confidence,
            });
        }

        let smoothed = median_filter_offsets(&kept, 5);

        let frame_threshold = 1.0 / frame_rate;
        let mut segments: Vec<Segment> = Vec::new();
        let mut seg_start_s = 0.0_f32;
        let mut current_offset = smoothed[0].offset_s;
        let mut suppressed_jumps = 0usize;

        // A window of length W starting at master time t reports the *dominant* offset
        // across [t, t+W], so a 50/50 split (where the reported offset flips between
        // adjacent anchors) happens at t + W/2 — not at t. Therefore the true transition
        // for the pair (t_a, t_b) lies in (t_a + W/2, t_b + W/2), and the midpoint estimate
        // is (t_a + t_b)/2 + W/2. Failing to add this term puts every coarse boundary
        // exactly W/2 seconds too early.
        let half_window = correlation_window_s * 0.5;
        for w in smoothed.windows(2) {
            let prev = w[0];
            let curr = w[1];
            let jump = curr.offset_s - current_offset;
            let jump_abs = jump.abs();
            if jump_abs <= frame_threshold {
                continue;
            }
            if jump_abs > max_jump_s {
                // GCC-PHAT picked a different correlation peak in a low-information
                // region (e.g. credits music). Real master/donor pairs rarely have
                // jumps this large — credits can shift by tens of seconds in theory,
                // but anything past `max_jump_s` is overwhelmingly more likely a
                // false positive than a real edit. Suppress and keep extrapolating
                // the current offset; the user can raise `--max-segment-jump-s` if
                // they're working with content that has genuinely large jumps.
                tracing::warn!(
                    master_time_s = curr.master_time_s,
                    proposed_offset_s = curr.offset_s,
                    current_offset_s = current_offset,
                    jump_s = jump,
                    max_allowed_s = max_jump_s,
                    "suppressing offset jump as likely false detection — raise --max-segment-jump-s if this is a real edit"
                );
                suppressed_jumps += 1;
                continue;
            }
            let boundary = (prev.master_time_s + curr.master_time_s) * 0.5 + half_window;
            let boundary = boundary.min(master_duration_s);
            segments.push(Segment {
                master_start_s: seg_start_s,
                master_end_s: boundary,
                donor_offset_s: current_offset,
            });
            seg_start_s = boundary;
            current_offset = curr.offset_s;
        }
        segments.push(Segment {
            master_start_s: seg_start_s,
            master_end_s: master_duration_s,
            donor_offset_s: current_offset,
        });

        // Anchor at master_time T correlated content over [T, T + window]. The first kept
        // anchor backs from its own start; the last extends evidence one window forward.
        let first_t = kept.first().unwrap().master_time_s;
        let last_t = kept.last().unwrap().master_time_s;
        let valid_master_end_s = (last_t + correlation_window_s).min(master_duration_s);

        Ok(OffsetMap {
            segments,
            master_duration_s,
            anchor_count: kept.len(),
            rejected_count,
            suppressed_jumps,
            valid_master_start_s: first_t,
            valid_master_end_s,
        })
    }

    /// Refine each interior segment boundary in two passes:
    ///
    /// 1. **Coarse pass** — sweep a 4 s-max window at 0.25 s hop across the bracket
    ///    (`±half_hop_s` plus a `FINE_WINDOW_MAX_S` edge buffer). Locates the transition
    ///    zone: where lag_a stops winning and lag_b starts.
    /// 2. **Sub-frame pass** — re-sweep `[last_a_center − 0.5, first_b_center + 0.5]`
    ///    with a 1 s window at 0.05 s hop. Same comparison; pins the boundary to ~50 ms
    ///    (≈ one frame at 24 fps). If sub-frame disagrees with coarse by > 1 s, the
    ///    transition zone is structurally ambiguous and we keep the coarse answer.
    ///
    /// Returns one `BoundaryTrace` per interior boundary so the caller can dump
    /// `transition_traces.json` for offline analysis.
    pub fn refine_transitions(
        &mut self,
        master: &[f32],
        donor: &[f32],
        sample_rate: u32,
        half_hop_s: f32,
    ) -> Vec<BoundaryTrace> {
        if self.segments.len() < 2 {
            return Vec::new();
        }
        let mut traces = Vec::with_capacity(self.segments.len() - 1);
        let edge_buffer_s = FINE_WINDOW_MAX_S;
        for i in 0..self.segments.len() - 1 {
            let original = self.segments[i].master_end_s;
            let off_a = self.segments[i].donor_offset_s;
            let off_b = self.segments[i + 1].donor_offset_s;
            let initial_lo = (original - half_hop_s - edge_buffer_s)
                .max(self.segments[i].master_start_s)
                .max(0.0);
            let initial_hi =
                (original + half_hop_s + edge_buffer_s).min(self.segments[i + 1].master_end_s);
            if initial_hi <= initial_lo {
                continue;
            }

            // Adaptive bracket extension: if the first sweep saw only one side win
            // (a `last_a` with no `first_b`, or vice versa) the true cut is past that
            // edge — extend in steps and re-sweep until both sides are seen, the cap
            // is hit, or we run out of segment to extend into.
            let seg_lo_bound = self.segments[i].master_start_s.max(0.0);
            let seg_hi_bound = self.segments[i + 1].master_end_s;
            let mut bracket_lo = initial_lo;
            let mut bracket_hi = initial_hi;
            let mut extended_lo_s = 0.0_f32;
            let mut extended_hi_s = 0.0_f32;
            let coarse = loop {
                let result = refine_one_boundary(
                    master,
                    donor,
                    sample_rate,
                    bracket_lo,
                    bracket_hi,
                    off_a,
                    off_b,
                    FINE_WINDOW_MIN_S,
                    FINE_WINDOW_MAX_S,
                    FINE_HOP_S,
                );

                let need_extend_hi =
                    result.last_a_center.is_some() && result.first_b_center.is_none();
                let need_extend_lo =
                    result.first_b_center.is_some() && result.last_a_center.is_none();

                if !need_extend_hi && !need_extend_lo {
                    break result;
                }
                if need_extend_hi
                    && extended_hi_s < BRACKET_EXTEND_MAX_S
                    && bracket_hi < seg_hi_bound
                {
                    let new_hi = (bracket_hi + BRACKET_EXTEND_STEP_S).min(seg_hi_bound);
                    if new_hi <= bracket_hi {
                        break result;
                    }
                    extended_hi_s += new_hi - bracket_hi;
                    tracing::info!(
                        boundary = i,
                        from = format!("{:.2}", bracket_hi),
                        to = format!("{:.2}", new_hi),
                        "extending refinement bracket forward (cut past initial bracket_hi)"
                    );
                    bracket_hi = new_hi;
                } else if need_extend_lo
                    && extended_lo_s < BRACKET_EXTEND_MAX_S
                    && bracket_lo > seg_lo_bound
                {
                    let new_lo = (bracket_lo - BRACKET_EXTEND_STEP_S).max(seg_lo_bound);
                    if new_lo >= bracket_lo {
                        break result;
                    }
                    extended_lo_s += bracket_lo - new_lo;
                    tracing::info!(
                        boundary = i,
                        from = format!("{:.2}", bracket_lo),
                        to = format!("{:.2}", new_lo),
                        "extending refinement bracket backward (cut before initial bracket_lo)"
                    );
                    bracket_lo = new_lo;
                } else {
                    break result;
                }
            };

            let bracket_str = format!("[{:.2}, {:.2}]", bracket_lo, bracket_hi);
            let mut chosen_boundary: Option<f32> = coarse.refined_s;
            let mut subframe_refined: Option<f32> = None;
            let mut subframe_trace: Vec<TracePoint> = Vec::new();

            // Sub-frame second pass — only meaningful if the coarse pass found both sides.
            if let (Some(a_center), Some(b_center)) = (coarse.last_a_center, coarse.first_b_center)
            {
                let sub_lo = (a_center - SUBFRAME_PAD_S).max(bracket_lo);
                let sub_hi = (b_center + SUBFRAME_PAD_S).min(bracket_hi);
                if sub_hi > sub_lo {
                    let sub = refine_one_boundary(
                        master,
                        donor,
                        sample_rate,
                        sub_lo,
                        sub_hi,
                        off_a,
                        off_b,
                        SUBFRAME_WINDOW_S.min((sub_hi - sub_lo) * 0.5),
                        SUBFRAME_WINDOW_S,
                        SUBFRAME_HOP_S,
                    );
                    subframe_trace = sub.trace.clone();
                    if let (Some(sub_r), Some(coarse_r)) = (sub.refined_s, coarse.refined_s) {
                        if (sub_r - coarse_r).abs() <= SUBFRAME_DISAGREEMENT_LIMIT_S {
                            subframe_refined = Some(sub_r);
                            chosen_boundary = Some(sub_r);
                        } else {
                            tracing::warn!(
                                boundary = i,
                                coarse = format!("{:.3}", coarse_r),
                                subframe = format!("{:.3}", sub_r),
                                "sub-frame refinement disagrees with coarse pass by > {SUBFRAME_DISAGREEMENT_LIMIT_S}s — transition zone is ambiguous, keeping coarse"
                            );
                        }
                    }
                }
            }

            match chosen_boundary {
                Some(refined) => {
                    tracing::info!(
                        boundary = i,
                        coarse_s = format!("{:.3}", original),
                        refined_s = format!("{:.3}", refined),
                        shift_s = format!("{:+.3}", refined - original),
                        bracket = %bracket_str,
                        last_a_center_s = coarse
                            .last_a_center
                            .map(|v| format!("{:.3}", v))
                            .unwrap_or_else(|| "—".into()),
                        first_b_center_s = coarse
                            .first_b_center
                            .map(|v| format!("{:.3}", v))
                            .unwrap_or_else(|| "—".into()),
                        subframe_refined_s = subframe_refined
                            .map(|v| format!("{:.3}", v))
                            .unwrap_or_else(|| "—".into()),
                        "transition refined"
                    );
                    self.segments[i].master_end_s = refined;
                    self.segments[i + 1].master_start_s = refined;
                }
                None => {
                    let reason = match (coarse.last_a_center, coarse.first_b_center) {
                        (Some(_), None) => "bracket fell entirely on the pre-cut side — true cut is outside bracket_hi",
                        (None, Some(_)) => "bracket fell entirely on the post-cut side — true cut is outside bracket_lo",
                        (None, None) => "no fine window scored cleanly — possibly silent stretch or non-sharp cut",
                        _ => "unexpected — both sides seen but result was None",
                    };
                    tracing::warn!(
                        boundary = i,
                        coarse_s = format!("{:.3}", original),
                        bracket = %bracket_str,
                        last_a_center_s = coarse
                            .last_a_center
                            .map(|v| format!("{:.3}", v))
                            .unwrap_or_else(|| "—".into()),
                        first_b_center_s = coarse
                            .first_b_center
                            .map(|v| format!("{:.3}", v))
                            .unwrap_or_else(|| "—".into()),
                        reason,
                        "transition refinement inconclusive — keeping coarse boundary"
                    );
                }
            }

            traces.push(BoundaryTrace {
                boundary_idx: i,
                coarse_boundary_s: original,
                bracket_s: (bracket_lo, bracket_hi),
                refined_coarse_s: coarse.refined_s,
                refined_subframe_s: subframe_refined,
                last_a_center_s: coarse.last_a_center,
                first_b_center_s: coarse.first_b_center,
                trace: coarse.trace,
                subframe_trace,
            });
        }
        traces
    }

    pub fn dump_json(&self, path: &Path) -> Result<()> {
        let f = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(f, self)?;
        Ok(())
    }
}

fn median_filter_offsets(anchors: &[AnchorPoint], k: usize) -> Vec<AnchorPoint> {
    let half = k / 2;
    let n = anchors.len();
    let mut out = Vec::with_capacity(n);
    let mut buf = Vec::with_capacity(k);
    for i in 0..n {
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(n);
        buf.clear();
        buf.extend(anchors[lo..hi].iter().map(|a| a.offset_s));
        buf.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = buf[buf.len() / 2];
        out.push(AnchorPoint {
            master_time_s: anchors[i].master_time_s,
            offset_s: median,
            confidence: anchors[i].confidence,
        });
    }
    out
}

/// Pearson cross-correlation of `master[master_start..+window]` against
/// `donor[master_start + lag..+window]`. Returns NEG_INFINITY if the donor slice
/// would fall outside its buffer.
fn corr_at_lag(
    master: &[f32],
    donor: &[f32],
    master_start: usize,
    lag_samples: i64,
    window: usize,
) -> f32 {
    let donor_start_i = master_start as i64 + lag_samples;
    if donor_start_i < 0 {
        return f32::NEG_INFINITY;
    }
    let donor_start = donor_start_i as usize;
    if master_start + window > master.len() || donor_start + window > donor.len() {
        return f32::NEG_INFINITY;
    }
    let mut sum_ab = 0.0_f64;
    let mut sum_aa = 0.0_f64;
    let mut sum_bb = 0.0_f64;
    for i in 0..window {
        let a = master[master_start + i] as f64;
        let b = donor[donor_start + i] as f64;
        sum_ab += a * b;
        sum_aa += a * a;
        sum_bb += b * b;
    }
    let denom = (sum_aa * sum_bb).sqrt();
    if denom < 1e-12 {
        0.0
    } else {
        (sum_ab / denom) as f32
    }
}

const FINE_WINDOW_MAX_S: f32 = 4.0;
const FINE_WINDOW_MIN_S: f32 = 0.5;
const FINE_HOP_S: f32 = 0.25;

// Sub-frame second-pass parameters: tighter window + 5x finer hop, called inside the
// narrow [last_a_center, first_b_center] subrange that the coarse pass identified.
const SUBFRAME_WINDOW_S: f32 = 1.0;
const SUBFRAME_HOP_S: f32 = 0.05;
const SUBFRAME_PAD_S: f32 = 0.5;
/// If sub-frame disagrees with coarse refinement by more than this, the transition
/// zone is structurally ambiguous (multiple cuts, intermediate offset) — keep coarse.
const SUBFRAME_DISAGREEMENT_LIMIT_S: f32 = 1.0;

// Bracket-extension parameters: when a coarse pass returns inconclusive because one
// side never won inside the bracket, the true cut is *past* the bracket. The W/2
// midpoint can be pulled wildly off by PHAT energy bias when one side of the cut has
// dramatically louder content than the other — the anchor offset flips at a different
// crossover than the 50%-content rule predicts. Adaptive extension finds the actual
// cut without trusting the midpoint.
const BRACKET_EXTEND_STEP_S: f32 = 5.0;
const BRACKET_EXTEND_MAX_S: f32 = 30.0;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct TracePoint {
    pub master_t_s: f32,
    pub s_a: f32,
    pub s_b: f32,
}

/// Full diagnostic record of one boundary refinement: the coarse-pass trace, the
/// sub-frame pass result, and the bracket geometry. Dumped to `transition_traces.json`
/// when --keep-temp is on, so we can read what the algorithm "saw" at each transition.
#[derive(Debug, Clone, Serialize)]
pub struct BoundaryTrace {
    pub boundary_idx: usize,
    pub coarse_boundary_s: f32,
    pub bracket_s: (f32, f32),
    pub refined_coarse_s: Option<f32>,
    pub refined_subframe_s: Option<f32>,
    pub last_a_center_s: Option<f32>,
    pub first_b_center_s: Option<f32>,
    pub trace: Vec<TracePoint>,
    pub subframe_trace: Vec<TracePoint>,
}

#[derive(Debug, Default)]
struct RefineResult {
    refined_s: Option<f32>,
    last_a_center: Option<f32>,
    first_b_center: Option<f32>,
    trace: Vec<TracePoint>,
}

/// Parametrized fine sweep over `[bracket_start_s, bracket_end_s]` comparing
/// `corr_at_lag` at `offset_a_s` and `offset_b_s`. The window length is adaptive
/// (clamped between `min_window_s` and `max_window_s`) and the hop is `hop_s`.
#[allow(clippy::too_many_arguments)]
fn refine_one_boundary(
    master: &[f32],
    donor: &[f32],
    sample_rate: u32,
    bracket_start_s: f32,
    bracket_end_s: f32,
    offset_a_s: f32,
    offset_b_s: f32,
    min_window_s: f32,
    max_window_s: f32,
    hop_s: f32,
) -> RefineResult {
    let sr = sample_rate as f32;
    let lag_a = (offset_a_s * sr).round() as i64;
    let lag_b = (offset_b_s * sr).round() as i64;

    let bracket_width_s = (bracket_end_s - bracket_start_s).max(0.0);
    let fine_window_s = max_window_s.min(bracket_width_s * 0.5).max(min_window_s);
    let window = (fine_window_s * sr).round() as usize;
    let hop = (hop_s * sr).round().max(1.0) as usize;
    if window == 0 {
        return RefineResult::default();
    }

    let lo_samp = (bracket_start_s * sr).round() as usize;
    let hi_samp = (bracket_end_s * sr).round() as usize;
    if lo_samp + window > hi_samp || lo_samp + window > master.len() {
        return RefineResult::default();
    }

    let mut last_a_center: Option<f32> = None;
    let mut first_b_center: Option<f32> = None;
    let mut trace = Vec::new();

    let mut pos = lo_samp;
    while pos + window <= hi_samp.min(master.len()) {
        let center_s = (pos as f32 + window as f32 * 0.5) / sr;
        let s_a = corr_at_lag(master, donor, pos, lag_a, window);
        let s_b = corr_at_lag(master, donor, pos, lag_b, window);
        trace.push(TracePoint {
            master_t_s: center_s,
            s_a,
            s_b,
        });
        if s_a.is_finite() || s_b.is_finite() {
            if s_a >= s_b {
                last_a_center = Some(center_s);
            } else if first_b_center.is_none() {
                first_b_center = Some(center_s);
            }
        }
        pos += hop;
    }

    let refined_s = match (last_a_center, first_b_center) {
        (Some(a), Some(b)) => Some((a + b) * 0.5),
        _ => None,
    };
    RefineResult {
        refined_s,
        last_a_center,
        first_b_center,
        trace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_single_jump() {
        // anchors at t = 0, 15, ..., 285 with W=30. The offset flips between the
        // anchor at t=135 (offset 0.5) and t=150 (offset 1.7), so the true transition
        // sits in (135 + W/2, 150 + W/2) = (150, 165). Midpoint estimate = 157.5.
        let mut anchors = Vec::new();
        for i in 0..20 {
            anchors.push(AnchorPoint {
                master_time_s: i as f32 * 15.0,
                offset_s: if i < 10 { 0.5 } else { 1.7 },
                confidence: 50.0,
            });
        }
        let map = OffsetMap::build(&anchors, 25.0, 300.0, 30.0, 6.0, 10.0).expect("build");
        assert_eq!(map.segments.len(), 2);
        assert!((map.segments[0].donor_offset_s - 0.5).abs() < 1e-3);
        assert!((map.segments[1].donor_offset_s - 1.7).abs() < 1e-3);
        let b = map.segments[0].master_end_s;
        assert!(
            b > 150.0 && b < 165.0,
            "boundary {b} not inside the window-corrected range (150, 165)"
        );
    }

    #[test]
    fn suppresses_jump_above_max_threshold() {
        // Reproduces the user-reported credits failure: the GCC-PHAT correlator picks
        // up a 58 s offset shift in the back third of an episode (typically caused by
        // repetitive credits music). With max_jump_s = 10, the jump should be
        // suppressed and the map should stay as a single segment at the original
        // offset.
        let mut anchors = Vec::new();
        for i in 0..20 {
            anchors.push(AnchorPoint {
                master_time_s: i as f32 * 15.0,
                // First half: offset 0; second half: offset -58 (simulated credits region).
                offset_s: if i < 13 { 0.0 } else { -58.0 },
                confidence: 50.0,
            });
        }
        let map = OffsetMap::build(&anchors, 25.0, 300.0, 30.0, 6.0, 10.0).expect("build");
        assert_eq!(
            map.segments.len(),
            1,
            "jump > max should not split segments"
        );
        assert!(
            map.segments[0].donor_offset_s.abs() < 1e-3,
            "single segment must keep the pre-jump offset"
        );
        assert!(
            map.suppressed_jumps >= 1,
            "expected at least one suppressed jump, got {}",
            map.suppressed_jumps
        );
    }

    #[test]
    fn allows_jump_within_max_threshold() {
        // Same shape as suppresses_jump_above_max_threshold but with a 5 s jump that
        // is within the default max_jump=10. Should split as before.
        let mut anchors = Vec::new();
        for i in 0..20 {
            anchors.push(AnchorPoint {
                master_time_s: i as f32 * 15.0,
                offset_s: if i < 10 { 0.0 } else { 5.0 },
                confidence: 50.0,
            });
        }
        let map = OffsetMap::build(&anchors, 25.0, 300.0, 30.0, 6.0, 10.0).expect("build");
        assert_eq!(
            map.segments.len(),
            2,
            "5s jump should still produce a split"
        );
        assert_eq!(map.suppressed_jumps, 0);
    }

    #[test]
    fn ignores_subframe_noise() {
        let mut anchors = Vec::new();
        for i in 0..20 {
            let jitter = if i % 2 == 0 { 0.0 } else { 0.005 };
            anchors.push(AnchorPoint {
                master_time_s: i as f32 * 15.0,
                offset_s: 0.5 + jitter,
                confidence: 50.0,
            });
        }
        let map = OffsetMap::build(&anchors, 25.0, 300.0, 30.0, 6.0, 10.0).expect("build");
        assert_eq!(map.segments.len(), 1);
    }

    #[test]
    fn drops_low_confidence() {
        let anchors = vec![
            AnchorPoint {
                master_time_s: 0.0,
                offset_s: 0.5,
                confidence: 50.0,
            },
            AnchorPoint {
                master_time_s: 15.0,
                offset_s: 100.0,
                confidence: 1.0,
            }, // outlier
            AnchorPoint {
                master_time_s: 30.0,
                offset_s: 0.5,
                confidence: 50.0,
            },
        ];
        let map = OffsetMap::build(&anchors, 25.0, 60.0, 30.0, 6.0, 10.0).expect("build");
        assert_eq!(map.rejected_count, 1);
        assert_eq!(map.segments.len(), 1);
    }

    #[test]
    fn valid_range_tracks_kept_anchors() {
        let anchors = vec![
            AnchorPoint {
                master_time_s: 0.0,
                offset_s: 0.5,
                confidence: 1.0, // dropped
            },
            AnchorPoint {
                master_time_s: 60.0,
                offset_s: 0.5,
                confidence: 50.0, // first kept
            },
            AnchorPoint {
                master_time_s: 240.0,
                offset_s: 0.5,
                confidence: 50.0, // last kept
            },
            AnchorPoint {
                master_time_s: 300.0,
                offset_s: 99.0,
                confidence: 0.5, // dropped
            },
        ];
        let map = OffsetMap::build(&anchors, 25.0, 360.0, 30.0, 6.0, 10.0).expect("build");
        assert_eq!(map.valid_master_start_s, 60.0);
        // last kept at 240 + window 30 = 270.
        assert!((map.valid_master_end_s - 270.0).abs() < 1e-3);
    }

    #[test]
    fn refine_finds_known_transition() {
        // Build synthetic master + donor anchor signals where the donor has been edited
        // at exactly master_time = 8.0 s: pre-transition is offset 0, post-transition is
        // offset +0.3 s (i.e. 0.3 s of extra content was inserted in donor at the cut).
        // We hand `refine_transitions` a *coarse* boundary at 7.0 s — the algorithm
        // should pull it back toward 8.0 s. The signal is long enough on both sides that
        // a 4 s fine window can land fully on either side of the cut.
        let sr = 16_000u32;
        let total_s = 16.0_f32;
        let n = (total_s * sr as f32) as usize;
        // Linear chirp 50 Hz → ~1650 Hz over the run. A pure sine would correlate equally
        // well at any offset that's a whole-cycle multiple — chirps decorrelate under any
        // shift, which is what real speech anchors do too.
        let signal: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (2.0 * std::f32::consts::PI * (50.0 + 100.0 * t) * t).sin()
            })
            .collect();

        let off_a = 0.0_f32;
        let off_b = 0.3_f32;
        let trans_s = 8.0_f32;
        let trans_samp = (trans_s * sr as f32) as usize;
        let delta_samp = (off_b * sr as f32) as usize;

        // donor = master[..trans] ++ silent_gap(delta) ++ master[trans..]
        let donor_len = n + delta_samp;
        let mut donor = vec![0.0_f32; donor_len];
        donor[..trans_samp].copy_from_slice(&signal[..trans_samp]);
        donor[trans_samp + delta_samp..trans_samp + delta_samp + (n - trans_samp)]
            .copy_from_slice(&signal[trans_samp..]);

        let mut map = OffsetMap {
            segments: vec![
                Segment {
                    master_start_s: 0.0,
                    master_end_s: 7.0, // coarse boundary 1.0 s away from the truth
                    donor_offset_s: off_a,
                },
                Segment {
                    master_start_s: 7.0,
                    master_end_s: total_s,
                    donor_offset_s: off_b,
                },
            ],
            master_duration_s: total_s,
            anchor_count: 2,
            rejected_count: 0,
            suppressed_jumps: 0,
            valid_master_start_s: 0.0,
            valid_master_end_s: total_s,
        };

        // half_hop=2 → strict bracket [5, 9]. With the +/-FINE_WINDOW_MAX_S buffer the
        // effective bracket is [1, 13], which fully straddles the true cut at 8.0.
        map.refine_transitions(&signal, &donor, sr, 2.0);
        let refined = map.segments[0].master_end_s;
        assert!(
            (refined - 8.0).abs() < 0.6,
            "refined boundary {refined} not near true 8.0 (within fine-window tolerance)"
        );
    }

    #[test]
    fn refinement_extends_bracket_when_cut_lies_past_initial_hi() {
        // Mirror the real-world failure: the W/2-corrected midpoint was pulled forward
        // by PHAT energy bias, so the initial bracket ENDS before the actual cut. We
        // expect adaptive extension to push bracket_hi forward and find the cut.
        let sr = 16_000u32;
        let total_s = 30.0_f32;
        let n = (total_s * sr as f32) as usize;
        let signal: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (2.0 * std::f32::consts::PI * (50.0 + 100.0 * t) * t).sin()
            })
            .collect();

        let off_a = 0.0_f32;
        let off_b = 0.3_f32;
        let trans_s = 22.0_f32;
        let trans_samp = (trans_s * sr as f32) as usize;
        let delta_samp = (off_b * sr as f32) as usize;

        let donor_len = n + delta_samp;
        let mut donor = vec![0.0_f32; donor_len];
        donor[..trans_samp].copy_from_slice(&signal[..trans_samp]);
        donor[trans_samp + delta_samp..trans_samp + delta_samp + (n - trans_samp)]
            .copy_from_slice(&signal[trans_samp..]);

        // Coarse boundary deliberately placed at 12.0 — far before the true cut at 22.
        // With half_hop=2 + edge_buffer=4, initial bracket = [6, 18]. Cut at 22 is OUT.
        let mut map = OffsetMap {
            segments: vec![
                Segment {
                    master_start_s: 0.0,
                    master_end_s: 12.0,
                    donor_offset_s: off_a,
                },
                Segment {
                    master_start_s: 12.0,
                    master_end_s: total_s,
                    donor_offset_s: off_b,
                },
            ],
            master_duration_s: total_s,
            anchor_count: 2,
            rejected_count: 0,
            suppressed_jumps: 0,
            valid_master_start_s: 0.0,
            valid_master_end_s: total_s,
        };

        let traces = map.refine_transitions(&signal, &donor, sr, 2.0);
        assert_eq!(traces.len(), 1);
        let t = &traces[0];
        // Final bracket should have extended past the initial hi=18 to cover trans_s=22.
        assert!(
            t.bracket_s.1 > 22.0,
            "bracket_hi should extend past true cut 22.0, got {}",
            t.bracket_s.1
        );
        let coarse = t
            .refined_coarse_s
            .expect("coarse pass should succeed after extension");
        assert!(
            (coarse - 22.0).abs() < 1.0,
            "extended-bracket coarse {coarse} not within 1 s of true 22.0"
        );
        let sub = t
            .refined_subframe_s
            .expect("sub-frame pass should succeed after extension");
        assert!(
            (sub - 22.0).abs() < 0.1,
            "sub-frame {sub} not within 0.1 s of true 22.0"
        );
    }

    #[test]
    fn subframe_pass_improves_precision() {
        // Same setup as refine_finds_known_transition — but now we expect the sub-frame
        // pass to land within 100 ms of the true cut, much tighter than the coarse-only
        // 600 ms tolerance.
        let sr = 16_000u32;
        let total_s = 16.0_f32;
        let n = (total_s * sr as f32) as usize;
        let signal: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (2.0 * std::f32::consts::PI * (50.0 + 100.0 * t) * t).sin()
            })
            .collect();

        let off_a = 0.0_f32;
        let off_b = 0.3_f32;
        let trans_s = 8.0_f32;
        let trans_samp = (trans_s * sr as f32) as usize;
        let delta_samp = (off_b * sr as f32) as usize;

        let donor_len = n + delta_samp;
        let mut donor = vec![0.0_f32; donor_len];
        donor[..trans_samp].copy_from_slice(&signal[..trans_samp]);
        donor[trans_samp + delta_samp..trans_samp + delta_samp + (n - trans_samp)]
            .copy_from_slice(&signal[trans_samp..]);

        let mut map = OffsetMap {
            segments: vec![
                Segment {
                    master_start_s: 0.0,
                    master_end_s: 7.0,
                    donor_offset_s: off_a,
                },
                Segment {
                    master_start_s: 7.0,
                    master_end_s: total_s,
                    donor_offset_s: off_b,
                },
            ],
            master_duration_s: total_s,
            anchor_count: 2,
            rejected_count: 0,
            suppressed_jumps: 0,
            valid_master_start_s: 0.0,
            valid_master_end_s: total_s,
        };

        let traces = map.refine_transitions(&signal, &donor, sr, 2.0);
        assert_eq!(traces.len(), 1);
        let t = &traces[0];
        let sub = t.refined_subframe_s.expect("sub-frame pass should succeed");
        assert!(
            (sub - 8.0).abs() < 0.1,
            "sub-frame boundary {sub} not within 0.1 s of true 8.0"
        );
        // Sub-frame should also have populated its trace.
        assert!(
            !t.subframe_trace.is_empty(),
            "subframe_trace should not be empty when sub-frame pass succeeded"
        );
    }
}
