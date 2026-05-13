//! dubsync core pipeline as a library, used by both the CLI binary
//! (`src/bin/dubsync.rs`) and the GUI binary (`src/bin/dubsync_gui.rs`).
//!
//! The single high-level entry point is [`run_pipeline`]: it takes a fully-resolved
//! [`cli::RunConfig`] (built either from `clap` or from GUI form fields) and runs
//! every phase from extraction through remux, returning the final output path.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

pub mod audio;
pub mod binpath;
pub mod cli;
pub mod correlation;
pub mod error;
pub mod ffmpeg;
pub mod ffprobe;
pub mod interactive;
pub mod progress;
pub mod report;
pub mod subtitle;
pub mod sync;
pub mod tempdir;

#[cfg(feature = "gui")]
pub mod gui;

use audio::vad::SilenceInterval;
use cli::RunConfig;
use correlation::OffsetMap;
use ffmpeg::{ExtractedAnchors, ExtractedDonorSub, ExtractedDub};
use progress::{PhaseId, PipelineEvent, ProgressReporter};
use report::{OffsetMapReport, RunSummary};
use sync::SyncedDub;
use tempdir::Workspace;

/// Initialise `tracing-subscriber` with `RUST_LOG`-compatible filtering.
/// Both binaries call this at startup; idempotent enough that a second call from a
/// test harness simply errors silently.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

