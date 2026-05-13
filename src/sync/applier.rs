//! Apply the offset map to each donor dub track.
//!
//! Splice strategy: every interior boundary is *snapped* into a moment that is silent
//! in the master-anchor track when one is available within `snap_radius_s` of the
//! refined boundary. Inside that silence we either insert |Δ| seconds of literal
//! silence (Δ < 0 — donor falls behind master, master needs to "wait") or hard-cut
//! at the silence centre and let the donor jump |Δ| seconds forward (Δ > 0). Either
//! way, the splice lands inside an already-silent master moment, so the cut is
//! imperceptible to the viewer regardless of dub content.
//!
//! When no master-anchor silence is available within `snap_radius_s`, we fall back
//! to mutual donor-dub silence (per-track), and finally to a hard cut + silence-gap
//! at the refined boundary itself. There is **no** WSOLA time-stretch path: stretch
//! cannot smoothly cross a hard donor edit, so it tended to glitch in exactly the
//! cases this module tries to fix.

use crate::audio::stretch;
use crate::audio::vad::{self, SilenceInterval, Verdict};
use crate::audio::wav::{self, Pcm};
use crate::correlation::OffsetMap;
use crate::error::Result;
use crate::ffmpeg::ExtractedDub;
use crate::progress::{PhaseId, PipelineEvent, ProgressReporter};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Result of applying the OffsetMap to one dub. The output WAV is f32le interleaved
/// at the dub's native sample rate / channel count, matching the master duration.
#[derive(Debug, Clone)]
pub struct SyncedDub {
    pub donor_index: u32,
    pub wav: PathBuf,
}

/// All splice-related configuration. Kept in one struct so adding future tunables
/// stays cheap.
#[derive(Debug, Clone)]
pub struct SpliceConfig {
    pub silence_db: f32,
    pub silence_min_ms: u32,
    /// Maximum master-time radius around each refined boundary in which the splicer
    /// is allowed to search for a master-anchor silence. Wider = more chance of
    /// finding a wide-enough silence, but the visual splice moves further from the
    /// detected scene transition.
    pub snap_radius_s: f32,
    /// Equal-power sin crossfade duration applied at every splice (segment ↔ segment,
    /// segment ↔ silence-gap, segment ↔ stretched gap-fill). Default 10 ms (middle of
    /// the perceptually-clean 5–15 ms range). Below this clicks become audible; above
    /// 50 ms the crossfade starts eating into segment audio.
    pub crossfade_ms: u32,
    /// Optional gap-filling — replace the literal silent gap at each splice with a
    /// time-stretched copy of the neighbouring dub audio (via `rubberband`). Speech
    /// is never stretched: if the neighbour buffer is dominated by speech the gap
    /// stays as silence. Default ON.
    pub smooth_gaps: bool,
    /// Length of dub audio sampled before AND after each gap as the stretch source.
    /// Wider = more material to stretch (less artefact), but more risk of catching
    /// dialog (which forces fallback to silence). Default 1.0 s.
    pub gap_fill_margin_s: f32,
    /// Above this dBFS level a neighbour buffer is treated as speech and the gap
    /// stays at silence. RMS+ZCR fallback only; ignored if Silero VAD is in use.
    pub speech_db: f32,
    /// Hard cap on `target / source` ratio for each independent pre/post stretch.
    /// Below the cap the stretched buffer can extend up to `gap_s/2` into the gap
    /// from its side; above the cap residual silence remains in the middle.
    /// Default 1.2 — inaudible on ambient/music; 1.5+ starts to drift on melodic
    /// content. Values < 1.0 are nonsensical and treated as 1.0.
    pub gap_fill_max_ratio: f32,
    /// Length of the soft fade-out (at end of stretched_pre) and fade-in (at start
    /// of stretched_post) used when the ratio cap leaves a residual silence in the
    /// middle of the gap. Wider = gentler transition into silence, less "hard cut"
    /// feel. Default 100 ms. Range [0, 500] — 0 reproduces the old hard-cut edge.
    pub gap_fill_silence_fade_ms: u32,
}

#[derive(Debug, Clone, Copy)]
enum SpliceStrategy {
    /// Splice centred at `snap_master_s`, an interior point of a master-anchor
    /// silence wide enough to absorb |Δ|. Renders as a hard cut + literal `gap_s`
    /// of silence — donor reads continuously through the gap when Δ < 0, or jumps
    /// `|Δ|` forward in donor time at the splice when Δ > 0. This is the preferred
    /// strategy: master is silent there, so the splice is inaudible.
    InMasterSilence { snap_master_s: f64, gap_s: f64 },
    /// No usable master-anchor silence within `snap_radius_s`, but the dub itself
    /// has mutual silence at both donor positions of the splice. Snap the boundary
    /// per-dub into the dub's silence; render as hard cut + `gap_s` of silence
    /// (`gap_s == |Δ|` for Δ < 0, `0` for Δ ≥ 0). The splice is still inaudible
    /// for that particular dub but moves to a different master time per track.
    InDubSilence { snap_master_s: f64, gap_s: f64 },
    /// Last-resort fallback: no silence (master or dub) within radius. Splice at the
    /// refined boundary with a 20 ms equal-power crossfade. If Δ < 0 we additionally
    /// insert `|Δ|` of silence centred on the boundary — audible but never glitches.
    AtBoundary { gap_s: f64 },
}

/// Internally-mutable view of one segment after boundary adjustments.
#[derive(Debug, Clone, Copy)]
struct AdjSegment {
    master_start_s: f64,
    master_end_s: f64,
    donor_offset_s: f64,
}

/// Hard floor for crossfade duration. Below this every splice becomes a click.
const MIN_CROSSFADE_MS: u32 = 1;
/// Hard ceiling — keeps the operator from accidentally setting it to seconds and
/// eating into segment audio.
const MAX_CROSSFADE_MS: u32 = 50;

fn clamp_crossfade_ms(cfg: &SpliceConfig) -> u32 {
    cfg.crossfade_ms.clamp(MIN_CROSSFADE_MS, MAX_CROSSFADE_MS)
}

fn crossfade_s(cfg: &SpliceConfig) -> f64 {
    clamp_crossfade_ms(cfg) as f64 * 0.001
}

