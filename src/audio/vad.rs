use crate::audio::wav::Pcm;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct SilenceInterval {
    pub start_s: f32,
    pub end_s: f32,
}

/// Three-way classification used by the optional gap-fill path to decide whether a
/// neighbouring buffer can safely be time-stretched. We deliberately keep the API
/// agnostic to the underlying detector so a future Silero VAD upgrade slots in
/// without changing the splicer call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Buffer is below the silence floor end-to-end — nothing useful to stretch.
    Silent,
    /// Energy and ZCR consistent with broadband ambience or music. Safe to stretch.
    Ambient,
    /// Likely speech. The gap filler must NOT stretch this — falls back to silence.
    Speech,
}

/// Classify an interleaved PCM buffer as Silent / Ambient / Speech using a simple
/// energy + zero-crossing-rate heuristic on a downmixed mono view. This is the
/// fallback detector when Silero VAD is unavailable; the same `Verdict` shape is
/// returned in either case so the gap-filler is detector-agnostic.
///
/// Heuristic rationale:
/// - **Silent** (RMS < `silence_db`): nothing to stretch; gap fill aborts and the
///   region stays at the buffer's zero initialisation.
/// - **Speech**: voiced speech has a distinctive *low* zero-crossing rate (vocal
///   cords drive the spectrum below ~4 kHz where energy concentrates) and a
///   middle-ish RMS level. Broadband noise / music tends to have ZCR > 0.15 and
///   often higher RMS extremes. We mark a buffer as `Speech` when RMS exceeds
///   `speech_db` AND ZCR is below 0.15 — both conditions to reduce false positives
///   on loud music with tonal content.
/// - **Ambient**: anything else (in-between RMS, or high ZCR even when loud).
pub fn classify_buffer(
    samples: &[f32],
    channels: usize,
    silence_db: f32,
    speech_db: f32,
) -> Verdict {
    if samples.is_empty() || channels == 0 {
        return Verdict::Silent;
    }
    let frames = samples.len() / channels;
    if frames == 0 {
        return Verdict::Silent;
    }

    let mut sum_sq = 0.0_f64;
    let mut zero_crossings = 0u64;
    let mut prev_mono: f32 = 0.0;
    for f in 0..frames {
        // Average channels for the mono view used by both metrics.
        let mut sum = 0.0_f32;
        for c in 0..channels {
            sum += samples[f * channels + c];
        }
        let mono = sum / channels as f32;
        sum_sq += (mono as f64) * (mono as f64);
        if f > 0 && (prev_mono.signum() != mono.signum()) && mono != 0.0 && prev_mono != 0.0 {
            zero_crossings += 1;
        }
        prev_mono = mono;
    }
    let rms = (sum_sq / frames as f64).sqrt() as f32;
    let rms_db = 20.0 * rms.max(1e-9_f32).log10();
    // Crossings per sample. Speech voiced regions sit around 0.02–0.10; music
    // typically 0.05–0.15; broadband noise > 0.20.
    let zcr = zero_crossings as f32 / frames.max(1) as f32;

    if rms_db < silence_db {
        Verdict::Silent
    } else if rms_db >= speech_db && zcr < 0.15 {
        Verdict::Speech
    } else {
        Verdict::Ambient
    }
}

/// RMS-based silence detection on an interleaved PCM buffer.
/// Frames below `silence_db` (dBFS) lasting at least `min_duration_s` are returned as
/// `SilenceInterval`s in source-track time.
pub fn detect_silence(pcm: &Pcm, silence_db: f32, min_duration_s: f32) -> Vec<SilenceInterval> {
    detect_silence_inner(
        &pcm.samples,
        pcm.channels(),
        pcm.sample_rate(),
        silence_db,
        min_duration_s,
    )
}

/// Same algorithm as [`detect_silence`], but operating on a raw mono `f32` buffer.
/// Used for master-anchor silence detection where we already have the PCM in memory
/// from the correlation pipeline and don't want to clone it into a `Pcm`.
pub fn detect_silence_mono(
    samples: &[f32],
    sample_rate: u32,
    silence_db: f32,
    min_duration_s: f32,
) -> Vec<SilenceInterval> {
    detect_silence_inner(samples, 1, sample_rate, silence_db, min_duration_s)
}

