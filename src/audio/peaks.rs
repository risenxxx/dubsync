use serde::Serialize;

/// Hop size for RMS frames. Matches `vad.rs` so the dBFS scale is directly comparable
/// between silence intervals and onset events.
const HOP_S: f32 = 0.020;

/// Frames whose dBFS sits below this floor are ignored even if the relative jump
/// crosses `min_jump_db` — prevents flagging noise-floor wobble in long quiet
/// passages where any tiny burst looks like a "huge" jump from −∞ dB.
const ONSET_NOISE_FLOOR_DB: f32 = -60.0;

/// A detected RMS jump in a mono anchor signal. `time_s` is the master/donor timeline
/// timestamp at the start of the frame whose dBFS exceeded the prior floor by `jump_db`.
/// Cross-referencing master onsets against donor onsets gives ground-truth offset
/// estimates at sample accuracy, independent of any sliding-window heuristic.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OnsetEvent {
    pub time_s: f32,
    pub rms_db: f32,
    pub jump_db: f32,
}

/// Detect frames where RMS rises by at least `min_jump_db` from the floor over the
/// preceding `refractory_s` seconds. After firing, suppress further detections for
/// `refractory_s` so a single onset doesn't emit a burst of events.
pub fn detect_onsets(
    samples: &[f32],
    sample_rate: u32,
    min_jump_db: f32,
    refractory_s: f32,
) -> Vec<OnsetEvent> {
    let sr = sample_rate as f32;
    let hop_frames = (HOP_S * sr).round().max(1.0) as usize;
    let refractory_hops = (refractory_s / HOP_S).round().max(1.0) as usize;

    // Compute one dBFS value per hop frame.
    let mut frame_db: Vec<f32> = Vec::with_capacity(samples.len() / hop_frames + 1);
    let mut frame = 0usize;
    while frame < samples.len() {
        let end = (frame + hop_frames).min(samples.len());
        let mut sum_sq = 0.0_f64;
        for s in &samples[frame..end] {
            let v = *s as f64;
            sum_sq += v * v;
        }
        let n = end - frame;
        let rms = if n == 0 {
            0.0
        } else {
            (sum_sq / n as f64).sqrt()
        };
        let db = 20.0 * (rms.max(1e-9_f64)).log10();
        frame_db.push(db as f32);
        frame = end;
    }

    // Floor = minimum dBFS over the preceding `refractory_hops` frames. An onset at
    // frame `i` fires when `frame_db[i] - floor[i-1..] >= min_jump_db` and we're
    // outside the refractory window from the last fired event.
    let mut events = Vec::new();
    let mut last_fire_hop: Option<usize> = None;
    for i in 1..frame_db.len() {
        if let Some(last) = last_fire_hop {
            if i - last < refractory_hops {
                continue;
            }
        }
        let lo = i.saturating_sub(refractory_hops);
        let floor = frame_db[lo..i]
            .iter()
            .copied()
            .fold(f32::INFINITY, f32::min);
        let jump = frame_db[i] - floor;
        if jump >= min_jump_db && frame_db[i] > ONSET_NOISE_FLOOR_DB {
            events.push(OnsetEvent {
                time_s: i as f32 * HOP_S,
                rms_db: frame_db[i],
                jump_db: jump,
            });
            last_fire_hop = Some(i);
        }
    }

    tracing::debug!(
        events = events.len(),
        min_jump_db,
        refractory_s,
        "onset detection complete"
    );
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tone burst on top of a silent floor: should produce exactly one onset at the
    /// burst start.
    #[test]
    fn detects_onset_after_silence() {
        let sr = 16_000u32;
        let total_s = 4.0_f32;
        let burst_start_s = 2.0_f32;
        let n = (total_s * sr as f32) as usize;
        let burst_start = (burst_start_s * sr as f32) as usize;
        let mut samples = vec![0.0_f32; n];
        for (i, s) in samples.iter_mut().enumerate().skip(burst_start) {
            let t = i as f32 / sr as f32;
            *s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
        }
        let onsets = detect_onsets(&samples, sr, 12.0, 0.5);
        assert!(
            !onsets.is_empty(),
            "expected at least one onset for tone-after-silence"
        );
        let first = onsets[0];
        assert!(
            (first.time_s - burst_start_s).abs() < 0.1,
            "onset time {} not near {}",
            first.time_s,
            burst_start_s
        );
        assert!(
            first.jump_db >= 12.0,
            "jump_db too small: {}",
            first.jump_db
        );
    }

    /// Two bursts with refractory: only one onset per burst (no double-fires inside one
    /// burst due to RMS jitter).
    #[test]
    fn refractory_suppresses_repeat_fires() {
        let sr = 16_000u32;
        let total_s = 6.0_f32;
        let n = (total_s * sr as f32) as usize;
        let mut samples = vec![0.0_f32; n];
        for (i, s) in samples.iter_mut().enumerate() {
            let t = i as f32 / sr as f32;
            let on = (1.0..=1.5).contains(&t) || (4.0..=4.5).contains(&t);
            if on {
                *s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            }
        }
        let onsets = detect_onsets(&samples, sr, 12.0, 0.5);
        assert_eq!(
            onsets.len(),
            2,
            "expected exactly two onsets, got {:?}",
            onsets
        );
    }
}
