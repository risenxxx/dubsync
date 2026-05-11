use crate::cli::{resolve_fps_mode, Cli, DubCodec, FpsMode, RunConfig};
use crate::error::{DubsyncError, Result};
use crate::ffprobe::{self, AudioStream, SubtitleStream};
use clap::Parser;
use inquire::{validator::Validation, Confirm, MultiSelect, Select, Text};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Wrapper so inquire's Select/MultiSelect render our custom label.
#[derive(Clone)]
struct StreamRow(AudioStream);

impl fmt::Display for StreamRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.display_label())
    }
}

/// Wrapper for SubtitleStream MultiSelect — mirrors `StreamRow` but renders the
/// sub-specific label (codec, language, forced/default tags).
#[derive(Clone)]
struct SubRow(SubtitleStream);

impl fmt::Display for SubRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.0;
        let lang = s.language().unwrap_or("und");
        let title = s.title().unwrap_or("");
        let mut tags = Vec::new();
        if s.is_forced() {
            tags.push("forced");
        }
        if s.is_default() {
            tags.push("default");
        }
        if s.is_image_based() {
            tags.push("image");
        }
        let tag_part = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(","))
        };
        let title_part = if title.is_empty() {
            String::new()
        } else {
            format!("  \"{title}\"")
        };
        write!(
            f,
            "#{idx:<3} {codec:<8} [{lang}]{tag_part}{title_part}",
            idx = s.index,
            codec = s.codec_name,
        )
    }
}

pub fn run_interactive(defaults: &Cli) -> Result<RunConfig> {
    let master_file = prompt_existing_path("Master file (high-quality, e.g. 4K Blu-ray):")?;
    let donor_file = prompt_existing_path("Donor file (lower-quality, holds the dubs):")?;

    let master_streams = ffprobe::list_audio_streams(&master_file)?;
    let donor_streams = ffprobe::list_audio_streams(&donor_file)?;

    println!("\nMaster audio streams:");
    for s in &master_streams {
        println!("  {}", s.display_label());
    }
    println!("\nDonor audio streams:");
    for s in &donor_streams {
        println!("  {}", s.display_label());
    }
    println!();

    let master_anchor = pick_one(
        "Master anchor track (reference language, e.g. English):",
        &master_streams,
    )?;
    let donor_anchor = pick_one(
        "Donor anchor track (same language as master anchor):",
        &donor_streams,
    )?;

    let dub_options: Vec<StreamRow> = donor_streams
        .iter()
        .filter(|s| s.index != donor_anchor.index)
        .cloned()
        .map(StreamRow)
        .collect();

    if dub_options.is_empty() {
        return Err(DubsyncError::NoAudioStreams(donor_file));
    }

    let dub_choices = MultiSelect::new(
        "Dub tracks to sync (space to toggle, enter to confirm):",
        dub_options,
    )
    .with_validator(|sel: &[inquire::list_option::ListOption<&StreamRow>]| {
        if sel.is_empty() {
            Ok(Validation::Invalid("pick at least one dub".into()))
        } else {
            Ok(Validation::Valid)
        }
    })
    .prompt()
    .map_err(|_| DubsyncError::InteractiveCancelled)?;

    let donor_dub_tracks: Vec<u32> = dub_choices.into_iter().map(|r| r.0.index).collect();

    let default_out = default_output_path(&master_file);
    let output_file = Text::new("Output MKV path:")
        .with_default(default_out.to_string_lossy().as_ref())
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)?;
    let output_file = PathBuf::from(output_file);

    let mut cfg = RunConfig {
        master_file,
        donor_file: donor_file.clone(),
        master_anchor_track: master_anchor.index,
        donor_anchor_track: donor_anchor.index,
        donor_dub_tracks,
        output_file,
        keep_temp: defaults.keep_temp,
        include_donor_anchor: defaults.include_donor_anchor,
        solo_dub: defaults.solo_dub,
        temp_dir: defaults.temp_dir.clone(),
        threads: defaults.threads,
        silence_db: defaults.silence_db,
        silence_min_ms: defaults.silence_min_ms,
        anchor_rate: defaults.anchor_rate,
        correlation_window_s: defaults.correlation_window_s,
        max_drift_s: defaults.max_drift_s,
        max_segment_jump_s: defaults.max_segment_jump_s,
        snap_radius_s: defaults.snap_radius_s,
        crossfade_ms: defaults.crossfade_ms,
        smooth_gaps: defaults.smooth_gaps,
        gap_fill_margin_s: defaults.gap_fill_margin_s,
        speech_db: defaults.speech_db,
        pal_pitch_correction: defaults.pal_pitch_correction,
        anchor_only_validation: defaults.anchor_only_validation,
        report_path: defaults.report.clone(),
        dub_codec: defaults.dub_codec,
        dub_bitrate_kbps: defaults.dub_bitrate,
        fps_mode: resolve_fps_mode(defaults.disable_fps_normalize, defaults.force_fps_ratio),
        keep_master_subs: !defaults.no_master_subs,
        include_donor_forced_subs: defaults.include_donor_forced_subs,
        donor_subs_explicit: defaults.include_donor_subs.clone().unwrap_or_default(),
    };

    // Advanced options gate. Default off so the typical user enters 4 prompts
    // (file/anchor/dubs/output) and runs. Power users opt in for the rest.
    println!();
    if prompt_bool("Configure advanced options?", false)? {
        prompt_subtitles_group(&mut cfg, &donor_file)?;
        prompt_codec_group(&mut cfg)?;
        prompt_fps_group(&mut cfg)?;
        prompt_workflow_group(&mut cfg)?;
        prompt_report(&mut cfg)?;
        if prompt_bool("Tune correlation / splice parameters?", false)? {
            prompt_tuning_group(&mut cfg)?;
        }
    }

    print_summary(&cfg);
    print_repeat_command(&cfg);
    Ok(cfg)
}

