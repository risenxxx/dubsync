//! egui-based desktop GUI for dubsync. Top-level module: owns the `App` state,
//! frame loop, drag-drop routing, and persistence.

mod chat_panel;
mod dropzones;
mod options_panel;
pub mod persistence;
pub mod run_controller;
mod title_bar;

use crate::cli::{DubCodec, FpsMode, RunConfig};
use chat_panel::ChatPanel;
use crossbeam_channel::Receiver;
use dropzones::{auto_select, draw_zone, output_path_for, ZoneState};
use eframe::egui;
use persistence::PersistedState;
use run_controller::{LogLine, RunOutcome, RunningHandle};
use std::collections::VecDeque;
use std::path::PathBuf;

const APP_TITLE: &str = "dubsync";
const LOG_BUFFER_CAP: usize = 2000;
const DEFAULT_WINDOW_SIZE: (f32, f32) = (920.0, 760.0);
/// Floor below which the master/donor columns get squashed and stream pickers
/// truncate. Enforced via `with_min_inner_size` so a previously-saved tiny window
/// can't lock the user out of the donor zone.
const MIN_WINDOW_SIZE: (f32, f32) = (760.0, 520.0);

pub struct App {
    state: PersistedState,
    master: ZoneState,
    donor: ZoneState,

    /// Output destination is held as folder + filename so each can be edited
    /// independently. Combined into a single path at run time. Folder-only edits
    /// persist between sessions; the filename auto-rewrites to `<master_stem>.synced.mkv`
    /// every time a new master is selected.
    output_dir: Option<PathBuf>,
    output_filename: String,

    log_rx: Receiver<LogLine>,
    log_buffer: VecDeque<LogLine>,

    chat: ChatPanel,

    run: RunState,
    last_output: Option<PathBuf>,
    last_error: Option<String>,

    /// Cached cursor position while a file is being dragged over the window.
    /// Used as a fallback in `route_drops` because `hover_pos()` can be `None`
    /// at the exact frame the drop is reported (the OS sometimes finalises the
    /// drop without firing a final `CursorMoved` event), which would otherwise
    /// fall through to the "fill the empty slot" heuristic and route incorrectly.
    drag_cursor_pos: Option<egui::Pos2>,
}

enum RunState {
    Idle,
    Running { handle: RunningHandle },
}