/// Apply the offset map to every dub in parallel.
///
/// Emits `PhaseProgress` after each dub finishes so the UI can render a 1/N… N/N bar.
pub fn apply_to_dubs(
    dubs: &[ExtractedDub],
    map: &OffsetMap,
    master_silences: &[SilenceInterval],
    out_dir: &Path,
    cfg: &SpliceConfig,
    reporter: &dyn ProgressReporter,
) -> Result<Vec<SyncedDub>> {
    let silence_min_s = cfg.silence_min_ms as f32 / 1000.0;
    let radius = cfg.snap_radius_s as f64;
    if cfg.crossfade_ms != clamp_crossfade_ms(cfg) {
        tracing::warn!(
            requested = cfg.crossfade_ms,
            applied = clamp_crossfade_ms(cfg),
            "crossfade_ms clamped to allowed range [1, 50]"
        );
    }

    // Compute master-side strategy decisions once: where master is silent, the same
    // splice geometry applies to every dub. Returns one entry per interior boundary
    // (None = no master silence available; per-dub mutual silence will be tried).
    let master_decisions: Vec<Option<MasterSnap>> =
        master_snap_per_boundary(map, master_silences, radius);
    log_master_snaps(map, &master_decisions);

    let total = dubs.len().max(1);
    let done = AtomicUsize::new(0);
    dubs.par_iter()
        .map(|dub| {
            let pcm = wav::read_interleaved_f32(&dub.wav)?;
            let dub_silences = vad::detect_silence(&pcm, cfg.silence_db, silence_min_s);
            tracing::info!(
                track = dub.donor_index,
                channels = pcm.channels(),
                rate = pcm.sample_rate(),
                duration_s = format!("{:.2}", pcm.duration_s()),
                silences = dub_silences.len(),
                "applying offset map"
            );

            let mut segments = build_segments(map);
            let strategies =
                classify_strategies(&segments, &master_decisions, &dub_silences, radius);
            for (i, strat) in strategies.iter().enumerate() {
                tracing::info!(
                    track = dub.donor_index,
                    boundary = i,
                    strategy = format!("{:?}", strat),
                    "splice strategy"
                );
            }
            let xfade_s = crossfade_s(cfg);
            apply_segment_geometry(&mut segments, &strategies, xfade_s);

            let mut synced_samples = render(
                &pcm,
                &segments,
                map.master_duration_s as f64,
                map.valid_master_start_s as f64,
                map.valid_master_end_s as f64,
                xfade_s,
            );

            if cfg.smooth_gaps {
                fill_gaps(
                    &mut synced_samples,
                    &pcm,
                    &segments,
                    map.master_duration_s as f64,
                    map.valid_master_start_s as f64,
                    map.valid_master_end_s as f64,
                    cfg,
                    out_dir,
                    dub.donor_index,
                );
            }

            let out_path = out_dir.join(format!("synced_dub_{}.wav", dub.donor_index));
            wav::write_interleaved_f32(&out_path, &synced_samples, &pcm.spec)?;

            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            reporter.emit(PipelineEvent::PhaseProgress {
                id: PhaseId::Splice,
                fraction: n as f32 / total as f32,
                detail: Some(format!("{n}/{total} tracks")),
            });

            Ok(SyncedDub {
                donor_index: dub.donor_index,
                wav: out_path,
            })
        })
        .collect()
}

/// Snap decision computed from master-anchor silences. `gap_s` is the amount of
/// silence the renderer should insert (0 for Δ ≥ 0, |Δ| for Δ < 0).
#[derive(Debug, Clone, Copy)]
struct MasterSnap {
    snap_master_s: f64,
    gap_s: f64,
    silence_width_s: f64,
}

fn master_snap_per_boundary(
    map: &OffsetMap,
    master_silences: &[SilenceInterval],
    radius: f64,
) -> Vec<Option<MasterSnap>> {
    if map.segments.len() < 2 {
        return Vec::new();
    }
    (0..map.segments.len() - 1)
        .map(|i| {
            let boundary = map.segments[i].master_end_s as f64;
            let delta =
                map.segments[i + 1].donor_offset_s as f64 - map.segments[i].donor_offset_s as f64;
            best_master_silence(boundary, delta, master_silences, radius)
        })
        .collect()
}

/// Pick the master silence best suited to absorb |Δ| at this boundary. We require
/// the silence to be at least `|Δ|` wide so the inserted gap fits *entirely inside*
/// the silence — the 20 ms crossfade at the gap edges then lands in silent content
/// and is therefore inaudible.
///
/// Among qualifying silences within `radius` of the refined boundary, we prefer the
/// one closest to the boundary (smallest visual splice movement); ties broken by
/// width (wider = more headroom).
fn best_master_silence(
    boundary: f64,
    delta: f64,
    silences: &[SilenceInterval],
    radius: f64,
) -> Option<MasterSnap> {
    let needed = delta.abs();
    let mut best: Option<(f64, f64, f64)> = None; // (distance, width, center)
    for s in silences {
        let start = s.start_s as f64;
        let end = s.end_s as f64;
        let width = end - start;
        if width < needed {
            continue;
        }
        let center = (start + end) * 0.5;
        let dist = (center - boundary).abs();
        if dist > radius {
            continue;
        }
        let better = match best {
            None => true,
            Some((d, w, _)) => dist < d || (dist == d && width > w),
        };
        if better {
            best = Some((dist, width, center));
        }
    }
    best.map(|(_, w, c)| MasterSnap {
        snap_master_s: c,
        gap_s: if delta < 0.0 { needed } else { 0.0 },
        silence_width_s: w,
    })
}

fn log_master_snaps(map: &OffsetMap, decisions: &[Option<MasterSnap>]) {
    for (i, d) in decisions.iter().enumerate() {
        let boundary = map.segments[i].master_end_s as f64;
        let delta =
            map.segments[i + 1].donor_offset_s as f64 - map.segments[i].donor_offset_s as f64;
        match d {
            Some(s) => tracing::info!(
                boundary = i,
                refined_s = format!("{:.3}", boundary),
                snap_s = format!("{:.3}", s.snap_master_s),
                shift_s = format!("{:+.3}", s.snap_master_s - boundary),
                gap_s = format!("{:.3}", s.gap_s),
                silence_width_s = format!("{:.3}", s.silence_width_s),
                delta_s = format!("{:+.3}", delta),
                "master-silence snap"
            ),
            None => tracing::info!(
                boundary = i,
                refined_s = format!("{:.3}", boundary),
                delta_s = format!("{:+.3}", delta),
                "no master-anchor silence within radius — will try dub silence per track"
            ),
        }
    }
}

fn build_segments(map: &OffsetMap) -> Vec<AdjSegment> {
    map.segments
        .iter()
        .map(|s| AdjSegment {
            master_start_s: s.master_start_s as f64,
            master_end_s: s.master_end_s as f64,
            donor_offset_s: s.donor_offset_s as f64,
        })
        .collect()
}

/// Decide one strategy per interior boundary by combining the precomputed master-
/// silence decisions with this dub's own silence intervals. Master silence is
/// preferred; mutual dub silence is the second choice; AtBoundary is the fallback.
fn classify_strategies(
    segs: &[AdjSegment],
    master_decisions: &[Option<MasterSnap>],
    dub_silences: &[SilenceInterval],
    radius: f64,
) -> Vec<SpliceStrategy> {
    if segs.len() < 2 {
        return Vec::new();
    }
    (0..segs.len() - 1)
        .map(|i| {
            let boundary = segs[i].master_end_s;
            let off_a = segs[i].donor_offset_s;
            let off_b = segs[i + 1].donor_offset_s;
            let delta = off_b - off_a;
            let abs_delta = delta.abs();

            if let Some(m) = master_decisions.get(i).and_then(|x| x.as_ref()) {
                return SpliceStrategy::InMasterSilence {
                    snap_master_s: m.snap_master_s,
                    gap_s: m.gap_s,
                };
            }

            if let Some(t) = find_dub_mutual_silence(boundary, off_a, off_b, dub_silences, radius) {
                return SpliceStrategy::InDubSilence {
                    snap_master_s: t,
                    gap_s: if delta < 0.0 { abs_delta } else { 0.0 },
                };
            }

            SpliceStrategy::AtBoundary {
                gap_s: if delta < 0.0 { abs_delta } else { 0.0 },
            }
        })
        .collect()
}

