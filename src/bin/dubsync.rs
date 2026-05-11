//! Console-subsystem CLI binary. Delegates to `dubsync::run_pipeline` for the actual
//! pipeline; this file only owns argument parsing and the user-facing summary line.

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use dubsync::cli::{resolve_fps_mode, Cli, RunConfig};
use dubsync::progress::StderrReporter;
use dubsync::{ffprobe, init_tracing, interactive, run_pipeline};

fn main() -> Result<()> {
    init_tracing();

    let args = Cli::parse();

    let cfg = if args.needs_interactive() {
        interactive::run_interactive(&args).context("interactive setup failed")?
    } else {
        resolve_from_cli(args).context("CLI argument validation failed")?
    };

    let reporter = StderrReporter::new();
    let output = run_pipeline(cfg, &reporter)?;
    println!("dubsync done — wrote {}", output.display());
    Ok(())
}

fn resolve_from_cli(args: Cli) -> Result<RunConfig> {
    let master_file = args
        .master_file
        .ok_or_else(|| anyhow!("--master-file is required"))?;
    let donor_file = args
        .donor_file
        .ok_or_else(|| anyhow!("--donor-file is required"))?;
    let master_anchor_track = args
        .master_anchor_track
        .ok_or_else(|| anyhow!("--master-anchor-track is required"))?;
    let donor_anchor_track = args
        .donor_anchor_track
        .ok_or_else(|| anyhow!("--donor-anchor-track is required"))?;
    // In anchor-only validation mode the donor anchor IS the only track processed,
    // so --donor-dub-tracks becomes optional. If still supplied, we ignore it.
    let donor_dub_tracks = if args.anchor_only_validation {
        vec![donor_anchor_track]
    } else {
        let tracks = args
            .donor_dub_tracks
            .ok_or_else(|| anyhow!("--donor-dub-tracks is required"))?;
        if tracks.is_empty() {
            return Err(anyhow!("--donor-dub-tracks must list at least one index"));
        }
        tracks
    };
    let output_file = args
        .output_file
        .ok_or_else(|| anyhow!("--output-file is required"))?;

    if !master_file.exists() {
        return Err(anyhow!(
            "master file does not exist: {}",
            master_file.display()
        ));
    }
    if !donor_file.exists() {
        return Err(anyhow!(
            "donor file does not exist: {}",
            donor_file.display()
        ));
    }

    let master_streams = ffprobe::list_audio_streams(&master_file)?;
    let donor_streams = ffprobe::list_audio_streams(&donor_file)?;
    ffprobe::ensure_indices_exist(&master_file, &master_streams, &[master_anchor_track])?;
    ffprobe::ensure_indices_exist(&donor_file, &donor_streams, &[donor_anchor_track])?;
    ffprobe::ensure_indices_exist(&donor_file, &donor_streams, &donor_dub_tracks)?;

    Ok(RunConfig {
        master_file,
        donor_file,
        master_anchor_track,
        donor_anchor_track,
        donor_dub_tracks,
        output_file,
        keep_temp: args.keep_temp,
        include_donor_anchor: args.include_donor_anchor,
        solo_dub: args.solo_dub,
        temp_dir: args.temp_dir,
        threads: args.threads,
        silence_db: args.silence_db,
        silence_min_ms: args.silence_min_ms,
        anchor_rate: args.anchor_rate,
        correlation_window_s: args.correlation_window_s,
        max_drift_s: args.max_drift_s,
        max_segment_jump_s: args.max_segment_jump_s,
        snap_radius_s: args.snap_radius_s,
        crossfade_ms: args.crossfade_ms,
        smooth_gaps: args.smooth_gaps,
        gap_fill_margin_s: args.gap_fill_margin_s,
        speech_db: args.speech_db,
        pal_pitch_correction: args.pal_pitch_correction,
        anchor_only_validation: args.anchor_only_validation,
        report_path: args.report,
        dub_codec: args.dub_codec,
        dub_bitrate_kbps: args.dub_bitrate,
        fps_mode: resolve_fps_mode(args.disable_fps_normalize, args.force_fps_ratio),
        keep_master_subs: !args.no_master_subs,
        include_donor_forced_subs: args.include_donor_forced_subs,
        donor_subs_explicit: args.include_donor_subs.unwrap_or_default(),
    })
}