impl App {
    pub fn new(log_rx: Receiver<LogLine>) -> Self {
        let mut state = PersistedState::load();
        let mut master = ZoneState::new("Master");
        let mut donor = ZoneState::new("Donor");

        // Re-attach last-used files if they still exist on disk.
        if let Some(p) = state.master_file.clone() {
            if p.exists() {
                master.set_file(p);
            } else {
                state.master_file = None;
            }
        }
        if let Some(p) = state.donor_file.clone() {
            if p.exists() {
                donor.set_file(p);
            } else {
                state.donor_file = None;
            }
        }

        Self {
            output_dir: state.output_dir.clone(),
            output_filename: state.output_filename.clone(),
            state,
            master,
            donor,
            log_rx,
            log_buffer: VecDeque::with_capacity(LOG_BUFFER_CAP),
            chat: ChatPanel::new(),
            run: RunState::Idle,
            last_output: None,
            last_error: None,
            drag_cursor_pos: None,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // While a file is hovering over the window, sample the cursor position
        // every frame so `route_drops` has a reliable last-known location even
        // when the drop event arrives without a fresh `CursorMoved`.
        let (any_hover, cursor_now) =
            ctx.input(|i| (!i.raw.hovered_files.is_empty(), i.pointer.hover_pos()));
        if any_hover {
            if let Some(pos) = cursor_now {
                self.drag_cursor_pos = Some(pos);
            }
        }

        // Drain background log events into our scrollback buffer.
        while let Ok(line) = self.log_rx.try_recv() {
            if self.log_buffer.len() >= LOG_BUFFER_CAP {
                self.log_buffer.pop_front();
            }
            self.log_buffer.push_back(line);
        }

        // Drain pipeline events into the chat panel.
        if let RunState::Running { handle } = &self.run {
            let mut got_event = false;
            while let Ok(ev) = handle.event_rx.try_recv() {
                self.chat.ingest(ev);
                got_event = true;
            }
            if got_event {
                ctx.request_repaint();
            }
        }

        self.master.poll_probe();
        self.donor.poll_probe();

        // Detect file changes driven by the zone's own Browse button (those don't
        // go through `route_drops`). Mirror the new file into the App-level state
        // and refresh the output path so the filename tracks the new master.
        if self.master.file != self.state.master_file {
            self.state.master_file = self.master.file.clone();
            if let Some(m) = self.master.file.clone() {
                self.refresh_output_for_master(&m);
            }
        }
        if self.donor.file != self.state.donor_file {
            self.state.donor_file = self.donor.file.clone();
        }

        // Pre-attach indices when probe results arrive — only the first time.
        attach_indices(&mut self.master, &mut self.state.last_master_anchor, None);
        attach_dub_indices(
            &mut self.donor,
            &mut self.state.last_donor_anchor,
            &mut self.state.last_donor_dubs,
        );
        attach_sub_indices(&self.donor, &mut self.state.last_donor_subs);

        // Check pipeline completion.
        if let RunState::Running { handle } = &self.run {
            if let Ok(outcome) = handle.result_rx.try_recv() {
                match outcome {
                    RunOutcome::Ok(p) => {
                        self.last_output = Some(p);
                        self.last_error = None;
                    }
                    RunOutcome::Err(e) => {
                        self.last_error = Some(e);
                    }
                }
                self.run = RunState::Idle;
                ctx.request_repaint();
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }

        let runnable = self.is_runnable();
        let running = matches!(self.run, RunState::Running { .. });

        let mut master_rect = egui::Rect::NOTHING;
        let mut donor_rect = egui::Rect::NOTHING;

        // Track maximized state every frame so `save()` (which has no ctx) can
        // flush a current value to disk. eframe persists window_size/pos on its
        // own; we only need to handle the maximized flag.
        if let Some(maximized) = ctx.input(|i| i.viewport().maximized) {
            self.state.window_maximized = Some(maximized);
        }

        // Custom title bar — must come first so it claims the topmost strip
        // before any other panel layout is computed.
        title_bar::show(ctx, APP_TITLE);

        // Right side panel: pipeline progress (chat-style cards) + collapsible
        // raw tracing log anchored at the bottom. Resizable so the user can
        // make the log column wider when they want to read full ffmpeg invocations.
        egui::SidePanel::right("dubsync-pipeline-panel")
            .resizable(true)
            .default_width(420.0)
            .min_width(300.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Pipeline").heading());
                ui.add_space(4.0);

                // Result / error banner — most prominent place after a run completes.
                if let Some(err) = &self.last_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("Error: {err}"));
                }
                if let Some(p) = self.last_output.clone() {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(120, 200, 130),
                            format!("Done — wrote {}", p.display()),
                        );
                        if ui.button("Open folder").clicked() {
                            let _ = open_in_file_manager(&p);
                        }
                    });
                }

                ui.separator();

                // Pin "Show full log" disclosure to the bottom so the chat panel
                // takes the remaining vertical space.
                egui::TopBottomPanel::bottom("dubsync-full-log-anchor")
                    .resizable(false)
                    .show_inside(ui, |ui| {
                        egui::CollapsingHeader::new("Show full log")
                            .default_open(false)
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("dubsync-log-scroll")
                                    .stick_to_bottom(true)
                                    .max_height(180.0)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        for line in &self.log_buffer {
                                            let color = match line.level {
                                                tracing::Level::ERROR => egui::Color32::LIGHT_RED,
                                                tracing::Level::WARN => {
                                                    egui::Color32::from_rgb(220, 170, 60)
                                                }
                                                tracing::Level::INFO => ui.visuals().text_color(),
                                                tracing::Level::DEBUG | tracing::Level::TRACE => {
                                                    ui.visuals().weak_text_color()
                                                }
                                            };
                                            ui.colored_label(color, &line.message);
                                        }
                                    });
                            });
                    });

                // Chat-style phase timeline fills the remaining space.
                if self.chat.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "Hit Run sync to begin. Each pipeline phase will appear \
                             here as a card with live progress, ETA, and a final summary.",
                        )
                        .small()
                        .color(ui.visuals().weak_text_color()),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("dubsync-chat-scroll")
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.chat.draw(ui);
                        });
                    if self.chat.any_running() {
                        ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    }
                }
            });

        // Central panel — controls column. Stacks vertically so master and donor
        // each get the full width of the controls column. Wrapped in a ScrollArea
        // so a tall Advanced-options expansion never hides the action bar below.
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("dubsync-controls-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Drop a master + donor video, pick anchor and dub tracks, hit Run.",
                        )
                        .small()
                        .color(ui.visuals().weak_text_color()),
                    );

                    ui.add_space(8.0);

                    // Master + donor stacked vertically. Each takes the full width
                    // of the controls column.
                    master_rect = draw_zone(
                        ui,
                        ctx,
                        &mut self.master,
                        &mut self.state.last_master_anchor,
                        &mut Vec::new(),
                        &mut Vec::new(),
                        false,
                    );
                    ui.add_space(6.0);
                    donor_rect = draw_zone(
                        ui,
                        ctx,
                        &mut self.donor,
                        &mut self.state.last_donor_anchor,
                        &mut self.state.last_donor_dubs,
                        &mut self.state.last_donor_subs,
                        true,
                    );

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("Folder:");
                        let mut folder_text = self
                            .output_dir
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default();
                        let avail = ui.available_width() - 90.0;
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut folder_text)
                                    .desired_width(avail.max(120.0)),
                            )
                            .changed()
                        {
                            self.output_dir = if folder_text.is_empty() {
                                None
                            } else {
                                Some(PathBuf::from(folder_text))
                            };
                        }
                        if ui.button("Browse…").clicked() {
                            let dialog = rfd::FileDialog::new();
                            let dialog = if let Some(dir) = self.output_dir.clone() {
                                dialog.set_directory(dir)
                            } else {
                                dialog
                            };
                            if let Some(p) = dialog.pick_folder() {
                                self.output_dir = Some(p);
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Filename:");
                        let avail = ui.available_width();
                        ui.add(
                            egui::TextEdit::singleline(&mut self.output_filename)
                                .desired_width(avail.max(120.0))
                                .hint_text("e.g. master.synced.mkv"),
                        );
                    });
                    if let Some(full) = self.compose_output_path() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} {}",
                                egui_phosphor::regular::ARROW_RIGHT,
                                full.display()
                            ))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                        );
                        if self.state.options.save_report {
                            let report =
                                derive_report_path(&full, &self.state.options.report_format);
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} {}",
                                    egui_phosphor::regular::ARROW_RIGHT,
                                    report.display()
                                ))
                                .small()
                                .color(ui.visuals().weak_text_color()),
                            );
                        }
                    }

                    ui.add_space(8.0);
                    options_panel::draw(ui, &mut self.state.options);

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // Action bar — Clear on the left, Run sync on the right.
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!running, egui::Button::new("Clear files"))
                            .on_hover_text(
                                "Wipe master + donor file paths and the auto-derived output \
                                 filename. Track indices, advanced options, and the output \
                                 folder are kept — drop new files and the filename auto-fills \
                                 from the new master's name.",
                            )
                            .clicked()
                        {
                            self.master.clear_file();
                            self.donor.clear_file();
                            self.state.master_file = None;
                            self.state.donor_file = None;
                            self.output_filename.clear();
                            self.state.output_filename.clear();
                            self.last_output = None;
                            self.last_error = None;
                            self.state.save();
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if running {
                                ui.spinner();
                                ui.label("Running…");
                            } else if ui
                                .add_enabled(
                                    runnable,
                                    egui::Button::new("Run sync").min_size(egui::vec2(140.0, 36.0)),
                                )
                                .clicked()
                            {
                                self.start_run();
                            }
                        });
                    });
                    ui.add_space(8.0);
                });
        });

        // Drops fire in the same frame as the panel layout, so master_rect /
        // donor_rect are populated above before this call.
        self.route_drops(ctx, master_rect, donor_rect);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.state.master_file = self.master.file.clone();
        self.state.donor_file = self.donor.file.clone();
        self.state.output_dir = self.output_dir.clone();
        self.state.output_filename = self.output_filename.clone();
        // window_maximized is updated every frame in `update()` so it's already
        // current here; save() just flushes the whole state to disk.
        self.state.save();
    }
}