/// Run every dubsync phase end-to-end. Returns the path of the produced MKV on
/// success. The caller is responsible for tracing-subscriber initialisation and for
/// any user-facing "done" output.
///
/// `reporter` receives a `PhaseStarted`/`PhaseFinished` (or `PhaseFailed`) event at
/// every phase boundary plus throttled `PhaseProgress` updates inside the long
/// phases (correlation, fps-stretch, splice, remux). Tracing events still flow
/// through the standard subscriber for free-form line logging.
pub fn run_pipeline(mut cfg: RunConfig, reporter: &dyn ProgressReporter) -> Result<PathBuf> {
    let started = std::time::Instant::now();
    apply_cfg_overrides(&mut cfg);

    // Validate codec × donor channel count before doing any work. Catches
    // "ac3 + 7.1 source" upfront so a config typo fails in <1 s instead of
    // after a 30-minute pipeline run.
    validate_codec_against_channels(&cfg).context("output codec validation failed")?;
    // Validate explicit donor subtitle picks (existence + non-image-based).
    validate_donor_subs(&cfg).context("donor subtitle validation failed")?;

    if let Some(n) = cfg.threads {
        // build_global is idempotent across the same process — we ignore the second
        // call's error so the GUI's run-twice scenario doesn't fail.
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global();
    }

    reporter.emit(PipelineEvent::PhaseStarted {
        id: PhaseId::Workspace,
        title: "Preparing workspace".into(),
        eta_hint: None,
        detail: None,
    });
    let workspace =
        match Workspace::new(cfg.temp_dir.as_deref(), cfg.keep_temp).context("create workspace") {
            Ok(w) => w,
            Err(e) => {
                reporter.emit(PipelineEvent::PhaseFailed {
                    id: PhaseId::Workspace,
                    error: format!("{e:#}"),
                });
                return Err(e);
            }
        };
    tracing::info!(workspace = %workspace.path().display(), "workspace ready");
    reporter.emit(PipelineEvent::PhaseFinished {
        id: PhaseId::Workspace,
        summary: Some(format!("temp dir: {}", workspace.path().display())),
    });

    reporter.emit(PipelineEvent::PhaseStarted {
        id: PhaseId::ExtractAnchors,
        title: "Extracting anchor tracks".into(),
        eta_hint: None,
        detail: Some(format!(
            "master #{} + donor #{}",
            cfg.master_anchor_track, cfg.donor_anchor_track
        )),
    });
    let anchors = match extract_anchors(&cfg, &workspace).context("anchor extraction failed") {
        Ok(a) => a,
        Err(e) => {
            reporter.emit(PipelineEvent::PhaseFailed {
                id: PhaseId::ExtractAnchors,
                error: format!("{e:#}"),
            });
            return Err(e);
        }
    };
    tracing::info!(?anchors, "anchors extracted");
    reporter.emit(PipelineEvent::PhaseFinished {
        id: PhaseId::ExtractAnchors,
        summary: None,
    });

    reporter.emit(PipelineEvent::PhaseStarted {
        id: PhaseId::ExtractDubs,
        title: format!("Extracting {} dub track(s)", cfg.donor_dub_tracks.len()),
        eta_hint: None,
        detail: Some(format!("{:?}", cfg.donor_dub_tracks)),
    });
    let mut dubs = match extract_dubs(&cfg, &workspace, reporter).context("dub extraction failed") {
        Ok(d) => d,
        Err(e) => {
            reporter.emit(PipelineEvent::PhaseFailed {
                id: PhaseId::ExtractDubs,
                error: format!("{e:#}"),
            });
            return Err(e);
        }
    };
    tracing::info!(dub_count = dubs.len(), "dubs extracted");
    // Forced subtitle extraction shares the ExtractDubs card — it's another
    // round of donor I/O and runs in well under a second per track.
    let donor_selected_subs = match extract_donor_selected_subs(&cfg, &workspace)
        .context("donor subtitle extraction failed")
    {
        Ok(s) => s,
        Err(e) => {
            reporter.emit(PipelineEvent::PhaseFailed {
                id: PhaseId::ExtractDubs,
                error: format!("{e:#}"),
            });
            return Err(e);
        }
    };
    let summary = if donor_selected_subs.is_empty() {
        format!("{} track(s)", dubs.len())
    } else {
        format!(
            "{} track(s) + {} sub(s)",
            dubs.len(),
            donor_selected_subs.len()
        )
    };
    reporter.emit(PipelineEvent::PhaseFinished {
        id: PhaseId::ExtractDubs,
        summary: Some(summary),
    });

    let mut anchors = anchors;
    let fps_info = match fps_normalize_donor(&cfg, &mut anchors, &mut dubs, &workspace, reporter)
        .context("fps-normalize phase failed")
    {
        Ok(fps) => fps,
        Err(e) => {
            reporter.emit(PipelineEvent::PhaseFailed {
                id: PhaseId::FpsNormalize,
                error: format!("{e:#}"),
            });
            return Err(e);
        }
    };

    reporter.emit(PipelineEvent::PhaseStarted {
        id: PhaseId::Correlate,
        title: "Correlating anchors".into(),
        eta_hint: None,
        detail: Some(format!(
            "window {:.0}s, max-drift {:.0}s",
            cfg.correlation_window_s, cfg.max_drift_s
        )),
    });
    let (offset_map, master_silences) =
        match build_offset_map(&cfg, &anchors, &workspace, fps_info.master_fps, reporter)
            .context("correlation phase failed")
        {
            Ok(pair) => pair,
            Err(e) => {
                reporter.emit(PipelineEvent::PhaseFailed {
                    id: PhaseId::Correlate,
                    error: format!("{e:#}"),
                });
                return Err(e);
            }
        };
    log_offset_map(&offset_map);
    reporter.emit(PipelineEvent::PhaseFinished {
        id: PhaseId::Correlate,
        summary: Some(format!(
            "{} segment(s), {} anchors kept",
            offset_map.segments.len(),
            offset_map.anchor_count
        )),
    });

    reporter.emit(PipelineEvent::PhaseStarted {
        id: PhaseId::Splice,
        title: "Splicing dubs onto master timeline".into(),
        eta_hint: None,
        detail: Some(format!("{} track(s)", dubs.len())),
    });
    let splice_cfg = sync::SpliceConfig {
        silence_db: cfg.silence_db,
        silence_min_ms: cfg.silence_min_ms,
        snap_radius_s: cfg.snap_radius_s,
        crossfade_ms: cfg.crossfade_ms,
        smooth_gaps: cfg.smooth_gaps,
        gap_fill_margin_s: cfg.gap_fill_margin_s,
        speech_db: cfg.speech_db,
        gap_fill_max_ratio: cfg.gap_fill_max_ratio,
        gap_fill_silence_fade_ms: cfg.gap_fill_silence_fade_ms,
    };
    let synced = match sync::apply_to_dubs(
        &dubs,
        &offset_map,
        &master_silences,
        workspace.path(),
        &splice_cfg,
        reporter,
    )
    .context("sync application failed")
    {
        Ok(s) => s,
        Err(e) => {
            reporter.emit(PipelineEvent::PhaseFailed {
                id: PhaseId::Splice,
                error: format!("{e:#}"),
            });
            return Err(e);
        }
    };
    tracing::info!(count = synced.len(), "dubs synced");
    reporter.emit(PipelineEvent::PhaseFinished {
        id: PhaseId::Splice,
        summary: Some(format!("{} track(s)", synced.len())),
    });

    let total_duration_s = offset_map.master_duration_s as f64;
    reporter.emit(PipelineEvent::PhaseStarted {
        id: PhaseId::Remux,
        title: "Remuxing final MKV".into(),
        eta_hint: None,
        detail: Some(format!(
            "master video copy + {} dub(s) {}",
            synced.len(),
            cfg.dub_codec.ffmpeg_name().to_uppercase()
        )),
    });
    if let Err(e) = remux_final(
        &cfg,
        &synced,
        total_duration_s,
        reporter,
        &donor_selected_subs,
        &offset_map,
        &workspace,
    )
    .context("remux phase failed")
    {
        reporter.emit(PipelineEvent::PhaseFailed {
            id: PhaseId::Remux,
            error: format!("{e:#}"),
        });
        return Err(e);
    }
    reporter.emit(PipelineEvent::PhaseFinished {
        id: PhaseId::Remux,
        summary: Some(format!("wrote {}", cfg.output_file.display())),
    });

    let summary = build_run_summary(&cfg, &fps_info, &offset_map, &master_silences, started);

    // Optional detailed report file.
    if let Some(report_path) = cfg.report_path.as_ref() {
        let report = OffsetMapReport::build(
            summary.clone(),
            &offset_map,
            &master_silences,
            cfg.snap_radius_s,
        );
        match report::write_report(report_path, &report) {
            Ok(()) => tracing::info!(path = %report_path.display(), "report written"),
            Err(e) => {
                tracing::warn!(path = %report_path.display(), error = %e, "failed to write report")
            }
        }
    }

    reporter.emit(PipelineEvent::RunSummary(Box::new(summary)));

    Ok(cfg.output_file.clone())
}

