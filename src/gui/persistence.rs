//! Persisted GUI state. Saved to disk on every relevant change so the next launch
//! restores file paths, last-used track indices, options, and window geometry.
//!
//! Storage location follows the LangLock pattern:
//!   - If a marker file `dubsync.portable` exists next to the executable → save in
//!     the executable's directory (portable mode, useful for USB-stick installs).
//!   - Otherwise → `%APPDATA%/dubsync/state.json` (Windows) or the platform
//!     equivalent via the `directories` crate.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const PORTABLE_MARKER: &str = "dubsync.portable";
const STATE_FILE_NAME: &str = "state.json";

/// Top-level GUI state. Empty by default; every field is `Option` or has a default
/// so a fresh install with a missing/corrupt file degrades gracefully.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    pub master_file: Option<PathBuf>,
    pub donor_file: Option<PathBuf>,

    /// Output is split into folder + filename so the user can pick a folder once
    /// (e.g. `./videos/synced`) and the filename auto-fills from each new master's
    /// stem. The two together form the final output path at run time.
    pub output_dir: Option<PathBuf>,

    /// Synced-MKV filename (just the leaf, no directory). Auto-rewritten to
    /// `<master_stem>.synced.mkv` whenever a new master is selected; user can
    /// edit the field freely between runs.
    #[serde(default)]
    pub output_filename: String,

    /// Last-used master anchor track index. Auto-applied to newly-dropped master
    /// files when that index exists in the new file's stream list.
    pub last_master_anchor: Option<u32>,
    pub last_donor_anchor: Option<u32>,
    pub last_donor_dubs: Vec<u32>,
    /// Last-used donor subtitle track indices (Phase C). Mirrors
    /// `last_donor_dubs` — auto-applied to newly-dropped donor files when the
    /// indices still exist in the new file's stream list.
    #[serde(default)]
    pub last_donor_subs: Vec<u32>,

    pub options: PersistedOptions,

    /// Window size + position so the user's preferred geometry persists.
    pub window_size: Option<(f32, f32)>,
    pub window_pos: Option<(f32, f32)>,
    /// Whether the window was maximized at last save. Independent of size/pos
    /// because a maximized window's logical inner_size doesn't equal the
    /// user's chosen restored size.
    #[serde(default)]
    pub window_maximized: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedOptions {
    pub keep_temp: bool,
    pub include_donor_anchor: bool,
    pub solo_dub: bool,
    pub silence_db: f32,
    pub silence_min_ms: u32,
    pub anchor_rate: u32,
    pub correlation_window_s: f32,
    pub max_drift_s: f32,
    #[serde(default = "default_max_segment_jump_s")]
    pub max_segment_jump_s: f32,
    pub snap_radius_s: f32,
    #[serde(default = "default_crossfade_ms")]
    pub crossfade_ms: u32,
    #[serde(default = "default_smooth_gaps")]
    pub smooth_gaps: bool,
    #[serde(default = "default_gap_fill_margin_s")]
    pub gap_fill_margin_s: f32,
    #[serde(default = "default_speech_db")]
    pub speech_db: f32,
    #[serde(default = "default_gap_fill_max_ratio")]
    pub gap_fill_max_ratio: f32,
    #[serde(default = "default_gap_fill_silence_fade_ms")]
    pub gap_fill_silence_fade_ms: u32,
    #[serde(default)]
    pub pal_pitch_correction: bool,
    #[serde(default)]
    pub anchor_only_validation: bool,
    /// When true, write a report file next to the output MKV with the same stem
    /// (e.g. `movie.synced.mkv` → `movie.synced.report.html`). Off by default.
    #[serde(default)]
    pub save_report: bool,
    /// Format for the auto-saved report file. One of: `"html"`, `"csv"`, `"json"`.
    /// Defaults to HTML — easy to open and read straight from a file manager.
    #[serde(default = "default_report_format")]
    pub report_format: String,
    /// Output codec for synced dubs. Stored as a lowercase token
    /// (`"flac"`/`"ac3"`/`"eac3"`/`"aac"`) so the JSON stays editor-friendly.
    /// Parsed back to `DubCodec` on Run via `DubCodec::from_token`.
    #[serde(default = "default_dub_codec")]
    pub dub_codec: String,
    /// Optional bitrate override (kbps) for lossy codecs. Stored as a string so
    /// "empty = auto" round-trips cleanly through serde + the GUI TextEdit.
    #[serde(default)]
    pub dub_bitrate: String,
    /// FPS-normalize mode: `"auto"` (default), `"disabled"`, or `"manual"`.
    /// Resolved into [`crate::cli::FpsMode`] on Run via the gui's start_run.
    #[serde(default = "default_fps_mode")]
    pub fps_mode: String,
    /// Manual donor/master fps ratio used only when `fps_mode == "manual"`.
    /// Range [0.5, 2.0]; 1.0 means "no stretch".
    #[serde(default = "default_fps_manual_ratio")]
    pub fps_manual_ratio: f32,
    /// Pass-through master subtitles into the output MKV unchanged. On by
    /// default — subtitles are universally useful and re-muxing them is
    /// effectively free (`-c:s copy`).
    #[serde(default = "default_keep_master_subs")]
    pub keep_master_subs: bool,
    /// Extract donor subtitle tracks marked as forced and time-shift them to
    /// the master timeline. Off by default — opt-in feature for users who
    /// want localised signs / on-screen text matching the donor's language.
    #[serde(default)]
    pub include_donor_forced_subs: bool,
}

