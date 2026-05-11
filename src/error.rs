use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DubsyncError {
    #[error("file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("ffprobe failed for {path}: {stderr}")]
    FfprobeFailed { path: PathBuf, stderr: String },

    #[error("ffmpeg failed: {stderr}")]
    FfmpegFailed { stderr: String },

    #[error("rubberband failed: {stderr}")]
    RubberbandFailed { stderr: String },

    #[error("ffprobe output had no audio streams in {0}")]
    NoAudioStreams(PathBuf),

    #[error("invalid stream index {index} (file has streams: {available:?})")]
    InvalidStreamIndex { index: u32, available: Vec<u32> },

    #[error("interactive prompt cancelled")]
    InteractiveCancelled,

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("wav: {0}")]
    Wav(#[from] hound::Error),

    #[error("subtitle parse error: {0}")]
    SubtitleParse(String),
}

pub type Result<T, E = DubsyncError> = std::result::Result<T, E>;
