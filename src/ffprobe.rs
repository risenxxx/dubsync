use crate::error::{DubsyncError, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<AudioStream>,
}

#[derive(Debug, Deserialize)]
struct VideoProbeOutput {
    #[serde(default)]
    streams: Vec<RawVideoStream>,
}

#[derive(Debug, Deserialize)]
struct SubtitleProbeOutput {
    #[serde(default)]
    streams: Vec<SubtitleStream>,
}

#[derive(Debug, Deserialize)]
struct RawVideoStream {
    #[serde(default)]
    r_frame_rate: String,
    #[serde(default)]
    avg_frame_rate: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AudioStream {
    pub index: u32,
    #[serde(default)]
    pub codec_name: String,
    #[serde(default)]
    pub channels: u32,
    /// ffprobe emits sample_rate as a JSON string (e.g. "48000").
    #[serde(default)]
    pub sample_rate: String,
    #[serde(default)]
    pub channel_layout: Option<String>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    #[serde(default)]
    pub disposition: HashMap<String, u8>,
}

impl AudioStream {
    pub fn language(&self) -> Option<&str> {
        self.tags.get("language").map(String::as_str)
    }

    pub fn title(&self) -> Option<&str> {
        self.tags.get("title").map(String::as_str)
    }

    pub fn is_default(&self) -> bool {
        self.disposition.get("default").copied().unwrap_or(0) != 0
    }