/// Roll up the cross-phase data into the [`RunSummary`] struct used by reporters.
fn build_run_summary(
    cfg: &RunConfig,
    fps: &FpsInfo,
    map: &OffsetMap,
    master_silences: &[SilenceInterval],
    started: std::time::Instant,
) -> RunSummary {
    let total_anchors = map.anchor_count + map.rejected_count;
    let low_confidence_pct = if total_anchors > 0 {
        100.0 * map.rejected_count as f32 / total_anchors as f32
    } else {
        0.0
    };

    // Largest interior offset jump (max |Δ| where Δ = next.offset - this.offset).
    let mut max_jump_s: Option<f32> = None;
    let mut max_jump_at_master_s: Option<f32> = None;
    for w in map.segments.windows(2) {
        let delta = w[1].donor_offset_s - w[0].donor_offset_s;
        if max_jump_s
            .map(|cur| delta.abs() > cur.abs())
            .unwrap_or(true)
        {
            max_jump_s = Some(delta);
            max_jump_at_master_s = Some(w[0].master_end_s);
        }
    }

    // Per-boundary master-silence availability + total silence inserted.
    let mut master_snap_count = 0usize;
    let mut fallback_required_count = 0usize;
    let mut total_silence_inserted_s = 0.0_f32;
    if map.segments.len() >= 2 {
        let radius = f64::from(cfg.snap_radius_s);
        for i in 0..map.segments.len() - 1 {
            let boundary = f64::from(map.segments[i].master_end_s);
            let delta = f64::from(map.segments[i + 1].donor_offset_s)
                - f64::from(map.segments[i].donor_offset_s);
            let needed = delta.abs();
            // Silence inserted only when donor falls behind master (Δ < 0); for Δ ≥ 0
            // the splicer hard-cuts and the donor jumps forward, no gap.
            if delta < 0.0 {
                total_silence_inserted_s += needed as f32;
            }
            // Mirror of `sync::applier::best_master_silence`'s qualifying condition:
            // any master silence wider than |Δ| within `snap_radius_s` of the boundary.
            let mut found = false;
            for s in master_silences {
                let start = f64::from(s.start_s);
                let end = f64::from(s.end_s);
                if end - start < needed {
                    continue;
                }
                let center = (start + end) * 0.5;
                if (center - boundary).abs() <= radius {
                    found = true;
                    break;
                }
            }
            if found {
                master_snap_count += 1;
            } else {
                fallback_required_count += 1;
            }
        }
    }

    RunSummary {
        master_duration_s: map.master_duration_s,
        master_fps: fps.master_fps,
        donor_fps: fps.donor_fps,
        fps_stretch_ratio: fps.stretch_ratio,
        pal_pitch_applied: fps.pal_pitch_applied,
        auto_fps_disabled: cfg.fps_mode.is_disabled(),
        forced_fps_ratio: cfg.fps_mode.forced_ratio(),
        anchor_only_validation: cfg.anchor_only_validation,
        total_anchors,
        kept_anchors: map.anchor_count,
        rejected_anchors: map.rejected_count,
        segments: map.segments.len(),
        low_confidence_pct,
        max_jump_s,
        max_jump_at_master_s,
        master_snap_count,
        fallback_required_count,
        total_silence_inserted_s,
        output_file: cfg.output_file.clone(),
        elapsed_s: started.elapsed().as_secs_f64(),
    }
}

/// Apply CLI-driven config tweaks that change which dubs end up processed:
/// - `--anchor-only-validation` collapses the dub list to just the donor anchor and
///   forces solo-dub + include-donor-anchor so the output MKV ships only the synced
///   donor anchor (the canonical A/B-against-master form).
/// - `--include-donor-anchor` prepends the donor anchor to the dub list.
/// - `--solo-dub` truncates to one track.
///
/// Pre-pipeline guard: refuse `--dub-codec ac3` / `eac3` when any donor dub
/// has more channels than the codec can carry (ffmpeg caps both at 6). The
/// error message names the offending track and suggests `flac`, which always
/// preserves the full layout.
fn validate_codec_against_channels(cfg: &RunConfig) -> Result<()> {
    let max = cfg.dub_codec.max_channels();
    let donor_streams = ffprobe::list_audio_streams(&cfg.donor_file)?;
    for &idx in &cfg.donor_dub_tracks {
        if let Some(s) = donor_streams.iter().find(|s| s.index == idx) {
            if s.channels > max {
                return Err(anyhow!(
                    "donor track #{} has {} channels but codec `{}` only supports up to {} — \
                     use `--dub-codec flac` to keep the full layout",
                    idx,
                    s.channels,
                    cfg.dub_codec.ffmpeg_name(),
                    max
                ));
            }
        }
    }
    Ok(())
}

/// Pre-pipeline guard for `--include-donor-subs <IDX[,IDX...]>` (Phase C of
/// the subtitle work). Each explicitly requested donor sub index must:
/// 1. Exist in the donor's subtitle stream list.
/// 2. Be a text-based codec we can time-shift — image-based subs (PGS,
///    DVD-SUB, DVB-SUB) need OCR which is out of scope.
///
/// Empty list short-circuits to Ok without probing — the auto-forced path
/// has its own runtime warn-and-skip for image-based picks, so we don't need
/// to validate when only that path is active.
fn validate_donor_subs(cfg: &RunConfig) -> Result<()> {
    if cfg.donor_subs_explicit.is_empty() {
        return Ok(());
    }
    let streams = ffprobe::list_subtitle_streams(&cfg.donor_file)?;
    for &idx in &cfg.donor_subs_explicit {
        let Some(s) = streams.iter().find(|s| s.index == idx) else {
            let available: Vec<u32> = streams.iter().map(|s| s.index).collect();
            return Err(anyhow!(
                "donor subtitle track #{idx} does not exist — donor has subtitle indices {available:?}"
            ));
        };
        if s.is_image_based() {
            return Err(anyhow!(
                "donor subtitle track #{} is `{}` (image-based) — can't be \
                 time-shifted (would need OCR). Drop it from `--include-donor-subs` \
                 and re-run; use `--include-donor-forced-subs` to auto-detect \
                 text-based forced tracks instead.",
                idx,
                s.codec_name
            ));
        }
    }
    Ok(())
}

/// All three mutate `cfg` in place because downstream stages read the adjusted
/// dub list.
fn apply_cfg_overrides(cfg: &mut RunConfig) {
    if cfg.anchor_only_validation {
        cfg.donor_dub_tracks = vec![cfg.donor_anchor_track];
        cfg.include_donor_anchor = true;
        cfg.solo_dub = true;
        tracing::info!(
            track = cfg.donor_anchor_track,
            "anchor-only validation: only the donor anchor will be synced + remuxed"
        );
        return;
    }
    if cfg.include_donor_anchor && !cfg.donor_dub_tracks.contains(&cfg.donor_anchor_track) {
        tracing::info!(
            track = cfg.donor_anchor_track,
            "include-donor-anchor: synced donor anchor will REPLACE master anchor in output"
        );
        cfg.donor_dub_tracks.insert(0, cfg.donor_anchor_track);
    }
    if cfg.solo_dub && cfg.donor_dub_tracks.len() > 1 {
        let dropped: Vec<u32> = cfg.donor_dub_tracks.split_off(1);
        tracing::warn!(
            kept = cfg.donor_dub_tracks[0],
            dropped = ?dropped,
            "solo-dub: keeping only the first selected dub; other dubs will not be processed"
        );
    }
}

