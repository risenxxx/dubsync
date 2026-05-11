//! Windows-subsystem GUI binary for dubsync. Boots the egui app from
//! `dubsync::gui` and installs a tracing subscriber that captures every event so
//! the in-app log pane can display them.
//!
//! `windows_subsystem = "windows"` is set for release builds only — debug builds
//! keep the console attached so `tracing` output and panic messages stay visible
//! during development.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dubsync::gui::{apply_native_window_decorations, run_controller, App};

fn main() -> Result<(), eframe::Error> {
    let log_rx = run_controller::init_tracing_with_capture();

    let state = dubsync::gui::persistence::PersistedState::load();
    let options = dubsync::gui::window_options(&state);

    eframe::run_native(
        "dubsync",
        options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            apply_native_window_decorations(cc);
            Ok(Box::new(App::new(log_rx)))
        }),
    )
}

/// Install the app's font stack:
/// - Rubik (Google Fonts, OFL) as the primary proportional face
/// - Phosphor Regular (egui-phosphor) as a fallback for icon glyphs
/// - egui's default fonts (Hack/Ubuntu-Light/NotoEmoji/emoji-icon-font) keep
///   their normal slots so anything we don't ship a glyph for still falls back
///   onto the bundled defaults.
fn install_fonts(ctx: &eframe::egui::Context) {
    use eframe::egui::{FontData, FontDefinitions, FontFamily};

    let mut fonts = FontDefinitions::default();

    // Rubik is a variable font (wght axis: Light → Black). ab_glyph picks the
    // default instance, which is Regular. The static binary embeds the file.
    fonts.font_data.insert(
        "Rubik".into(),
        FontData::from_static(include_bytes!("../../assets/fonts/Rubik.ttf")),
    );
    if let Some(prop) = fonts.families.get_mut(&FontFamily::Proportional) {
        prop.insert(0, "Rubik".into());
    }

    // Phosphor icon font — fallback for ✓/✗/etc. that the default fonts don't
    // contain. Adds itself to both Proportional and Monospace fallback chains.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    ctx.set_fonts(fonts);
}
