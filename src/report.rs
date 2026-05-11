//! Run summary + detailed offset-map report.
//!
//! [`RunSummary`] is the high-level digest that always gets emitted at the end of
//! every pipeline run (CLI: prints to stderr; GUI: rendered as a final card; both
//! via [`crate::progress::PipelineEvent::RunSummary`]).
//!
//! [`OffsetMapReport`] is the detailed structure written to disk when the user
//! passes `--report <path>`. Format is dispatched on the path's extension:
//! - `.html`/`.htm` → styled HTML with summary block + per-segment table
//! - `.csv`         → header comment block + per-segment CSV rows
//! - `.json`        → pretty JSON dump of the whole [`OffsetMapReport`]
//! - other          → defaults to CSV
//!
//! The report is intentionally derivable from [`OffsetMap`] + master silences +
//! the cli config, with no per-dub coupling — the same report describes every dub
//! synced from a given master/donor pair.

use crate::audio::vad::SilenceInterval;
use crate::correlation::OffsetMap;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub master_duration_s: f32,
    pub master_fps: f64,
    pub donor_fps: Option<f64>,
    pub fps_stretch_ratio: Option<f64>,
    pub pal_pitch_applied: bool,
    /// User explicitly disabled the auto fps-normalize via
    /// `--disable-fps-normalize` / GUI "Disabled" mode. When true the donor was
    /// not probed for fps and no stretch was applied, regardless of any actual
    /// fps mismatch.
    #[serde(default)]
    pub auto_fps_disabled: bool,
    /// User-supplied fps ratio override via `--force-fps-ratio` / GUI "Manual
    /// ratio" mode. Some(r) means the ratio came from the user, not a probe.
    /// `1.0` is functionally equivalent to `auto_fps_disabled = true` but still
    /// recorded here for traceability.
    #[serde(default)]
    pub forced_fps_ratio: Option<f64>,
    pub anchor_only_validation: bool,
    pub total_anchors: usize,
    pub kept_anchors: usize,
    pub rejected_anchors: usize,
    pub segments: usize,
    pub low_confidence_pct: f32,
    pub max_jump_s: Option<f32>,
    pub max_jump_at_master_s: Option<f32>,
    pub master_snap_count: usize,
    pub fallback_required_count: usize,
    pub total_silence_inserted_s: f32,
    pub output_file: PathBuf,
    pub elapsed_s: f64,
}