fn log_offset_map(map: &OffsetMap) {
    let total_kept = map.anchor_count;
    let total = total_kept + map.rejected_count;
    let pct_kept = if total > 0 {
        100.0 * total_kept as f32 / total as f32
    } else {
        0.0
    };
    tracing::info!(
        segments = map.segments.len(),
        master_duration_s = format!("{:.2}", map.master_duration_s),
        anchors_kept = total_kept,
        anchors_rejected = map.rejected_count,
        anchors_kept_pct = format!("{:.1}", pct_kept),
        valid_start_s = format!("{:.2}", map.valid_master_start_s),
        valid_end_s = format!("{:.2}", map.valid_master_end_s),
        "offset map built"
    );

    let muted_lead = map.valid_master_start_s;
    let muted_tail = map.master_duration_s - map.valid_master_end_s;
    if muted_lead > 0.5 {
        tracing::info!(
            "  lead-in [0.00..{:.2}s] has no correlation evidence — synced dubs will be SILENCED there",
            map.valid_master_start_s
        );
    }
    if muted_tail > 0.5 {
        tracing::info!(
            "  tail-out [{:.2}..{:.2}s] has no correlation evidence — synced dubs will be SILENCED there",
            map.valid_master_end_s, map.master_duration_s
        );
    }

    let mut prev_offset: Option<f32> = None;
    for (i, seg) in map.segments.iter().enumerate() {
        let dur = seg.master_end_s - seg.master_start_s;
        match prev_offset {
            None => {
                tracing::info!(
                    "  seg #{i}: master {:>8.2}..{:>8.2}s ({:>6.2}s)  offset {:+.3}s",
                    seg.master_start_s,
                    seg.master_end_s,
                    dur,
                    seg.donor_offset_s
                );
            }
            Some(prev) => {
                let delta = seg.donor_offset_s - prev;
                tracing::info!(
                    "  seg #{i}: master {:>8.2}..{:>8.2}s ({:>6.2}s)  offset {:+.3}s  jump {:+.3}s",
                    seg.master_start_s,
                    seg.master_end_s,
                    dur,
                    seg.donor_offset_s,
                    delta
                );
            }
        }
        prev_offset = Some(seg.donor_offset_s);
    }
}

#[allow(clippy::too_many_arguments)]
fn remux_final(
    cfg: &RunConfig,
    synced: &[SyncedDub],
    total_duration_s: f64,
    reporter: &dyn ProgressReporter,
    donor_selected_subs: &[ExtractedDonorSub],
    offset_map: &OffsetMap,
    workspace: &Workspace,
) -> Result<()> {
    let master_streams = ffprobe::list_audio_streams(&cfg.master_file)?;
    let donor_streams = ffprobe::list_audio_streams(&cfg.donor_file)?;

    let (master_anchor_track, master_anchor_lang) = if cfg.include_donor_anchor || cfg.solo_dub {
        (None, None)
    } else {
        let lang = master_streams
            .iter()
            .find(|s| s.index == cfg.master_anchor_track)
            .and_then(|s| s.language())
            .map(str::to_owned);
        (Some(cfg.master_anchor_track), lang)
    };

    let mut for_remux: Vec<ffmpeg::DubForRemux> = Vec::with_capacity(synced.len());
    let mut bitrate_per_dub: Vec<Option<u32>> = Vec::with_capacity(synced.len());
    for s in synced {
        let donor_meta = donor_streams.iter().find(|d| d.index == s.donor_index);
        // Resolve "user override → per-codec/per-channel default → None" once
        // per dub. FLAC always yields None; lossy codecs return Some unless the
        // donor channels are zero (treated as no info → encoder default).
        let channels = donor_meta.map(|d| d.channels).unwrap_or(0);
        let bitrate = cfg
            .dub_bitrate_kbps
            .or_else(|| cfg.dub_codec.default_bitrate_kbps(channels));
        bitrate_per_dub.push(bitrate);
        for_remux.push(ffmpeg::DubForRemux {
            synced: s,
            language: donor_meta.and_then(|d| d.language()).map(str::to_owned),
            title: donor_meta.and_then(|d| d.title()).map(str::to_owned),
        });
    }

    // Phase B: shift extracted donor forced subs through the offset map and
    // write each shifted .srt to disk so ffmpeg can pick it up as a new input.
    // Fast (text manipulation, milliseconds for an hour-long episode), so no
    // separate chat-panel card.
    let mut donor_synced_subs: Vec<ffmpeg::DonorSyncedSub> =
        Vec::with_capacity(donor_selected_subs.len());
    for sub in donor_selected_subs {
        let raw = std::fs::read_to_string(&sub.srt_path)
            .with_context(|| format!("read donor sub {}", sub.srt_path.display()))?;
        let events = subtitle::srt::parse(&raw)
            .with_context(|| format!("parse donor sub {}", sub.srt_path.display()))?;
        let shifted = subtitle::apply_offset_map(&events, offset_map);
        let shifted_text = subtitle::srt::serialize(&shifted);
        let shifted_path = workspace.child(&format!("donor_sub_{}_synced.srt", sub.donor_index));
        std::fs::write(&shifted_path, shifted_text)
            .with_context(|| format!("write shifted donor sub {}", shifted_path.display()))?;
        tracing::info!(
            track = sub.donor_index,
            input_events = events.len(),
            shifted_events = shifted.len(),
            out = %shifted_path.display(),
            "shifted donor forced subtitle"
        );
        donor_synced_subs.push(ffmpeg::DonorSyncedSub {
            srt_path: shifted_path,
            language: sub.language.clone(),
            title: sub.title.clone(),
            forced: sub.forced,
            default: sub.default,
        });
    }

    let progress_cb = move |frac: f32| {
        reporter.emit(PipelineEvent::PhaseProgress {
            id: PhaseId::Remux,
            fraction: frac,
            detail: None,
        });
    };

    ffmpeg::remux(
        &cfg.master_file,
        master_anchor_track,
        master_anchor_lang.as_deref(),
        &for_remux,
        &cfg.output_file,
        Some(total_duration_s),
        &progress_cb,
        cfg.dub_codec,
        &bitrate_per_dub,
        cfg.keep_master_subs,
        &donor_synced_subs,
    )
    .context("ffmpeg remux invocation")?;
    Ok(())
}

