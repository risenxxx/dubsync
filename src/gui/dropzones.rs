//! Drag-and-drop zones for master + donor video files. Each zone shows the current
//! file path, an audio-track picker (anchor + dub multi-select), and Browse / Clear
//! buttons. ffprobe runs on a background thread so the UI doesn't freeze on drop.
//!
//! For the donor zone we also probe subtitle streams in the same background pass
//! so the Phase C sub-picker can render without a second async round-trip.

use crate::ffprobe::{self, AudioStream, SubtitleStream};
use crossbeam_channel::Receiver;
use eframe::egui;
use std::path::{Path, PathBuf};

/// Combined audio+subtitle probe result — produced by `set_file`'s background
/// thread, consumed by `poll_probe`. Either field may be `Err` independently;
/// in practice subtitle probe only fails when the file is corrupt (the helper
/// returns an empty Vec rather than an error when there are no subs).
pub struct ProbeResult {
    pub audio: Result<Vec<AudioStream>, String>,
    pub subtitle: Result<Vec<SubtitleStream>, String>,
}

/// Per-zone state — one for master, one for donor. Owned by the App.
pub struct ZoneState {
    pub label: &'static str,
    pub file: Option<PathBuf>,
    pub streams: Option<Vec<AudioStream>>,
    pub subtitle_streams: Option<Vec<SubtitleStream>>,
    pub probe: Option<Receiver<ProbeResult>>,
    pub probe_error: Option<String>,
    /// Subtitle probe failures are non-fatal — we still show the audio track
    /// picker. The error is retained for the optional log/disclosure but
    /// doesn't block the run button.
    pub subtitle_probe_error: Option<String>,
}

impl ZoneState {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            file: None,
            streams: None,
            subtitle_streams: None,
            probe: None,
            probe_error: None,
            subtitle_probe_error: None,
        }
    }

    pub fn clear_file(&mut self) {
        self.file = None;
        self.streams = None;
        self.subtitle_streams = None;
        self.probe = None;
        self.probe_error = None;
        self.subtitle_probe_error = None;
    }

    /// Set the file path and kick off a background ffprobe. The same thread
    /// runs the audio + subtitle probes sequentially — both calls take tens
    /// of ms each so a single thread keeps the code simple without making the
    /// UI feel slower.
    pub fn set_file(&mut self, path: PathBuf) {
        self.file = Some(path.clone());
        self.streams = None;
        self.subtitle_streams = None;
        self.probe_error = None;
        self.subtitle_probe_error = None;
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("dubsync-ffprobe".into())
            .spawn(move || {
                let audio = ffprobe::list_audio_streams(&path).map_err(|e| format!("{e:#}"));
                let subtitle = ffprobe::list_subtitle_streams(&path).map_err(|e| format!("{e:#}"));
                let _ = tx.send(ProbeResult { audio, subtitle });
            })
            .expect("failed to spawn ffprobe thread");
        self.probe = Some(rx);
    }

    /// Drain any pending ffprobe result. Call once per frame on each zone.
    pub fn poll_probe(&mut self) {
        let take = if let Some(rx) = &self.probe {
            rx.try_recv().ok()
        } else {
            None
        };
        if let Some(result) = take {
            self.probe = None;
            match result.audio {
                Ok(streams) => self.streams = Some(streams),
                Err(e) => self.probe_error = Some(e),
            }
            match result.subtitle {
                Ok(streams) => self.subtitle_streams = Some(streams),
                Err(e) => self.subtitle_probe_error = Some(e),
            }
        }
    }
}

