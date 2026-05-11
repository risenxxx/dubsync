//! Chat-style timeline of pipeline phases.
//!
//! Each [`PhaseId`] gets one card. The card lifecycle is driven by `PipelineEvent`s
//! drained from the worker-thread channel:
//! - `PhaseStarted`  → upsert a card with `Running` status
//! - `PhaseProgress` → update fraction + detail in place
//! - `PhaseFinished` → flip status to `Complete`, store summary, freeze elapsed
//! - `PhaseFailed`   → flip status to `Failed`, store error
//!
//! Visual: each card is an `egui::Frame::group`-bordered block with a status glyph,
//! bold title, mm:ss elapsed, optional progress bar, optional weak-text detail, and
//! a green/red footer for completion summary or error. Auto-scroll-to-bottom keeps
//! the most recent card visible.

use crate::progress::{PhaseId, PipelineEvent};
use crate::report::RunSummary;
use eframe::egui;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStatus {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone)]
pub struct PhaseCard {
    pub id: PhaseId,
    pub title: String,
    pub status: CardStatus,
    pub fraction: Option<f32>,
    pub detail: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub eta_hint: Option<Duration>,
    pub started: Instant,
    pub finished: Option<Instant>,
}

impl PhaseCard {
    fn new(id: PhaseId, title: String, eta_hint: Option<Duration>, detail: Option<String>) -> Self {
        Self {
            id,
            title,
            status: CardStatus::Running,
            fraction: None,
            detail,
            summary: None,
            error: None,
            eta_hint,
            started: Instant::now(),
            finished: None,
        }
    }

    fn elapsed(&self) -> Duration {
        self.finished
            .map(|f| f.duration_since(self.started))
            .unwrap_or_else(|| self.started.elapsed())
    }
}

#[derive(Default)]
pub struct ChatPanel {
    cards: Vec<PhaseCard>,
    summary: Option<RunSummary>,
}

impl ChatPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.cards.clear();
        self.summary = None;
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty() && self.summary.is_none()
    }

    /// Apply one event. Cards are upserted by `PhaseId` so re-runs of the same phase
    /// (the user may run the pipeline multiple times in one session) overwrite the
    /// previous card cleanly.
    pub fn ingest(&mut self, ev: PipelineEvent) {
        match ev {
            PipelineEvent::PhaseStarted {
                id,
                title,
                eta_hint,
                detail,
            } => {
                let card = PhaseCard::new(id, title, eta_hint, detail);
                if let Some(slot) = self.cards.iter_mut().find(|c| c.id == id) {
                    *slot = card;
                } else {
                    self.cards.push(card);
                }
            }
            PipelineEvent::PhaseProgress {
                id,
                fraction,
                detail,
            } => {
                if let Some(card) = self.cards.iter_mut().find(|c| c.id == id) {
                    card.fraction = Some(fraction);
                    if let Some(d) = detail {
                        card.detail = Some(d);
                    }
                }
            }
            PipelineEvent::PhaseFinished { id, summary } => {
                if let Some(card) = self.cards.iter_mut().find(|c| c.id == id) {
                    card.status = CardStatus::Complete;
                    card.fraction = Some(1.0);
                    card.summary = summary;
                    card.finished = Some(Instant::now());
                }
            }
            PipelineEvent::PhaseFailed { id, error } => {
                if let Some(card) = self.cards.iter_mut().find(|c| c.id == id) {
                    card.status = CardStatus::Failed;
                    card.error = Some(error);
                    card.finished = Some(Instant::now());
                }
            }
            PipelineEvent::RunSummary(summary) => {
                self.summary = Some(*summary);
            }
        }
    }

    /// Render all cards in order. Caller is responsible for wrapping in a scroll
    /// area if desired — see `App::update`.
    pub fn draw(&self, ui: &mut egui::Ui) {
        for card in &self.cards {
            draw_card(ui, card);
            ui.add_space(4.0);
        }
        if let Some(s) = &self.summary {
            draw_summary(ui, s);
        }
    }

    /// True if any card is in Running state — used by the App to decide whether to
    /// keep requesting frequent repaints for animation/elapsed updates.
    pub fn any_running(&self) -> bool {
        self.cards.iter().any(|c| c.status == CardStatus::Running)
    }
}