impl App {
    /// Combine `output_dir` + `output_filename` into the final destination path,
    /// or `None` if either piece is missing.
    fn compose_output_path(&self) -> Option<PathBuf> {
        let dir = self.output_dir.as_ref()?;
        let name = self.output_filename.trim();
        if name.is_empty() {
            return None;
        }
        Some(dir.join(name))
    }

    fn is_runnable(&self) -> bool {
        // In anchor-only validation mode the donor anchor IS the only track
        // processed, so an empty dub multi-select is fine.
        let dubs_ok =
            self.state.options.anchor_only_validation || !self.state.last_donor_dubs.is_empty();
        matches!(self.run, RunState::Idle)
            && self.master.file.is_some()
            && self.donor.file.is_some()
            && self.master.streams.is_some()
            && self.donor.streams.is_some()
            && self.state.last_master_anchor.is_some()
            && self.state.last_donor_anchor.is_some()
            && dubs_ok
            && self.compose_output_path().is_some()
    }

    fn start_run(&mut self) {
        // Build RunConfig from current form state.
        let Some(master_file) = self.master.file.clone() else {
            return;
        };
        let Some(donor_file) = self.donor.file.clone() else {
            return;
        };
        let Some(master_anchor_track) = self.state.last_master_anchor else {
            return;
        };
        let Some(donor_anchor_track) = self.state.last_donor_anchor else {
            return;
        };
        let donor_dub_tracks = if self.state.options.anchor_only_validation {
            vec![donor_anchor_track]
        } else {
            let dubs = self.state.last_donor_dubs.clone();
            if dubs.is_empty() {
                self.last_error = Some("Pick at least one dub track on the donor side.".into());
                return;
            }
            dubs
        };
        let Some(output_file) = self.compose_output_path() else {
            self.last_error =
                Some("Set both an output folder and a filename before running.".into());
            return;
        };

        let opts = &self.state.options;
        let report_path = if opts.save_report {
            Some(derive_report_path(&output_file, &opts.report_format))
        } else {
            None
        };
        let dub_codec = DubCodec::from_token(&opts.dub_codec);
        // Empty / non-numeric bitrate string → fall through to per-codec auto.
        let dub_bitrate_kbps = opts.dub_bitrate.trim().parse::<u32>().ok();
        let fps_mode = match opts.fps_mode.as_str() {
            "disabled" => FpsMode::Disabled,
            "manual" => FpsMode::Forced(opts.fps_manual_ratio as f64),
            _ => FpsMode::Auto,
        };
        let cfg = RunConfig {
            master_file,
            donor_file,
            master_anchor_track,
            donor_anchor_track,
            donor_dub_tracks,
            output_file,
            keep_temp: opts.keep_temp,
            include_donor_anchor: opts.include_donor_anchor,
            solo_dub: opts.solo_dub,
            temp_dir: None,
            threads: None,
            silence_db: opts.silence_db,
            silence_min_ms: opts.silence_min_ms,
            anchor_rate: opts.anchor_rate,
            correlation_window_s: opts.correlation_window_s,
            max_drift_s: opts.max_drift_s,
            max_segment_jump_s: opts.max_segment_jump_s,
            snap_radius_s: opts.snap_radius_s,
            crossfade_ms: opts.crossfade_ms,
            smooth_gaps: opts.smooth_gaps,
            gap_fill_margin_s: opts.gap_fill_margin_s,
            speech_db: opts.speech_db,
            pal_pitch_correction: opts.pal_pitch_correction,
            anchor_only_validation: opts.anchor_only_validation,
            report_path,
            dub_codec,
            dub_bitrate_kbps,
            fps_mode,
            keep_master_subs: opts.keep_master_subs,
            include_donor_forced_subs: opts.include_donor_forced_subs,
            donor_subs_explicit: self.state.last_donor_subs.clone(),
        };

        // Persist current state before kicking off the run so a crash mid-run still
        // leaves the next launch in a sensible place.
        self.state.master_file = Some(cfg.master_file.clone());
        self.state.donor_file = Some(cfg.donor_file.clone());
        self.state.output_dir = self.output_dir.clone();
        self.state.output_filename = self.output_filename.clone();
        self.state.save();

        self.last_error = None;
        self.last_output = None;
        self.chat.reset();
        let handle = run_controller::start_pipeline(cfg);
        self.run = RunState::Running { handle };
    }

