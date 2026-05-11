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
    /// stays as silence. Default OFF — opt-in feature.
    pub smooth_gaps: bool,
    /// Length of dub audio sampled before AND after each gap as the stretch source.
    /// Wider = more material to stretch (less artefact), but more risk of catching
    /// dialog (which forces fallback to silence). Default 0.5 s.
    pub gap_fill_margin_s: f32,
    /// Above this dBFS level a neighbour buffer is treated as speech and the gap
    /// stays at silence. RMS+ZCR fallback only; ignored if Silero VAD is in use.
    pub speech_db: f32,
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

/// Optional gap-fill pass. For every interior boundary in `segments`, if there's a
/// real gap between `segs[i].master_end_s` and `segs[i+1].master_start_s`, sample
/// the donor neighbours, classify them with VAD, and (if neither is dominated by
/// speech) replace the silent gap with a rubberband-stretched copy of the neighbour
/// material. Errors are logged but never fatal — the gap simply stays as silence.
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
    let xfade = (crossfade_s(cfg) * sr).round() as usize;
    let margin_s = cfg.gap_fill_margin_s.max(0.0) as f64;
    if margin_s <= 0.0 {
        return;
    }
    let margin_frames = (margin_s * sr).round() as usize;

    for i in 0..segments.len() - 1 {
        let gap_start_s = segments[i].master_end_s;
        let gap_end_s = segments[i + 1].master_start_s;
        let gap_s = gap_end_s - gap_start_s;
        if gap_s <= 1.0 / sr {
            continue; // no real gap (HardCut overlap or collapsed boundary)
        }

        // Donor frames either side of the gap. The pre-gap donor position is the last
        // sample played by segs[i] (master_end + offset_a); the post-gap position is
        // the first sample played by segs[i+1] (master_start + offset_b).
        let pre_donor_end = ((gap_start_s + segments[i].donor_offset_s) * sr).round() as i64;
        let post_donor_start = ((gap_end_s + segments[i + 1].donor_offset_s) * sr).round() as i64;

        let pre_donor_lo = (pre_donor_end - margin_frames as i64).max(0) as usize;
        let pre_donor_hi = pre_donor_end.max(0) as usize;
        let post_donor_lo = post_donor_start.max(0) as usize;
        let post_donor_hi = (post_donor_start + margin_frames as i64).max(0) as usize;
        let total_frames = pcm.frames();
        let pre_donor_hi = pre_donor_hi.min(total_frames);
        let post_donor_hi = post_donor_hi.min(total_frames);

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

        // Build the stretch source by concatenating pre + post neighbour samples.
        // Total source length = pre_len + post_len frames; target length covers the
        // gap plus xfade headroom on each side so we can crossfade into segs.
        let pre_frames = pre_donor_hi - pre_donor_lo;
        let post_frames = post_donor_hi - post_donor_lo;
        let mut source = Vec::with_capacity((pre_frames + post_frames) * channels);
        source.extend_from_slice(pre_slice);
        source.extend_from_slice(post_slice);

        let target_duration_s = gap_s + 2.0 * crossfade_s(cfg);
        let stretched = match stretch::stretch_to_duration(
            &source,
            channels as u16,
            pcm.sample_rate(),
            target_duration_s,
            workspace,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    track = track_id,
                    boundary = i,
                    error = %e,
                    "gap-fill: rubberband failed — leaving silence"
                );
                continue;
            }
        };

        // Place the stretched buffer with crossfade headroom on each side. Leading
        // xfade overlaps segs[i]'s rendered fade-out (which is already ramping
        // 1→0), and trailing xfade overlaps segs[i+1]'s rendered fade-in.
        let zone_start_s = gap_start_s - crossfade_s(cfg);
        let zone_start = (zone_start_s * sr).round() as i64;
        let zone_frames = stretched.len() / channels;

        tracing::info!(
            track = track_id,
            boundary = i,
            gap_master_s = format!("[{:.3}, {:.3}]", gap_start_s, gap_end_s),
            gap_s = format!("{:.3}", gap_s),
            stretched_frames = zone_frames,
            "gap-fill: stretched neighbour audio into gap"
        );

        for f in 0..zone_frames {
            let dst_frame = zone_start + f as i64;
            if dst_frame < 0 || (dst_frame as usize) >= out.len() / channels {
                continue;
            }
            // Equal-power sin crossfade at both ends so the stretched buffer blends
            // with segs[i]'s fade-out (left) and segs[i+1]'s fade-in (right).
            let mut gain = 1.0_f32;
            if xfade > 0 && f < xfade {
                let t = (f as f32 + 0.5) / xfade as f32;
                gain *= (t * std::f32::consts::FRAC_PI_2).sin();
            }
            if xfade > 0 && zone_frames > xfade && f >= zone_frames - xfade {
                let pos = zone_frames - 1 - f;
                let t = (pos as f32 + 0.5) / xfade as f32;
                gain *= (t * std::f32::consts::FRAC_PI_2).sin();
            }
            for c in 0..channels {
                out[dst_frame as usize * channels + c] += stretched[f * channels + c] * gain;
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