fn default_report_format() -> String {
    "html".to_string()
}
fn default_dub_codec() -> String {
    "eac3".to_string()
}
fn default_fps_mode() -> String {
    "auto".to_string()
}
fn default_fps_manual_ratio() -> f32 {
    1.0
}
fn default_keep_master_subs() -> bool {
    true
}

fn default_crossfade_ms() -> u32 {
    10
}
fn default_smooth_gaps() -> bool {
    true
}
fn default_gap_fill_margin_s() -> f32 {
    1.0
}
fn default_speech_db() -> f32 {
    -25.0
}
fn default_gap_fill_max_ratio() -> f32 {
    1.2
}
fn default_gap_fill_silence_fade_ms() -> u32 {
    100
}
fn default_max_segment_jump_s() -> f32 {
    10.0
}

impl Default for PersistedOptions {
    fn default() -> Self {
        // Mirror the CLI defaults from src/cli.rs so the two paths are identical
        // on a fresh install.
        Self {
            keep_temp: false,
            include_donor_anchor: false,
            solo_dub: false,
            silence_db: -45.0,
            silence_min_ms: 200,
            anchor_rate: 16_000,
            correlation_window_s: 30.0,
            max_drift_s: 60.0,
            max_segment_jump_s: default_max_segment_jump_s(),
            snap_radius_s: 30.0,
            crossfade_ms: default_crossfade_ms(),
            smooth_gaps: default_smooth_gaps(),
            gap_fill_margin_s: default_gap_fill_margin_s(),
            speech_db: default_speech_db(),
            gap_fill_max_ratio: default_gap_fill_max_ratio(),
            gap_fill_silence_fade_ms: default_gap_fill_silence_fade_ms(),
            pal_pitch_correction: false,
            anchor_only_validation: false,
            save_report: false,
            report_format: default_report_format(),
            dub_codec: default_dub_codec(),
            dub_bitrate: String::new(),
            fps_mode: default_fps_mode(),
            fps_manual_ratio: default_fps_manual_ratio(),
            keep_master_subs: default_keep_master_subs(),
            include_donor_forced_subs: false,
        }
    }
}

impl PersistedState {
    pub fn load() -> Self {
        let path = match state_path() {
            Some(p) => p,
            None => return Self::default(),
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "could not parse persisted state — starting fresh");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not read persisted state");
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let Some(path) = state_path() else {
            tracing::warn!("no writable location for persisted state — skipping save");
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(parent = %parent.display(), error = %e, "could not create state dir");
                return;
            }
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    tracing::warn!(path = %path.display(), error = %e, "could not write state");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not serialise state"),
        }
    }
}

fn state_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join(PORTABLE_MARKER).exists() {
                return Some(dir.join(STATE_FILE_NAME));
            }
        }
    }
    let proj = directories::ProjectDirs::from("dev", "risen", "dubsync")?;
    Some(proj.config_dir().join(STATE_FILE_NAME))
}