fn build_offset_map(
    cfg: &RunConfig,
    anchors: &ExtractedAnchors,
    workspace: &Workspace,
    master_fps: f64,
    reporter: &dyn ProgressReporter,
) -> Result<(OffsetMap, Vec<SilenceInterval>)> {
    let (master_samples, master_sr) =
        audio::wav::read_mono_f32(&anchors.master_anchor_wav).context("read master anchor WAV")?;
    let (donor_samples, donor_sr) =
        audio::wav::read_mono_f32(&anchors.donor_anchor_wav).context("read donor anchor WAV")?;
    if master_sr != donor_sr {
        return Err(anyhow!(
            "anchor sample rates differ: master {master_sr}Hz vs donor {donor_sr}Hz"
        ));
    }

    // Throttle Correlate progress emissions: correlate_sliding calls our callback
    // once per FFT window (hundreds for an hour-long episode) but the GUI / CLI
    // only need ~10 updates per second. We coalesce by tracking the last emitted
    // integer percent across all rayon worker threads.
    let last_pct = std::sync::atomic::AtomicI32::new(-1);
    let progress_cb = |frac: f32| {
        let pct = (frac * 100.0).clamp(0.0, 100.0) as i32;
        let prev = last_pct.load(std::sync::atomic::Ordering::Relaxed);
        if pct == prev {
            return;
        }
        // CAS so concurrent workers don't all emit for the same percent.
        if last_pct
            .compare_exchange(
                prev,
                pct,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return;
        }
        reporter.emit(PipelineEvent::PhaseProgress {
            id: PhaseId::Correlate,
            fraction: frac,
            detail: None,
        });
    };

    let hop_s = cfg.correlation_window_s * 0.5;
    let raw = correlation::correlate_sliding(
        &master_samples,
        &donor_samples,
        master_sr,
        cfg.correlation_window_s,
        hop_s,
        cfg.max_drift_s,
        &progress_cb,
    );

    if cfg.keep_temp {
        dump_json(&workspace.child("anchors.json"), &raw, "anchors.json");

        let master_onsets = audio::peaks::detect_onsets(&master_samples, master_sr, 12.0, 0.5);
        let donor_onsets = audio::peaks::detect_onsets(&donor_samples, donor_sr, 12.0, 0.5);
        tracing::info!(
            master_onsets = master_onsets.len(),
            donor_onsets = donor_onsets.len(),
            "onset detection complete"
        );
        dump_json(
            &workspace.child("master_onsets.json"),
            &master_onsets,
            "master_onsets.json",
        );
        dump_json(
            &workspace.child("donor_onsets.json"),
            &donor_onsets,
            "donor_onsets.json",
        );
    }

    let master_duration_s = master_samples.len() as f32 / master_sr as f32;
    let mut map = OffsetMap::build(
        &raw,
        master_fps as f32,
        master_duration_s,
        cfg.correlation_window_s,
        6.0,
        cfg.max_segment_jump_s,
    )
    .context("offset-map construction failed")?;

    let half_hop_s = cfg.correlation_window_s * 0.25;
    let traces = map.refine_transitions(&master_samples, &donor_samples, master_sr, half_hop_s);

    // Master-anchor silence detection. This is the primary signal the splicer uses to
    // place each segment boundary in a moment that is *visually and audibly* silent in
    // the master video — so the inserted silence gap (Δ<0) or hard cut (Δ≥0) is
    // imperceptible regardless of dialog state.
    let master_silences = audio::vad::detect_silence_mono(
        &master_samples,
        master_sr,
        cfg.silence_db,
        cfg.silence_min_ms as f32 / 1000.0,
    );
    tracing::info!(
        intervals = master_silences.len(),
        threshold_db = cfg.silence_db,
        min_ms = cfg.silence_min_ms,
        "master-anchor silence detected"
    );

    if cfg.keep_temp {
        dump_json(
            &workspace.child("transition_traces.json"),
            &traces,
            "transition_traces.json",
        );

        let dump_path = workspace.child("offset_map.json");
        if let Err(e) = map.dump_json(&dump_path) {
            tracing::warn!(error = %e, "failed to dump offset_map.json");
        } else {
            tracing::info!(path = %dump_path.display(), "offset_map.json written");
        }

        dump_json(
            &workspace.child("master_silences.json"),
            &master_silences,
            "master_silences.json",
        );
    }
    Ok((map, master_silences))
}