/// Draw one drop-zone widget. Returns the rect covered by the zone — caller uses it
/// to route OS-level drops to the right zone based on cursor position.
///
/// `selected_subs` is populated only for the donor zone (Phase C). The master
/// zone passes `&mut Vec::new()` since master subs flow as pass-through, not
/// per-track selection.
#[allow(clippy::too_many_arguments)]
pub fn draw_zone(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    zone: &mut ZoneState,
    selected_anchor: &mut Option<u32>,
    selected_dubs: &mut Vec<u32>,
    selected_subs: &mut Vec<u32>,
    is_donor: bool,
) -> egui::Rect {
    let frame = egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(12.0))
        .rounding(egui::Rounding::same(8.0));

    let response = frame
        .show(ui, |ui| {
            // Hard-cap the inner width to the column the parent allotted us.
            // Without this, long file paths and stream labels widen the frame
            // past the column boundary and overlap the neighbouring zone.
            ui.set_max_width(ui.available_width());
            ui.set_min_height(120.0);

            ui.horizontal(|ui| {
                ui.heading(zone.label);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if zone.file.is_some() && ui.button("Clear").clicked() {
                        zone.clear_file();
                    }
                    if ui.button("Browse…").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("video", &["mkv", "mp4", "ts", "avi", "mov", "webm"])
                            .pick_file()
                        {
                            zone.set_file(p);
                        }
                    }
                });
            });

            match &zone.file {
                Some(p) => {
                    // Truncate (with `…`) so long paths don't widen the column.
                    let path_text = egui::RichText::new(p.display().to_string())
                        .small()
                        .color(ui.visuals().weak_text_color());
                    ui.add(egui::Label::new(path_text).truncate())
                        .on_hover_text(p.display().to_string());
                }
                None => {
                    ui.label(
                        egui::RichText::new("Drop a video file here, or click Browse…")
                            .italics()
                            .color(ui.visuals().weak_text_color()),
                    );
                }
            }

            if zone.probe.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Probing audio tracks…");
                });
            } else if let Some(err) = &zone.probe_error {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("ffprobe failed: {err}"));
            } else if let Some(streams) = &zone.streams {
                draw_track_selectors(
                    ui,
                    streams,
                    zone.subtitle_streams.as_deref(),
                    selected_anchor,
                    selected_dubs,
                    selected_subs,
                    is_donor,
                );
            }
        })
        .response;

    // Highlight the zone the cursor is hovering during a drag — gives visual
    // feedback that we'll accept the drop here.
    let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
    if hovering_files && response.contains_pointer() {
        ui.painter().rect_stroke(
            response.rect,
            egui::Rounding::same(8.0),
            egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 180, 255)),
        );
    }

    response.rect
}

#[allow(clippy::too_many_arguments)]
fn draw_track_selectors(
    ui: &mut egui::Ui,
    streams: &[AudioStream],
    subtitle_streams: Option<&[SubtitleStream]>,
    selected_anchor: &mut Option<u32>,
    selected_dubs: &mut Vec<u32>,
    selected_subs: &mut Vec<u32>,
    is_donor: bool,
) {
    if streams.is_empty() {
        ui.colored_label(egui::Color32::LIGHT_RED, "No audio tracks in this file.");
        return;
    }

    ui.add_space(6.0);
    ui.label("Anchor (reference language):");
    let current_label = selected_anchor
        .and_then(|idx| streams.iter().find(|s| s.index == idx))
        .map(|s| s.display_label())
        .unwrap_or_else(|| "(pick one)".to_string());
    let combobox_width = (ui.available_width() - 8.0).max(120.0);
    egui::ComboBox::from_id_salt((streams.as_ptr() as usize, "anchor"))
        .selected_text(current_label)
        .width(combobox_width)
        .show_ui(ui, |ui| {
            for s in streams {
                let chosen = matches!(*selected_anchor, Some(i) if i == s.index);
                if ui.selectable_label(chosen, s.display_label()).clicked() {
                    *selected_anchor = Some(s.index);
                    if is_donor {
                        selected_dubs.retain(|&i| i != s.index);
                    }
                }
            }
        });

    if is_donor {
        ui.add_space(4.0);
        ui.label("Dub tracks to sync:");
        ui.indent("dub-list", |ui| {
            for s in streams {
                if Some(s.index) == *selected_anchor {
                    continue;
                }
                let mut chosen = selected_dubs.contains(&s.index);
                let label = s.display_label();
                // Render the checkbox with a short stub so its bounding box
                // doesn't try to claim the full label width — then put the
                // (possibly long) label next to it as a truncating Label that
                // wraps cleanly within the column.
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut chosen, "").changed() {
                        if chosen {
                            if !selected_dubs.contains(&s.index) {
                                selected_dubs.push(s.index);
                            }
                        } else {
                            selected_dubs.retain(|&i| i != s.index);
                        }
                    }
                    ui.add(egui::Label::new(label.clone()).truncate())
                        .on_hover_text(label);
                });
            }
        });

        // Phase C subtitle picker. Hidden inside a CollapsingHeader so the
        // donor zone stays compact when the user doesn't care about subs.
        if let Some(subs) = subtitle_streams {
            if !subs.is_empty() {
                ui.add_space(4.0);
                egui::CollapsingHeader::new(format!("Subtitle tracks ({} available)", subs.len()))
                    .id_salt("dubsync-donor-sub-picker")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Forced tracks marked. Image-based codecs (PGS / DVD-SUB) \
                                 can't be time-shifted and are disabled.",
                            )
                            .small()
                            .color(ui.visuals().weak_text_color()),
                        );
                        ui.indent("sub-list", |ui| {
                            for s in subs {
                                let label = sub_label(s);
                                let mut chosen = selected_subs.contains(&s.index);
                                let enabled = !s.is_image_based();
                                let tooltip = if s.is_image_based() {
                                    format!("{label}\n(image-based — can't be shifted)")
                                } else {
                                    label.clone()
                                };
                                ui.horizontal(|ui| {
                                    ui.add_enabled_ui(enabled, |ui| {
                                        if ui.checkbox(&mut chosen, "").changed() {
                                            if chosen {
                                                if !selected_subs.contains(&s.index) {
                                                    selected_subs.push(s.index);
                                                }
                                            } else {
                                                selected_subs.retain(|&i| i != s.index);
                                            }
                                        }
                                    });
                                    ui.add(egui::Label::new(label).truncate())
                                        .on_hover_text(tooltip);
                                });
                            }
                        });
                    });
            }
        }
    }
}

