use super::gcc_phat::{PhatEngine, PhatResult};
use rayon::prelude::*;
use rustfft::num_complex::Complex;
use serde::Serialize;
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AnchorPoint {
    pub master_time_s: f32,
    pub offset_s: f32,
    pub confidence: f32,
}

type FftBuffers = (Vec<Complex<f32>>, Vec<Complex<f32>>, Vec<Complex<f32>>);

thread_local! {
    static FFT_BUFS: RefCell<FftBuffers> =
        const { RefCell::new((Vec::new(), Vec::new(), Vec::new())) };
}

/// Sliding-window GCC-PHAT correlation between two mono anchor signals at the same sample rate.
///
/// Strategy: process in chunks of `chunk_size` windows. All windows in a chunk share a seed
/// offset (taken from the previous chunk's last high-confidence anchor); within a chunk we
/// dispatch via `rayon::par_iter`. This buys real parallelism on the FFT cost while keeping
/// the seed adaptive across the timeline so accumulated drift > max_drift_s still tracks.
///
/// `progress` is called with `fraction` in `[0.0, 1.0]` after each window. The callback
/// must be cheap (the inner loop calls it once per FFT window across all rayon threads).
pub fn correlate_sliding(
    master: &[f32],
    donor: &[f32],
    sample_rate: u32,
    window_s: f32,
    hop_s: f32,
    max_drift_s: f32,
    progress: &(dyn Fn(f32) + Sync),
) -> Vec<AnchorPoint> {
    let sr = sample_rate as f32;
    let window_samps = (window_s * sr).round() as usize;
    let drift_samps = (max_drift_s * sr).round() as usize;
    let donor_slice_samps = window_samps + 2 * drift_samps;

    if master.len() < window_samps {
        tracing::warn!("master shorter than correlation window; skipping correlation");
        progress(1.0);
        return Vec::new();
    }

    let hop_samps = (hop_s * sr).round() as usize;
    let total_windows = ((master.len() - window_samps) / hop_samps) + 1;

    let engine = Arc::new(PhatEngine::new(
        window_samps,
        donor_slice_samps,
        sample_rate,
    ));

    let chunk_size = rayon::current_num_threads().max(1) * 2;
    let mut anchors: Vec<AnchorPoint> = Vec::with_capacity(total_windows);
    let mut seed_offset_samps: i64 = 0;
    const MIN_CONF_FOR_SEED: f32 = 6.0;

    let done = AtomicUsize::new(0);
    let report = |done: &AtomicUsize| {
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        let frac = (n as f32 / total_windows as f32).min(1.0);
        progress(frac);
    };

    for chunk_start in (0..total_windows).step_by(chunk_size) {
        let chunk_end = (chunk_start + chunk_size).min(total_windows);

        let chunk: Vec<AnchorPoint> = (chunk_start..chunk_end)
            .into_par_iter()
            .map(|i| {
                let master_start = i * hop_samps;
                let master_slice = &master[master_start..master_start + window_samps];

                // Search range in donor: [master_start + seed - drift, ... + window + 2*drift].
                let target_start: i64 =
                    master_start as i64 + seed_offset_samps - drift_samps as i64;
                let donor_start = target_start.max(0) as usize;
                let donor_end = (donor_start + donor_slice_samps).min(donor.len());

                if donor_end <= donor_start + window_samps {
                    report(&done);
                    return AnchorPoint {
                        master_time_s: master_start as f32 / sr,
                        offset_s: seed_offset_samps as f32 / sr,
                        confidence: 0.0,
                    };
                }
                let donor_slice = &donor[donor_start..donor_end];

                let res = with_thread_buffers(|a, b, s| {
                    engine.locate(master_slice, donor_slice, a, b, s)
                });

                let anchor = anchor_from_phat(&res, master_start, donor_start, sr);
                report(&done);
                anchor
            })
            .collect();

        // Update seed from the last good anchor in this chunk so the next chunk's
        // search window centres on the actual offset rather than drifting away.
        if let Some(last_good) = chunk
            .iter()
            .rev()
            .find(|a| a.confidence >= MIN_CONF_FOR_SEED)
        {
            seed_offset_samps = (last_good.offset_s * sr).round() as i64;
        }

        anchors.extend(chunk);
    }

    progress(1.0);
    anchors
}

fn anchor_from_phat(
    res: &PhatResult,
    master_start: usize,
    donor_start: usize,
    sr: f32,
) -> AnchorPoint {
    // donor index of master content = donor_start + res.donor_offset_in_slice.
    // offset_s = donor_time - master_time at that anchor.
    let donor_index = donor_start + res.donor_offset_in_slice;
    let offset_samps = donor_index as i64 - master_start as i64;
    AnchorPoint {
        master_time_s: master_start as f32 / sr,
        offset_s: offset_samps as f32 / sr,
        confidence: res.confidence,
    }
}

fn with_thread_buffers<F, R>(f: F) -> R
where
    F: FnOnce(&mut Vec<Complex<f32>>, &mut Vec<Complex<f32>>, &mut Vec<Complex<f32>>) -> R,
{
    FFT_BUFS.with(|cell| {
        let mut bufs = cell.borrow_mut();
        let (a, b, s) = &mut *bufs;
        f(a, b, s)
    })
}