fn pick_one(prompt: &str, streams: &[AudioStream]) -> Result<AudioStream> {
    let rows: Vec<StreamRow> = streams.iter().cloned().map(StreamRow).collect();
    let chosen = Select::new(prompt, rows)
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)?;
    Ok(chosen.0)
}

fn prompt_existing_path(prompt: &str) -> Result<PathBuf> {
    let raw = Text::new(prompt)
        .with_validator(|s: &str| {
            if s.trim().is_empty() {
                Ok(Validation::Invalid("path is required".into()))
            } else if !Path::new(s.trim()).exists() {
                Ok(Validation::Invalid("file does not exist".into()))
            } else {
                Ok(Validation::Valid)
            }
        })
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)?;
    Ok(PathBuf::from(raw.trim()))
}

fn default_output_path(master: &Path) -> PathBuf {
    let stem = master
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let mut p = master.parent().map(Path::to_path_buf).unwrap_or_default();
    p.push(format!("{stem}.synced.mkv"));
    p
}

fn print_summary(cfg: &RunConfig) {
    println!("\n=== dubsync run config ===");
    println!("master:        {}", cfg.master_file.display());
    println!("donor:         {}", cfg.donor_file.display());
    println!("master anchor: #{}", cfg.master_anchor_track);
    println!("donor anchor:  #{}", cfg.donor_anchor_track);
    println!("dub tracks:    {:?}", cfg.donor_dub_tracks);
    println!("output:        {}", cfg.output_file.display());
    println!("==========================\n");
}

