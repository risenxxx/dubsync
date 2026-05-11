use crate::cli::DubCodec;
use crate::error::{DubsyncError, Result};
use crate::sync::SyncedDub;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Where the extracted PCM WAVs live.
#[derive(Debug, Clone)]
pub struct ExtractedAnchors {
    pub master_anchor_wav: PathBuf,
    pub donor_anchor_wav: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ExtractedDub {
    pub donor_index: u32,
    pub wav: PathBuf,
}

/// One donor subtitle stream extracted as SRT, ready for offset-map shifting.
/// Phase B input — produced by [`crate::lib::extract_donor_forced_subs`] (an
/// internal helper) and consumed by `remux_final` which reads/shifts/writes
/// the corresponding [`DonorSyncedSub`].
#[derive(Debug, Clone)]
pub struct ExtractedDonorSub {
    pub donor_index: u32,
    pub srt_path: PathBuf,
    pub language: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
    pub default: bool,
}

/// Extract one mono f32le WAV at `anchor_rate` Hz, suitable for FFT correlation.
pub fn extract_anchor(src: &Path, stream_index: u32, anchor_rate: u32, out: &Path) -> Result<()> {
    let status = crate::binpath::ffmpeg_command()
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args([
            "-map",
            &format!("0:{stream_index}"),
            "-vn",
            "-sn",
            "-dn",
            "-ac",
            "1",
            "-ar",
            &anchor_rate.to_string(),
            "-c:a",
            "pcm_f32le",
            "-f",
            "wav",
        ])
        .arg(out)
        .output()?;

    if !status.status.success() {
        return Err(DubsyncError::FfmpegFailed {
            stderr: String::from_utf8_lossy(&status.stderr).into_owned(),
        });
    }
    Ok(())
}

/// One synced dub plus its language tag (carried through from the donor stream).
#[derive(Debug, Clone)]
pub struct DubForRemux<'a> {
    pub synced: &'a SyncedDub,
    pub language: Option<String>,
    pub title: Option<String>,
}

/// One donor subtitle stream that's been extracted as SRT, time-shifted through
/// the offset map, and is ready to be muxed into the output. Phase B input.
#[derive(Debug, Clone)]
pub struct DonorSyncedSub {
    pub srt_path: PathBuf,
    pub language: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
    pub default: bool,
}

