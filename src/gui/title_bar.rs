//! Custom window title bar.
//!
//! On Windows/Linux the native chrome is hidden (`with_decorations(false)` at
//! ViewportBuilder time) and we render close/minimize/maximize ourselves. On
//! macOS the native title bar is hidden but the traffic lights stay visible
//! and AppKit-rendered (via `with_titlebar_shown(false)`,
//! `with_titlebar_buttons_shown(true)`, `with_fullsize_content_view(true)`) so
//! they look identical to other apps; we just reserve a left-side gutter for
//! them and fill the rest with a theme-coloured strip plus the app name and
//! theme toggles.

#[cfg(not(target_os = "macos"))]
use eframe::egui::Color32;
use eframe::egui::{self, Align, Frame, Layout, Margin, Sense, Stroke, ViewportCommand};

/// Title bar height. macOS gets 38px so the theme-toggle buttons (~22px) end up
/// with ~8px top/bottom inset matching the right-side `Margin::symmetric` value;
/// the AppKit traffic-light row stays at its ~14px-from-window-top position and
/// sits in the upper half of the bar (matches Safari/Xcode-style chrome).
/// Windows/Linux get a roomier 40px bar matching the proportions of recent
/// Windows/Discord/VSCode chromeless windows.
#[cfg(target_os = "macos")]
const BAR_HEIGHT: f32 = 38.0;
#[cfg(not(target_os = "macos"))]
const BAR_HEIGHT: f32 = 40.0;

/// Title text size. Larger on Windows/Linux where the taller bar has the room.
#[cfg(target_os = "macos")]
const TITLE_SIZE: f32 = 13.0;
#[cfg(not(target_os = "macos"))]
const TITLE_SIZE: f32 = 16.0;

/// Gutter reserved on macOS for native traffic lights. Apple draws three 12px
/// circles starting ~12px from the left with 8px between them; 78px gives a
/// little breathing room before our content begins.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHTS_GUTTER: f32 = 78.0;

/// Render the title bar. Call from `App::update` before any other panels.
pub fn show(ctx: &egui::Context, app_title: &str) {
    let is_maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);

    let panel_fill = ctx.style().visuals.window_fill();
    let separator = ctx.style().visuals.widgets.noninteractive.bg_stroke;

    // Margins: pad only the left so the close button stays flush with the
    // window's right edge — that lets `title_button` give the X equal top /
    // bottom / right insets purely through its internal padding.
    #[cfg(target_os = "macos")]
    let inner_margin = Margin::symmetric(8.0, 0.0);
    #[cfg(not(target_os = "macos"))]
    let inner_margin = Margin {
        left: 12.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    };

    egui::TopBottomPanel::top("dubsync-titlebar")
        .frame(
            Frame::none()
                .fill(panel_fill)
                .inner_margin(inner_margin)
                .stroke(Stroke::NONE),
        )
        .show_separator_line(false)
        .show(ctx, |ui| {
            ui.set_height(BAR_HEIGHT);
            ui.horizontal_centered(|ui| {
                #[cfg(target_os = "macos")]
                ui.add_space(TRAFFIC_LIGHTS_GUTTER);

                // Title text doubles as the window drag-handle. Anywhere the
                // user clicks on the bar that isn't a button should let them
                // move the window, so we also collect leftover space below.
                let title_resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(app_title)
                            .strong()
                            .size(TITLE_SIZE)
                            .color(ui.visuals().text_color()),
                    )
                    .sense(Sense::click_and_drag()),
                );
                handle_drag(ctx, &title_resp, is_maximized);

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Right-to-left: first widget = rightmost on screen.
                    // 1) close/max/min buttons hug the right window edge.
                    // 2) a small gap.
                    // 3) theme toggles.
                    // 4) the remaining space is a drag handle.
                    #[cfg(not(target_os = "macos"))]
                    {
                        draw_window_buttons(ctx, ui, is_maximized);
                        ui.add_space(8.0);
                    }
                    egui::widgets::global_theme_preference_buttons(ui);

                    let drag_resp = ui.allocate_response(
                        ui.available_size_before_wrap(),
                        Sense::click_and_drag(),
                    );
                    handle_drag(ctx, &drag_resp, is_maximized);
                });
            });
        });

    // Manual 1px bottom separator drawn after the panel so it sits exactly at
    // the panel's bottom edge regardless of the inner margin.
    ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("titlebar-sep"),
    ))
    .hline(
        0.0..=ctx.screen_rect().width(),
        BAR_HEIGHT,
        Stroke::new(1.0, separator.color),
    );
}

fn handle_drag(ctx: &egui::Context, resp: &egui::Response, is_maximized: bool) {
    if resp.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if resp.double_clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!is_maximized));
    }
}

#[cfg(not(target_os = "macos"))]
fn draw_window_buttons(ctx: &egui::Context, ui: &mut egui::Ui, is_maximized: bool) {
    use egui_phosphor::regular as icons;

    // Order is right-to-left so close lands on the far right (Windows/Linux convention).
    if title_button(ui, icons::X, ButtonKind::Close).clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    let max_glyph = if is_maximized {
        icons::COPY
    } else {
        icons::SQUARE
    };
    if title_button(ui, max_glyph, ButtonKind::Normal).clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!is_maximized));
    }

    if title_button(ui, icons::MINUS, ButtonKind::Normal).clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }
}

#[cfg(not(target_os = "macos"))]
#[derive(Copy, Clone, PartialEq, Eq)]
enum ButtonKind {
    Normal,
    /// Close button gets a red hover fill, matching the Windows convention.
    Close,
}

#[cfg(not(target_os = "macos"))]
fn title_button(ui: &mut egui::Ui, glyph: &str, kind: ButtonKind) -> egui::Response {
    // Square buttons: the same inset above / below / right of the glyph.
    // BAR_HEIGHT = 40, glyph = 16 → 12px padding on every side.
    let size = egui::vec2(BAR_HEIGHT, BAR_HEIGHT);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let visuals = ui.visuals();
    let fg = visuals.text_color();

    let bg = if resp.is_pointer_button_down_on() {
        match kind {
            ButtonKind::Close => Color32::from_rgb(180, 40, 40),
            ButtonKind::Normal => visuals.widgets.active.bg_fill,
        }
    } else if resp.hovered() {
        match kind {
            ButtonKind::Close => Color32::from_rgb(220, 60, 60),
            ButtonKind::Normal => visuals.widgets.hovered.bg_fill,
        }
    } else {
        Color32::TRANSPARENT
    };

    let painter = ui.painter_at(rect);
    if bg != Color32::TRANSPARENT {
        // No rounding — the close button is flush with the top-right window
        // corner; rounding would create a visible gap at the corner.
        painter.rect_filled(rect, 0.0, bg);
    }
    let glyph_color = if matches!(kind, ButtonKind::Close)
        && (resp.hovered() || resp.is_pointer_button_down_on())
    {
        Color32::WHITE
    } else {
        fg
    };
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(16.0),
        glyph_color,
    );
    resp
}