/// Print the equivalent headless CLI invocation so the user can copy-paste it to repeat
/// the run without going through the prompts again. Only emits flags whose values differ
/// from the clap defaults, keeping the output minimal.
fn print_repeat_command(cfg: &RunConfig) {
    let defaults: Cli = Cli::parse_from(["dubsync"]);
    let prog = std::env::args()
        .next()
        .unwrap_or_else(|| "dubsync".to_string());

    let dubs = cfg
        .donor_dub_tracks
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let mut parts: Vec<String> = vec![
        shell_quote(&prog),
        format!(
            "--master-file {}",
            shell_quote(&cfg.master_file.to_string_lossy())
        ),
        format!(
            "--donor-file {}",
            shell_quote(&cfg.donor_file.to_string_lossy())
        ),
        format!("--master-anchor-track {}", cfg.master_anchor_track),
        format!("--donor-anchor-track {}", cfg.donor_anchor_track),
        format!("--donor-dub-tracks {dubs}"),
        format!(
            "--output-file {}",
            shell_quote(&cfg.output_file.to_string_lossy())
        ),
    ];

    if cfg.keep_temp {
        parts.push("--keep-temp".to_string());
    }
    if cfg.include_donor_anchor {
        parts.push("--include-donor-anchor".to_string());
    }
    if cfg.solo_dub {
        parts.push("--solo-dub".to_string());
    }
    if let Some(t) = &cfg.temp_dir {
        parts.push(format!("--temp-dir {}", shell_quote(&t.to_string_lossy())));
    }
    if let Some(n) = cfg.threads {
        parts.push(format!("--threads {n}"));
    }
    if (cfg.silence_db - defaults.silence_db).abs() > f32::EPSILON {
        parts.push(format!("--silence-db {}", cfg.silence_db));
    }
    if cfg.silence_min_ms != defaults.silence_min_ms {
        parts.push(format!("--silence-min-ms {}", cfg.silence_min_ms));
    }
    if cfg.anchor_rate != defaults.anchor_rate {
        parts.push(format!("--anchor-rate {}", cfg.anchor_rate));
    }
    if (cfg.correlation_window_s - defaults.correlation_window_s).abs() > f32::EPSILON {
        parts.push(format!(
            "--correlation-window-s {}",
            cfg.correlation_window_s
        ));
    }
    if (cfg.max_drift_s - defaults.max_drift_s).abs() > f32::EPSILON {
        parts.push(format!("--max-drift-s {}", cfg.max_drift_s));
    }
    if (cfg.max_segment_jump_s - defaults.max_segment_jump_s).abs() > f32::EPSILON {
        parts.push(format!("--max-segment-jump-s {}", cfg.max_segment_jump_s));
    }
    if (cfg.snap_radius_s - defaults.snap_radius_s).abs() > f32::EPSILON {
        parts.push(format!("--snap-radius-s {}", cfg.snap_radius_s));
    }
    if cfg.crossfade_ms != defaults.crossfade_ms {
        parts.push(format!("--crossfade-ms {}", cfg.crossfade_ms));
    }
    if cfg.smooth_gaps != defaults.smooth_gaps {
        parts.push(format!("--smooth-gaps {}", cfg.smooth_gaps));
    }
    if (cfg.gap_fill_margin_s - defaults.gap_fill_margin_s).abs() > f32::EPSILON {
        parts.push(format!("--gap-fill-margin-s {}", cfg.gap_fill_margin_s));
    }
    if (cfg.speech_db - defaults.speech_db).abs() > f32::EPSILON {
        parts.push(format!("--speech-db {}", cfg.speech_db));
    }
    if cfg.pal_pitch_correction != defaults.pal_pitch_correction {
        parts.push(format!(
            "--pal-pitch-correction {}",
            cfg.pal_pitch_correction
        ));
    }
    if cfg.anchor_only_validation {
        parts.push("--anchor-only-validation".to_string());
    }
    if let Some(p) = &cfg.report_path {
        parts.push(format!("--report {}", shell_quote(&p.to_string_lossy())));
    }
    if cfg.dub_codec != defaults.dub_codec {
        parts.push(format!("--dub-codec {}", cfg.dub_codec.as_token()));
    }
    if let Some(kbps) = cfg.dub_bitrate_kbps {
        parts.push(format!("--dub-bitrate {kbps}"));
    }
    if cfg.fps_mode.is_disabled() {
        parts.push("--disable-fps-normalize".to_string());
    } else if let Some(r) = cfg.fps_mode.forced_ratio() {
        parts.push(format!("--force-fps-ratio {r}"));
    }
    if !cfg.keep_master_subs {
        parts.push("--no-master-subs".to_string());
    }
    if cfg.include_donor_forced_subs {
        parts.push("--include-donor-forced-subs".to_string());
    }
    if !cfg.donor_subs_explicit.is_empty() {
        let csv = cfg
            .donor_subs_explicit
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("--include-donor-subs {csv}"));
    }

    println!("To repeat this run headlessly, copy:");
    println!("  {}\n", parts.join(" \\\n    "));
}