/// Build the final MKV: master video + master anchor (copy) + every synced dub
/// re-encoded with `dub_codec`.
///
/// `total_duration_s` enables progress reporting via ffmpeg's `-progress pipe:1`
/// machine-readable stats (parsed `out_time_us` / `total_duration_s`). Pass `None`
/// to skip progress (caller can't or doesn't care to compute total duration).
/// `progress` is called with `fraction` in `[0.0, 1.0]` ~10x per second while ffmpeg
/// is running.
///
/// `bitrate_per_dub_kbps` parallels `synced` (one entry per dub track). `Some(N)`
/// emits `-b:a:n Nk`; `None` lets the codec encoder pick its default. FLAC ignores
/// the bitrate flag entirely. Caller (`lib::remux_final`) is responsible for
/// resolving "user override → per-codec/per-channel default → None for lossless".
#[allow(clippy::too_many_arguments)]
pub fn remux(
    master_file: &Path,
    master_anchor_track: Option<u32>,
    master_anchor_lang: Option<&str>,
    synced: &[DubForRemux<'_>],
    output: &Path,
    total_duration_s: Option<f64>,
    progress: &(dyn Fn(f32) + Sync),
    dub_codec: DubCodec,
    bitrate_per_dub_kbps: &[Option<u32>],
    keep_master_subs: bool,
    donor_synced_subs: &[DonorSyncedSub],
) -> Result<()> {
    debug_assert_eq!(
        synced.len(),
        bitrate_per_dub_kbps.len(),
        "bitrate_per_dub_kbps must parallel synced"
    );

    // Pre-flight: when keep_master_subs is on we need to know how many subs the
    // master has so we can emit the right number of `-c:s:N copy` directives.
    // `list_subtitle_streams` returns an empty Vec for files without subs, so
    // a `-map 0:s` against a sub-less master is a no-op (ffmpeg silently maps
    // zero streams).
    let master_sub_count = if keep_master_subs {
        crate::ffprobe::list_subtitle_streams(master_file)?.len()
    } else {
        0
    };

    let mut cmd = crate::binpath::ffmpeg_command();
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    if total_duration_s.is_some() {
        // Machine-readable progress stream on stdout. ffmpeg writes one block
        // every ~500ms by default; lines look like `out_time_us=3500000\nprogress=continue\n…`.
        cmd.args(["-progress", "pipe:1"]);
    }
    cmd.arg("-i").arg(master_file);
    for d in synced {
        cmd.arg("-i").arg(&d.synced.wav);
    }
    // Donor synced subtitles are appended AFTER all dub WAV inputs; their input
    // index is `1 + synced.len() + i` for the i-th donor sub.
    for sub in donor_synced_subs {
        cmd.arg("-i").arg(&sub.srt_path);
    }
    let donor_subs_first_input = 1 + synced.len();

    // Output stream layout: 0 = master video, then (optionally) master anchor, then
    // synced dubs in order. When master_anchor_track is None the synced dubs slide
    // up by one slot — used by --include-donor-anchor for diagnostic A/B against the
    // master video. Subtitles (master pass-through + donor synced) come last.
    cmd.args(["-map", "0:v"]);
    let master_anchor_present = master_anchor_track.is_some();
    if let Some(t) = master_anchor_track {
        cmd.args(["-map", &format!("0:{t}")]);
    }
    for (i, _) in synced.iter().enumerate() {
        cmd.args(["-map", &format!("{}:a", i + 1)]);
    }
    if keep_master_subs && master_sub_count > 0 {
        cmd.args(["-map", "0:s"]);
    }
    for i in 0..donor_synced_subs.len() {
        cmd.args(["-map", &format!("{}:0", donor_subs_first_input + i)]);
    }

    cmd.args(["-c:v", "copy"]);
    let mut audio_idx = 0usize;
    if master_anchor_present {
        cmd.args(["-c:a:0", "copy"]);
        audio_idx = 1;
    }
    let synced_first_idx = audio_idx;
    let codec_name = dub_codec.ffmpeg_name();
    for (i, bitrate) in bitrate_per_dub_kbps.iter().enumerate() {
        let n = synced_first_idx + i;
        cmd.args([format!("-c:a:{n}").as_str(), codec_name]);
        if let Some(kbps) = bitrate {
            cmd.args([format!("-b:a:{n}").as_str(), &format!("{kbps}k")]);
        }
    }

    // Subtitle codecs: master subs copy through unchanged; donor synced subs are
    // SRT text, also `copy` (we wrote them in canonical SubRip).
    let total_subs = master_sub_count + donor_synced_subs.len();
    for n in 0..total_subs {
        cmd.args([format!("-c:s:{n}").as_str(), "copy"]);
    }

    // Master anchor metadata (refresh in case the container drops it during remux).
    if master_anchor_present {
        if let Some(lang) = master_anchor_lang {
            cmd.args(["-metadata:s:a:0", &format!("language={lang}")]);
        }
        cmd.args(["-disposition:a:0", "default"]);
    }

    for (i, d) in synced.iter().enumerate() {
        let n = synced_first_idx + i;
        if let Some(lang) = &d.language {
            cmd.args([
                format!("-metadata:s:a:{n}").as_str(),
                &format!("language={lang}"),
            ]);
        }
        let title = d
            .title
            .clone()
            .unwrap_or_else(|| format!("dubsync (track #{})", d.synced.donor_index));
        cmd.args([
            format!("-metadata:s:a:{n}").as_str(),
            &format!("title={title}"),
        ]);
        // When master anchor is absent the first synced track inherits the default
        // disposition; otherwise dubs are non-default.
        let disposition = if !master_anchor_present && i == 0 {
            "default"
        } else {
            "0"
        };
        cmd.args([format!("-disposition:a:{n}").as_str(), disposition]);
    }

    // Donor synced subtitle metadata + disposition. Master subs (when kept)
    // occupy stream indices [0..master_sub_count); donor synced subs follow.
    for (i, sub) in donor_synced_subs.iter().enumerate() {
        let n = master_sub_count + i;
        if let Some(lang) = &sub.language {
            cmd.args([
                format!("-metadata:s:s:{n}").as_str(),
                &format!("language={lang}"),
            ]);
        }
        if let Some(title) = &sub.title {
            cmd.args([
                format!("-metadata:s:s:{n}").as_str(),
                &format!("title={title}"),
            ]);
        }
        // Preserve forced/default flags from the donor; default is rarely set
        // on forced-sub tracks but we honour it if it was.
        let mut flags: Vec<&str> = Vec::new();
        if sub.forced {
            flags.push("forced");
        }
        if sub.default {
            flags.push("default");
        }
        let disposition = if flags.is_empty() {
            "0".to_string()
        } else {
            flags.join("+")
        };
        cmd.args([format!("-disposition:s:{n}").as_str(), &disposition]);
    }

    // Disable the muxer's per-stream interleave-delta gate (output option, so it
    // must sit immediately before the output path). With sparse subtitle packets
    // (one SubRip event every few minutes) and audio coming from a second input
    // (the synced dub WAV) while video copies from the master, the default 10 s
    // gate makes the MKV muxer back-load whole minutes of audio into two giant
    // trailing blocks — decodable by ffmpeg-cli but silent in mpv / VLC /
    // Jellyfin because seek-by-cluster never reaches the audio for the bulk of
    // the timeline. Setting this to 0 forces packets to be written as they
    // arrive, restoring the normal cluster-by-cluster interleaving.
    cmd.args(["-max_interleave_delta", "0"]);
    cmd.arg(output);

    tracing::info!(?cmd, "remuxing");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    // Drain stdout on a worker thread. If `total_duration_s` is known we parse
    // ffmpeg's `-progress pipe:1` lines and forward fractions through an mpsc
    // channel; otherwise we just discard stdout so the OS pipe never stalls ffmpeg.
    let (progress_rx, progress_thread) = match (child.stdout.take(), total_duration_s) {
        (Some(stdout), Some(total)) => {
            let (tx, rx) = std::sync::mpsc::channel::<f32>();
            let handle = std::thread::Builder::new()
                .name("dubsync-ffmpeg-progress".into())
                .spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines().map_while(std::result::Result::ok) {
                        if let Some(rest) = line.strip_prefix("out_time_us=") {
                            if let Ok(us) = rest.trim().parse::<u64>() {
                                let frac = (us as f64 / 1_000_000.0 / total).clamp(0.0, 1.0) as f32;
                                let _ = tx.send(frac);
                            }
                        } else if line.trim() == "progress=end" {
                            let _ = tx.send(1.0);
                        }
                    }
                })
                .ok();
            (Some(rx), handle)
        }
        (Some(mut stdout), None) => {
            let handle = std::thread::Builder::new()
                .name("dubsync-ffmpeg-stdout-sink".into())
                .spawn(move || {
                    use std::io::Read;
                    let mut sink = [0u8; 4096];
                    while let Ok(n) = stdout.read(&mut sink) {
                        if n == 0 {
                            break;
                        }
                    }
                })
                .ok();
            (None, handle)
        }
        _ => (None, None),
    };

    // Foreground: forward fractions while the child runs. The closure stays on the
    // foreground thread so it doesn't need a `'static` bound — the worker only sends
    // f32s.
    if let Some(rx) = &progress_rx {
        loop {
            for frac in rx.try_iter() {
                progress(frac);
            }
            match child.try_wait()? {
                Some(_) => break,
                None => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        for frac in rx.try_iter() {
            progress(frac);
        }
    }

    let status = child.wait()?;
    if let Some(handle) = progress_thread {
        let _ = handle.join();
    }

    if !status.success() {
        let mut stderr_buf = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut stderr_buf);
        }
        return Err(DubsyncError::FfmpegFailed { stderr: stderr_buf });
    }
    progress(1.0);
    Ok(())
}