    /// Recompute the output filename whenever the master changes. The folder
    /// (`output_dir`) is decoupled — it stays whatever the user picked. Called
    /// every time the master file changes so the filename tracks the new stem
    /// even mid-session. If the user has set neither folder nor filename yet,
    /// also seed the folder from the master's own parent directory.
    fn refresh_output_for_master(&mut self, master: &std::path::Path) {
        // Default the folder if it's still unset (first ever run, or after a
        // wipe of the persisted state).
        if self.output_dir.is_none() {
            if let Some(parent) = master.parent() {
                if !parent.as_os_str().is_empty() {
                    self.output_dir = Some(parent.to_path_buf());
                }
            }
        }
        // Always rewrite the filename to the new master's stem. (User edits made
        // since the last drop are intentionally clobbered — the user can change
        // it again after the drop if they want a different name.)
        let derived = output_path_for(master, None);
        if let Some(name) = derived.file_name().and_then(|s| s.to_str()) {
            self.output_filename = name.to_string();
        }
    }

    fn route_drops(
        &mut self,
        ctx: &egui::Context,
        master_rect: egui::Rect,
        donor_rect: egui::Rect,
    ) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        // Prefer the live cursor; fall back to the position we cached during the
        // last drag-hover (sampled every frame in `update`). This matters because
        // `hover_pos()` can be `None` at the exact frame the drop event lands.
        let cursor = ctx
            .input(|i| i.pointer.hover_pos())
            .or(self.drag_cursor_pos);
        for file in dropped {
            let Some(path) = file.path else { continue };
            let target_master = match cursor {
                Some(pos) if master_rect.contains(pos) => true,
                Some(pos) if donor_rect.contains(pos) => false,
                // Cursor not over either zone (or unknown) — fill the empty slot,
                // preferring master since that's the more common "swap episode"
                // workflow.
                _ => self.master.file.is_none(),
            };
            if target_master {
                self.master.set_file(path.clone());
                self.state.master_file = Some(path.clone());
                self.refresh_output_for_master(&path);
            } else {
                self.donor.set_file(path.clone());
                self.state.donor_file = Some(path);
            }
        }
        self.drag_cursor_pos = None;
        self.state.save();
    }
}