fn draw_card(ui: &mut egui::Ui, card: &PhaseCard) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            // Force the frame to span the parent's available width so cards line
            // up flush regardless of which content row is widest. Without this,
            // a card with only a short title (e.g. Workspace) collapses to its
            // text width and breaks the visual stack rhythm.
            ui.set_min_width(ui.available_width());
            // Title row.
            ui.horizontal(|ui| {
                match card.status {
                    CardStatus::Running => {
                        ui.spinner();
                    }
                    CardStatus::Complete => {
                        // Phosphor icons. egui's bundled fonts (Hack/Ubuntu/NotoEmoji)
                        // don't actually have ✓/✗ — they'd render as missing-glyph
                        // squares. Phosphor is registered in `bin/dubsync_gui.rs`'s
                        // creation context.
                        ui.colored_label(
                            egui::Color32::from_rgb(120, 200, 130),
                            egui::RichText::new(egui_phosphor::regular::CHECK_CIRCLE).size(18.0),
                        );
                    }
                    CardStatus::Failed => {
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            egui::RichText::new(egui_phosphor::regular::X_CIRCLE).size(18.0),
                        );
                    }
                }
                ui.label(
                    egui::RichText::new(format!(
                        "[{}/{}] {}",
                        card.id.ordinal(),
                        PhaseId::TOTAL,
                        card.title
                    ))
                    .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let elapsed = format_elapsed(card.elapsed());
                    let t = if let (CardStatus::Running, Some(eta)) = (card.status, card.eta_hint) {
                        format!("{elapsed} / ~{}", format_elapsed(eta))
                    } else {
                        elapsed
                    };
                    ui.label(
                        egui::RichText::new(t)
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });

            // Progress bar (Running phases only — completed cards already show ✓).
            //
            // No `.desired_width(...)` — egui defaults to `available_size_before_wrap().x`
            // which respects the surrounding Frame's content rect. Setting INFINITY here
            // makes egui allocate a Rect with infinite width and breaks layout (the bar
            // renders past the window edge and Correlate phase's high-frequency progress
            // updates can crash the renderer).
            if card.status == CardStatus::Running {
                if let Some(frac) = card.fraction {
                    ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).animate(true));
                }
            }

            // Detail line.
            if let Some(detail) = &card.detail {
                ui.label(
                    egui::RichText::new(detail)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }

            // Summary or error footer.
            match &card.status {
                CardStatus::Complete => {
                    if let Some(s) = &card.summary {
                        ui.label(
                            egui::RichText::new(s)
                                .small()
                                .color(egui::Color32::from_rgb(120, 200, 130)),
                        );
                    }
                }
                CardStatus::Failed => {
                    if let Some(err) = &card.error {
                        ui.label(
                            egui::RichText::new(err)
                                .small()
                                .color(egui::Color32::LIGHT_RED),
                        );
                    }
                }
                _ => {}
            }
        });
}

fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs_f64();
    if total >= 60.0 {
        let m = (total / 60.0) as u64;
        let s = total - (m as f64 * 60.0);
        format!("{m}:{s:04.1}")
    } else {
        format!("{total:.1}s")
    }
}

/// Final summary card: rendered after all phase cards once the pipeline emits a
/// `RunSummary` event. Distinguishable from regular phase cards via heading +
/// muted background tint.
fn draw_summary(ui: &mut egui::Ui, s: &RunSummary) {
    ui.add_space(8.0);
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(120, 200, 130),
                    egui::RichText::new(egui_phosphor::regular::CHECK_CIRCLE).size(18.0),
                );
                ui.label(egui::RichText::new("Run summary").strong());
            });
            ui.add_space(2.0);
            for line in s.human_lines() {
                ui.label(egui::RichText::new(line).small());
            }
        });
}
