use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Output codec for synced dubs in the remuxed MKV.
///
/// E-AC3 is the default — it matches the typical donor (streaming / BD-rip
/// dubs are almost always AC3 or E-AC3), plays back natively on TVs / AVRs
/// over HDMI, and keeps file size in line with the donor (FLAC inflates 2–3×).
/// The pipeline always decodes dubs to PCM for splicing, so a lossless output
/// codec preserves the spliced PCM — not the donor's original bits — making
/// FLAC's lossless guarantee mostly cosmetic. Choose FLAC for 7.1 sources
/// (AC3 / E-AC3 cap at 6 channels) or when an extra lossy generation is
/// unacceptable; AC3 for maximum S/PDIF-bitstream compatibility on older AVRs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum DubCodec {
    #[default]
    Eac3,
    Ac3,
    Flac,
    Aac,
}

impl DubCodec {
    /// ffmpeg encoder name (used as the `-c:a` argument value).
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            DubCodec::Flac => "flac",
            DubCodec::Ac3 => "ac3",
            DubCodec::Eac3 => "eac3",
            DubCodec::Aac => "aac",
        }
    }

    /// Maximum supported channel count for this codec. Used by the pre-pipeline
    /// validation to fail fast when the donor has more channels than the codec
    /// can carry (so the user fixes their config before a 30-minute pipeline run).
    /// Limits are ffmpeg's encoder caps, not codec-spec caps.
    pub fn max_channels(self) -> u32 {
        match self {
            DubCodec::Flac | DubCodec::Aac => 8,
            DubCodec::Ac3 | DubCodec::Eac3 => 6,
        }
    }

    /// True for codecs that don't take a `-b:a` bitrate flag.
    pub fn is_lossless(self) -> bool {
        matches!(self, DubCodec::Flac)
    }

    /// Human-friendly label for GUI comboboxes / CLI help.
    pub fn display_label(self) -> &'static str {
        match self {
            DubCodec::Flac => "FLAC (lossless)",
            DubCodec::Ac3 => "AC3 (Dolby Digital)",
            DubCodec::Eac3 => "E-AC3 (Dolby Digital Plus)",
            DubCodec::Aac => "AAC",
        }
    }

    /// Channel-aware default bitrate in kbps. `None` for lossless codecs.
    /// Tunes per the common practice for each codec at consumer-quality:
    /// - AC3: 448k stereo, 640k 5.1 (standard Dolby Digital BD/DVD rates)
    /// - E-AC3: 256k stereo, 640k 5.1 (lower stereo overhead, same surround cap)
    /// - AAC: 192k stereo, 384k 5.1
    pub fn default_bitrate_kbps(self, channels: u32) -> Option<u32> {
        match self {
            DubCodec::Flac => None,
            DubCodec::Ac3 => Some(if channels <= 2 { 448 } else { 640 }),
            DubCodec::Eac3 => Some(if channels <= 2 { 256 } else { 640 }),
            DubCodec::Aac => Some(if channels <= 2 { 192 } else { 384 }),
        }
    }

    /// Lowercase token used both as the CLI value (`--dub-codec ac3`) and the
    /// persisted GUI string. Stable across renames in the enum.
    pub fn as_token(self) -> &'static str {
        self.ffmpeg_name()
    }

    /// Inverse of `as_token` for parsing persisted GUI state. Falls back to
    /// `Flac` on unknown input so a hand-edited state.json with garbage
    /// can't lock the user out of the GUI.
    pub fn from_token(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "ac3" => DubCodec::Ac3,
            "eac3" => DubCodec::Eac3,
            "aac" => DubCodec::Aac,
            _ => DubCodec::Flac,
        }
    }
}

/// User-resolved policy for the FPS-normalize phase. Constructed from the
/// mutually-exclusive `--disable-fps-normalize` / `--force-fps-ratio` CLI flags
/// (or the equivalent GUI combobox) and consumed by
/// [`crate::run_pipeline`]'s fps phase.
///
/// - `Auto`: probe both files, stretch when `|donor/master − 1| > 0.001`. Today's default.
/// - `Disabled`: skip the donor probe entirely; never stretch. Use when the
///   container's reported r_frame_rate is wrong but the audio is already at
///   master speed (VFR mess, broken WEBRips, mis-muxed files).
/// - `Forced(ratio)`: bypass the probe and pretend `donor_fps/master_fps == ratio`.
///   `1.0` is functionally equivalent to `Disabled` but more explicit.
#[derive(Debug, Clone, Copy)]
pub enum FpsMode {
    Auto,
    Disabled,
    Forced(f64),
}