/// Derive the report file path from the output MKV path and a format string
/// (`"html"`, `"csv"`, `"json"`). Result lives next to the output file with the
/// same stem and `.report.<ext>` appended — e.g. `/foo/movie.synced.mkv` →
/// `/foo/movie.synced.report.html`.
fn derive_report_path(output: &std::path::Path, format: &str) -> PathBuf {
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("dubsync");
    let parent = output.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    parent.join(format!("{stem}.report.{format}"))
}

/// When a probe result lands for the master zone, auto-pick the persisted anchor
/// index if it exists in the new file's stream list.
fn attach_indices(
    zone: &mut ZoneState,
    last_anchor: &mut Option<u32>,
    _filler: Option<&mut Vec<u32>>,
) {
    if let Some(streams) = &zone.streams {
        if last_anchor.is_none() || !streams.iter().any(|s| Some(s.index) == *last_anchor) {
            let (resolved, _) = auto_select(streams, *last_anchor, &[]);
            *last_anchor = resolved;
        }
    }
}

/// Donor variant: also reconciles the dub multi-selection against what's in the
/// newly-probed file (drops anything that no longer exists, keeps the rest).
fn attach_dub_indices(
    zone: &mut ZoneState,
    last_anchor: &mut Option<u32>,
    last_dubs: &mut Vec<u32>,
) {
    if let Some(streams) = &zone.streams {
        let needs_anchor =
            last_anchor.is_none() || !streams.iter().any(|s| Some(s.index) == *last_anchor);
        let needs_dubs = last_dubs.is_empty()
            || last_dubs
                .iter()
                .any(|idx| !streams.iter().any(|s| s.index == *idx));
        if needs_anchor || needs_dubs {
            let (a, d) = auto_select(streams, *last_anchor, last_dubs);
            if needs_anchor {
                *last_anchor = a;
            }
            if needs_dubs {
                *last_dubs = d;
            }
        }
    }
}

