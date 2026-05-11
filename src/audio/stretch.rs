//! Time-stretching wrapper around the external `rubberband` CLI.
//!
//! Two entry points:
//! - [`stretch_to_duration`] — in-memory stretch of an `&[f32]` buffer to an exact
//!   target duration. Used by the gap-fill path (`--smooth-gaps`) where the stretch
//!   is small (≤1 s neighbour material) and needs frame-accurate output length.
//! - [`stretch_file`] — file-to-file stretch with progress polling (output WAV size)
//!   and optional pitch shift. Used by the global fps-normalize phase where inputs
//!   can be hour-long episodes and the user needs a live progress bar + ETA.
//!
//! Both call `rubberband -D <target_seconds>`, so subprocess invariants from
//! [`crate::binpath`] (CREATE_NO_WINDOW on Windows) apply uniformly.

use crate::audio::wav;
use crate::error::{DubsyncError, Result};
use hound::WavSpec;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Minimum and maximum stretch ratio (target_duration / input_duration) we'll attempt.
/// Beyond ~3× artefacts dominate even with rubberband's high-quality engine; below
/// 0.33× the algorithm runs out of overlap material. Practically the gap-fill path
/// keeps the ratio close to 1 since `gap_fill_margin_s` is configured to be at least
/// half the gap length.
const MIN_RATIO: f64 = 1.0 / 3.0;
const MAX_RATIO: f64 = 3.0;

/// Options for [`stretch_file`].
pub struct StretchOpts {
    /// Target output duration in seconds. Passed to rubberband as `-D <seconds>`.
    pub target_duration_s: f64,
    /// Optional pitch shift in semitones. None preserves pitch (rubberband's default).
    /// For PAL-undo (donor 25→24): pass `Some(12.0 * (24.0/25.0).log2())` ≈ -0.7067.
    pub pitch_semitones: Option<f64>,
}

/// Stretch `samples` to occupy exactly `target_duration_s` seconds at `sample_rate`.
///
/// Returns the stretched buffer in the same interleaved layout. The caller is
/// expected to keep the output `(target_duration_s * sample_rate).round() as usize`
/// frames; rubberband's `-D` flag is sample-accurate to within a few frames, so we
/// trim/zero-pad to the exact target length before returning.
pub fn stretch_to_duration(
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
    target_duration_s: f64,
    workspace: &Path,
) -> Result<Vec<f32>> {
    if channels == 0 {
        return Err(DubsyncError::RubberbandFailed {
            stderr: "stretch_to_duration called with channels=0".to_string(),
        });
    }
    let chans = channels as usize;
    let in_frames = samples.len() / chans;
    if in_frames == 0 || target_duration_s <= 0.0 {
        return Ok(Vec::new());
    }
    let in_dur_s = in_frames as f64 / sample_rate as f64;
    let ratio = target_duration_s / in_dur_s;
    if !(MIN_RATIO..=MAX_RATIO).contains(&ratio) {
        return Err(DubsyncError::RubberbandFailed {
            stderr: format!(
                "stretch ratio {ratio:.3} outside acceptable range [{MIN_RATIO:.3}, {MAX_RATIO:.3}] \
                 (input {in_dur_s:.3}s → target {target_duration_s:.3}s)"
            ),
        });
    }

    // RAII temp file pair, cleaned up on Drop even if the rubberband call fails.
    let temps = StretchTemps::new(workspace)?;
    let in_spec = WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    wav::write_interleaved_f32(&temps.input, samples, &in_spec)?;

    stretch_file(
        &temps.input,
        &temps.output,
        StretchOpts {
            target_duration_s,
            pitch_semitones: None,
        },
        &|_| {},
    )?;

    let stretched = wav::read_interleaved_f32(&temps.output)?;
    if stretched.channels() != chans || stretched.sample_rate() != sample_rate {
        return Err(DubsyncError::RubberbandFailed {
            stderr: format!(
                "rubberband output spec mismatch: expected {channels}ch/{sample_rate}Hz, got \
                 {}ch/{}Hz",
                stretched.channels(),
                stretched.sample_rate()
            ),
        });
    }

    // Trim or zero-pad to the exact target frame count. rubberband -D is typically
    // accurate to within a few frames; the splice math downstream depends on the
    // length being exact so a deterministic clamp keeps the renderer simple.
    let target_frames = (target_duration_s * sample_rate as f64).round() as usize;
    let mut buf = stretched.samples;
    let target_len = target_frames * chans;
    if buf.len() > target_len {
        buf.truncate(target_len);
    } else if buf.len() < target_len {
        buf.resize(target_len, 0.0);
    }
    Ok(buf)
}