impl FpsMode {
    pub fn is_disabled(self) -> bool {
        matches!(self, FpsMode::Disabled)
    }

    /// Returns the user-supplied ratio when in `Forced` mode, else `None`.
    pub fn forced_ratio(self) -> Option<f64> {
        match self {
            FpsMode::Forced(r) => Some(r),
            _ => None,
        }
    }
}

/// Resolve the mutually-exclusive `--disable-fps-normalize` / `--force-fps-ratio`
/// flags into a single [`FpsMode`]. Extracted so unit tests can exercise the
/// resolution rules without going through `clap`.
pub fn resolve_fps_mode(disable: bool, force_ratio: Option<f64>) -> FpsMode {
    if disable {
        FpsMode::Disabled
    } else if let Some(r) = force_ratio {
        FpsMode::Forced(r)
    } else {
        FpsMode::Auto
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "dubsync",
    version,
    about = "Sync localized dubs from a donor release onto a master video.",
    long_about = None,
)]
pub struct Cli {
    #[arg(long, value_name = "PATH")]
    pub master_file: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    pub donor_file: Option<PathBuf>,

    #[arg(long, value_name = "INDEX")]
    pub master_anchor_track: Option<u32>,

    #[arg(long, value_name = "INDEX")]
    pub donor_anchor_track: Option<u32>,

    #[arg(long, value_name = "IDX[,IDX...]", value_delimiter = ',', num_args = 1..)]
    pub donor_dub_tracks: Option<Vec<u32>>,

    #[arg(long, value_name = "PATH")]
    pub output_file: Option<PathBuf>,

    #[arg(long)]
    pub keep_temp: bool,

    /// Diagnostic: replace the master anchor in the output with the donor anchor run
    /// through the sync pipeline. The output MKV will have the synced donor English in
    /// place of the master English so you can verify the offset map directly — if this
    /// track drifts against the master video, the boundaries are wrong (not the dubs).
    #[arg(long)]
    pub include_donor_anchor: bool,

    /// Output an MKV with exactly one audio track: the (first) synced dub. Drops the
    /// master anchor and any additional selected dubs from the output. Useful for
    /// "Cast to device" so the TV plays the only available track instead of defaulting
    /// to English. Composes with --include-donor-anchor (= ship just the synced donor
    /// anchor as the lone audio track).
    #[arg(long)]
    pub solo_dub: bool,

    #[arg(long, value_name = "PATH")]
    pub temp_dir: Option<PathBuf>,

    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    #[arg(short, long)]
    pub verbose: bool,

    #[arg(long, default_value_t = -45.0, value_name = "DBFS")]
    pub silence_db: f32,

    #[arg(long, default_value_t = 200, value_name = "MS")]
    pub silence_min_ms: u32,

    #[arg(long, default_value_t = 16_000, value_name = "HZ")]
    pub anchor_rate: u32,

    #[arg(long, default_value_t = 30.0, value_name = "S")]
    pub correlation_window_s: f32,

    #[arg(long, default_value_t = 60.0, value_name = "S")]
    pub max_drift_s: f32,

    /// Maximum offset jump (in seconds) accepted between consecutive segments.
    /// Larger proposed jumps are suppressed as likely false positives — typically
    /// GCC-PHAT confused by repetitive end-credits music. Default 10.0 catches
    /// obvious failures while still allowing real intro/outro shifts; raise this
    /// for content with genuinely large mid-show edits (e.g. ad-block differences).
    #[arg(long, default_value_t = 10.0, value_name = "S")]
    pub max_segment_jump_s: f32,

    /// Maximum master-time radius around each refined boundary that the splicer is
    /// allowed to search for a master-anchor silence interval. The splice snaps into
    /// the nearest qualifying silence within this radius; the wider the radius, the
    /// more visible the visual cut moves but the higher the chance of finding a
    /// silence wide enough to absorb |Δ| inaudibly.
    #[arg(long, default_value_t = 30.0, value_name = "S")]
    pub snap_radius_s: f32,