impl RunSummary {
    /// One-line strings suitable for the chat-panel summary card and the CLI's
    /// end-of-run banner. The first line is always the elapsed wall-clock time so
    /// the eye can find it instantly when scanning a log.
    pub fn human_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!(
            "Total runtime: {} ({:.1}s of master)",
            format_duration(self.elapsed_s),
            self.master_duration_s
        ));
        // Mode prevails over probe results: a forced/disabled run says so up
        // front, the auto path falls back to the (donor_fps, stretch_ratio) tuple.
        if self.auto_fps_disabled {
            out.push(format!(
                "FPS: auto-normalize disabled by user (master {:.3})",
                self.master_fps
            ));
        } else if let Some(r) = self.forced_fps_ratio {
            if self.fps_stretch_ratio.is_some() {
                let pitch_note = if self.pal_pitch_applied {
                    " + PAL pitch correction"
                } else {
                    ""
                };
                out.push(format!(
                    "FPS: forced ratio {r:.4} (stretch applied{pitch_note})",
                ));
            } else {
                out.push(format!(
                    "FPS: forced ratio {r:.4} (no stretch — within 0.1% of identity)",
                ));
            }
        } else {
            match (self.donor_fps, self.fps_stretch_ratio) {
                (Some(d), Some(r)) => out.push(format!(
                    "FPS: master {:.3} vs donor {:.3} → stretched ratio {:.4}{}",
                    self.master_fps,
                    d,
                    r,
                    if self.pal_pitch_applied {
                        " + PAL pitch correction"
                    } else {
                        ""
                    }
                )),
                (Some(d), None) => out.push(format!(
                    "FPS: master {:.3} = donor {:.3} (no stretch)",
                    self.master_fps, d
                )),
                (None, _) => out.push(format!(
                    "FPS: master {:.3} (donor has no video stream)",
                    self.master_fps
                )),
            }
        }
        let kept_pct = if self.total_anchors > 0 {
            100.0 * self.kept_anchors as f32 / self.total_anchors as f32
        } else {
            0.0
        };
        out.push(format!(
            "Anchors: {}/{} kept ({:.1}%), {} rejected",
            self.kept_anchors, self.total_anchors, kept_pct, self.rejected_anchors
        ));
        out.push(format!("Segments: {}", self.segments));
        if let (Some(mag), Some(at)) = (self.max_jump_s, self.max_jump_at_master_s) {
            out.push(format!("Largest jump: {:+.3}s at master t={:.2}s", mag, at));
        }
        out.push(format!(
            "Splices: {} into master silence, {} need per-dub fallback",
            self.master_snap_count, self.fallback_required_count
        ));
        if self.total_silence_inserted_s > 0.001 {
            out.push(format!(
                "Total silence inserted: {:.3}s",
                self.total_silence_inserted_s
            ));
        }
        if self.anchor_only_validation {
            out.push("Mode: anchor-only validation (no dubs synced)".to_string());
        }
        out.push(format!("Output: {}", self.output_file.display()));
        out
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentRow {
    pub index: usize,
    pub master_start_s: f32,
    pub master_end_s: f32,
    pub duration_s: f32,
    pub donor_offset_s: f32,
    pub jump_from_prev_s: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BoundaryRow {
    pub index: usize,
    pub boundary_master_s: f32,
    pub delta_s: f32,
    pub master_silence_used: bool,
    pub master_silence_at_s: Option<f32>,
    pub master_silence_width_s: Option<f32>,
    pub gap_s: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct OffsetMapReport {
    pub summary: RunSummary,
    pub segments: Vec<SegmentRow>,
    pub boundaries: Vec<BoundaryRow>,
}

impl OffsetMapReport {
    /// Build a detailed report from the offset map + master-anchor silences. This
    /// captures the same per-boundary decision the splicer makes for the master
    /// side (master-silence snap or fall back to per-dub silence).
    pub fn build(
        summary: RunSummary,
        map: &OffsetMap,
        master_silences: &[SilenceInterval],
        snap_radius_s: f32,
    ) -> Self {
        let segments = build_segment_rows(map);
        let boundaries = build_boundary_rows(map, master_silences, f64::from(snap_radius_s));
        Self {
            summary,
            segments,
            boundaries,
        }
    }
}

fn build_segment_rows(map: &OffsetMap) -> Vec<SegmentRow> {
    let mut rows = Vec::with_capacity(map.segments.len());
    let mut prev_offset: Option<f32> = None;
    for (i, seg) in map.segments.iter().enumerate() {
        let jump = prev_offset.map(|p| seg.donor_offset_s - p);
        rows.push(SegmentRow {
            index: i,
            master_start_s: seg.master_start_s,
            master_end_s: seg.master_end_s,
            duration_s: seg.master_end_s - seg.master_start_s,
            donor_offset_s: seg.donor_offset_s,
            jump_from_prev_s: jump,
        });
        prev_offset = Some(seg.donor_offset_s);
    }
    rows
}

fn build_boundary_rows(
    map: &OffsetMap,
    master_silences: &[SilenceInterval],
    snap_radius_s: f64,
) -> Vec<BoundaryRow> {
    if map.segments.len() < 2 {
        return Vec::new();
    }
    let mut rows = Vec::with_capacity(map.segments.len() - 1);
    for i in 0..map.segments.len() - 1 {
        let boundary = map.segments[i].master_end_s;
        let delta = map.segments[i + 1].donor_offset_s - map.segments[i].donor_offset_s;
        let snap = best_master_silence_summary(
            f64::from(boundary),
            f64::from(delta),
            master_silences,
            snap_radius_s,
        );
        let gap_s = if delta < 0.0 { delta.abs() } else { 0.0 };
        rows.push(BoundaryRow {
            index: i,
            boundary_master_s: boundary,
            delta_s: delta,
            master_silence_used: snap.is_some(),
            master_silence_at_s: snap.map(|s| s.0 as f32),
            master_silence_width_s: snap.map(|s| s.1 as f32),
            gap_s,
        });
    }
    rows
}

/// Mirror of `sync::applier::best_master_silence` but returns just (center, width)
/// to keep this module decoupled from the splicer's internal types.
fn best_master_silence_summary(
    boundary: f64,
    delta: f64,
    silences: &[SilenceInterval],
    radius: f64,
) -> Option<(f64, f64)> {
    let needed = delta.abs();
    let mut best: Option<(f64, f64, f64)> = None;
    for s in silences {
        let start = s.start_s as f64;
        let end = s.end_s as f64;
        let width = end - start;
        if width < needed {
            continue;
        }
        let center = (start + end) * 0.5;
        let dist = (center - boundary).abs();
        if dist > radius {
            continue;
        }
        let better = match best {
            None => true,
            Some((d, w, _)) => dist < d || (dist == d && width > w),
        };
        if better {
            best = Some((dist, width, center));
        }
    }
    best.map(|(_, w, c)| (c, w))
}

/// Write `report` to `path`. Format dispatched on extension; unknown extensions
/// fall back to CSV.
pub fn write_report(path: &Path, report: &OffsetMapReport) -> std::io::Result<()> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("html" | "htm") => write_html(path, report),
        Some("json") => write_json(path, report),
        _ => write_csv(path, report),
    }
}

