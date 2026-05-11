use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Treat cross-spectrum bins below this magnitude as zero — avoids dividing by
/// near-zero noise when a frequency bin is silent in both channels (which would
/// otherwise inject random phase into the whitened spectrum).
const PHAT_DENOM_EPS: f32 = 1e-12;

/// PSR guard width: samples on each side of the peak that are excluded from the
/// noise-floor mean when computing confidence. PHAT peaks are very narrow but can
/// have small adjacent sidelobes from spectral leakage — excluding them gives a
/// confidence value that reflects the *true* peak-vs-noise contrast.
const PSR_GUARD_S: f32 = 0.020;

#[derive(Debug, Clone, Copy)]
pub struct PhatResult {
    /// Index into the donor slice where the best match for the master slice begins.
    /// Range: [0, donor_slice.len() - master_slice.len()].
    pub donor_offset_in_slice: usize,
    /// Peak-to-sidelobe ratio of the GCC-PHAT response (peak / mean-abs of the
    /// noise floor, with a 20 ms guard region around the peak excluded). Higher
    /// = sharper match.
    pub confidence: f32,
}

/// Reusable forward+inverse FFT pair sized for one (master, donor) length pair.
/// Build once; share `Arc<PhatEngine>` across rayon workers (rustfft's `Fft` is `Sync`).
pub struct PhatEngine {
    n: usize,
    sample_rate: u32,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
}

impl PhatEngine {
    pub fn new(master_len: usize, donor_len: usize, sample_rate: u32) -> Self {
        // Linear (non-circular) cross-correlation needs N >= master_len + donor_len - 1.
        let need = master_len + donor_len;
        let n = need.next_power_of_two();
        let mut planner = FftPlanner::<f32>::new();
        Self {
            n,
            sample_rate,
            fft: planner.plan_fft_forward(n),
            ifft: planner.plan_fft_inverse(n),
        }
    }

    /// Locate `master` inside `donor`. `donor.len()` must be `>= master.len()`.
    /// Returns the donor-relative starting index of the best match.
    ///
    /// `buf_a`, `buf_b`, and `scratch` are caller-owned per-thread buffers
    /// sized to `self.n()`, `self.n()`, and the FFT scratch length respectively.
    pub fn locate(
        &self,
        master: &[f32],
        donor: &[f32],
        buf_a: &mut Vec<Complex<f32>>,
        buf_b: &mut Vec<Complex<f32>>,
        scratch: &mut Vec<Complex<f32>>,
    ) -> PhatResult {
        debug_assert!(donor.len() >= master.len());
        debug_assert!(master.len() + donor.len() <= self.n);

        ensure_len(buf_a, self.n);
        ensure_len(buf_b, self.n);
        ensure_len(
            scratch,
            self.fft
                .get_inplace_scratch_len()
                .max(self.ifft.get_inplace_scratch_len()),
        );

        // Load `donor` into buf_a, `master` into buf_b, zero-pad both.
        // We compute r = IFFT( FFT(donor) * conj(FFT(master)) ), so r[k] is large
        // when donor[i+k] aligns with master[i] — i.e. master starts at donor index k.
        for (i, x) in buf_a.iter_mut().enumerate() {
            *x = if i < donor.len() {
                Complex::new(donor[i], 0.0)
            } else {
                Complex::new(0.0, 0.0)
            };
        }
        for (i, x) in buf_b.iter_mut().enumerate() {
            *x = if i < master.len() {
                Complex::new(master[i], 0.0)
            } else {
                Complex::new(0.0, 0.0)
            };
        }

        self.fft.process_with_scratch(buf_a, scratch);
        self.fft.process_with_scratch(buf_b, scratch);

        // Cross-spectrum with phase whitening: R[f] = A[f] * conj(B[f]) / |...|.
        for i in 0..self.n {
            let prod = buf_a[i] * buf_b[i].conj();
            let mag = prod.norm();
            buf_a[i] = if mag > PHAT_DENOM_EPS {
                prod / mag
            } else {
                Complex::new(0.0, 0.0)
            };
        }

        self.ifft.process_with_scratch(buf_a, scratch);

        // Search range: master must fit entirely inside donor, so valid k ∈ [0, donor.len() - master.len()].
        let max_k = donor.len() - master.len();
        // Pass 1: locate the peak.
        let mut peak_idx = 0usize;
        let mut peak_val = buf_a[0].re;
        for (k, sample) in buf_a.iter().take(max_k + 1).enumerate() {
            if sample.re > peak_val {
                peak_val = sample.re;
                peak_idx = k;
            }
        }
        // Pass 2: noise-floor mean-abs, excluding ±guard around the peak so the peak
        // and its immediate sidelobes don't bias the noise estimate. This is the
        // standard radar/DSP "peak-to-sidelobe ratio" formulation.
        let guard = (PSR_GUARD_S * self.sample_rate as f32).round() as usize;
        let lo = peak_idx.saturating_sub(guard);
        let hi = (peak_idx + guard).min(max_k);
        let (mut sum_abs, mut count) = (0.0_f64, 0usize);
        for (k, sample) in buf_a.iter().take(max_k + 1).enumerate() {
            if k >= lo && k <= hi {
                continue;
            }
            sum_abs += sample.re.abs() as f64;
            count += 1;
        }
        let mean_abs = (sum_abs / count.max(1) as f64).max(PHAT_DENOM_EPS as f64) as f32;
        let confidence = peak_val / mean_abs;

        PhatResult {
            donor_offset_in_slice: peak_idx,
            confidence,
        }
    }
}

fn ensure_len(v: &mut Vec<Complex<f32>>, n: usize) {
    if v.len() < n {
        v.resize(n, Complex::new(0.0, 0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic delay recovery: bury a chirp inside silence and assert the lag.
    #[test]
    fn recovers_known_lag() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};

        let master_len = 4096;
        let true_lag = 1234;
        let donor_len = master_len + 2 * 2048;
        let sample_rate = 16_000u32;

        let mut master = Vec::with_capacity(master_len);
        for i in 0..master_len {
            let t = i as f32 / master_len as f32;
            master.push((2.0 * std::f32::consts::PI * (50.0 + 200.0 * t) * t).sin());
        }

        let mut donor = vec![0.0_f32; donor_len];
        donor[true_lag..true_lag + master_len].copy_from_slice(&master);
        // Add some unrelated noise everywhere so the PHAT response isn't trivially
        // perfect. Seeded so the test is deterministic.
        let mut rng = StdRng::seed_from_u64(0xdead_beef);
        for s in donor.iter_mut() {
            *s += rng.gen_range(-0.025_f32..0.025);
        }

        let engine = PhatEngine::new(master.len(), donor.len(), sample_rate);
        let mut a = Vec::new();
        let mut b = Vec::new();
        let mut s = Vec::new();
        let res = engine.locate(&master, &donor, &mut a, &mut b, &mut s);
        assert_eq!(
            res.donor_offset_in_slice, true_lag,
            "lag mismatch (conf {})",
            res.confidence
        );
        assert!(
            res.confidence > 5.0,
            "confidence too low: {}",
            res.confidence
        );
    }
}
