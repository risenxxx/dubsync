use crate::error::Result;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::path::Path;

/// Read a mono f32 WAV (the format we always extract anchors as).
/// Returns the samples plus the sample rate.
pub fn read_mono_f32(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader = WavReader::open(path)?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;

    let samples: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, 32) => reader.samples::<f32>().collect::<hound::Result<Vec<_>>>()?,
        (SampleFormat::Int, bits) => {
            // Defensive path — we always ask ffmpeg for f32le, but if a user pipes in
            // something else we still cope. Scale to [-1, 1].
            let scale = 1.0_f32 / ((1i64 << (bits - 1)) as f32);
            reader
                .samples::<i32>()
                .collect::<hound::Result<Vec<_>>>()?
                .into_iter()
                .map(|s| s as f32 * scale)
                .collect()
        }
        other => {
            return Err(crate::error::DubsyncError::Wav(hound::Error::Unsupported)).map_err(|e| {
                tracing::error!(?other, "unexpected WAV sample format");
                e
            })
        }
    };

    let channels = spec.channels as usize;
    let mono: Vec<f32> = if channels == 1 {
        samples
    } else {
        // Defensive path. Anchors are always extracted with ffmpeg `-ac 1` upstream,
        // so this branch should never fire in production. If a future caller pipes a
        // multichannel WAV through here, we keep only the first channel rather than
        // averaging — averaging would silently zero out content that is symmetric and
        // out-of-phase across channels (e.g. inverted-stereo masters), which would
        // corrupt the GCC-PHAT correlation.
        tracing::debug!(
            channels,
            "read_mono_f32 received multichannel input — using first channel only"
        );
        let frames = samples.len() / channels;
        (0..frames).map(|f| samples[f * channels]).collect()
    };

    Ok((mono, sample_rate))
}

/// Decoded interleaved PCM exactly as it lives on disk.
pub struct Pcm {
    pub samples: Vec<f32>,
    pub spec: WavSpec,
}

impl Pcm {
    pub fn channels(&self) -> usize {
        self.spec.channels as usize
    }
    pub fn sample_rate(&self) -> u32 {
        self.spec.sample_rate
    }
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels()
    }
    pub fn duration_s(&self) -> f64 {
        self.frames() as f64 / self.sample_rate() as f64
    }
}

/// Read a multichannel f32 WAV with channels left interleaved.
pub fn read_interleaved_f32(path: &Path) -> Result<Pcm> {
    let mut reader = WavReader::open(path)?;
    let spec = reader.spec();
    if spec.sample_format != SampleFormat::Float || spec.bits_per_sample != 32 {
        // Be strict here — we control upstream extraction.
        return Err(crate::error::DubsyncError::Wav(hound::Error::Unsupported));
    }
    let samples = reader.samples::<f32>().collect::<hound::Result<Vec<_>>>()?;
    Ok(Pcm { samples, spec })
}

/// Write an interleaved f32 WAV.
pub fn write_interleaved_f32(path: &Path, samples: &[f32], spec: &WavSpec) -> Result<()> {
    let mut writer = WavWriter::create(path, *spec)?;
    for s in samples {
        writer.write_sample(*s)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Header-only metadata for a WAV file: frames, sample rate, channel count. Used by
/// the fps-stretch phase to compute expected output sizes without reading the entire
/// file into memory (a 1-hour episode at 48 kHz stereo is ~1.4 GB on disk).
pub struct WavInfo {
    pub frames: u64,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Read just the WAV header — does not load samples.
pub fn probe_frames(path: &Path) -> Result<WavInfo> {
    let reader = WavReader::open(path)?;
    let spec = reader.spec();
    Ok(WavInfo {
        frames: u64::from(reader.duration()),
        sample_rate: spec.sample_rate,
        channels: spec.channels,
    })
}

#[cfg(test)]
pub(crate) fn float_spec(channels: u16, sample_rate: u32) -> WavSpec {
    WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    }
}
