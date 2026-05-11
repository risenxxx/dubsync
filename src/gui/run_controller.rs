//! Pipeline driver for the GUI. Spawns `run_pipeline` on a background thread and
//! routes its log + completion events back to the UI thread via crossbeam channels.
//!
//! Two channels flow from worker → UI:
//! - `LogLine`s (free-form `tracing` output) feed the "Show full log" disclosure.
//! - `PipelineEvent`s (structured phase boundaries + sub-progress) feed the chat
//!   panel.
//!
//! Tracing setup: `init_tracing_with_capture` installs a global subscriber composed
//! of the standard `EnvFilter` + a stderr `fmt` layer (for development) + a custom
//! `ChannelLayer` that fans every event to a `crossbeam_channel::Receiver` returned
//! to the caller.

use crate::progress::{ChannelReporter, PipelineEvent};
use crossbeam_channel::{Receiver, Sender};
use std::path::PathBuf;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: Level,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum RunOutcome {
    Ok(PathBuf),
    Err(String),
}

pub struct RunningHandle {
    pub result_rx: Receiver<RunOutcome>,
    pub event_rx: Receiver<PipelineEvent>,
}

/// Build and install the global tracing subscriber. Returns a Receiver that emits
/// every formatted log event from anywhere in the process. Call exactly once at
/// app startup.
pub fn init_tracing_with_capture() -> Receiver<LogLine> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer().with_target(false);
    let channel_layer = ChannelLayer { tx };

    // `try_init` so a stray double-init in tests doesn't panic.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(channel_layer)
        .try_init();
    rx
}

/// Spawn a background thread that runs the dubsync pipeline. Returns a handle with
/// two receivers — `result_rx` for final success/failure and `event_rx` for the
/// stream of `PipelineEvent`s that feeds the chat panel. The log-line receiver is
/// shared at the App level (installed by `init_tracing_with_capture`).
pub fn start_pipeline(cfg: crate::cli::RunConfig) -> RunningHandle {
    let (result_tx, result_rx) = crossbeam_channel::bounded(1);
    // Bounded so a stalled UI thread can't queue unbounded events; `try_send` in
    // ChannelReporter drops on full so the pipeline never blocks.
    let (event_tx, event_rx) = crossbeam_channel::bounded(256);
    let reporter = ChannelReporter::new(event_tx);
    std::thread::Builder::new()
        .name("dubsync-pipeline".into())
        .spawn(move || {
            let outcome = match crate::run_pipeline(cfg, &reporter) {
                Ok(p) => RunOutcome::Ok(p),
                Err(e) => RunOutcome::Err(format!("{e:#}")),
            };
            let _ = result_tx.send(outcome);
        })
        .expect("failed to spawn pipeline thread");
    RunningHandle {
        result_rx,
        event_rx,
    }
}

struct ChannelLayer {
    tx: Sender<LogLine>,
}

impl<S> Layer<S> for ChannelLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let line = LogLine {
            level: *event.metadata().level(),
            message: visitor.into_message(),
        };
        // Drop on full / closed — never block the pipeline thread on UI lag.
        let _ = self.tx.try_send(line);
    }
}

#[derive(Default)]
struct MessageVisitor {
    main: Option<String>,
    extras: String,
}

impl MessageVisitor {
    fn into_message(mut self) -> String {
        let mut out = self.main.take().unwrap_or_default();
        if !self.extras.is_empty() {
            out.push_str(&self.extras);
        }
        out
    }
}

impl tracing::field::Visit for MessageVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.main = Some(value.to_string());
        } else {
            self.extras.push_str(&format!(" {}={value}", field.name()));
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let formatted = format!("{value:?}");
        if field.name() == "message" {
            // tracing's `info!("text")` lands here as a Debug-formatted string with
            // surrounding quotes that we want to strip.
            let trimmed = formatted
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .map(|s| s.to_string())
                .unwrap_or(formatted);
            self.main = Some(trimmed);
        } else {
            self.extras
                .push_str(&format!(" {}={formatted}", field.name()));
        }
    }
}