fn write_csv(path: &Path, report: &OffsetMapReport) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    // Header comment block — survived by every common CSV consumer (lines starting
    // with `#` are skipped by pandas/awk-with-flag, ignored as data by Excel).
    for line in report.summary.human_lines() {
        writeln!(f, "# {line}")?;
    }
    writeln!(f, "#")?;
    writeln!(
        f,
        "section,index,master_start_s,master_end_s,duration_s,donor_offset_s,jump_from_prev_s,boundary_master_s,delta_s,master_silence_used,master_silence_at_s,master_silence_width_s,gap_s"
    )?;
    for s in &report.segments {
        writeln!(
            f,
            "segment,{},{},{},{},{},{},,,,,,",
            s.index,
            s.master_start_s,
            s.master_end_s,
            s.duration_s,
            s.donor_offset_s,
            s.jump_from_prev_s
                .map(|v| format!("{v}"))
                .unwrap_or_default()
        )?;
    }
    for b in &report.boundaries {
        writeln!(
            f,
            "boundary,{},,,,,,{},{},{},{},{},{}",
            b.index,
            b.boundary_master_s,
            b.delta_s,
            b.master_silence_used,
            b.master_silence_at_s
                .map(|v| format!("{v}"))
                .unwrap_or_default(),
            b.master_silence_width_s
                .map(|v| format!("{v}"))
                .unwrap_or_default(),
            b.gap_s
        )?;
    }
    Ok(())
}

fn write_json(path: &Path, report: &OffsetMapReport) -> std::io::Result<()> {
    let f = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(f, report).map_err(std::io::Error::other)
}