/// File-to-file stretch with live progress.
///
/// Spawns `rubberband -D <target> [-p <semitones>] -q <in> <out>` as a subprocess
/// and polls the output WAV file size every ~200 ms to estimate completion fraction.
/// The estimate is `(out_size_bytes - 44) / (expected_size_bytes - 44)` (44 = WAV
/// header); rubberband writes streamingly so the size grows monotonically.
///
/// `progress(fraction)` is called from a background thread with values in `[0.0, 0.99]`
/// while running, then `1.0` exactly once when the subprocess exits cleanly. The
/// callback must be `Send + Sync` and is invoked at most ~5 Hz.
pub fn stretch_file(
    in_wav: &Path,
    out_wav: &Path,
    opts: StretchOpts,
    progress: &(dyn Fn(f32) + Sync),
) -> Result<()> {
    let info = wav::probe_frames(in_wav)?;
    let in_frames = info.frames;
    let in_dur_s = in_frames as f64 / info.sample_rate as f64;
    if in_frames == 0 || opts.target_duration_s <= 0.0 {
        return Err(DubsyncError::RubberbandFailed {
            stderr: format!(
                "empty input or non-positive target ({} frames, target {:.3}s)",
                in_frames, opts.target_duration_s
            ),
        });
    }
    let ratio = opts.target_duration_s / in_dur_s;
    if !(MIN_RATIO..=MAX_RATIO).contains(&ratio) {
        return Err(DubsyncError::RubberbandFailed {
            stderr: format!(
                "stretch ratio {ratio:.3} outside acceptable range [{MIN_RATIO:.3}, {MAX_RATIO:.3}] \
                 (input {in_dur_s:.3}s → target {:.3}s)",
                opts.target_duration_s
            ),
        });
    }

    // Expected bytes: WAV header (44) + (frames × channels × 4 bytes/f32). Used as
    // the denominator of the progress fraction.
    let expected_frames = (in_frames as f64 * ratio).round() as u64;
    let expected_bytes = 44 + expected_frames * info.channels as u64 * 4;

    let mut cmd = crate::binpath::rubberband_command();
    cmd.arg("-D")
        .arg(format!("{:.6}", opts.target_duration_s))
        .arg("-q");
    if let Some(semitones) = opts.pitch_semitones {
        cmd.arg("-p").arg(format!("{semitones:.6}"));
    }
    cmd.arg(in_wav).arg(out_wav);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let started_at = Instant::now();
    let mut child = cmd.spawn()?;

    // Background poller scoped via `std::thread::scope` so it can borrow `progress`
    // without 'static. The poller exits when `stop` flips; we set `stop` after
    // `child.wait()` returns.
    let stop = Arc::new(AtomicBool::new(false));
    let out_path = out_wav.to_path_buf();
    const RAMP_GRACE_S: f64 = 2.0; // suppress fraction emissions during the first 2 s

    let status = std::thread::scope(|s| -> std::io::Result<std::process::ExitStatus> {
        let stop_for_thread = stop.clone();
        s.spawn(move || {
            while !stop_for_thread.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                if started_at.elapsed().as_secs_f64() < RAMP_GRACE_S {
                    continue;
                }
                let bytes = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
                if bytes < 44 || expected_bytes <= 44 {
                    continue;
                }
                let frac =
                    ((bytes - 44) as f64 / (expected_bytes - 44) as f64).clamp(0.0, 0.99) as f32;
                progress(frac);
            }
        });
        let status = child.wait();
        stop.store(true, Ordering::Relaxed);
        status
    })?;

    if !status.success() {
        let mut stderr_buf = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut stderr_buf);
        }
        return Err(DubsyncError::RubberbandFailed { stderr: stderr_buf });
    }

    progress(1.0);
    Ok(())
}

struct StretchTemps {
    input: PathBuf,
    output: PathBuf,
}

impl StretchTemps {
    fn new(workspace: &Path) -> Result<Self> {
        // Process-id + nanos give us per-call uniqueness without pulling in the
        // `uuid` crate. Multiple gap-fills running in parallel via rayon need
        // different filenames so they don't stomp on each other.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let stem = format!("rubberband_{pid}_{nanos}");
        Ok(Self {
            input: workspace.join(format!("{stem}_in.wav")),
            output: workspace.join(format!("{stem}_out.wav")),
        })
    }
}

impl Drop for StretchTemps {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.input);
        let _ = std::fs::remove_file(&self.output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn stretch_preserves_length_and_energy() {
        let sr = 16_000u32;
        let in_dur_s = 1.0_f64;
        let out_dur_s = 1.5_f64;
        let n = (in_dur_s * sr as f64) as usize;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
            })
            .collect();
        let dir = tempdir().expect("tempdir");

        // Skip if rubberband is not installed locally — this test runs only when the
        // dev environment has it (CI installs it as a release-archive dep).
        if which_rubberband().is_none() {
            eprintln!("skipping stretch_preserves_length_and_energy: rubberband not in PATH");
            return;
        }

        let out = stretch_to_duration(&samples, 1, sr, out_dur_s, dir.path())
            .expect("stretch should succeed");
        let target_frames = (out_dur_s * sr as f64).round() as usize;
        assert_eq!(out.len(), target_frames, "output length must match target");

        let in_energy: f32 = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
        let out_energy: f32 = out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32;
        let ratio = out_energy / in_energy.max(1e-9);
        assert!(
            (0.5..=1.5).contains(&ratio),
            "energy ratio out/in = {ratio} outside [0.5, 1.5]"
        );
    }

    #[test]
    fn rejects_extreme_stretch_ratios() {
        let sr = 16_000u32;
        let samples = vec![0.0_f32; sr as usize]; // 1 s of silence
        let dir = tempdir().expect("tempdir");
        // 5× — above MAX_RATIO. Should fail before invoking rubberband, so the test
        // passes even on systems without rubberband installed.
        let res = stretch_to_duration(&samples, 1, sr, 5.0, dir.path());
        assert!(matches!(res, Err(DubsyncError::RubberbandFailed { .. })));
    }

    fn which_rubberband() -> Option<PathBuf> {
        // Quick PATH probe: the `binpath::rubberband()` resolver returns a bare name
        // when nothing is found next-to-exe, so we have to spawn it to confirm.
        match std::process::Command::new("rubberband")
            .arg("--version")
            .output()
        {
            Ok(out) if out.status.success() => Some(PathBuf::from("rubberband")),
            _ => None,
        }
    }
}