/// Phase C: when the donor's subtitle probe arrives, drop any persisted sub
/// indices that no longer exist in the new file or have become image-based
/// (defensive — `validate_donor_subs` would also reject these on Run, but the
/// GUI shouldn't pre-tick a track the user can't actually use).
fn attach_sub_indices(zone: &ZoneState, last_donor_subs: &mut Vec<u32>) {
    if let Some(subs) = &zone.subtitle_streams {
        let reconciled = dropzones::reconcile_subs(subs, last_donor_subs);
        if reconciled != *last_donor_subs {
            *last_donor_subs = reconciled;
        }
    }
}

fn open_in_file_manager(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn()
            .map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()
            .map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let parent = path.parent().unwrap_or(path);
        std::process::Command::new("xdg-open")
            .arg(parent)
            .spawn()
            .map(|_| ())
    }
}

/// Ask Windows DWM for rounded window corners on Win11+. We turn off native
/// decorations to draw our own title bar, which also turns off Win11's automatic
/// corner rounding — this call brings it back. Silently no-op on Windows 10
/// (DWM ignores the unknown attribute) and on non-Windows builds.
pub fn apply_native_window_decorations(cc: &eframe::CreationContext<'_>) {
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Dwm::{
            DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        };

        let handle = match cc.window_handle() {
            Ok(h) => h,
            Err(_) => return,
        };
        let hwnd = match handle.as_raw() {
            RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
            _ => return,
        };
        let pref = DWMWCP_ROUND;
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const _,
                std::mem::size_of_val(&pref) as u32,
            );
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cc;
    }
}

/// Window options derived from persisted state. Falls back to a sensible default
/// size if there's no record.
///
/// Platform-specific window chrome:
/// - **macOS**: hide the native title-bar text strip but keep the traffic
///   lights drawn by AppKit. Content extends under the title bar so we can
///   paint a theme-coloured strip behind the lights, mimicking VSCode.
/// - **Windows / Linux**: drop native decorations entirely; we render
///   close/minimize/maximize ourselves in [`title_bar`].
pub fn window_options(state: &PersistedState) -> eframe::NativeOptions {
    // Clamp the persisted size to the floor so a previously-saved tiny window
    // can't be restored in a state that hides the donor column.
    let restored = state.window_size.unwrap_or(DEFAULT_WINDOW_SIZE);
    let initial = (
        restored.0.max(MIN_WINDOW_SIZE.0),
        restored.1.max(MIN_WINDOW_SIZE.1),
    );
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_TITLE)
        .with_inner_size(initial)
        .with_min_inner_size(MIN_WINDOW_SIZE);

    #[cfg(target_os = "macos")]
    {
        viewport = viewport
            .with_titlebar_shown(false)
            .with_title_shown(false)
            .with_fullsize_content_view(true);
    }
    #[cfg(not(target_os = "macos"))]
    {
        viewport = viewport.with_decorations(false);
    }

    if let Some(pos) = state.window_pos {
        viewport = viewport.with_position(pos);
    }
    if state.window_maximized.unwrap_or(false) {
        viewport = viewport.with_maximized(true);
    }
    eframe::NativeOptions {
        viewport,
        ..Default::default()
    }
}