/// Minimal POSIX-shell-safe quoting. Bare-prints values made of "safe" chars; otherwise
/// wraps in single quotes and escapes embedded single quotes as `'\''`. Works in
/// bash, zsh, and fish.
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || "/_-.+,~=:".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ── Advanced options prompts ──────────────────────────────────────────────────

fn prompt_subtitles_group(cfg: &mut RunConfig, donor_file: &Path) -> Result<()> {
    println!("\n── Subtitles ──");

    cfg.keep_master_subs = prompt_bool("Pass through master subtitles?", cfg.keep_master_subs)?;

    let donor_subs = ffprobe::list_subtitle_streams(donor_file).unwrap_or_default();
    if donor_subs.is_empty() {
        println!("  (donor has no subtitle tracks — skipping donor sub options)");
        return Ok(());
    }

    cfg.include_donor_forced_subs = prompt_bool(
        "Auto-include donor forced subs (localised signs/on-screen text)?",
        cfg.include_donor_forced_subs,
    )?;

    // Explicit picker — show only text-based tracks; image-based ones can't be
    // time-shifted so we don't even let the user select them. Default = all
    // text-based tracks pre-selected (Enter accepts the lot).
    let pickable: Vec<SubRow> = donor_subs
        .iter()
        .filter(|s| !s.is_image_based())
        .cloned()
        .map(SubRow)
        .collect();
    if pickable.is_empty() {
        println!(
            "  (only image-based subtitle tracks present — none can be time-shifted; \
             skipping explicit picker)"
        );
        return Ok(());
    }
    let all_indices: Vec<usize> = (0..pickable.len()).collect();
    let chosen = MultiSelect::new(
        "Donor subtitle tracks (space toggle, enter to accept):",
        pickable,
    )
    .with_default(&all_indices)
    .prompt()
    .map_err(|_| DubsyncError::InteractiveCancelled)?;
    cfg.donor_subs_explicit = chosen.into_iter().map(|r| r.0.index).collect();
    Ok(())
}

fn prompt_codec_group(cfg: &mut RunConfig) -> Result<()> {
    println!("\n── Output codec ──");

    let options = [DubCodec::Flac, DubCodec::Ac3, DubCodec::Eac3, DubCodec::Aac];
    let cursor = options
        .iter()
        .position(|c| *c == cfg.dub_codec)
        .unwrap_or(0);
    let labels: Vec<&'static str> = options.iter().map(|c| c.display_label()).collect();
    let chosen_label = Select::new("Codec:", labels.clone())
        .with_starting_cursor(cursor)
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)?;
    let chosen_idx = labels.iter().position(|l| *l == chosen_label).unwrap_or(0);
    cfg.dub_codec = options[chosen_idx];

    if !cfg.dub_codec.is_lossless() {
        let default = cfg
            .dub_bitrate_kbps
            .map(|k| k.to_string())
            .unwrap_or_default();
        let raw = Text::new("Bitrate (kbps, blank=auto):")
            .with_default(&default)
            .prompt()
            .map_err(|_| DubsyncError::InteractiveCancelled)?;
        cfg.dub_bitrate_kbps = if raw.trim().is_empty() {
            None
        } else {
            raw.trim().parse::<u32>().ok()
        };
    }
    Ok(())
}

fn prompt_fps_group(cfg: &mut RunConfig) -> Result<()> {
    println!("\n── FPS handling ──");

    let mode_label = match cfg.fps_mode {
        FpsMode::Auto => "Auto",
        FpsMode::Disabled => "Disabled",
        FpsMode::Forced(_) => "Manual ratio",
    };
    let labels = vec!["Auto", "Disabled", "Manual ratio"];
    let cursor = labels.iter().position(|l| *l == mode_label).unwrap_or(0);
    let chosen = Select::new("FPS mode:", labels)
        .with_starting_cursor(cursor)
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)?;
    cfg.fps_mode = match chosen {
        "Disabled" => FpsMode::Disabled,
        "Manual ratio" => {
            let default = cfg.fps_mode.forced_ratio().unwrap_or(1.0);
            let r = prompt_f64("Manual donor/master ratio:", default)?;
            FpsMode::Forced(r)
        }
        _ => FpsMode::Auto,
    };

    cfg.pal_pitch_correction = prompt_bool(
        "Apply PAL pitch correction when stretching donor audio?",
        cfg.pal_pitch_correction,
    )?;
    Ok(())
}

