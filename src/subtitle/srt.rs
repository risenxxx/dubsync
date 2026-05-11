//! SubRip (SRT) parser + serializer.
//!
//! SRT is the lingua franca for text subtitles in MKV — ffmpeg's `-c:s srt`
//! converts ASS / WebVTT / etc. to SRT during extraction, so this module only
//! needs to handle one format. The grammar is informal; we tolerate common
//! deviations (CRLF, BOM, missing blank lines, missing index numbers) and
//! always emit a canonical form on serialize.

use crate::error::{DubsyncError, Result};
use crate::subtitle::SubtitleEvent;

/// Parse an SRT document into a list of events.
///
/// Tolerates:
/// - LF or CRLF line endings (normalized internally).
/// - UTF-8 BOM at the start of the file.
/// - Missing or non-sequential index numbers (events are renumbered on
///   serialize anyway).
/// - Extra blank lines between events.
///
/// Fails with [`DubsyncError::SubtitleParse`] when a timestamp line can't be
/// parsed — that's structurally broken and we can't safely guess.
pub fn parse(text: &str) -> Result<Vec<SubtitleEvent>> {
    // Strip BOM + normalize line endings to '\n' so the rest of the parser
    // doesn't have to handle every combination.
    let cleaned = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");

    let mut events = Vec::new();
    let mut lines = cleaned.lines().peekable();

    while lines.peek().is_some() {
        // Skip blank lines between events.
        while lines.peek().map(|l| l.trim().is_empty()).unwrap_or(false) {
            lines.next();
        }
        if lines.peek().is_none() {
            break;
        }

        // Optional index line. If the line parses as a u32 and the next line
        // looks like a timestamp, consume it; otherwise treat the current line
        // as a timestamp.
        let first = lines.next().unwrap();
        let (idx, ts_line) = if let Ok(n) = first.trim().parse::<u32>() {
            // Look ahead: if the next non-empty line is a timestamp, this was
            // an index. Otherwise, the digit was actually part of the
            // timestamp range (defensive — shouldn't happen for valid SRT).
            match lines.next() {
                Some(l) if l.contains("-->") => (n, l.to_string()),
                Some(l) => {
                    return Err(DubsyncError::SubtitleParse(format!(
                        "expected timestamp after index line `{first}`, got `{l}`"
                    )));
                }
                None => {
                    return Err(DubsyncError::SubtitleParse(format!(
                        "trailing index `{first}` with no timestamp line"
                    )));
                }
            }
        } else if first.contains("-->") {
            (events.len() as u32 + 1, first.to_string())
        } else {
            return Err(DubsyncError::SubtitleParse(format!(
                "expected index or timestamp line, got `{first}`"
            )));
        };

        // Parse the timestamp line: "HH:MM:SS,mmm --> HH:MM:SS,mmm".
        let (start_s, end_s) = parse_timestamps(&ts_line)?;

        // Collect text lines until blank or EOF.
        let mut text_buf = String::new();
        while let Some(line) = lines.peek() {
            if line.trim().is_empty() {
                break;
            }
            if !text_buf.is_empty() {
                text_buf.push('\n');
            }
            text_buf.push_str(line);
            lines.next();
        }

        events.push(SubtitleEvent {
            index: idx,
            start_s,
            end_s,
            text: text_buf,
        });
    }

    Ok(events)
}

/// Serialize events back to canonical SRT: sequential indices, `,` decimal
/// separator, blank line between events.
pub fn serialize(events: &[SubtitleEvent]) -> String {
    let mut out = String::with_capacity(events.len() * 80);
    for (i, ev) in events.iter().enumerate() {
        // Always renumber sequentially regardless of `ev.index` to guarantee
        // canonical output.
        let n = i + 1;
        out.push_str(&format!(
            "{n}\n{} --> {}\n{}\n\n",
            format_timestamp(ev.start_s),
            format_timestamp(ev.end_s),
            ev.text,
        ));
    }
    out
}

fn parse_timestamps(line: &str) -> Result<(f64, f64)> {
    let mut parts = line.split("-->");
    let start = parts.next().ok_or_else(|| {
        DubsyncError::SubtitleParse(format!("missing start timestamp in `{line}`"))
    })?;
    let end = parts
        .next()
        .ok_or_else(|| DubsyncError::SubtitleParse(format!("missing end timestamp in `{line}`")))?;
    Ok((parse_one_timestamp(start)?, parse_one_timestamp(end)?))
}