/// Mutual-silence search on the donor dub itself: find a master time `t'` near the
/// boundary such that *both* donor reads (`t' + off_a` and `t' + off_b`) sit inside
/// detected dub silence intervals.
fn find_dub_mutual_silence(
    boundary: f64,
    off_a: f64,
    off_b: f64,
    silences: &[SilenceInterval],
    radius: f64,
) -> Option<f64> {
    let center_a = boundary + off_a;
    let center_b = boundary + off_b;
    let s_a_iter = silences.iter().filter(|s| {
        let (ss, se) = (s.start_s as f64, s.end_s as f64);
        se >= center_a - radius && ss <= center_a + radius
    });
    let s_b_set: Vec<&SilenceInterval> = silences
        .iter()
        .filter(|s| {
            let (ss, se) = (s.start_s as f64, s.end_s as f64);
            se >= center_b - radius && ss <= center_b + radius
        })
        .collect();

    let mut best: Option<(f64, f64)> = None;
    for s_a in s_a_iter {
        let m_a_lo = s_a.start_s as f64 - off_a;
        let m_a_hi = s_a.end_s as f64 - off_a;
        for s_b in &s_b_set {
            let m_b_lo = s_b.start_s as f64 - off_b;
            let m_b_hi = s_b.end_s as f64 - off_b;
            let lo = m_a_lo.max(m_b_lo);
            let hi = m_a_hi.min(m_b_hi);
            if lo > hi {
                continue;
            }
            let t_prime = boundary.clamp(lo, hi);
            let dev = (t_prime - boundary).abs();
            if best.map_or(true, |(d, _)| dev < d) {
                best = Some((dev, t_prime));
            }
        }
    }
    best.map(|(_, t)| t)
}

/// Bake the per-strategy splice geometry into the segment endpoints.
///
/// After this, each segment's `[master_start_s, master_end_s]` is the exact range
/// that the renderer fills with donor audio at constant offset.
///
/// **Crossfade overlap for `gap_s == 0`:** when adjacent segments would otherwise
/// touch at a single frame (HardCut), expand each side by `xfade_s / 2` so the two
/// renders overlap by `xfade_s`. The renderer's fade-out and fade-in then form a
/// true equal-power overlap-add (sum gain ≈ 1) instead of meeting at zero (which
/// produced a 1-frame near-silent dip — audible as a click on loud splices).
fn apply_segment_geometry(segs: &mut [AdjSegment], strategies: &[SpliceStrategy], xfade_s: f64) {
    if segs.len() < 2 {
        return;
    }
    for i in 0..segs.len() - 1 {
        let strat = strategies[i];
        let (snap, gap) = match strat {
            SpliceStrategy::InMasterSilence {
                snap_master_s,
                gap_s,
            }
            | SpliceStrategy::InDubSilence {
                snap_master_s,
                gap_s,
            } => (snap_master_s, gap_s),
            SpliceStrategy::AtBoundary { gap_s } => (segs[i].master_end_s, gap_s),
        };
        // For hard cuts (gap_s == 0) make the two segments overlap by xfade_s so the
        // crossfade produces equal-power summing instead of two ramps meeting at zero.
        // For real gaps (gap_s > 0) the silence between them already separates the
        // fades — overlap is impossible and unnecessary there.
        let overlap_each_side = if gap <= 0.0 { xfade_s * 0.5 } else { 0.0 };
        let half = gap * 0.5;
        let new_end = (snap - half + overlap_each_side).max(segs[i].master_start_s);
        let new_start = (snap + half - overlap_each_side).min(segs[i + 1].master_end_s);
        if new_end > new_start && gap > 0.0 {
            tracing::warn!(
                boundary = i,
                snap_s = format!("{:.3}", snap),
                gap_s = format!("{:.3}", gap),
                "splice geometry collapses adjacent segments — clamping"
            );
        }
        // For gap > 0 we still want new_end <= new_start; for gap = 0 with overlap
        // the relationship inverts (new_end > new_start) and that's intentional.
        if gap > 0.0 {
            segs[i].master_end_s = new_end.min(new_start);
            segs[i + 1].master_start_s = new_start.max(new_end);
        } else {
            segs[i].master_end_s = new_end;
            segs[i + 1].master_start_s = new_start;
        }
    }
}

/// Render the dub onto the master timeline. Each segment writes donor samples at
/// constant offset within `[master_start_s, master_end_s]`; intervals between
/// adjacent segments stay at zero (silence). Equal-power sin crossfades of length
/// `xfade_s` smooth the borders between rendered segments and silence (or
/// stretched gap-fill content added later) so splices are click-free.
fn render(
    pcm: &Pcm,
    segments: &[AdjSegment],
    master_duration_s: f64,
    valid_start_s: f64,
    valid_end_s: f64,
    xfade_s: f64,
) -> Vec<f32> {
    let sr = pcm.sample_rate() as f64;
    let channels = pcm.channels();
    let total_frames = (master_duration_s * sr).round() as usize;
    let mut out = vec![0.0_f32; total_frames * channels];

    let xfade = (xfade_s * sr).round() as usize;
    let valid_lo_frame = (valid_start_s * sr).round() as i64;
    let valid_hi_frame = (valid_end_s * sr).round() as i64;

    for (idx, seg) in segments.iter().enumerate() {
        let m_start = (seg.master_start_s * sr).round() as i64;
        let m_end = (seg.master_end_s * sr).round() as i64;
        if m_end <= m_start {
            continue;
        }
        let donor_start = ((seg.master_start_s + seg.donor_offset_s) * sr).round() as i64;
        let seg_len = (m_end - m_start) as usize;

        // Fades only at the splice boundaries — not at the master-timeline edges.
        let fade_in = idx > 0;
        let fade_out = idx + 1 < segments.len();

        for i in 0..seg_len {
            let dst_frame = m_start + i as i64;
            if dst_frame < 0 || dst_frame as usize >= total_frames {
                continue;
            }
            if dst_frame < valid_lo_frame || dst_frame >= valid_hi_frame {
                continue;
            }
            let src_frame = donor_start + i as i64;

            let mut gain = 1.0_f32;
            if fade_in && xfade > 0 && i < xfade {
                let t = (i as f32 + 0.5) / xfade as f32;
                gain *= (t * std::f32::consts::FRAC_PI_2).sin();
            }
            if fade_out && xfade > 0 && seg_len > xfade && i >= seg_len - xfade {
                let pos = seg_len - 1 - i;
                let t = (pos as f32 + 0.5) / xfade as f32;
                gain *= (t * std::f32::consts::FRAC_PI_2).sin();
            }

            for c in 0..channels {
                let s = if src_frame >= 0 && (src_frame as usize) < pcm.frames() {
                    pcm.samples[src_frame as usize * channels + c]
                } else {
                    0.0
                };
                out[dst_frame as usize * channels + c] += s * gain;
            }
        }
    }

    out
}