fn detect_silence_inner(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    silence_db: f32,
    min_duration_s: f32,
) -> Vec<SilenceInterval> {
    let sr = sample_rate as f32;
    let hop_s = 0.020_f32; // 20 ms
    let hop_frames = (hop_s * sr).round().max(1.0) as usize;
    let total_frames = samples.len() / channels.max(1);

    let threshold = 10f32.powf(silence_db / 20.0); // dBFS → linear amplitude
    let threshold_sq = (threshold * threshold) as f64;

    let mut silent_flags = Vec::with_capacity(total_frames / hop_frames + 1);
    let mut frame = 0usize;
    while frame < total_frames {
        let end = (frame + hop_frames).min(total_frames);
        let mut sum_sq = 0.0_f64;
        let mut n = 0usize;
        for f in frame..end {
            for c in 0..channels {
                let s = samples[f * channels + c] as f64;
                sum_sq += s * s;
                n += 1;
            }
        }
        let mean_sq = if n == 0 { 0.0 } else { sum_sq / n as f64 };
        silent_flags.push(mean_sq < threshold_sq);
        frame = end;
    }

    // Coalesce contiguous silent hops into intervals; drop those shorter than min_duration_s.
    let mut intervals = Vec::new();
    let mut run_start: Option<usize> = None;
    for (i, &silent) in silent_flags.iter().enumerate() {
        match (silent, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(start)) => {
                push_if_long_enough(&mut intervals, start, i, hop_s, min_duration_s);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        push_if_long_enough(
            &mut intervals,
            start,
            silent_flags.len(),
            hop_s,
            min_duration_s,
        );
    }

    tracing::debug!(
        intervals = intervals.len(),
        threshold_db = silence_db,
        min_ms = (min_duration_s * 1000.0) as u32,
        "VAD pass complete"
    );
    intervals
}

fn push_if_long_enough(
    intervals: &mut Vec<SilenceInterval>,
    start_hop: usize,
    end_hop: usize,
    hop_s: f32,
    min_duration_s: f32,
) {
    let start_s = start_hop as f32 * hop_s;
    let end_s = end_hop as f32 * hop_s;
    if end_s - start_s >= min_duration_s {
        intervals.push(SilenceInterval { start_s, end_s });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wav::float_spec;

    fn make_tone_then_silence_then_tone(sr: u32) -> Pcm {
        let total_s = 6.0_f32;
        let frames = (total_s * sr as f32) as usize;
        let mut samples = Vec::with_capacity(frames);
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let env = if t < 2.0 {
                1.0
            } else if t < 4.0 {
                0.0
            } else {
                1.0
            };
            samples.push((2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5 * env);
        }
        Pcm {
            samples,
            spec: float_spec(1, sr),
        }
    }

    #[test]
    fn finds_middle_silence() {
        let pcm = make_tone_then_silence_then_tone(16_000);
        let silences = detect_silence(&pcm, -45.0, 0.2);
        assert_eq!(silences.len(), 1, "expected one silence interval");
        let s = silences[0];
        assert!(
            s.start_s > 1.9 && s.start_s < 2.2,
            "start was {}",
            s.start_s
        );
        assert!(s.end_s > 3.9 && s.end_s < 4.1, "end was {}", s.end_s);
    }

    #[test]
    fn classify_buffer_silence_speech_ambient() {
        let sr = 48_000u32;
        let n = (sr as usize) / 2; // 0.5 s

        // Silent: zeros.
        let silent: Vec<f32> = vec![0.0; n];
        assert_eq!(classify_buffer(&silent, 1, -45.0, -25.0), Verdict::Silent);

        // Speech-like: 200 Hz sine at -15 dBFS (loud, low ZCR).
        let amp = 10f32.powf(-15.0 / 20.0);
        let speechy: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 200.0 * i as f32 / sr as f32).sin() * amp)
            .collect();
        assert_eq!(classify_buffer(&speechy, 1, -45.0, -25.0), Verdict::Speech);

        // Ambient-like: white noise at -30 dBFS (broadband, high ZCR, mid energy).
        let mut state: u64 = 0xdead_beef;
        let amp_n = 10f32.powf(-30.0 / 20.0);
        let noise: Vec<f32> = (0..n)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((state >> 33) as f32 / u32::MAX as f32 - 0.5) * 2.0 * amp_n
            })
            .collect();
        assert_eq!(classify_buffer(&noise, 1, -45.0, -25.0), Verdict::Ambient);
    }
}