    /// Single-line label suitable for an inquire Select row.
    pub fn display_label(&self) -> String {
        let lang = self.language().unwrap_or("und");
        let title = self.title().unwrap_or("");
        let layout = self
            .channel_layout
            .as_deref()
            .map(String::from)
            .unwrap_or_else(|| format!("{}ch", self.channels));
        let default_marker = if self.is_default() { " *" } else { "" };
        let title_part = if title.is_empty() {
            String::new()
        } else {
            format!("  \"{title}\"")
        };
        format!(
            "#{idx:<3} {codec:<8} {layout:<10} {rate:>6}Hz  [{lang}]{def}{title}",
            idx = self.index,
            codec = self.codec_name,
            layout = layout,
            rate = self.sample_rate,
            lang = lang,
            def = default_marker,
            title = title_part,
        )
    }
}

/// Run `ffprobe` and return all audio streams in `path`.
pub fn list_audio_streams(path: &Path) -> Result<Vec<AudioStream>> {
    if !path.exists() {
        return Err(DubsyncError::FileNotFound(path.to_path_buf()));
    }

    let output = crate::binpath::ffprobe_command()
        .arg("-v")
        .arg("quiet")
        .arg("-print_format")
        .arg("json")
        .arg("-show_streams")
        .arg("-select_streams")
        .arg("a")
        .arg(path)
        .output()?;

    if !output.status.success() {
        return Err(DubsyncError::FfprobeFailed {
            path: path.to_path_buf(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let probe: ProbeOutput = serde_json::from_slice(&output.stdout)?;
    if probe.streams.is_empty() {
        return Err(DubsyncError::NoAudioStreams(path.to_path_buf()));
    }
    Ok(probe.streams)
}

#[derive(Debug, Deserialize, Clone)]
pub struct SubtitleStream {
    pub index: u32,
    #[serde(default)]
    pub codec_name: String,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    #[serde(default)]
    pub disposition: HashMap<String, u8>,
}

impl SubtitleStream {
    pub fn language(&self) -> Option<&str> {
        self.tags.get("language").map(String::as_str)
    }

    pub fn title(&self) -> Option<&str> {
        self.tags.get("title").map(String::as_str)
    }

    pub fn is_forced(&self) -> bool {
        self.disposition.get("forced").copied().unwrap_or(0) != 0
    }

    pub fn is_default(&self) -> bool {
        self.disposition.get("default").copied().unwrap_or(0) != 0
    }

    /// True for codecs that store subtitles as bitmap images (PGS / DVD-SUB /
    /// DVB-SUB). Time-shifting these would require OCR + re-rendering, which
    /// is out of scope; the pipeline detects + skips them with a warning.
    pub fn is_image_based(&self) -> bool {
        matches!(
            self.codec_name.as_str(),
            "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle"
        )
    }
}

/// Run `ffprobe` and return all subtitle streams in `path`. Returns an empty
/// `Vec` when there are no subtitle tracks (Phase A's pass-through must not
/// fail when the master happens to have none).
pub fn list_subtitle_streams(path: &Path) -> Result<Vec<SubtitleStream>> {
    if !path.exists() {
        return Err(DubsyncError::FileNotFound(path.to_path_buf()));
    }

    let output = crate::binpath::ffprobe_command()
        .arg("-v")
        .arg("quiet")
        .arg("-print_format")
        .arg("json")
        .arg("-show_streams")
        .arg("-select_streams")
        .arg("s")
        .arg(path)
        .output()?;

    if !output.status.success() {
        return Err(DubsyncError::FfprobeFailed {
            path: path.to_path_buf(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let probe: SubtitleProbeOutput = serde_json::from_slice(&output.stdout)?;
    Ok(probe.streams)
}

/// Probe the first video stream's frame rate. Returns `Ok(None)` for audio-only files
/// (donor that's just an MKA, etc.), `Err` only on subprocess / parse failure.
///
/// Prefers `r_frame_rate` (the raw container fps, closest to "what the source says")
/// over `avg_frame_rate`. For variable-frame-rate sources these differ; the offset
/// map's frame-jump threshold is calibrated against the container fps so `r_frame_rate`
/// gives the right behaviour.
pub fn probe_video_fps(path: &Path) -> Result<Option<f64>> {
    if !path.exists() {
        return Err(DubsyncError::FileNotFound(path.to_path_buf()));
    }

    let output = crate::binpath::ffprobe_command()
        .arg("-v")
        .arg("quiet")
        .arg("-print_format")
        .arg("json")
        .arg("-show_streams")
        .arg("-select_streams")
        .arg("v:0")
        .arg(path)
        .output()?;

    if !output.status.success() {
        return Err(DubsyncError::FfprobeFailed {
            path: path.to_path_buf(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let probe: VideoProbeOutput = serde_json::from_slice(&output.stdout)?;
    let Some(stream) = probe.streams.into_iter().next() else {
        return Ok(None);
    };
    let fps =
        parse_rational(&stream.r_frame_rate).or_else(|| parse_rational(&stream.avg_frame_rate));
    Ok(fps)
}

/// Parse ffprobe's "p/q" rational frame-rate strings. Returns `None` for malformed
/// input or `0/0` (which ffprobe emits for audio-only / unknown streams).
pub fn parse_rational(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut parts = s.splitn(2, '/');
    let num: f64 = parts.next()?.trim().parse().ok()?;
    let den: f64 = match parts.next() {
        Some(d) => d.trim().parse().ok()?,
        None => 1.0,
    };
    if den == 0.0 || !num.is_finite() || !den.is_finite() {
        return None;
    }
    let v = num / den;
    if v <= 0.0 || !v.is_finite() {
        None
    } else {
        Some(v)
    }
}

/// Validate that every requested stream index actually exists in `streams`.
pub fn ensure_indices_exist(file: &Path, streams: &[AudioStream], requested: &[u32]) -> Result<()> {
    let available: Vec<u32> = streams.iter().map(|s| s.index).collect();
    for idx in requested {
        if !available.contains(idx) {
            tracing::error!(file = %file.display(), index = idx, ?available, "invalid stream index");
            return Err(DubsyncError::InvalidStreamIndex {
                index: *idx,
                available,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_rational;

    #[test]
    fn parses_integer_rates() {
        assert_eq!(parse_rational("24/1"), Some(24.0));
        assert_eq!(parse_rational("25/1"), Some(25.0));
        assert_eq!(parse_rational("30/1"), Some(30.0));
    }

    #[test]
    fn parses_ntsc_pulldown() {
        let v = parse_rational("24000/1001").unwrap();
        assert!((v - 23.976).abs() < 0.001);
    }

    #[test]
    fn parses_ntsc_30() {
        let v = parse_rational("30000/1001").unwrap();
        assert!((v - 29.97).abs() < 0.01);
    }

    #[test]
    fn rejects_zero_division() {
        assert_eq!(parse_rational("0/0"), None);
        assert_eq!(parse_rational("24/0"), None);
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(parse_rational(""), None);
        assert_eq!(parse_rational("garbage"), None);
        assert_eq!(parse_rational("24/x"), None);
    }

    #[test]
    fn parses_bare_number() {
        assert_eq!(parse_rational("24"), Some(24.0));
    }
}