fn extract_anchors(cfg: &RunConfig, ws: &Workspace) -> Result<ExtractedAnchors> {
    let master_anchor_wav = ws.child(&format!("master_anchor_{}.wav", cfg.master_anchor_track));
    let donor_anchor_wav = ws.child(&format!("donor_anchor_{}.wav", cfg.donor_anchor_track));

    tracing::info!(out = %master_anchor_wav.display(), "extracting master anchor");
    ffmpeg::extract_anchor(
        &cfg.master_file,
        cfg.master_anchor_track,
        cfg.anchor_rate,
        &master_anchor_wav,
    )
    .context("extract master anchor")?;

    tracing::info!(out = %donor_anchor_wav.display(), "extracting donor anchor");
    ffmpeg::extract_anchor(
        &cfg.donor_file,
        cfg.donor_anchor_track,
        cfg.anchor_rate,
        &donor_anchor_wav,
    )
    .context("extract donor anchor")?;

    Ok(ExtractedAnchors {
        master_anchor_wav,
        donor_anchor_wav,
    })
}

/// Extract donor subtitle tracks selected via either the auto-forced filter
/// (`cfg.include_donor_forced_subs`) or the explicit index list
/// (`cfg.donor_subs_explicit`), as SRT files into the workspace. The two
/// sources compose as a UNION — a track that satisfies either condition is
/// extracted. Duplicates (forced + explicit on the same index) are deduped.
///
/// Image-based codecs (PGS / DVD-SUB / DVB-SUB) are detected via
/// [`ffprobe::SubtitleStream::is_image_based`]:
/// - When picked by **forced auto**, they're skipped with a warning (the user
///   asked for "all forced", not specifically a PGS track, so silent skip is
///   the right thing).
/// - When picked **explicitly**, [`validate_donor_subs`] (called earlier in
///   `run_pipeline`) has already rejected them with a clear error, so this
///   branch never sees them in the explicit case in practice.
///
/// Returns an empty Vec when neither selection mechanism is active. The
/// remux path treats an empty list as "no donor subs to mux", so this never
/// fails the pipeline on its own.
fn extract_donor_selected_subs(cfg: &RunConfig, ws: &Workspace) -> Result<Vec<ExtractedDonorSub>> {
    if !cfg.include_donor_forced_subs && cfg.donor_subs_explicit.is_empty() {
        return Ok(Vec::new());
    }
    let streams =
        ffprobe::list_subtitle_streams(&cfg.donor_file).context("list donor subtitle streams")?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in &streams {
        let pick_forced = cfg.include_donor_forced_subs && s.is_forced();
        let pick_explicit = cfg.donor_subs_explicit.contains(&s.index);
        if !pick_forced && !pick_explicit {
            continue;
        }
        if !seen.insert(s.index) {
            continue;
        }
        if s.is_image_based() {
            // Validation already rejected explicit picks of image-based; this
            // branch only fires for forced-auto picks of PGS / DVD-SUB tracks.
            tracing::warn!(
                track = s.index,
                codec = %s.codec_name,
                "skipping forced donor sub: image-based codec needs OCR (not supported)"
            );
            continue;
        }
        let srt_path = ws.child(&format!("donor_sub_{}.srt", s.index));
        tracing::info!(
            track = s.index,
            codec = %s.codec_name,
            out = %srt_path.display(),
            forced = s.is_forced(),
            explicit = pick_explicit,
            "extracting donor subtitle"
        );
        ffmpeg::extract_subtitle(&cfg.donor_file, s.index, &srt_path)
            .with_context(|| format!("extract donor sub track {}", s.index))?;
        out.push(ExtractedDonorSub {
            donor_index: s.index,
            srt_path,
            language: s.language().map(str::to_owned),
            title: s.title().map(str::to_owned),
            forced: s.is_forced(),
            default: s.is_default(),
        });
    }
    Ok(out)
}

fn extract_dubs(
    cfg: &RunConfig,
    ws: &Workspace,
    reporter: &dyn ProgressReporter,
) -> Result<Vec<ExtractedDub>> {
    let total = cfg.donor_dub_tracks.len().max(1);
    let mut dubs = Vec::with_capacity(cfg.donor_dub_tracks.len());
    for (i, &idx) in cfg.donor_dub_tracks.iter().enumerate() {
        let out = ws.child(&format!("donor_dub_{idx}.wav"));
        tracing::info!(track = idx, out = %out.display(), "extracting dub");
        ffmpeg::extract_dub(&cfg.donor_file, idx, &out)
            .with_context(|| format!("extract dub track {idx}"))?;
        dubs.push(ExtractedDub {
            donor_index: idx,
            wav: out,
        });
        reporter.emit(PipelineEvent::PhaseProgress {
            id: PhaseId::ExtractDubs,
            fraction: (i + 1) as f32 / total as f32,
            detail: Some(format!("{}/{total} tracks", i + 1)),
        });
    }
    Ok(dubs)
}

/// FPS information captured by [`fps_normalize_donor`] — fed into the final
/// [`RunSummary`] for the post-run report. `stretch_ratio` is `Some` only when a
/// stretch actually ran; when None the donor played back at master speed already.
#[derive(Debug, Clone, Copy)]
struct FpsInfo {
    master_fps: f64,
    donor_fps: Option<f64>,
    stretch_ratio: Option<f64>,
    pal_pitch_applied: bool,
}

/// Helper: emit a "skip" card in one shot (Started + Finished back-to-back) for
/// the FpsNormalize phase. Used by every short-circuit path so the chat panel
/// always shows a card with a clear reason instead of nothing for the phase.
fn emit_fps_skip(reporter: &dyn ProgressReporter, title: &str, summary: &str) {
    reporter.emit(PipelineEvent::PhaseStarted {
        id: PhaseId::FpsNormalize,
        title: title.into(),
        eta_hint: None,
        detail: None,
    });
    reporter.emit(PipelineEvent::PhaseFinished {
        id: PhaseId::FpsNormalize,
        summary: Some(summary.into()),
    });
}