/// Optional gap-fill pass. For every interior boundary with a real master-time gap,
/// independently stretch the pre-gap and post-gap donor margins so each side
/// extends into the gap from its edge. The two stretched buffers either meet at the
/// midpoint (full coverage) or leave a residual silence with soft fade-in/out
/// transitions. Speech neighbours are never touched — the gap stays as literal
/// silence so lip-sync is preserved. Errors are logged but never fatal.
///
/// Key contrast with the older concat-source design: each stretched buffer covers a
/// *single contiguous* donor region (the one segs[i] / segs[i+1] would have played
/// at full speed in that location), and it **replaces** the corresponding margin
/// audio in `out[]` rather than adding to it. The listener never hears the same
/// donor sample twice — fixes the "looped end of phrase" artefact users reported on
/// the previous concat-then-stretch path.
#[allow(clippy::too_many_arguments)]
fn fill_gaps(
    out: &mut [f32],
    pcm: &Pcm,
    segments: &[AdjSegment],
    master_duration_s: f64,
    valid_start_s: f64,
    valid_end_s: f64,
    cfg: &SpliceConfig,
    workspace: &Path,
    track_id: u32,
) {
    let _ = (master_duration_s, valid_start_s, valid_end_s); // bounds already enforced by render
    if segments.len() < 2 {
        return;
    }
    let sr = pcm.sample_rate() as f64;
    let channels = pcm.channels();
    let xfade_s = crossfade_s(cfg);
    let xfade_n = (xfade_s * sr).round() as usize;
    let margin_s = cfg.gap_fill_margin_s.max(0.0) as f64;
    if margin_s <= 0.0 {
        return;
    }
    let max_ratio = cfg.gap_fill_max_ratio.max(1.0) as f64;
    let silence_fade_n = ((cfg.gap_fill_silence_fade_ms as f64 / 1000.0) * sr).round() as usize;
    let total_out_frames = out.len() / channels;

    for i in 0..segments.len() - 1 {
        let gap_start_s = segments[i].master_end_s;
        let gap_end_s = segments[i + 1].master_start_s;
        let gap_s = gap_end_s - gap_start_s;
        if gap_s <= 1.0 / sr {
            continue; // no real gap (HardCut overlap or collapsed boundary)
        }

        // Per-side margin is capped by the segment's own master-time length minus its
        // own fade-in/out zone. Without this, a short segment can have its head/tail
        // overwritten by the gap-fill, corrupting the splice from the previous/next
        // boundary. `(seg_len - xfade)` keeps the fade-in/out regions of the segment
        // intact for their own crossfades.
        let segs_i_master_s = segments[i].master_end_s - segments[i].master_start_s;
        let segs_ip1_master_s = segments[i + 1].master_end_s - segments[i + 1].master_start_s;
        let margin_pre_s = margin_s.min((segs_i_master_s - xfade_s).max(0.0));
        let margin_post_s = margin_s.min((segs_ip1_master_s - xfade_s).max(0.0));
        if margin_pre_s <= 0.0 || margin_post_s <= 0.0 {
            tracing::info!(
                track = track_id,
                boundary = i,
                "gap-fill skipped: adjacent segment shorter than crossfade — no margin available"
            );
            continue;
        }
        let margin_pre_frames = (margin_pre_s * sr).round() as usize;
        let margin_post_frames = (margin_post_s * sr).round() as usize;

        // Donor frames either side of the gap. The pre-gap donor position is the last
        // sample played by segs[i] (master_end + offset_a); the post-gap position is
        // the first sample played by segs[i+1] (master_start + offset_b).
        let pre_donor_end = ((gap_start_s + segments[i].donor_offset_s) * sr).round() as i64;
        let post_donor_start = ((gap_end_s + segments[i + 1].donor_offset_s) * sr).round() as i64;

        let pre_donor_lo = (pre_donor_end - margin_pre_frames as i64).max(0) as usize;
        let pre_donor_hi = pre_donor_end.max(0) as usize;
        let post_donor_lo = post_donor_start.max(0) as usize;
        let post_donor_hi = (post_donor_start + margin_post_frames as i64).max(0) as usize;
        let total_donor = pcm.frames();
        let pre_donor_hi = pre_donor_hi.min(total_donor);
        let post_donor_hi = post_donor_hi.min(total_donor);

        if pre_donor_hi <= pre_donor_lo || post_donor_hi <= post_donor_lo {
            tracing::info!(
                track = track_id,
                boundary = i,
                "gap-fill skipped: donor neighbour out of bounds"
            );
            continue;
        }

        let pre_slice = &pcm.samples[pre_donor_lo * channels..pre_donor_hi * channels];
        let post_slice = &pcm.samples[post_donor_lo * channels..post_donor_hi * channels];

        // Speech protection: if either half looks like dialog, refuse to stretch and
        // leave the gap as silence — preserves lip sync at the cost of missing a fill.
        let v_pre = vad::classify_buffer(pre_slice, channels, cfg.silence_db, cfg.speech_db);
        let v_post = vad::classify_buffer(post_slice, channels, cfg.silence_db, cfg.speech_db);
        if matches!(v_pre, Verdict::Speech) || matches!(v_post, Verdict::Speech) {
            tracing::info!(
                track = track_id,
                boundary = i,
                pre = format!("{:?}", v_pre),
                post = format!("{:?}", v_post),
                "gap-fill skipped: speech in neighbour buffer"
            );
            continue;
        }
        // Skip ONLY if both neighbours are truly digital silence (energy ≈ 0).
        // We deliberately do NOT skip on `Verdict::Silent`: that classification fires
        // when RMS < `silence_db` (default −45 dB), but real dub tracks frequently
        // sit at −50 to −60 dB room-tone for entire scenes. The user perceives the
        // contrast between that quiet room-tone and the gap's absolute zero as a
        // "silence pit". Stretching even very-quiet ambient material avoids that
        // contrast. Skipping is reserved for truly empty buffers.
        let pre_energy: f64 = pre_slice.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let post_energy: f64 = post_slice.iter().map(|&s| (s as f64) * (s as f64)).sum();
        if pre_energy < 1e-9 && post_energy < 1e-9 {
            tracing::debug!(
                track = track_id,
                boundary = i,
                "gap-fill skipped: both neighbours are digitally zero (no signal to stretch)"
            );
            continue;
        }

        // ── Geometry: how far each side extends into the gap ────────────────────
        //
        // Each side extends by min(gap/2, src_len * (R - 1)) seconds. Source lengths
        // can differ from `margin_s` if the donor was clamped to file bounds — that's
        // OK, we compute pre/post independently.
        let src_pre_n = pre_donor_hi - pre_donor_lo;
        let src_post_n = post_donor_hi - post_donor_lo;
        let src_pre_s = src_pre_n as f64 / sr;
        let src_post_s = src_post_n as f64 / sr;
        let half_gap_s = gap_s * 0.5;
        let ext_pre_s = half_gap_s.min(src_pre_s * (max_ratio - 1.0));
        let ext_post_s = half_gap_s.min(src_post_s * (max_ratio - 1.0));
        let target_pre_s = src_pre_s + ext_pre_s;
        let target_post_s = src_post_s + ext_post_s;

        let stretched_pre = match stretch::stretch_to_duration(
            pre_slice,
            channels as u16,
            pcm.sample_rate(),
            target_pre_s,
            workspace,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    track = track_id,
                    boundary = i,
                    error = %e,
                    "gap-fill: pre-side rubberband failed — leaving silence"
                );
                continue;
            }
        };
        let stretched_post = match stretch::stretch_to_duration(
            post_slice,
            channels as u16,
            pcm.sample_rate(),
            target_post_s,
            workspace,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    track = track_id,
                    boundary = i,
                    error = %e,
                    "gap-fill: post-side rubberband failed — leaving silence"
                );
                continue;
            }
        };
        // Stretch lengths may differ from target by a few frames; trust the actual
        // buffer length so the mix-buffer offsets stay consistent.
        let stretched_pre_n = stretched_pre.len() / channels;
        let stretched_post_n = stretched_post.len() / channels;

        // ── Mix-buffer construction ────────────────────────────────────────────
        //
        // Zone in master time: [gap_start - src_pre_s, gap_end + src_post_s]. We
        // overwrite this entire window in `out[]`. `mix` is the replacement content.
        let zone_n = ((gap_s + src_pre_s + src_post_s) * sr).round() as usize;
        let mut mix = vec![0.0_f32; zone_n * channels];

        let pre_end = stretched_pre_n.min(zone_n);
        let post_start = zone_n.saturating_sub(stretched_post_n);
        // Coverage check: do the two stretched buffers meet/overlap?
        let meets = pre_end + stretched_post_n >= zone_n;

        // Place stretched_pre at the front. We may apply silence-fade-out to its
        // tail later (residual case) or it may participate in a meeting crossfade.
        for f in 0..pre_end {
            for c in 0..channels {
                mix[f * channels + c] = stretched_pre[f * channels + c];
            }
        }

        if meets {
            // Equal-power crossfade in the overlap region [post_start, pre_end).
            // post_start <= pre_end here by construction.
            let overlap_n = pre_end - post_start;
            if overlap_n > 0 {
                for k in 0..overlap_n {
                    let mix_idx = post_start + k;
                    let t = (k as f32 + 0.5) / overlap_n as f32;
                    let cos_g = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
                    let sin_g = (t * std::f32::consts::FRAC_PI_2).sin();
                    for c in 0..channels {
                        let pre_v = mix[mix_idx * channels + c];
                        let post_v = stretched_post[k * channels + c];
                        mix[mix_idx * channels + c] = pre_v * cos_g + post_v * sin_g;
                    }
                }
            }
            // Tail of stretched_post past the overlap is plain copy.
            for k in overlap_n..stretched_post_n {
                let mix_idx = post_start + k;
                if mix_idx >= zone_n {
                    break;
                }
                for c in 0..channels {
                    mix[mix_idx * channels + c] = stretched_post[k * channels + c];
                }
            }
        } else {
            // Residual silence in the middle: apply soft fade-out to the last
            // `fade_n` frames of stretched_pre's region and a soft fade-in to the
            // first `fade_n` frames of stretched_post's region. Clamp so the fade
            // fits within each stretched buffer.
            let fade_n = silence_fade_n.min(pre_end).min(stretched_post_n);

            if fade_n > 0 {
                // Fade-out on tail of stretched_pre.
                for k in 0..fade_n {
                    let mix_idx = pre_end - fade_n + k;
                    let t = (k as f32 + 0.5) / fade_n as f32;
                    let cos_g = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
                    for c in 0..channels {
                        mix[mix_idx * channels + c] *= cos_g;
                    }
                }
            }

            // Copy stretched_post into mix[post_start..], with fade-in applied to
            // its first `fade_n` frames. Equal-power sin pair with the cos
            // fade-out above — together they form the canonical sin/cos
            // crossfade-around-silence (time-symmetric: reversing the audio
            // turns one into the other).
            for k in 0..stretched_post_n {
                let mix_idx = post_start + k;
                if mix_idx >= zone_n {
                    break;
                }
                let gain = if k < fade_n {
                    let t = (k as f32 + 0.5) / fade_n as f32;
                    (t * std::f32::consts::FRAC_PI_2).sin()
                } else {
                    1.0
                };
                for c in 0..channels {
                    mix[mix_idx * channels + c] = stretched_post[k * channels + c] * gain;
                }
            }
        }

        // ── Write into out[] with replace-then-crossfade-at-edges ──────────────
        //
        // The zone is [zone_start_frame, zone_start_frame + zone_n). At the left
        // edge we crossfade from existing out[] (which contains segs[i] at full
        // amplitude past its own fade-in) into mix. At the right edge we crossfade
        // from mix back into existing out[] (segs[i+1] at full amplitude before its
        // own fade-out). In the bulk we replace.
        let zone_start_s = gap_start_s - src_pre_s;
        let zone_start_frame = (zone_start_s * sr).round() as i64;

        tracing::info!(
            track = track_id,
            boundary = i,
            gap_master_s = format!("[{:.3}, {:.3}]", gap_start_s, gap_end_s),
            gap_s = format!("{:.3}", gap_s),
            stretched_pre_s = format!("{:.3}", stretched_pre_n as f64 / sr),
            stretched_post_s = format!("{:.3}", stretched_post_n as f64 / sr),
            meets,
            "gap-fill: stretched neighbour audio into gap"
        );

        // Edge fades sit fully inside the stretched buffers on each side so they
        // never reach into the residual-silence middle. Default xfade is 10 ms,
        // which is < margin in any realistic config, so clamping is just safety.
        let left_edge = xfade_n.min(stretched_pre_n).min(zone_n / 2);
        let right_edge = xfade_n.min(stretched_post_n).min(zone_n / 2);
        for f in 0..zone_n {
            let dst_frame = zone_start_frame + f as i64;
            if dst_frame < 0 || (dst_frame as usize) >= total_out_frames {
                continue;
            }
            let dst = dst_frame as usize;
            if left_edge > 0 && f < left_edge {
                let t = (f as f32 + 0.5) / left_edge as f32;
                let cos_g = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
                let sin_g = (t * std::f32::consts::FRAC_PI_2).sin();
                for c in 0..channels {
                    let existing = out[dst * channels + c];
                    let new_v = mix[f * channels + c];
                    out[dst * channels + c] = existing * cos_g + new_v * sin_g;
                }
            } else if right_edge > 0 && f >= zone_n - right_edge {
                // s = 0 at start of right-edge window, right_edge-1 at last frame.
                // mix fades 1 → 0, existing (segs[i+1]) fades 0 → 1.
                let s = f - (zone_n - right_edge);
                let t = (s as f32 + 0.5) / right_edge as f32;
                let cos_g = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
                let sin_g = (t * std::f32::consts::FRAC_PI_2).sin();
                for c in 0..channels {
                    let existing = out[dst * channels + c];
                    let new_v = mix[f * channels + c];
                    out[dst * channels + c] = new_v * cos_g + existing * sin_g;
                }
            } else {
                for c in 0..channels {
                    out[dst * channels + c] = mix[f * channels + c];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wav::float_spec;
    use crate::correlation::Segment;

    fn pcm_from_mono(samples: Vec<f32>, sr: u32) -> Pcm {
        Pcm {
            samples,
            spec: float_spec(1, sr),
        }
    }

    #[test]
    fn output_length_matches_master_duration() {
        let sr = 16_000u32;
        let donor_secs = 10.0_f32;
        let donor: Vec<f32> = (0..(donor_secs * sr as f32) as usize)
            .map(|i| (i as f32 / sr as f32).sin() * 0.1)
            .collect();
        let pcm = pcm_from_mono(donor, sr);

        let map = OffsetMap {
            segments: vec![Segment {
                master_start_s: 0.0,
                master_end_s: 5.0,
                donor_offset_s: 0.0,
            }],
            master_duration_s: 5.0,
            anchor_count: 1,
            rejected_count: 0,
            suppressed_jumps: 0,
            valid_master_start_s: 0.0,
            valid_master_end_s: 5.0,
        };
        let segs = build_segments(&map);
        let out = render(
            &pcm,
            &segs,
            map.master_duration_s as f64,
            map.valid_master_start_s as f64,
            map.valid_master_end_s as f64,
            0.020,
        );
        assert_eq!(out.len(), 5 * sr as usize);
    }

    #[test]
    fn master_silence_snap_picks_centred_silence() {
        // Boundary at 100 s, master silence [99.0, 101.0]. Width 2 > |Δ|=1, so the
        // snap is the silence centre = 100.0. Δ=-1 → gap_s=1.
        let silences = vec![SilenceInterval {
            start_s: 99.0,
            end_s: 101.0,
        }];
        let map = OffsetMap {
            segments: vec![
                Segment {
                    master_start_s: 0.0,
                    master_end_s: 100.0,
                    donor_offset_s: 0.0,
                },
                Segment {
                    master_start_s: 100.0,
                    master_end_s: 200.0,
                    donor_offset_s: -1.0,
                },
            ],
            master_duration_s: 200.0,
            anchor_count: 2,
            rejected_count: 0,
            suppressed_jumps: 0,
            valid_master_start_s: 0.0,
            valid_master_end_s: 200.0,
        };
        let decisions = master_snap_per_boundary(&map, &silences, 30.0);
        assert_eq!(decisions.len(), 1);
        let s = decisions[0].expect("expected master snap");
        assert!((s.snap_master_s - 100.0).abs() < 1e-6);
        assert!((s.gap_s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn master_silence_too_narrow_is_rejected() {
        // Width 0.5 < |Δ|=1 → reject.
        let silences = vec![SilenceInterval {
            start_s: 99.75,
            end_s: 100.25,
        }];
        let map = OffsetMap {
            segments: vec![
                Segment {
                    master_start_s: 0.0,
                    master_end_s: 100.0,
                    donor_offset_s: 0.0,
                },
                Segment {
                    master_start_s: 100.0,
                    master_end_s: 200.0,
                    donor_offset_s: -1.0,
                },
            ],
            master_duration_s: 200.0,
            anchor_count: 2,
            rejected_count: 0,
            suppressed_jumps: 0,
            valid_master_start_s: 0.0,
            valid_master_end_s: 200.0,
        };
        let decisions = master_snap_per_boundary(&map, &silences, 30.0);
        assert!(decisions[0].is_none());
    }

    #[test]
    fn classify_falls_back_to_at_boundary() {
        let segs = vec![
            AdjSegment {
                master_start_s: 0.0,
                master_end_s: 100.0,
                donor_offset_s: 0.0,
            },
            AdjSegment {
                master_start_s: 100.0,
                master_end_s: 200.0,
                donor_offset_s: -1.0,
            },
        ];
        let strategies = classify_strategies(&segs, &[None], &[], 30.0);
        match strategies[0] {
            SpliceStrategy::AtBoundary { gap_s } => assert!((gap_s - 1.0).abs() < 1e-6),
            other => panic!("expected AtBoundary, got {other:?}"),
        }
    }

    #[test]
    fn classify_prefers_master_silence_over_dub_silence() {
        let master = vec![SilenceInterval {
            start_s: 99.0,
            end_s: 101.0,
        }];
        let dub = vec![SilenceInterval {
            start_s: 105.0,
            end_s: 107.0,
        }];
        let map = OffsetMap {
            segments: vec![
                Segment {
                    master_start_s: 0.0,
                    master_end_s: 100.0,
                    donor_offset_s: 0.0,
                },
                Segment {
                    master_start_s: 100.0,
                    master_end_s: 200.0,
                    donor_offset_s: -1.0,
                },
            ],
            master_duration_s: 200.0,
            anchor_count: 2,
            rejected_count: 0,
            suppressed_jumps: 0,
            valid_master_start_s: 0.0,
            valid_master_end_s: 200.0,
        };
        let segs = build_segments(&map);
        let master_decisions = master_snap_per_boundary(&map, &master, 30.0);
        let strategies = classify_strategies(&segs, &master_decisions, &dub, 30.0);
        assert!(matches!(
            strategies[0],
            SpliceStrategy::InMasterSilence { .. }
        ));
    }

    #[test]
    fn segment_geometry_inserts_silence_gap_centred_on_snap() {
        let mut segs = vec![
            AdjSegment {
                master_start_s: 0.0,
                master_end_s: 100.0,
                donor_offset_s: 0.0,
            },
            AdjSegment {
                master_start_s: 100.0,
                master_end_s: 200.0,
                donor_offset_s: -1.0,
            },
        ];
        let strategies = vec![SpliceStrategy::InMasterSilence {
            snap_master_s: 100.0,
            gap_s: 1.0,
        }];
        apply_segment_geometry(&mut segs, &strategies, 0.020);
        assert!((segs[0].master_end_s - 99.5).abs() < 1e-6);
        assert!((segs[1].master_start_s - 100.5).abs() < 1e-6);
    }

    #[test]
    fn hardcut_overlap_avoids_zero_dip() {
        // Two adjacent segments meeting via HardCut (gap_s = 0). With the overlap
        // fix, the renderer's fade-out and fade-in produce equal-power summing
        // (sum gain ≈ 1) over the crossfade window — no near-silent dip at the
        // splice frame.
        let mut segs = vec![
            AdjSegment {
                master_start_s: 0.0,
                master_end_s: 100.0,
                donor_offset_s: 0.0,
            },
            AdjSegment {
                master_start_s: 100.0,
                master_end_s: 200.0,
                donor_offset_s: 0.0,
            },
        ];
        let strategies = vec![SpliceStrategy::AtBoundary { gap_s: 0.0 }];
        apply_segment_geometry(&mut segs, &strategies, 0.020);
        // After overlap, segs[0].master_end_s is 100 + 0.010, segs[1].master_start_s
        // is 100 - 0.010 — the segments overlap by 20 ms on the master timeline.
        assert!(
            (segs[0].master_end_s - 100.010).abs() < 1e-6,
            "segs[0].master_end_s = {}",
            segs[0].master_end_s
        );
        assert!(
            (segs[1].master_start_s - 99.990).abs() < 1e-6,
            "segs[1].master_start_s = {}",
            segs[1].master_start_s
        );
    }

    /// Defaults aligned with the CLI/`SpliceConfig` defaults so tests construct
    /// realistic configs without spelling out every field.
    fn test_splice_cfg() -> SpliceConfig {
        SpliceConfig {
            silence_db: -45.0,
            silence_min_ms: 200,
            snap_radius_s: 30.0,
            crossfade_ms: 10,
            smooth_gaps: true,
            gap_fill_margin_s: 1.0,
            speech_db: -25.0,
            gap_fill_max_ratio: 1.2,
            gap_fill_silence_fade_ms: 100,
        }
    }

    /// Spawn `rubberband --version` to detect whether the binary is on PATH. Tests
    /// that actually call `stretch_to_duration` short-circuit when this returns
    /// false so they pass on dev machines without a local rubberband.
    fn rubberband_available() -> bool {
        std::process::Command::new("rubberband")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Build a Pcm + two AdjSegments + an `out[]` rendered via the standard
    /// pipeline for a synthetic gap. Returns everything fill_gaps needs.
    ///
    /// Layout: segs[0] = master [0, gap_start_s] @ offset 0, segs[1] = master
    /// [gap_end_s, total_s] @ offset -gap_s. Donor occupies enough range to cover
    /// pre/post margin sampling either side of the gap.
    fn build_gap_fixture(
        sr: u32,
        pre_samples: &[f32],
        post_samples: &[f32],
        gap_s: f64,
    ) -> (Pcm, Vec<AdjSegment>, Vec<f32>) {
        // Master timeline: 5s segment, 1s gap (configurable), 5s segment.
        let body_s = 5.0_f64;
        let total_s = body_s + gap_s + body_s;
        let gap_start_s = body_s;
        let gap_end_s = body_s + gap_s;
        let pre_n = pre_samples.len();
        let post_n = post_samples.len();
        // Donor: 5s body up to gap_start, then immediately the post-side body. The
        // post slice sits at donor_t = gap_end + (-gap_s) = body_s in donor time,
        // overwriting the same region the pre body owned. To avoid that collision
        // we use distinct sample positions: pre fills the last `pre_n` samples of
        // segs[0]'s donor playback, post fills the first `post_n` samples after.
        let donor_total_n =
            ((total_s * sr as f64).round() as usize).max(pre_n + post_n + sr as usize);
        let mut donor = vec![0.0_f32; donor_total_n];
        // Pre sits at donor [gap_start_s - pre_n/sr, gap_start_s)
        let pre_donor_start = (gap_start_s * sr as f64).round() as usize - pre_n;
        donor[pre_donor_start..pre_donor_start + pre_n].copy_from_slice(pre_samples);
        // Post sits at donor [gap_start_s, gap_start_s + post_n/sr)  (because
        // segs[1] reads donor at master_t - gap_s = gap_end_s - gap_s = gap_start_s)
        let post_donor_start = (gap_start_s * sr as f64).round() as usize;
        let upper = (post_donor_start + post_n).min(donor.len());
        donor[post_donor_start..upper].copy_from_slice(&post_samples[..upper - post_donor_start]);

        let pcm = pcm_from_mono(donor, sr);
        let segs = vec![
            AdjSegment {
                master_start_s: 0.0,
                master_end_s: gap_start_s,
                donor_offset_s: 0.0,
            },
            AdjSegment {
                master_start_s: gap_end_s,
                master_end_s: total_s,
                donor_offset_s: -gap_s,
            },
        ];
        let out = render(&pcm, &segs, total_s, 0.0, total_s, 0.010);
        (pcm, segs, out)
    }

    /// Compute mean abs amplitude in a master-time window of `out[]`.
    fn rms_window(out: &[f32], sr: u32, start_s: f64, end_s: f64) -> f32 {
        let lo = (start_s * sr as f64).round() as usize;
        let hi = ((end_s * sr as f64).round() as usize).min(out.len());
        if hi <= lo {
            return 0.0;
        }
        let sum_sq: f64 = out[lo..hi].iter().map(|&s| (s as f64) * (s as f64)).sum();
        ((sum_sq / (hi - lo) as f64).sqrt()) as f32
    }

    #[test]
    fn fill_gaps_speech_in_pre_leaves_silence() {
        let sr = 16_000u32;
        let n = sr as usize; // 1 s
                             // Speech: 200 Hz sine at amplitude 0.1 → RMS ≈ -23 dBFS, ZCR ≈ 0.025 → Speech.
        let pre: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 200.0 * i as f32 / sr as f32).sin() * 0.1)
            .collect();
        // Ambient: alternating +/-0.01 → RMS -40 dBFS, ZCR ≈ 1.0 → Ambient.
        let post: Vec<f32> = (0..n)
            .map(|i| if i % 2 == 0 { 0.01 } else { -0.01 })
            .collect();
        let (pcm, segs, mut out) = build_gap_fixture(sr, &pre, &post, 0.5);
        let cfg = test_splice_cfg();
        let workspace = tempfile::tempdir().expect("tempdir");
        let total_s = 5.0 + 0.5 + 5.0;
        fill_gaps(
            &mut out,
            &pcm,
            &segs,
            total_s,
            0.0,
            total_s,
            &cfg,
            workspace.path(),
            0,
        );
        // Gap [5.0, 5.5] should remain digital zero — speech protection refuses
        // to stretch, leaving the rendered silence intact.
        let rms = rms_window(&out, sr, 5.05, 5.45);
        assert!(rms < 1e-6, "expected silent gap, got RMS={rms}");
    }

    #[test]
    fn fill_gaps_speech_in_post_leaves_silence() {
        let sr = 16_000u32;
        let n = sr as usize;
        let pre: Vec<f32> = (0..n)
            .map(|i| if i % 2 == 0 { 0.01 } else { -0.01 })
            .collect();
        let post: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 200.0 * i as f32 / sr as f32).sin() * 0.1)
            .collect();
        let (pcm, segs, mut out) = build_gap_fixture(sr, &pre, &post, 0.5);
        let cfg = test_splice_cfg();
        let workspace = tempfile::tempdir().expect("tempdir");
        let total_s = 5.0 + 0.5 + 5.0;
        fill_gaps(
            &mut out,
            &pcm,
            &segs,
            total_s,
            0.0,
            total_s,
            &cfg,
            workspace.path(),
            0,
        );
        let rms = rms_window(&out, sr, 5.05, 5.45);
        assert!(rms < 1e-6, "expected silent gap, got RMS={rms}");
    }

    #[test]
    fn fill_gaps_digital_zero_short_circuits() {
        let sr = 16_000u32;
        let n = sr as usize;
        let pre = vec![0.0_f32; n];
        let post = vec![0.0_f32; n];
        let (pcm, segs, mut out) = build_gap_fixture(sr, &pre, &post, 0.5);
        let cfg = test_splice_cfg();
        let workspace = tempfile::tempdir().expect("tempdir");
        let total_s = 5.0 + 0.5 + 5.0;
        fill_gaps(
            &mut out,
            &pcm,
            &segs,
            total_s,
            0.0,
            total_s,
            &cfg,
            workspace.path(),
            0,
        );
        // Both neighbours are digital zero → fill_gaps must skip. No rubberband
        // invoked (verifies the early return path works without the binary).
        let rms = rms_window(&out, sr, 5.05, 5.45);
        assert!(rms < 1e-6, "expected silent gap, got RMS={rms}");
    }

    #[test]
    fn fill_gaps_zero_margin_returns_immediately() {
        let sr = 16_000u32;
        let n = sr as usize;
        let pre: Vec<f32> = (0..n)
            .map(|i| if i % 2 == 0 { 0.01 } else { -0.01 })
            .collect();
        let post = pre.clone();
        let (pcm, segs, mut out) = build_gap_fixture(sr, &pre, &post, 0.5);
        let mut cfg = test_splice_cfg();
        cfg.gap_fill_margin_s = 0.0;
        let workspace = tempfile::tempdir().expect("tempdir");
        let total_s = 5.0 + 0.5 + 5.0;
        let snapshot = out.clone();
        fill_gaps(
            &mut out,
            &pcm,
            &segs,
            total_s,
            0.0,
            total_s,
            &cfg,
            workspace.path(),
            0,
        );
        assert_eq!(out, snapshot, "fill_gaps with margin=0 must be a no-op");
    }

    #[test]
    fn fill_gaps_ambient_neighbours_writes_into_gap() {
        if !rubberband_available() {
            eprintln!("skipping: rubberband not in PATH");
            return;
        }
        let sr = 16_000u32;
        let n = sr as usize;
        // Constant low-amplitude alternating signal → broadband Ambient.
        let pre: Vec<f32> = (0..n)
            .map(|i| if i % 2 == 0 { 0.02 } else { -0.02 })
            .collect();
        let post = pre.clone();
        let (pcm, segs, mut out) = build_gap_fixture(sr, &pre, &post, 0.3);
        let cfg = test_splice_cfg();
        let workspace = tempfile::tempdir().expect("tempdir");
        let total_s = 5.0 + 0.3 + 5.0;
        fill_gaps(
            &mut out,
            &pcm,
            &segs,
            total_s,
            0.0,
            total_s,
            &cfg,
            workspace.path(),
            0,
        );
        // gap=0.3 < 2*M*(R-1)=0.4 → full coverage path. Gap region (excluding
        // tiny xfade edges) must contain stretched content.
        let rms = rms_window(&out, sr, 5.05, 5.25);
        assert!(
            rms > 1e-3,
            "expected stretched content in gap, got RMS={rms}"
        );
    }

    #[test]
    fn fill_gaps_ratio_cap_leaves_residual_middle() {
        if !rubberband_available() {
            eprintln!("skipping: rubberband not in PATH");
            return;
        }
        let sr = 16_000u32;
        let margin_n = sr as usize; // 1 s margin
                                    // Wide ambient buffer either side.
        let pre: Vec<f32> = (0..margin_n)
            .map(|i| if i % 2 == 0 { 0.02 } else { -0.02 })
            .collect();
        let post = pre.clone();
        // gap = 1.0 s > 2*M*(R-1) = 0.4 s coverage → 0.6 s residual silence.
        let gap_s = 1.0;
        let (pcm, segs, mut out) = build_gap_fixture(sr, &pre, &post, gap_s);
        let cfg = test_splice_cfg();
        let workspace = tempfile::tempdir().expect("tempdir");
        let total_s = 5.0 + gap_s + 5.0;
        fill_gaps(
            &mut out,
            &pcm,
            &segs,
            total_s,
            0.0,
            total_s,
            &cfg,
            workspace.path(),
            0,
        );
        // Just inside the gap, near pre's extension end (gap_start + ~0.15 s):
        // stretched content should still be present.
        let rms_pre_ext = rms_window(&out, sr, 5.05, 5.15);
        assert!(
            rms_pre_ext > 1e-3,
            "pre-side extension should have content, got RMS={rms_pre_ext}"
        );
        // Middle of the gap (after the fade-out, before fade-in): residual silence.
        // With ratio cap 1.2 the stretched extensions reach to gap_start+0.2 and
        // gap_end-0.2; with silence_fade 100 ms the fades occupy
        // [gap_start+0.1, gap_start+0.2] and [gap_end-0.2, gap_end-0.1]. So the
        // window [gap_start+0.25, gap_end-0.25] = [5.25, 5.75] must be pure silence.
        let rms_mid = rms_window(&out, sr, 5.30, 5.70);
        assert!(
            rms_mid < 1e-6,
            "residual middle should be silent, got RMS={rms_mid}"
        );
    }

    #[test]
    fn at_boundary_renders_silence_gap_for_negative_delta() {
        let sr = 16_000u32;
        let donor: Vec<f32> = (0..(20.0 * sr as f32) as usize).map(|_| 0.5).collect();
        let pcm = pcm_from_mono(donor, sr);
        let segs = vec![
            AdjSegment {
                master_start_s: 0.0,
                master_end_s: 9.5, // 0.5 s gap inserted by AtBoundary { gap_s = 1.0 }
                donor_offset_s: 0.0,
            },
            AdjSegment {
                master_start_s: 10.5,
                master_end_s: 20.0,
                donor_offset_s: -1.0,
            },
        ];
        let out = render(&pcm, &segs, 20.0, 0.0, 20.0, 0.020);
        // Mid-gap at master 10s — must be silent.
        let f = (10.0 * sr as f32) as usize;
        assert_eq!(out[f], 0.0);
        // Bodies should be ~0.5.
        assert!((out[(5.0 * sr as f32) as usize] - 0.5).abs() < 1e-3);
        assert!((out[(15.0 * sr as f32) as usize] - 0.5).abs() < 1e-3);
    }
}
