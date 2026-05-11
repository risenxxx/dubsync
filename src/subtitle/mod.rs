//! Subtitle handling for the donor forced-subs sync path.
//!
//! ffmpeg extracts donor subtitle tracks as SubRip text (`-c:s srt` auto-converts
//! ASS / WebVTT during extraction), so this module only knows about SRT. The
//! shape is simple: a list of [`SubtitleEvent`]s with `start_s`/`end_s` in
//! donor time, plus the raw multi-line text. [`apply_offset_map`] walks each
//! event through the [`OffsetMap`]'s segments, converts donor-time → master-time,
//! and drops events that fall outside any segment's donor coverage.

pub mod srt;

use crate::correlation::OffsetMap;

#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleEvent {
    pub index: u32,
    pub start_s: f64,
    pub end_s: f64,
    /// Multi-line text. Original markup (italics tags, line breaks, etc.) is
    /// preserved verbatim — we don't reformat the cue body.
    pub text: String,
}

/// Convert donor-time events into master-time events using the offset map.
///
/// Strategy: for each event, compute its donor-time midpoint and find the
/// segment whose donor-time coverage `[m_start + off, m_end + off]` contains
/// it. Subtract that segment's offset to produce master-time start/end.
/// Events whose midpoint falls outside every segment's donor coverage
/// (typically intro/outro mismatches that don't have anchor evidence in the
/// master) are dropped. Surviving events are renumbered sequentially so the
/// output is valid SRT.
///
/// Limitation (v1): events that genuinely span a segment-jump boundary use
/// the offset of the segment containing their midpoint. Since events are
/// usually <5 s and segments are usually >60 s, this rarely matters in
/// practice. Splitting events at boundaries is left as a future refinement.
pub fn apply_offset_map(events: &[SubtitleEvent], map: &OffsetMap) -> Vec<SubtitleEvent> {
    let mut out: Vec<SubtitleEvent> = Vec::with_capacity(events.len());
    for ev in events {
        let mid_donor = (ev.start_s + ev.end_s) * 0.5;
        let Some(seg) = find_segment_for_donor_time(map, mid_donor) else {
            tracing::debug!(
                index = ev.index,
                start_s = ev.start_s,
                end_s = ev.end_s,
                "subtitle event outside any segment's donor-time coverage — dropping"
            );
            continue;
        };
        let off = f64::from(seg.donor_offset_s);
        let new_start = ev.start_s - off;
        let new_end = ev.end_s - off;
        out.push(SubtitleEvent {
            index: 0, // assigned during sequential renumbering below
            start_s: new_start,
            end_s: new_end,
            text: ev.text.clone(),
        });
    }
    // Sort by start time + renumber sequentially so the output is canonical SRT.
    out.sort_by(|a, b| {
        a.start_s
            .partial_cmp(&b.start_s)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (i, ev) in out.iter_mut().enumerate() {
        ev.index = (i + 1) as u32;
    }
    out
}

/// For donor-time `t`, find the segment whose donor coverage
/// `[master_start + offset, master_end + offset]` contains `t`. Returns
/// `None` if no segment matches (event lies in pre-intro / post-outro or in a
/// gap caused by overlapping segments after geometry adjustment).
fn find_segment_for_donor_time(
    map: &OffsetMap,
    donor_t: f64,
) -> Option<&crate::correlation::Segment> {
    map.segments.iter().find(|seg| {
        let off = f64::from(seg.donor_offset_s);
        let lo = f64::from(seg.master_start_s) + off;
        let hi = f64::from(seg.master_end_s) + off;
        donor_t >= lo && donor_t < hi
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::correlation::Segment;

    fn ev(idx: u32, start: f64, end: f64, text: &str) -> SubtitleEvent {
        SubtitleEvent {
            index: idx,
            start_s: start,
            end_s: end,
            text: text.into(),
        }
    }

    fn map_with_segments(segs: Vec<Segment>) -> OffsetMap {
        let last_end = segs.last().map(|s| s.master_end_s).unwrap_or(0.0);
        OffsetMap {
            segments: segs,
            master_duration_s: last_end,
            anchor_count: 1,
            rejected_count: 0,
            suppressed_jumps: 0,
            valid_master_start_s: 0.0,
            valid_master_end_s: last_end,
        }
    }

    #[test]
    fn shifts_events_within_single_segment() {
        // Single segment 0..100s in master with offset 0 → events pass through unchanged.
        let map = map_with_segments(vec![Segment {
            master_start_s: 0.0,
            master_end_s: 100.0,
            donor_offset_s: 0.0,
        }]);
        let input = vec![ev(1, 10.0, 12.0, "hi"), ev(2, 50.0, 51.0, "world")];
        let out = apply_offset_map(&input, &map);
        assert_eq!(out.len(), 2);
        assert!((out[0].start_s - 10.0).abs() < 1e-9);
        assert!((out[0].end_s - 12.0).abs() < 1e-9);
        assert!((out[1].start_s - 50.0).abs() < 1e-9);
        // Indices renumbered sequentially.
        assert_eq!(out[0].index, 1);
        assert_eq!(out[1].index, 2);
    }

    #[test]
    fn shifts_events_per_segment_offset() {
        // Two-segment map: seg 0 [0..50, off=0], seg 1 [50..100, off=-1].
        // Donor coverage: seg 0 covers donor 0..50, seg 1 covers donor 49..99.
        // Event at donor t=10 → seg 0 → master t=10.
        // Event at donor t=80 → seg 1 (covers 49..99) → master = 80 - (-1) = 81.
        let map = map_with_segments(vec![
            Segment {
                master_start_s: 0.0,
                master_end_s: 50.0,
                donor_offset_s: 0.0,
            },
            Segment {
                master_start_s: 50.0,
                master_end_s: 100.0,
                donor_offset_s: -1.0,
            },
        ]);
        let input = vec![ev(1, 10.0, 12.0, "a"), ev(2, 80.0, 82.0, "b")];
        let out = apply_offset_map(&input, &map);
        assert_eq!(out.len(), 2);
        assert!((out[0].start_s - 10.0).abs() < 1e-9);
        assert!((out[1].start_s - 81.0).abs() < 1e-9);
        assert!((out[1].end_s - 83.0).abs() < 1e-9);
    }

    #[test]
    fn drops_events_outside_any_segment() {
        let map = map_with_segments(vec![Segment {
            master_start_s: 10.0,
            master_end_s: 50.0,
            donor_offset_s: 0.0,
        }]);
        // Donor coverage: 10..50. Events at t=5 and t=60 are outside.
        let input = vec![
            ev(1, 4.0, 6.0, "early"),
            ev(2, 20.0, 22.0, "middle"),
            ev(3, 58.0, 62.0, "late"),
        ];
        let out = apply_offset_map(&input, &map);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "middle");
        assert_eq!(out[0].index, 1); // renumbered
    }

    #[test]
    fn renumbers_after_drops() {
        let map = map_with_segments(vec![Segment {
            master_start_s: 0.0,
            master_end_s: 100.0,
            donor_offset_s: 0.0,
        }]);
        let input = vec![
            ev(1, 10.0, 11.0, "a"),
            ev(99, 200.0, 201.0, "out"), // dropped
            ev(7, 50.0, 51.0, "b"),
        ];
        let out = apply_offset_map(&input, &map);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].index, 1);
        assert_eq!(out[1].index, 2);
        // Sorted by time after renumbering.
        assert!(out[0].start_s < out[1].start_s);
    }
}