    /// Equal-power crossfade applied at every splice (segment ↔ segment, segment ↔
    /// silence, segment ↔ stretched gap-fill). Default 10 ms. Range [1, 50] —
    /// values below ~5 ms can click; values above ~15 ms start eating segment audio.
    #[arg(long, default_value_t = 10, value_name = "MS")]
    pub crossfade_ms: u32,

    /// Replace literal silence at segment splices with time-stretched ambient copied
    /// from the neighbouring dub audio (via the bundled `rubberband` CLI). Off by
    /// default. Speech is never stretched — the splicer falls back to silence if
    /// the neighbour buffer is dominated by dialog.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = false, value_name = "BOOL")]
    pub smooth_gaps: bool,

    /// Length of dub audio sampled before AND after each gap to use as the stretch
    /// source. Wider = more material to stretch (less artefact), narrower = less
    /// chance of the neighbour catching speech.
    #[arg(long, default_value_t = 0.5, value_name = "S")]
    pub gap_fill_margin_s: f32,

    /// Above this dBFS level a neighbour buffer is treated as speech and the
    /// gap-fill path falls back to literal silence (preserving lip-sync).
    #[arg(long, default_value_t = -25.0, value_name = "DBFS")]
    pub speech_db: f32,

    /// When fps mismatch is auto-corrected, additionally lower the donor's pitch by
    /// `12 * log2(master_fps/donor_fps)` semitones (≈ -0.71 for 25→24) to undo the
    /// PAL speed-up pitch shift. Off by default — just time-stretch, preserve pitch.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = false, value_name = "BOOL")]
    pub pal_pitch_correction: bool,

    /// Skip the auto fps-stretch entirely. Use when the donor's r_frame_rate
    /// metadata lies (VFR sources, mis-encoded WEBRips, broken muxers) and the
    /// audio is already at master speed despite an apparent fps mismatch.
    /// Mutually exclusive with `--force-fps-ratio`.
    #[arg(long, conflicts_with = "force_fps_ratio")]
    pub disable_fps_normalize: bool,

    /// Force a specific donor/master fps ratio for the stretch, bypassing the
    /// probe. 1.0 is functionally identical to `--disable-fps-normalize` but
    /// more explicit. 25/24 ≈ 1.0417 corrects PAL→film when ffprobe lies.
    /// Mutually exclusive with `--disable-fps-normalize`.
    #[arg(long, value_name = "RATIO", conflicts_with = "disable_fps_normalize")]
    pub force_fps_ratio: Option<f64>,

    /// Drop the master's subtitle tracks from the output. By default every
    /// master subtitle stream is copied through unchanged (lossless, fast).
    #[arg(long)]
    pub no_master_subs: bool,

    /// Extract donor subtitle tracks marked as "forced" (localised signs /
    /// on-screen text), shift their timecodes through the offset map, and mux
    /// them alongside the master's subs in the output. Off by default.
    /// Image-based codecs (PGS / DVD-SUB / DVB-SUB) are skipped with a
    /// warning — they need OCR to time-shift.
    #[arg(long)]
    pub include_donor_forced_subs: bool,

    /// Explicit donor subtitle track indices to extract + sync, in addition to
    /// (or instead of) `--include-donor-forced-subs`. Comma-separated list of
    /// stream indices as reported by `ffprobe`. Invalid indices and image-based
    /// codecs (PGS / DVD-SUB / DVB-SUB) are caught at validation time. Use
    /// this to grab a non-forced track (e.g. full localised dialogue subs) or
    /// a specific subset of forced tracks.
    #[arg(long, value_name = "IDX[,IDX...]", value_delimiter = ',', num_args = 1..)]
    pub include_donor_subs: Option<Vec<u32>>,

    /// Anchor-only validation mode: build the offset map, sync ONLY the donor anchor
    /// (no other dubs), and emit an MKV with just that synced track. Useful for A/B
    /// verifying the offset map against the master video before committing to a full
    /// dub run. When set, `--donor-dub-tracks` is ignored — the donor anchor is the
    /// only track processed.
    #[arg(long)]
    pub anchor_only_validation: bool,