/// Extract one dub track at native sample rate / channel layout as f32le PCM.
pub fn extract_dub(src: &Path, stream_index: u32, out: &Path) -> Result<()> {
    let status = crate::binpath::ffmpeg_command()
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args([
            "-map",
            &format!("0:{stream_index}"),
            "-vn",
            "-sn",
            "-dn",
            "-c:a",
            "pcm_f32le",
            "-f",
            "wav",
        ])
        .arg(out)
        .output()?;

    if !status.status.success() {
        return Err(DubsyncError::FfmpegFailed {
            stderr: String::from_utf8_lossy(&status.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Extract one subtitle stream as SRT. ffmpeg's `-c:s srt` auto-converts
/// ASS / SSA / WebVTT / etc. to SubRip text during extraction so the parser
/// downstream only has to handle one format. Image-based codecs (PGS,
/// DVD-SUB, DVB-SUB) fail this conversion — the caller is expected to detect
/// and skip those upfront via [`crate::ffprobe::SubtitleStream::is_image_based`].
pub fn extract_subtitle(src: &Path, stream_index: u32, out_srt: &Path) -> Result<()> {
    let status = crate::binpath::ffmpeg_command()
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args([
            "-map",
            &format!("0:{stream_index}"),
            "-vn",
            "-an",
            "-dn",
            "-c:s",
            "srt",
        ])
        .arg(out_srt)
        .output()?;

    if !status.status.success() {
        return Err(DubsyncError::FfmpegFailed {
            stderr: String::from_utf8_lossy(&status.stderr).into_owned(),
        });
    }
    Ok(())
}