fn prompt_workflow_group(cfg: &mut RunConfig) -> Result<()> {
    println!("\n── Workflow modes ──");

    cfg.anchor_only_validation = prompt_bool(
        "Anchor-only validation mode (sync only the donor anchor, skip dubs)?",
        cfg.anchor_only_validation,
    )?;
    cfg.solo_dub = prompt_bool(
        "Solo-dub output (single audio track, drop master anchor)?",
        cfg.solo_dub,
    )?;
    cfg.include_donor_anchor = prompt_bool(
        "Diagnostic: replace master anchor with synced donor anchor?",
        cfg.include_donor_anchor,
    )?;
    cfg.keep_temp = prompt_bool("Keep temp workspace + diagnostic JSONs?", cfg.keep_temp)?;
    Ok(())
}

fn prompt_report(cfg: &mut RunConfig) -> Result<()> {
    println!("\n── Report ──");

    let default = cfg
        .report_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let raw = Text::new("Report file path (blank=skip; .html/.csv/.json by extension):")
        .with_default(&default)
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)?;
    cfg.report_path = if raw.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(raw.trim()))
    };
    Ok(())
}

fn prompt_tuning_group(cfg: &mut RunConfig) -> Result<()> {
    println!("\n── Tuning (correlation / splice / VAD) ──");

    cfg.snap_radius_s = prompt_f32("Splice snap radius (s):", cfg.snap_radius_s)?;
    cfg.crossfade_ms = prompt_u32("Crossfade (ms):", cfg.crossfade_ms)?;
    cfg.smooth_gaps = prompt_bool(
        "Smooth silence gaps with stretched ambient?",
        cfg.smooth_gaps,
    )?;
    if cfg.smooth_gaps {
        cfg.gap_fill_margin_s = prompt_f32("Gap-fill margin (s):", cfg.gap_fill_margin_s)?;
        cfg.speech_db = prompt_f32("Gap-fill speech threshold (dBFS):", cfg.speech_db)?;
    }
    cfg.silence_db = prompt_f32("Silence threshold (dBFS):", cfg.silence_db)?;
    cfg.silence_min_ms = prompt_u32("Silence min duration (ms):", cfg.silence_min_ms)?;
    cfg.correlation_window_s = prompt_f32("Correlation window (s):", cfg.correlation_window_s)?;
    cfg.max_drift_s = prompt_f32("Max drift (s):", cfg.max_drift_s)?;
    cfg.max_segment_jump_s = prompt_f32("Max segment jump (s):", cfg.max_segment_jump_s)?;
    cfg.anchor_rate = prompt_u32("Anchor sample rate (Hz):", cfg.anchor_rate)?;
    Ok(())
}

// ── Generic typed prompts ────────────────────────────────────────────────────

fn prompt_bool(prompt: &str, default: bool) -> Result<bool> {
    Confirm::new(prompt)
        .with_default(default)
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)
}

fn prompt_f32(prompt: &str, default: f32) -> Result<f32> {
    let raw = Text::new(prompt)
        .with_default(&format!("{default}"))
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)?;
    Ok(f32::from_str(raw.trim()).unwrap_or(default))
}

fn prompt_f64(prompt: &str, default: f64) -> Result<f64> {
    let raw = Text::new(prompt)
        .with_default(&format!("{default}"))
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)?;
    Ok(f64::from_str(raw.trim()).unwrap_or(default))
}

fn prompt_u32(prompt: &str, default: u32) -> Result<u32> {
    let raw = Text::new(prompt)
        .with_default(&format!("{default}"))
        .prompt()
        .map_err(|_| DubsyncError::InteractiveCancelled)?;
    Ok(u32::from_str(raw.trim()).unwrap_or(default))
}
