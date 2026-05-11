//! Collapsible "Advanced options" panel. Mirrors every CLI flag that affects the
//! splice / correlation / output behaviour. All fields write straight into the
//! caller's `PersistedOptions`, which is auto-saved when the user clicks Run or
//! changes window state.

use super::persistence::PersistedOptions;
use crate::cli::DubCodec;
use eframe::egui;

pub fn draw(ui: &mut egui::Ui, opts: &mut PersistedOptions) {
    egui::CollapsingHeader::new("Advanced options")
        .default_open(false)
        .show(ui, |ui| {
            // ── Subtitles ──────────────────────────────────────────────────
            ui.label("Subtitles:");
            ui.checkbox(&mut opts.keep_master_subs, "Include master subtitles")
                .on_hover_text(
                    "Pass through every subtitle stream from the master into \
                     the output MKV unchanged (`-c:s copy`). On by default — \
                     subs are universally useful and re-muxing them is free.",
                );
            ui.checkbox(
                &mut opts.include_donor_forced_subs,
                "Include donor forced subtitles",
            )
            .on_hover_text(
                "Extract donor subtitle tracks marked as 'forced' (typical \
                 localised signs / on-screen text), apply the offset map to \
                 their timecodes, and mux them alongside the master subs. \
                 Image-based codecs (PGS / DVD-SUB / DVB-SUB) are skipped \
                 with a warning — they need OCR to time-shift.",
            );

            ui.separator();

            // ── Output codec ───────────────────────────────────────────────
            ui.label("Output codec:");
            let mut current = DubCodec::from_token(&opts.dub_codec);
            ui.horizontal(|ui| {
                ui.label("Codec:");
                egui::ComboBox::from_id_salt("dubsync-dub-codec")
                    .selected_text(current.display_label())
                    .show_ui(ui, |ui| {
                        for choice in [DubCodec::Flac, DubCodec::Ac3, DubCodec::Eac3, DubCodec::Aac]
                        {
                            ui.selectable_value(&mut current, choice, choice.display_label());
                        }
                    })
                    .response
                    .on_hover_text(
                        "FLAC is lossless but inflates output 2–3× and isn't \
                         well-supported by all TVs/receivers in MKV. ac3/eac3/aac \
                         match common donor codecs and play more reliably. ac3 and \
                         eac3 are capped at 6 channels — use FLAC for 7.1 sources.",
                    );
            });
            opts.dub_codec = current.as_token().to_string();
            ui.add_enabled_ui(!current.is_lossless(), |ui| {
                ui.horizontal(|ui| {
                    ui.label("Bitrate (kbps):");
                    let hint_5_1 = current.default_bitrate_kbps(6).unwrap_or(0);
                    let hint_2_0 = current.default_bitrate_kbps(2).unwrap_or(0);
                    let hint = format!("auto ({hint_2_0} stereo / {hint_5_1} 5.1)");
                    ui.add(
                        egui::TextEdit::singleline(&mut opts.dub_bitrate)
                            .desired_width(100.0)
                            .hint_text(&hint),
                    )
                    .on_hover_text(
                        "Override the per-codec / per-channel auto bitrate. Numeric \
                         only (kbps). Leave empty to let dubsync pick — the hint shows \
                         the default values for stereo and 5.1 at the selected codec.",
                    );
                });
            });

            ui.separator();

            // ── Output / behaviour toggles ─────────────────────────────────
            ui.checkbox(
                &mut opts.solo_dub,
                "Solo dub (single-track output for casting)",
            )
            .on_hover_text(
                "Output an MKV with exactly one audio track — the (first) synced \
                     dub. Drops the master anchor and any other dubs. Useful for TVs \
                     that auto-pick the only audio track.",
            );
            ui.checkbox(
                &mut opts.include_donor_anchor,
                "Diagnostic: replace master anchor with synced donor anchor",
            )
            .on_hover_text(
                "Replace the master English in the output with the synced donor English. \
                 If this drifts against the master video, the offset map is wrong.",
            );
            ui.checkbox(
                &mut opts.keep_temp,
                "Keep temp workspace + diagnostic JSONs",
            )
            .on_hover_text(
                "Retains the workspace dir with extracted WAVs, anchors.json, \
                     offset_map.json, transition_traces.json, and onset events.",
            );

            ui.separator();

            // ── Splice snap ────────────────────────────────────────────────
            ui.label("Splice snap (master-anchor silence search):");
            ui.horizontal(|ui| {
                ui.label("Snap radius (s)");
                ui.add(
                    egui::DragValue::new(&mut opts.snap_radius_s)
                        .range(2.0..=120.0)
                        .speed(1.0),
                )
                .on_hover_text(
                    "How far the splicer looks around each refined boundary for a \
                     master-anchor silence wide enough to absorb |Δ|. Wider = more \
                     chance of finding a clean splice, but the visual cut moves \
                     further from the detected scene transition.",
                );
            });
            ui.horizontal(|ui| {
                ui.label("Crossfade (ms)");
                ui.add(
                    egui::DragValue::new(&mut opts.crossfade_ms)
                        .range(1u32..=50u32)
                        .speed(1.0),
                )
                .on_hover_text(
                    "Equal-power crossfade applied at every splice. Default 10 ms; \
                     below 5 ms can click, above 15 ms eats segment audio.",
                );
            });

            ui.separator();

            // ── Gap filling (rubberband) ───────────────────────────────────
            ui.label("Gap filling (rubberband time-stretch):");
            ui.checkbox(&mut opts.smooth_gaps, "Smooth gaps with stretched ambient")
                .on_hover_text(
                    "Replace literal silence at splices with time-stretched neighbour \
                 audio. Speech is never stretched — falls back to silence in \
                 dialog. Requires `rubberband` CLI on PATH or bundled with the app.",
                );
            ui.add_enabled_ui(opts.smooth_gaps, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Margin (s)");
                    ui.add(
                        egui::DragValue::new(&mut opts.gap_fill_margin_s)
                            .range(0.1..=5.0)
                            .speed(0.05),
                    )
                    .on_hover_text(
                        "Length of dub audio sampled before AND after each gap as the \
                         stretch source.",
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Speech threshold (dBFS)");
                    ui.add(
                        egui::DragValue::new(&mut opts.speech_db)
                            .range(-60.0..=-10.0)
                            .speed(1.0),
                    )
                    .on_hover_text(
                        "Above this RMS level a neighbour buffer is treated as speech \
                         and the gap stays as silence (preserves lip-sync).",
                    );
                });
            });

            ui.separator();

            // ── Silence detection / VAD ────────────────────────────────────
            ui.label("Silence detection (master-anchor + dub fallback):");
            ui.horizontal(|ui| {
                ui.label("Threshold (dBFS)");
                ui.add(
                    egui::DragValue::new(&mut opts.silence_db)
                        .range(-90.0..=-20.0)
                        .speed(1.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Min duration (ms)");
                ui.add(
                    egui::DragValue::new(&mut opts.silence_min_ms)
                        .range(50..=2000)
                        .speed(10.0),
                );
            });

            ui.separator();

            // ── Validation / report ────────────────────────────────────────
            ui.label("Validation & report:");
            ui.checkbox(
                &mut opts.anchor_only_validation,
                "Anchor-only validation mode",
            )
            .on_hover_text(
                "Build the offset map and sync ONLY the donor anchor — emit an MKV \
                 with just that synced track. Lets you verify the offset map against \
                 the master video before committing to a full dub run. The dub picker \
                 selection is ignored when this is enabled.",
            );
            ui.checkbox(&mut opts.save_report, "Save report next to output")
                .on_hover_text(
                    "Write a detailed report alongside the output MKV using the \
                     same stem — e.g. `movie.synced.mkv` produces \
                     `movie.synced.report.html`. Includes the run summary plus \
                     per-segment timing details for inspection or series tracking.",
                );
            ui.add_enabled_ui(opts.save_report, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Format:");
                    egui::ComboBox::from_id_salt("dubsync-report-format")
                        .selected_text(opts.report_format.to_uppercase())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut opts.report_format,
                                "html".to_string(),
                                "HTML — styled page",
                            );
                            ui.selectable_value(
                                &mut opts.report_format,
                                "csv".to_string(),
                                "CSV — spreadsheet rows",
                            );
                            ui.selectable_value(
                                &mut opts.report_format,
                                "json".to_string(),
                                "JSON — structured dump",
                            );
                        });
                });
                ui.label(
                    egui::RichText::new(format!(
                        "{} saved as <output stem>.report.{}",
                        egui_phosphor::regular::ARROW_RIGHT,
                        opts.report_format
                    ))
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
            });

            ui.separator();

            // ── FPS / PAL ──────────────────────────────────────────────────
            ui.label("FPS / PAL:");
            ui.horizontal(|ui| {
                ui.label("Mode:");
                let mode_label = match opts.fps_mode.as_str() {
                    "disabled" => "Disabled",
                    "manual" => "Manual ratio",
                    _ => "Auto",
                };
                egui::ComboBox::from_id_salt("dubsync-fps-mode")
                    .selected_text(mode_label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut opts.fps_mode, "auto".to_string(), "Auto");
                        ui.selectable_value(&mut opts.fps_mode, "disabled".to_string(), "Disabled");
                        ui.selectable_value(
                            &mut opts.fps_mode,
                            "manual".to_string(),
                            "Manual ratio",
                        );
                    })
                    .response
                    .on_hover_text(
                        "How dubsync handles fps mismatch between master and donor.\n\
                         • Auto: probe both files, stretch when fps differs by >0.1%.\n\
                         • Disabled: never stretch — use when ffprobe lies (VFR sources, \
                            mis-encoded WEBRips) and the audio is already at master speed.\n\
                         • Manual ratio: bypass the probe and use a user-supplied \
                            donor/master fps ratio.",
                    );
            });
            ui.add_enabled_ui(opts.fps_mode == "manual", |ui| {
                ui.horizontal(|ui| {
                    ui.label("Manual ratio:");
                    ui.add(
                        egui::DragValue::new(&mut opts.fps_manual_ratio)
                            .range(0.5..=2.0)
                            .speed(0.001)
                            .fixed_decimals(4),
                    )
                    .on_hover_text(
                        "donor_fps / master_fps. 1.0 = no stretch (identity); \
                         25/24 ≈ 1.0417 corrects PAL→film when ffprobe doesn't.",
                    );
                });
            });
            ui.checkbox(
                &mut opts.pal_pitch_correction,
                "Undo PAL pitch shift when stretching donor audio",
            )
            .on_hover_text(format!(
                "When master/donor fps differ (e.g. 24 vs 25), the donor audio is \
                 globally time-stretched to match. With this on, also lower the donor \
                 pitch by 12·log2(master_fps/donor_fps) semitones (≈ -0.71 for 25{}24) \
                 to undo the PAL speed-up's pitch raise. Off = preserve donor pitch. \
                 Applies to both Auto and Manual modes; ignored when Disabled.",
                egui_phosphor::regular::ARROW_RIGHT,
            ));

            ui.separator();

            // ── Correlation engine ─────────────────────────────────────────
            ui.label("Correlation engine:");
            ui.horizontal(|ui| {
                ui.label("Anchor sample rate (Hz)");
                ui.add(
                    egui::DragValue::new(&mut opts.anchor_rate)
                        .range(8_000u32..=48_000u32)
                        .speed(1000.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Correlation window (s)");
                ui.add(
                    egui::DragValue::new(&mut opts.correlation_window_s)
                        .range(5.0..=120.0)
                        .speed(1.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Max drift (s)");
                ui.add(
                    egui::DragValue::new(&mut opts.max_drift_s)
                        .range(5.0..=300.0)
                        .speed(5.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Max segment jump (s)");
                ui.add(
                    egui::DragValue::new(&mut opts.max_segment_jump_s)
                        .range(0.5..=600.0)
                        .speed(0.5),
                )
                .on_hover_text(
                    "Maximum offset jump accepted between consecutive segments. \
                     Larger jumps are suppressed as likely false positives — \
                     typically GCC-PHAT confused by repetitive end-credits music. \
                     Default 10 s catches obvious failures while still allowing \
                     real intro/outro shifts. Raise for content with genuinely \
                     large mid-show edits.",
                );
            });

            ui.separator();

            if ui
                .button("Reset all options to defaults")
                .on_hover_text("Restores every field on this panel to the CLI defaults.")
                .clicked()
            {
                *opts = PersistedOptions::default();
            }
        });
}