/// Branch on the user's chosen `FpsMode` and either skip the stretch entirely
/// (Disabled, or Forced ratio ≈ 1), or run [`run_donor_stretch`] with the right
/// ratio + pitch correction.
///
/// Returns the FPS info used downstream (offset-map jump threshold) and surfaced
/// in the run summary.
fn fps_normalize_donor(
    cfg: &RunConfig,
    anchors: &mut ExtractedAnchors,
    dubs: &mut [ExtractedDub],
    workspace: &Workspace,
    reporter: &dyn ProgressReporter,
) -> Result<FpsInfo> {
    let master_fps = ffprobe::probe_video_fps(&cfg.master_file)
        .ok()
        .flatten()
        .unwrap_or(25.0);

    match cfg.fps_mode {
        cli::FpsMode::Disabled => {
            // User explicitly opted out — don't even probe the donor; trust the
            // raw audio is already at master speed.
            emit_fps_skip(
                reporter,
                "FPS normalize: skipped",
                "disabled by --disable-fps-normalize",
            );
            Ok(FpsInfo {
                master_fps,
                donor_fps: None,
                stretch_ratio: None,
                pal_pitch_applied: false,
            })
        }
        cli::FpsMode::Forced(ratio) => {
            // 1.0 ≈ identity → still record as forced for the summary, but skip
            // the multi-minute rubberband pass.
            if (ratio - 1.0).abs() <= 0.001 {
                emit_fps_skip(
                    reporter,
                    &format!("FPS normalize: skipped (forced ratio {ratio:.4})"),
                    "forced ratio is identity — no stretch needed",
                );
                return Ok(FpsInfo {
                    master_fps,
                    donor_fps: Some(master_fps * ratio),
                    stretch_ratio: None,
                    pal_pitch_applied: false,
                });
            }
            // Pitch formula in ratio-only form (equivalent to the original
            // `12·log2(master_fps/donor_fps)`):
            //   semitones = -12 · log2(ratio)
            // Verified by `cli::tests::pitch_formula_equivalence`.
            let semitones = if cfg.pal_pitch_correction {
                Some(-12.0 * ratio.log2())
            } else {
                None
            };
            let title = format!("Time-stretching donor audio (forced ratio {ratio:.4})");
            run_donor_stretch(
                cfg, anchors, dubs, workspace, reporter, ratio, semitones, title,
            )?;
            Ok(FpsInfo {
                master_fps,
                donor_fps: Some(master_fps * ratio),
                stretch_ratio: Some(ratio),
                pal_pitch_applied: cfg.pal_pitch_correction,
            })
        }
        cli::FpsMode::Auto => {
            // Probe donor; auto-stretch iff |ratio - 1| > 0.1%.
            let donor_fps = ffprobe::probe_video_fps(&cfg.donor_file).ok().flatten();
            let donor_fps = match donor_fps {
                Some(f) if f > 0.0 => f,
                _ => {
                    emit_fps_skip(
                        reporter,
                        "Checking fps",
                        "donor has no video stream — skipping",
                    );
                    return Ok(FpsInfo {
                        master_fps,
                        donor_fps: None,
                        stretch_ratio: None,
                        pal_pitch_applied: false,
                    });
                }
            };
            let ratio = donor_fps / master_fps;
            if (ratio - 1.0).abs() <= 0.001 {
                emit_fps_skip(
                    reporter,
                    &format!("Checking fps ({master_fps:.3} vs {donor_fps:.3})"),
                    "no fps mismatch",
                );
                return Ok(FpsInfo {
                    master_fps,
                    donor_fps: Some(donor_fps),
                    stretch_ratio: None,
                    pal_pitch_applied: false,
                });
            }
            // Same equivalence as the Forced branch — keep the formulas in sync.
            let semitones = if cfg.pal_pitch_correction {
                Some(-12.0 * ratio.log2())
            } else {
                None
            };
            let title = format!(
                "Time-stretching donor audio ({:.3} → {:.3} fps, ratio {:.4})",
                donor_fps, master_fps, ratio
            );
            run_donor_stretch(
                cfg, anchors, dubs, workspace, reporter, ratio, semitones, title,
            )?;
            Ok(FpsInfo {
                master_fps,
                donor_fps: Some(donor_fps),
                stretch_ratio: Some(ratio),
                pal_pitch_applied: cfg.pal_pitch_correction,
            })
        }
    }
}