    /// Optional path to write a detailed offset-map report. Format determined by
    /// extension: `.html`/`.htm` → styled HTML page, `.csv` → spreadsheet rows,
    /// `.json` → pretty JSON, anything else → CSV. Independent of `--keep-temp`;
    /// the report only needs the offset map, not the workspace artifacts.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Audio codec for synced dubs in the output MKV. eac3 is the default
    /// (matches the typical donor and plays back natively on TVs / AVRs).
    /// flac is lossless but inflates output 2–3×; use it for 7.1 sources or
    /// to skip the lossy re-encode. ac3 / eac3 cap at 6 channels.
    #[arg(long, value_enum, default_value_t = DubCodec::Eac3, value_name = "CODEC")]
    pub dub_codec: DubCodec,

    /// Bitrate (kbps) for the synced dubs when the codec is lossy. Ignored for
    /// FLAC. When unset, dubsync picks a sensible per-codec / per-channel default
    /// (e.g. ac3 5.1 → 640k, ac3 stereo → 448k, aac stereo → 192k).
    #[arg(long, value_name = "KBPS")]
    pub dub_bitrate: Option<u32>,
}

impl Cli {
    /// Returns true if the user supplied no track-selection / file arguments,
    /// signalling that we should drop into the interactive picker.
    pub fn needs_interactive(&self) -> bool {
        self.master_file.is_none()
            && self.donor_file.is_none()
            && self.master_anchor_track.is_none()
            && self.donor_anchor_track.is_none()
            && self.donor_dub_tracks.is_none()
            && self.output_file.is_none()
    }
}