/// Single-line label for a subtitle stream, mirroring [`AudioStream::display_label`]
/// shape but tagging forced/default and codec name. Used by the Phase C
/// donor sub-picker.
fn sub_label(s: &SubtitleStream) -> String {
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
    format!(
        "#{idx:<3} {codec:<8} [{lang}]{tag_part}{title_part}",
        idx = s.index,
        codec = s.codec_name,
    )
}

/// Reconcile remembered donor subtitle picks against the newly-probed file:
/// drop indices that no longer exist OR are now image-based (defensive — the
/// CLI validator catches that on Run, but the GUI picker shouldn't pre-tick
/// a track the user can't actually use).
pub fn reconcile_subs(streams: &[SubtitleStream], last_subs: &[u32]) -> Vec<u32> {
    last_subs
        .iter()
        .copied()
        .filter(|idx| {
            streams
                .iter()
                .find(|s| s.index == *idx)
                .map(|s| !s.is_image_based())
                .unwrap_or(false)
        })
        .collect()
}

/// Auto-select track indices in `streams` based on previously remembered indices.
/// Returns the resolved `(anchor, dubs)` pair after filtering for what actually
/// exists in the new file.
pub fn auto_select(
    streams: &[AudioStream],
    last_anchor: Option<u32>,
    last_dubs: &[u32],
) -> (Option<u32>, Vec<u32>) {
    let resolved_anchor = last_anchor.filter(|idx| streams.iter().any(|s| s.index == *idx));
    let anchor = resolved_anchor.or_else(|| {
        // Fallback: prefer an English-tagged track, else the first.
        streams
            .iter()
            .find(|s| s.language() == Some("eng"))
            .or_else(|| streams.first())
            .map(|s| s.index)
    });
    let dubs: Vec<u32> = last_dubs
        .iter()
        .copied()
        .filter(|idx| streams.iter().any(|s| s.index == *idx) && Some(*idx) != anchor)
        .collect();
    (anchor, dubs)
}

/// Build the synced-output path for a given master, preferring the supplied
/// directory if any. Order of folder preference:
///   1. Explicit `preferred_dir` (typically the parent of the previous output).
///   2. The master's own parent directory.
///   3. Current working directory (degenerate case — master has no parent).
///
/// The filename is always `<master_stem>.synced.mkv` so re-dropping a new master
/// in the same session updates the name to track the new file.
pub fn output_path_for(master: &Path, preferred_dir: Option<&Path>) -> PathBuf {
    let stem = master
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let dir = preferred_dir
        .map(Path::to_path_buf)
        .or_else(|| master.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    dir.join(format!("{stem}.synced.mkv"))
}