/// Parse a single "HH:MM:SS,mmm" or "HH:MM:SS.mmm" into seconds-as-f64.
fn parse_one_timestamp(raw: &str) -> Result<f64> {
    let s = raw.trim();
    // Allow both `,` (SRT canonical) and `.` (some tools emit) as the decimal
    // separator. WebVTT uses `.`; ffmpeg's srt encoder uses `,`.
    let s = s.replace(',', ".");
    let mut hms = s.splitn(3, ':');
    let h: u32 = hms
        .next()
        .ok_or_else(|| DubsyncError::SubtitleParse(format!("missing hours in `{raw}`")))?
        .parse()
        .map_err(|e| DubsyncError::SubtitleParse(format!("bad hours in `{raw}`: {e}")))?;
    let m: u32 = hms
        .next()
        .ok_or_else(|| DubsyncError::SubtitleParse(format!("missing minutes in `{raw}`")))?
        .parse()
        .map_err(|e| DubsyncError::SubtitleParse(format!("bad minutes in `{raw}`: {e}")))?;
    let secs: f64 = hms
        .next()
        .ok_or_else(|| DubsyncError::SubtitleParse(format!("missing seconds in `{raw}`")))?
        .parse()
        .map_err(|e| DubsyncError::SubtitleParse(format!("bad seconds in `{raw}`: {e}")))?;
    Ok(f64::from(h) * 3600.0 + f64::from(m) * 60.0 + secs)
}

fn format_timestamp(seconds: f64) -> String {
    // Negative seconds shouldn't happen post-shift if the offset map is sane,
    // but clamp defensively so we never emit "-00:00:01,500" which most
    // players parse as garbage.
    let s = seconds.max(0.0);
    let total_ms = (s * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let secs = total_s % 60;
    let mins = (total_s / 60) % 60;
    let hours = total_s / 3600;
    format!("{hours:02}:{mins:02}:{secs:02},{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_two_event_srt() {
        let text = "1\n00:00:01,000 --> 00:00:03,500\nHello\n\n2\n00:00:10,250 --> 00:00:12,000\nWorld\nLine 2\n\n";
        let events = parse(text).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].index, 1);
        assert!((events[0].start_s - 1.0).abs() < 1e-9);
        assert!((events[0].end_s - 3.5).abs() < 1e-9);
        assert_eq!(events[0].text, "Hello");
        assert!((events[1].start_s - 10.25).abs() < 1e-9);
        assert_eq!(events[1].text, "World\nLine 2");
    }

    #[test]
    fn handles_crlf_and_bom() {
        let text =
            "\u{feff}1\r\n00:00:01,000 --> 00:00:02,000\r\nHi\r\n\r\n2\r\n00:00:05,000 --> 00:00:06,000\r\nBye\r\n";
        let events = parse(text).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].text, "Hi");
        assert_eq!(events[1].text, "Bye");
    }

    #[test]
    fn tolerates_dot_decimal_separator() {
        // WebVTT-style timestamps with `.` instead of `,` — some tools emit
        // these for SRT too. Should parse identically.
        let text = "1\n00:00:01.500 --> 00:00:03.250\nHi\n\n";
        let events = parse(text).unwrap();
        assert!((events[0].start_s - 1.5).abs() < 1e-9);
        assert!((events[0].end_s - 3.25).abs() < 1e-9);
    }

    #[test]
    fn tolerates_missing_index_lines() {
        let text = "00:00:01,000 --> 00:00:02,000\nA\n\n00:00:03,000 --> 00:00:04,000\nB\n\n";
        let events = parse(text).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].index, 1);
        assert_eq!(events[1].index, 2);
    }

    #[test]
    fn round_trip_preserves_timing_and_text() {
        let original = "1\n00:00:01,000 --> 00:00:03,500\nHello\nworld\n\n2\n00:01:30,250 --> 00:01:32,000\nBye\n\n";
        let events = parse(original).unwrap();
        let out = serialize(&events);
        let reparsed = parse(&out).unwrap();
        assert_eq!(events.len(), reparsed.len());
        for (a, b) in events.iter().zip(reparsed.iter()) {
            assert!((a.start_s - b.start_s).abs() < 1e-3);
            assert!((a.end_s - b.end_s).abs() < 1e-3);
            assert_eq!(a.text, b.text);
        }
    }

    #[test]
    fn timestamp_round_trip() {
        for s in [0.0, 0.001, 1.5, 59.999, 60.0, 3600.0, 3661.123] {
            let formatted = format_timestamp(s);
            let parsed = parse_one_timestamp(&formatted).unwrap();
            assert!(
                (s - parsed).abs() < 1e-3,
                "round trip failed for {s}: formatted={formatted}, parsed={parsed}"
            );
        }
    }

    #[test]
    fn rejects_garbage_timestamps() {
        let bad = "1\nnot a timestamp\nHi\n\n";
        assert!(parse(bad).is_err());
    }

    #[test]
    fn serialize_clamps_negative_to_zero() {
        // Defensive: negative pre-shift events shouldn't reach serialize, but if they do,
        // clamp so the output stays valid SRT.
        let events = vec![SubtitleEvent {
            index: 1,
            start_s: -5.0,
            end_s: -3.0,
            text: "invalid".into(),
        }];
        let out = serialize(&events);
        assert!(out.contains("00:00:00,000 --> 00:00:00,000"));
    }
}