/// Resolved configuration produced after either CLI parsing or the interactive flow.
/// Both paths converge here so downstream code only sees one shape.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub master_file: PathBuf,
    pub donor_file: PathBuf,
    pub master_anchor_track: u32,
    pub donor_anchor_track: u32,
    pub donor_dub_tracks: Vec<u32>,
    pub output_file: PathBuf,
    pub keep_temp: bool,
    pub include_donor_anchor: bool,
    pub solo_dub: bool,
    pub temp_dir: Option<PathBuf>,
    pub threads: Option<usize>,
    pub silence_db: f32,
    pub silence_min_ms: u32,
    pub anchor_rate: u32,
    pub correlation_window_s: f32,
    pub max_drift_s: f32,
    pub max_segment_jump_s: f32,
    pub snap_radius_s: f32,
    pub crossfade_ms: u32,
    pub smooth_gaps: bool,
    pub gap_fill_margin_s: f32,
    pub speech_db: f32,
    pub pal_pitch_correction: bool,
    pub anchor_only_validation: bool,
    pub report_path: Option<PathBuf>,
    pub dub_codec: DubCodec,
    pub dub_bitrate_kbps: Option<u32>,
    pub fps_mode: FpsMode,
    pub keep_master_subs: bool,
    pub include_donor_forced_subs: bool,
    pub donor_subs_explicit: Vec<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dub_codec_ffmpeg_names() {
        assert_eq!(DubCodec::Flac.ffmpeg_name(), "flac");
        assert_eq!(DubCodec::Ac3.ffmpeg_name(), "ac3");
        assert_eq!(DubCodec::Eac3.ffmpeg_name(), "eac3");
        assert_eq!(DubCodec::Aac.ffmpeg_name(), "aac");
    }

    #[test]
    fn dub_codec_max_channels() {
        // ac3/eac3 are encoder-capped at 6 in ffmpeg; flac/aac at 8.
        assert_eq!(DubCodec::Flac.max_channels(), 8);
        assert_eq!(DubCodec::Aac.max_channels(), 8);
        assert_eq!(DubCodec::Ac3.max_channels(), 6);
        assert_eq!(DubCodec::Eac3.max_channels(), 6);
    }

    #[test]
    fn dub_codec_lossless_flag() {
        assert!(DubCodec::Flac.is_lossless());
        assert!(!DubCodec::Ac3.is_lossless());
        assert!(!DubCodec::Eac3.is_lossless());
        assert!(!DubCodec::Aac.is_lossless());
    }

    #[test]
    fn dub_codec_default_bitrate_per_channels() {
        // Flac is lossless — no bitrate.
        assert_eq!(DubCodec::Flac.default_bitrate_kbps(1), None);
        assert_eq!(DubCodec::Flac.default_bitrate_kbps(2), None);
        assert_eq!(DubCodec::Flac.default_bitrate_kbps(6), None);
        assert_eq!(DubCodec::Flac.default_bitrate_kbps(8), None);

        // ac3: stereo = 448, surround = 640.
        assert_eq!(DubCodec::Ac3.default_bitrate_kbps(1), Some(448));
        assert_eq!(DubCodec::Ac3.default_bitrate_kbps(2), Some(448));
        assert_eq!(DubCodec::Ac3.default_bitrate_kbps(6), Some(640));

        // eac3: lower stereo overhead, same surround cap.
        assert_eq!(DubCodec::Eac3.default_bitrate_kbps(2), Some(256));
        assert_eq!(DubCodec::Eac3.default_bitrate_kbps(6), Some(640));

        // aac: 192 stereo, 384 surround.
        assert_eq!(DubCodec::Aac.default_bitrate_kbps(2), Some(192));
        assert_eq!(DubCodec::Aac.default_bitrate_kbps(6), Some(384));
        assert_eq!(DubCodec::Aac.default_bitrate_kbps(8), Some(384));
    }

    #[test]
    fn dub_codec_token_round_trip() {
        for codec in [DubCodec::Flac, DubCodec::Ac3, DubCodec::Eac3, DubCodec::Aac] {
            assert_eq!(DubCodec::from_token(codec.as_token()), codec);
        }
        // Unknown / garbage falls back to Flac.
        assert_eq!(DubCodec::from_token("opus"), DubCodec::Flac);
        assert_eq!(DubCodec::from_token(""), DubCodec::Flac);
        assert_eq!(DubCodec::from_token("AC3"), DubCodec::Ac3); // case-insensitive
    }

    #[test]
    fn fps_mode_resolution() {
        // Defaults: neither flag set → Auto.
        assert!(matches!(resolve_fps_mode(false, None), FpsMode::Auto));

        // Disabled trumps everything (CLI's `conflicts_with` rejects the combo
        // before it reaches us, but the resolver is still defensive).
        assert!(matches!(resolve_fps_mode(true, None), FpsMode::Disabled));

        // Forced ratio passes through verbatim.
        let m = resolve_fps_mode(false, Some(1.0417));
        assert!(matches!(m, FpsMode::Forced(_)));
        assert!((m.forced_ratio().unwrap() - 1.0417).abs() < 1e-9);

        // 1.0 is forced-but-no-op (equivalence checked downstream by
        // `fps_normalize_donor`'s threshold guard, not here).
        assert!(matches!(
            resolve_fps_mode(false, Some(1.0)),
            FpsMode::Forced(_)
        ));
    }

    #[test]
    fn fps_mode_helpers() {
        assert!(FpsMode::Disabled.is_disabled());
        assert!(!FpsMode::Auto.is_disabled());
        assert!(!FpsMode::Forced(1.5).is_disabled());

        assert_eq!(FpsMode::Auto.forced_ratio(), None);
        assert_eq!(FpsMode::Disabled.forced_ratio(), None);
        assert_eq!(FpsMode::Forced(1.25).forced_ratio(), Some(1.25));
    }

    #[test]
    fn pitch_formula_equivalence() {
        // The `Forced` branch uses `-12 * log2(ratio)` to avoid needing the
        // donor_fps split. Verify it agrees with the original
        // `12 * log2(master_fps / donor_fps)` for the typical conversion ratios.
        for ratio in [0.96_f64, 1.0, 1.0_f64 / 0.96, 25.0 / 24.0] {
            let new = -12.0 * ratio.log2();
            // Equivalent shape: with master_fps = 1.0, donor_fps = ratio,
            // master_fps / donor_fps = 1/ratio.
            let old = 12.0 * (1.0_f64 / ratio).log2();
            assert!(
                (new - old).abs() < 1e-12,
                "pitch formulas disagree at ratio {ratio}: new={new}, old={old}"
            );
        }
    }
}