fn write_html(path: &Path, report: &OffsetMapReport) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    let title = format!("dubsync report — {}", report.summary.output_file.display());
    writeln!(
        f,
        "<!doctype html>\n<html lang=\"en\"><head>\n\
         <meta charset=\"utf-8\">\n\
         <title>{}</title>\n\
         <style>\n\
         body {{ font-family: -apple-system, system-ui, sans-serif; margin: 24px; max-width: 980px; }}\n\
         h1 {{ font-size: 1.4em; margin-bottom: 0.2em; }}\n\
         .summary ul {{ list-style: none; padding-left: 0; }}\n\
         .summary li {{ padding: 2px 0; }}\n\
         table {{ border-collapse: collapse; margin-top: 12px; font-size: 0.92em; }}\n\
         th, td {{ border: 1px solid #ddd; padding: 4px 8px; text-align: right; }}\n\
         th {{ background: #f4f4f4; font-weight: 600; }}\n\
         tr:nth-child(even) td {{ background: #fbfbfb; }}\n\
         .yes {{ color: #2a8a4f; font-weight: 600; }}\n\
         .no {{ color: #b04848; font-weight: 600; }}\n\
         .num {{ font-variant-numeric: tabular-nums; }}\n\
         </style>\n\
         </head><body>",
        html_escape(&title)
    )?;

    writeln!(
        f,
        "<h1>dubsync report</h1>\n<section class=\"summary\"><ul>"
    )?;
    for line in report.summary.human_lines() {
        writeln!(f, "<li>{}</li>", html_escape(&line))?;
    }
    writeln!(f, "</ul></section>")?;

    writeln!(
        f,
        "<h2>Segments</h2>\n<table>\n<thead><tr>\
         <th>#</th><th>master_start_s</th><th>master_end_s</th>\
         <th>duration_s</th><th>donor_offset_s</th><th>jump_from_prev_s</th>\
         </tr></thead>\n<tbody>"
    )?;
    for s in &report.segments {
        let jump = s
            .jump_from_prev_s
            .map(|v| format!("{v:+.3}"))
            .unwrap_or_else(|| "—".to_string());
        writeln!(
            f,
            "<tr class=\"num\"><td>{}</td><td>{:.3}</td><td>{:.3}</td><td>{:.3}</td><td>{:+.3}</td><td>{}</td></tr>",
            s.index, s.master_start_s, s.master_end_s, s.duration_s, s.donor_offset_s, jump
        )?;
    }
    writeln!(f, "</tbody></table>")?;

    if !report.boundaries.is_empty() {
        writeln!(
            f,
            "<h2>Boundaries</h2>\n<table>\n<thead><tr>\
             <th>#</th><th>boundary_master_s</th><th>delta_s</th>\
             <th>master_silence_used</th><th>master_silence_at_s</th>\
             <th>master_silence_width_s</th><th>gap_s</th>\
             </tr></thead>\n<tbody>"
        )?;
        for b in &report.boundaries {
            let used = if b.master_silence_used {
                "<span class=\"yes\">yes</span>"
            } else {
                "<span class=\"no\">fallback</span>"
            };
            let at = b
                .master_silence_at_s
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "—".to_string());
            let width = b
                .master_silence_width_s
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "—".to_string());
            writeln!(
                f,
                "<tr class=\"num\"><td>{}</td><td>{:.3}</td><td>{:+.3}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.3}</td></tr>",
                b.index, b.boundary_master_s, b.delta_s, used, at, width, b.gap_s
            )?;
        }
        writeln!(f, "</tbody></table>")?;
    }

    writeln!(f, "</body></html>")?;
    Ok(())
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0);
    let m = (total / 60.0) as u64;
    let s = total - (m as f64 * 60.0);
    if m > 0 {
        format!("{m}m{s:.1}s")
    } else {
        format!("{s:.1}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::correlation::Segment;

    fn fake_summary(output: PathBuf) -> RunSummary {
        RunSummary {
            master_duration_s: 1830.5,
            master_fps: 24.0,
            donor_fps: Some(25.0),
            fps_stretch_ratio: Some(25.0 / 24.0),
            pal_pitch_applied: true,
            auto_fps_disabled: false,
            forced_fps_ratio: None,
            anchor_only_validation: false,
            total_anchors: 122,
            kept_anchors: 119,
            rejected_anchors: 3,
            segments: 4,
            low_confidence_pct: 2.5,
            max_jump_s: Some(-1.250),
            max_jump_at_master_s: Some(305.4),
            master_snap_count: 2,
            fallback_required_count: 1,
            total_silence_inserted_s: 3.250,
            output_file: output,
            elapsed_s: 312.7,
        }
    }

    fn fake_map() -> OffsetMap {
        OffsetMap {
            segments: vec![
                Segment {
                    master_start_s: 0.0,
                    master_end_s: 305.0,
                    donor_offset_s: 0.0,
                },
                Segment {
                    master_start_s: 305.0,
                    master_end_s: 1830.5,
                    donor_offset_s: -1.25,
                },
            ],
            master_duration_s: 1830.5,
            anchor_count: 119,
            rejected_count: 3,
            suppressed_jumps: 0,
            valid_master_start_s: 0.0,
            valid_master_end_s: 1830.5,
        }
    }

    #[test]
    fn html_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.html");
        let report = OffsetMapReport::build(
            fake_summary(PathBuf::from("/tmp/out.mkv")),
            &fake_map(),
            &[],
            30.0,
        );
        write_report(&path, &report).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("<title>dubsync report"));
        assert!(content.contains("<h1>dubsync report"));
        assert!(content.contains("Segments"));
        assert!(content.contains("305.000"));
    }

    #[test]
    fn csv_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.csv");
        let report = OffsetMapReport::build(
            fake_summary(PathBuf::from("/tmp/out.mkv")),
            &fake_map(),
            &[],
            30.0,
        );
        write_report(&path, &report).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("# Total runtime:"));
        assert!(content.contains("section,index,master_start_s"));
        assert!(content.contains("segment,0,0,305"));
    }

    #[test]
    fn unknown_extension_falls_back_to_csv() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.weird");
        let report = OffsetMapReport::build(
            fake_summary(PathBuf::from("/tmp/out.mkv")),
            &fake_map(),
            &[],
            30.0,
        );
        write_report(&path, &report).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("section,index,master_start_s"));
    }

    #[test]
    fn json_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        let report = OffsetMapReport::build(
            fake_summary(PathBuf::from("/tmp/out.mkv")),
            &fake_map(),
            &[],
            30.0,
        );
        write_report(&path, &report).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(parsed["summary"]["segments"], 4);
        assert_eq!(parsed["segments"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn human_lines_emit_eta_and_anchors() {
        let s = fake_summary(PathBuf::from("/tmp/out.mkv"));
        let lines = s.human_lines();
        assert!(lines.iter().any(|l| l.starts_with("Total runtime:")));
        assert!(lines.iter().any(|l| l.contains("FPS:")));
        assert!(lines.iter().any(|l| l.contains("Anchors:")));
        assert!(lines.iter().any(|l| l.contains("Largest jump:")));
    }
}