/// Run rubberband over donor anchor + every dub in parallel to apply a global
/// time-stretch by `ratio` (target duration = input × ratio) with optional pitch
/// shift `semitones`. Mutates the passed handles to point at the stretched
/// outputs and removes the originals when `cfg.keep_temp == false`.
///
/// `title` is the chat-panel card heading; the function builds its own `detail`
/// (file count + ETA) from the input WAVs.
#[allow(clippy::too_many_arguments)]
fn run_donor_stretch(
    cfg: &RunConfig,
    anchors: &mut ExtractedAnchors,
    dubs: &mut [ExtractedDub],
    workspace: &Workspace,
    reporter: &dyn ProgressReporter,
    ratio: f64,
    semitones: Option<f64>,
    title: String,
) -> Result<()> {
    use crate::audio::stretch::{stretch_file, StretchOpts};

    let stretched_dir = workspace.child("stretched");
    if !stretched_dir.exists() {
        std::fs::create_dir_all(&stretched_dir).context("create stretched workspace dir")?;
    }

    struct Job {
        in_path: PathBuf,
        out_path: PathBuf,
        in_frames: u64,
        target_duration_s: f64,
        slot: usize, // index in shared progress array
    }

    let mut jobs: Vec<Job> = Vec::with_capacity(1 + dubs.len());
    let mut total_in_duration_s: f64 = 0.0;

    let push_job =
        |jobs: &mut Vec<Job>, total: &mut f64, in_path: &std::path::Path| -> Result<()> {
            let info = audio::wav::probe_frames(in_path)
                .with_context(|| format!("probe WAV {}", in_path.display()))?;
            let in_dur_s = info.frames as f64 / info.sample_rate as f64;
            let target_dur_s = in_dur_s * ratio;
            let stem = in_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("stretched");
            let out_path = stretched_dir.join(format!("{stem}_stretched.wav"));
            let slot = jobs.len();
            jobs.push(Job {
                in_path: in_path.to_path_buf(),
                out_path,
                in_frames: info.frames,
                target_duration_s: target_dur_s,
                slot,
            });
            *total += in_dur_s;
            Ok(())
        };

    push_job(
        &mut jobs,
        &mut total_in_duration_s,
        &anchors.donor_anchor_wav,
    )?;
    for dub in dubs.iter() {
        push_job(&mut jobs, &mut total_in_duration_s, &dub.wav)?;
    }
    let _ = stretched_dir; // jobs hold their own paths now

    // ETA estimate: rubberband typically processes ~6× realtime.
    let eta_s = (total_in_duration_s / 6.0).max(1.0);
    let detail = format!(
        "{} file(s), {:.1} min total{}",
        jobs.len(),
        total_in_duration_s / 60.0,
        if cfg.pal_pitch_correction {
            ", with PAL pitch correction"
        } else {
            ""
        }
    );
    reporter.emit(PipelineEvent::PhaseStarted {
        id: PhaseId::FpsNormalize,
        title,
        eta_hint: Some(std::time::Duration::from_secs_f64(eta_s)),
        detail: Some(detail),
    });

    // Per-job progress bytes done. Driving thread sums these and emits PhaseProgress.
    use std::sync::atomic::{AtomicU64, Ordering};
    let per_job_done: Vec<AtomicU64> = (0..jobs.len()).map(|_| AtomicU64::new(0)).collect();
    let per_job_expected: Vec<u64> = jobs
        .iter()
        .map(|j| (j.in_frames as f64 * ratio).round() as u64)
        .collect();
    let total_expected: u64 = per_job_expected.iter().sum::<u64>().max(1);

    let stop = std::sync::atomic::AtomicBool::new(false);
    rayon::scope(|s| {
        // Reporting thread — emits at most ~5 Hz so the GUI / CLI \r updates stay smooth.
        let per_job_done = &per_job_done;
        let per_job_expected = &per_job_expected;
        let stop_ref = &stop;
        s.spawn(move |_| {
            while !stop_ref.load(Ordering::Relaxed) {
                let sum: u64 = per_job_done.iter().map(|a| a.load(Ordering::Relaxed)).sum();
                let frac = (sum as f64 / total_expected as f64).min(1.0) as f32;
                let done_files = per_job_done
                    .iter()
                    .zip(per_job_expected.iter())
                    .filter(|(d, e)| d.load(Ordering::Relaxed) >= **e && **e > 0)
                    .count();
                reporter.emit(PipelineEvent::PhaseProgress {
                    id: PhaseId::FpsNormalize,
                    fraction: frac,
                    detail: Some(format!("{done_files}/{} files", per_job_expected.len())),
                });
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        });

        use rayon::prelude::*;
        let results: Vec<Result<()>> = jobs
            .par_iter()
            .map(|job| {
                let slot = job.slot;
                let progress_fn = |frac: f32| {
                    let bytes = (frac as f64 * per_job_expected[slot] as f64) as u64;
                    per_job_done[slot].store(bytes, Ordering::Relaxed);
                };
                stretch_file(
                    &job.in_path,
                    &job.out_path,
                    StretchOpts {
                        target_duration_s: job.target_duration_s,
                        pitch_semitones: semitones,
                    },
                    &progress_fn,
                )
                .with_context(|| format!("rubberband stretch {}", job.in_path.display()))
            })
            .collect();

        stop.store(true, Ordering::Relaxed);

        // Bubble up the first failure, if any.
        for r in results {
            r?;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // Sanity: every job finished and produced an output WAV.
    for job in &jobs {
        if !job.out_path.exists() {
            return Err(anyhow!(
                "rubberband did not produce expected output: {}",
                job.out_path.display()
            ));
        }
    }

    // Mutate the handles to point at the stretched outputs. Index 0 = donor anchor;
    // 1.. = dubs.
    anchors.donor_anchor_wav = jobs[0].out_path.clone();
    for (i, dub) in dubs.iter_mut().enumerate() {
        dub.wav = jobs[i + 1].out_path.clone();
    }

    // Remove originals unless `keep_temp` was requested (workspace cleanup will
    // remove the stretched dir on Drop anyway, but leaving both copies doubles disk
    // pressure during a 1-hour episode run).
    if !cfg.keep_temp {
        for job in &jobs {
            let _ = std::fs::remove_file(&job.in_path);
        }
    }

    let summary = format!(
        "stretched {} file(s) by ratio {:.4}{}",
        jobs.len(),
        ratio,
        if cfg.pal_pitch_correction {
            format!(" (pitch {:+.3} semitones)", semitones.unwrap_or(0.0))
        } else {
            String::new()
        }
    );
    reporter.emit(PipelineEvent::PhaseFinished {
        id: PhaseId::FpsNormalize,
        summary: Some(summary),
    });

    Ok(())
}

/// Pretty-print `value` as JSON to `path`. Logs success or failure; never panics —
/// dumping diagnostic artifacts must not fail the actual sync run. Public so the GUI
/// can use it for its own diagnostic dumps.
pub fn dump_json<T: serde::Serialize>(path: &std::path::Path, value: &T, label: &str) {
    match std::fs::File::create(path) {
        Ok(f) => match serde_json::to_writer_pretty(f, value) {
            Ok(()) => tracing::info!(path = %path.display(), "{label} written"),
            Err(e) => tracing::warn!(error = %e, "failed to dump {label}"),
        },
        Err(e) => tracing::warn!(error = %e, "failed to create {label}"),
    }
}
