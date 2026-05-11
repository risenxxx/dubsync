//! Drag-to-resize handles for the borderless window on Windows / Linux.
//!
//! `ViewportBuilder::with_decorations(false)` removes the OS-drawn chrome —
//! including the edge hit-test zones the window manager normally uses to start
//! a native resize. We restore that by overlaying eight invisible interact
//! rectangles (four edges + four corners) at `Order::Foreground` and converting
//! the first drag-frame into a `ViewportCommand::BeginResize(direction)` — the
//! native window manager takes over from there, so the rest of the resize feels
//! identical to a system-decorated window.
//!
//! No-op on macOS: the macOS branch keeps the native title bar (just hidden
//! visually), so AppKit still serves the resize edges from its own content
//! view. No-op when maximized: the OS doesn't honour resize commands on a
//! maximized window, and showing the resize cursor would be misleading.
//!
//! Note on layering with the title bar: the N / NW / NE zones sit *on top* of
//! the topmost ~4 px of the custom title bar. That's intentional — without
//! this overlay you'd lose the top-edge resize entirely. The corner zones are
//! kept thin (6 px) so the close / minimize / maximize buttons at the top-right
//! still receive clicks across the bulk of their hit area.

use eframe::egui::{
    self, Area, CursorIcon, Id, Order, Pos2, Rect, ResizeDirection, Sense, Vec2, ViewportCommand,
};

const EDGE_THICKNESS: f32 = 4.0;
const CORNER_SIZE: f32 = 6.0;

#[cfg(target_os = "macos")]
pub fn show(_ctx: &egui::Context, _is_maximized: bool) {}

#[cfg(not(target_os = "macos"))]
pub fn show(ctx: &egui::Context, is_maximized: bool) {
    if is_maximized {
        return;
    }

    let screen = ctx.screen_rect();
    let t = EDGE_THICKNESS;
    let c = CORNER_SIZE;

    // Corners are listed first so their `Area`s register before edges — when
    // hover regions overlap at a corner, the corner's diagonal cursor wins.
    let zones: [(&str, Rect, ResizeDirection, CursorIcon); 8] = [
        (
            "dubsync-resize-nw",
            Rect::from_min_size(screen.min, Vec2::splat(c)),
            ResizeDirection::NorthWest,
            CursorIcon::ResizeNwSe,
        ),
        (
            "dubsync-resize-ne",
            Rect::from_min_size(Pos2::new(screen.max.x - c, screen.min.y), Vec2::splat(c)),
            ResizeDirection::NorthEast,
            CursorIcon::ResizeNeSw,
        ),
        (
            "dubsync-resize-sw",
            Rect::from_min_size(Pos2::new(screen.min.x, screen.max.y - c), Vec2::splat(c)),
            ResizeDirection::SouthWest,
            CursorIcon::ResizeNeSw,
        ),
        (
            "dubsync-resize-se",
            Rect::from_min_size(
                Pos2::new(screen.max.x - c, screen.max.y - c),
                Vec2::splat(c),
            ),
            ResizeDirection::SouthEast,
            CursorIcon::ResizeNwSe,
        ),
        (
            "dubsync-resize-n",
            Rect::from_min_max(
                Pos2::new(screen.min.x + c, screen.min.y),
                Pos2::new(screen.max.x - c, screen.min.y + t),
            ),
            ResizeDirection::North,
            CursorIcon::ResizeVertical,
        ),
        (
            "dubsync-resize-s",
            Rect::from_min_max(
                Pos2::new(screen.min.x + c, screen.max.y - t),
                Pos2::new(screen.max.x - c, screen.max.y),
            ),
            ResizeDirection::South,
            CursorIcon::ResizeVertical,
        ),
        (
            "dubsync-resize-w",
            Rect::from_min_max(
                Pos2::new(screen.min.x, screen.min.y + c),
                Pos2::new(screen.min.x + t, screen.max.y - c),
            ),
            ResizeDirection::West,
            CursorIcon::ResizeHorizontal,
        ),
        (
            "dubsync-resize-e",
            Rect::from_min_max(
                Pos2::new(screen.max.x - t, screen.min.y + c),
                Pos2::new(screen.max.x, screen.max.y - c),
            ),
            ResizeDirection::East,
            CursorIcon::ResizeHorizontal,
        ),
    ];

    for (id, rect, direction, cursor) in zones {
        Area::new(Id::new(id))
            .order(Order::Foreground)
            .fixed_pos(rect.min)
            .interactable(true)
            .show(ctx, |ui| {
                let resp = ui.allocate_response(rect.size(), Sense::click_and_drag());
                if resp.hovered() || resp.dragged() {
                    ctx.set_cursor_icon(cursor);
                }
                if resp.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::BeginResize(direction));
                }
            });
    }
}
