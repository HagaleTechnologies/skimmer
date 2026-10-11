//! Narrowband carrier estimator: turns wideband IQ around one known,
//! nominal carrier offset into per-segment frequency estimates plus an
//! averaged-spectrum ambiguity figure.
//!
//! Pipeline: mix the nominal offset to DC with an f64 NCO, decimate with
//! the `decimate` module's halfband stages until the search window just
//! fits, then cut the decimated stream into back-to-back 1 s segments
//! (`round(fs_decimated)` samples, so bins are ~1 Hz apart), Hann-window
//! and FFT each one, and locate the strongest bin inside the search window
//! with the channelizer's dB-domain parabolic interpolator.
//!
//! Pure and streaming: no thresholds or policy beyond the two exported
//! constants (the caller decides what a usable segment is), and fully
//! deterministic -- sequential f64 accumulation, ascending scans with
//! strict `>` (ties go to the lowest bin), no hashing, no parallelism.

// MAN-127: frequency-calibration measurement core.

use crate::channelizer::interpolate_offset;
use crate::decimate::{design_halfband, HalfbandStage, HALFBAND_TAPS};
use coppa_dsp::fft::FftProcessor;
use num_complex::Complex32;
use std::f64::consts::PI;

/// Minimum distance (Hz) between the averaged spectrum's strongest bin and
/// the runner-up bin `ambiguity_margin_db` compares it against, so the
/// main lobe of the strongest carrier never counts as its own competitor.
pub const AMBIGUITY_EXCLUSION_HZ: f64 = 25.0;

/// Smallest accepted search-window half-width (Hz).
pub const MIN_HALF_WIDTH_HZ: f64 = 100.0;

/// A halfband stage is inserted only while the whole search window sits
/// inside `STAGE_PASSBAND_FRACTION * fs` of that stage's input rate --
/// below `decimate::CUTOFF_FRACTION` (0.235) and its transition band, so
/// the window stays in the flat passband and nothing aliases into it.
const STAGE_PASSBAND_FRACTION: f64 = 0.2;

/// One completed segment's estimate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegmentEstimate {
    /// Carrier offset (Hz) relative to the nominal offset passed to
    /// `CarrierEstimator::new`: a carrier exactly at the nominal offset
    /// reads ~0.0.
    pub offset_hz: f64,
    /// Peak bin power over the median bin power of the search window, dB.
    pub peak_over_floor_db: f64,
}

/// Streaming narrowband carrier estimator; see the module docs.
pub struct CarrierEstimator {
    /// NCO phase (radians, kept in `[-PI, PI)`) and per-sample step.
    phase: f64,
    phase_step: f64,
    stages: Vec<HalfbandStage>,
    fs_dec: f64,
    seg_len: usize,
    window: Vec<f32>,
    fft: FftProcessor,
    /// Search bins are the signed indices `-k_max..=k_max`.
    k_max: usize,
    /// Decimated samples not yet forming a complete segment.
    partial: Vec<Complex32>,
    segments: Vec<SegmentEstimate>,
    /// Sum over completed segments of each search bin's power; index `i`
    /// holds signed bin `i - k_max`.
    accum: Vec<f64>,
}

impl CarrierEstimator {
    /// `offset_hz`: nominal carrier offset from the source centre (Hz);
    /// `half_width_hz`: search window half-width (Hz).
    ///
    /// Errors when `fs` is not finite and positive, `offset_hz` is not
    /// finite, `half_width_hz` is not finite or below `MIN_HALF_WIDTH_HZ`,
    /// or the window `offset_hz ± half_width_hz` reaches past `±fs/2`.
    pub fn new(fs: f64, offset_hz: f64, half_width_hz: f64) -> Result<Self, String> {
        if !fs.is_finite() || fs <= 0.0 {
            return Err(format!("sample rate {fs} must be finite and positive"));
        }
        if !offset_hz.is_finite() {
            return Err(format!("carrier offset {offset_hz} must be finite"));
        }
        if !half_width_hz.is_finite() || half_width_hz < MIN_HALF_WIDTH_HZ {
            return Err(format!(
                "search half-width {half_width_hz} Hz must be finite and >= {MIN_HALF_WIDTH_HZ} Hz"
            ));
        }
        if offset_hz.abs() + half_width_hz > fs / 2.0 {
            return Err(format!(
                "search window {offset_hz} ± {half_width_hz} Hz exceeds the ±{} Hz passband",
                fs / 2.0
            ));
        }

        let taps = design_halfband(HALFBAND_TAPS);
        let mut stages = Vec::new();
        let mut fs_dec = fs;
        while STAGE_PASSBAND_FRACTION * fs_dec >= half_width_hz {
            stages.push(HalfbandStage::new(taps.clone()));
            fs_dec /= 2.0;
        }

        let seg_len = (fs_dec.round() as usize).max(3);
        let bin_hz = fs_dec / seg_len as f64;
        // Signed bins |k| <= (L-1)/2 are distinct modulo L.
        let k_max = ((half_width_hz / bin_hz).floor() as usize).min((seg_len - 1) / 2);
        let window = (0..seg_len)
            .map(|i| {
                let w = 0.5 * (1.0 - (2.0 * PI * i as f64 / (seg_len - 1) as f64).cos());
                w as f32
            })
            .collect();

        Ok(CarrierEstimator {
            phase: 0.0,
            phase_step: -2.0 * PI * offset_hz / fs,
            stages,
            fs_dec,
            seg_len,
            window,
            fft: FftProcessor::new(seg_len),
            k_max,
            partial: Vec::with_capacity(seg_len),
            segments: Vec::new(),
            accum: vec![0.0; 2 * k_max + 1],
        })
    }

    /// Feed IQ at the construction rate. Every segment that completes is
    /// estimated and appended to `segments()`.
    pub fn push(&mut self, iq: &[Complex32]) {
        let mut cur: Vec<Complex32> = Vec::with_capacity(iq.len());
        for &x in iq {
            let (s, c) = self.phase.sin_cos();
            let (re, im) = (x.re as f64, x.im as f64);
            cur.push(Complex32::new(
                (re * c - im * s) as f32,
                (re * s + im * c) as f32,
            ));
            self.phase += self.phase_step;
            if self.phase >= PI {
                self.phase -= 2.0 * PI;
            } else if self.phase < -PI {
                self.phase += 2.0 * PI;
            }
        }
        for stage in &mut self.stages {
            cur = stage.process(&cur);
        }
        self.partial.extend_from_slice(&cur);

        let mut start = 0;
        while self.partial.len() - start >= self.seg_len {
            let seg: Vec<Complex32> = self.partial[start..start + self.seg_len].to_vec();
            self.estimate_segment(&seg);
            start += self.seg_len;
        }
        self.partial.drain(..start);
    }

    /// Mark an input discontinuity (dropped or non-contiguous samples):
    /// discard the partial segment and clear every halfband stage's
    /// history, so no completed segment ever spans the gap.
    pub fn discontinuity(&mut self) {
        self.partial.clear();
        for stage in &mut self.stages {
            stage.reset();
        }
    }

    /// The decimated rate the segments are analysed at, Hz.
    pub fn fs_decimated(&self) -> f64 {
        self.fs_dec
    }

    /// One estimate per completed segment, in input order.
    pub fn segments(&self) -> &[SegmentEstimate] {
        &self.segments
    }

    /// Strongest averaged-spectrum bin over the strongest bin more than
    /// `AMBIGUITY_EXCLUSION_HZ` away from it, in dB (`+inf` if that
    /// runner-up has zero power); `None` before one segment completes.
    pub fn ambiguity_margin_db(&self) -> Option<f64> {
        let (peak, runner) = self.ambiguity_bins()?;
        Some(ratio_db(self.accum[peak], self.accum[runner]))
    }

    /// Offset (Hz, relative to the nominal offset, same frame as
    /// `SegmentEstimate::offset_hz`) of the runner-up bin
    /// `ambiguity_margin_db` compares against; `None` before one segment
    /// completes.
    pub fn ambiguity_runner_up_offset_hz(&self) -> Option<f64> {
        let (_, runner) = self.ambiguity_bins()?;
        Some((runner as f64 - self.k_max as f64) * self.bin_hz())
    }

    fn bin_hz(&self) -> f64 {
        self.fs_dec / self.seg_len as f64
    }

    /// Indices into `accum` of the strongest bin and of the strongest bin
    /// more than `AMBIGUITY_EXCLUSION_HZ` away from it.
    fn ambiguity_bins(&self) -> Option<(usize, usize)> {
        if self.segments.is_empty() {
            return None;
        }
        let peak = argmax(self.accum.iter().copied().enumerate())?;
        let bin_hz = self.bin_hz();
        let runner =
            argmax(self.accum.iter().copied().enumerate().filter(|&(i, _)| {
                (i as f64 - peak as f64).abs() * bin_hz > AMBIGUITY_EXCLUSION_HZ
            }))?;
        Some((peak, runner))
    }

    fn estimate_segment(&mut self, seg: &[Complex32]) {
        let buf: Vec<Complex32> = seg.iter().zip(&self.window).map(|(&x, &w)| x * w).collect();
        let spec = self.fft.forward(&buf);
        let l = self.seg_len as isize;
        let power = |k: isize| spec[k.rem_euclid(l) as usize].norm_sqr();

        let k_max = self.k_max as isize;
        let window_powers: Vec<f32> = (-k_max..=k_max).map(power).collect();
        let peak = argmax(window_powers.iter().map(|&p| p as f64).enumerate())
            .expect("search window is never empty");
        let k_peak = peak as isize - k_max;
        let delta =
            interpolate_offset(power(k_peak - 1), power(k_peak), power(k_peak + 1)).unwrap_or(0.0);

        let mut sorted: Vec<f64> = window_powers.iter().map(|&p| p as f64).collect();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];

        self.segments.push(SegmentEstimate {
            offset_hz: (k_peak as f64 + delta) * self.bin_hz(),
            peak_over_floor_db: ratio_db(window_powers[peak] as f64, median),
        });
        for (acc, &p) in self.accum.iter_mut().zip(&window_powers) {
            *acc += p as f64;
        }
    }
}

/// Index of the largest value: ascending scan, strict `>`, so ties keep the
/// lowest index.
fn argmax(values: impl Iterator<Item = (usize, f64)>) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, v) in values {
        match best {
            Some((_, b)) if v <= b => {}
            _ => best = Some((i, v)),
        }
    }
    best.map(|(i, _)| i)
}

/// `10·log10(num/den)`, with a zero denominator giving `+inf` (or 0 dB when
/// both are zero) instead of NaN.
fn ratio_db(num: f64, den: f64) -> f64 {
    if den > 0.0 {
        10.0 * (num / den).log10()
    } else if num > 0.0 {
        f64::INFINITY
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;
    const OFFSET: f64 = 1_500.0;
    const W: f64 = 500.0;
    /// The carrier's offset from `OFFSET`.
    const CARRIER_REL: f64 = -24.99994;

    fn tone(fs: f64, hz: f64, n: usize) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let cycles = (hz * i as f64 / fs).fract();
                let phi = 2.0 * PI * cycles;
                Complex32::new(phi.cos() as f32, phi.sin() as f32)
            })
            .collect()
    }

    /// `a += gain * b`, elementwise.
    fn add(a: &mut [Complex32], b: &[Complex32], gain: f32) {
        for (x, y) in a.iter_mut().zip(b) {
            *x += y * gain;
        }
    }

    /// Seeded complex Gaussian noise, per-component standard deviation
    /// `sigma` (xorshift64 + Box-Muller; the crate has no rand dependency).
    fn noise(n: usize, sigma: f64, seed: u64) -> Vec<Complex32> {
        let mut state = seed.max(1);
        let mut uniform = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // (0, 1]: never 0, so ln() is finite.
            ((state >> 11) as f64 + 1.0) / (1u64 << 53) as f64
        };
        (0..n)
            .map(|_| {
                let r = (-2.0 * uniform().ln()).sqrt() * sigma;
                let theta = 2.0 * PI * uniform();
                Complex32::new((r * theta.cos()) as f32, (r * theta.sin()) as f32)
            })
            .collect()
    }

    fn run(iq: &[Complex32]) -> CarrierEstimator {
        let mut est = CarrierEstimator::new(FS, OFFSET, W).unwrap();
        for chunk in iq.chunks(4_099) {
            est.push(chunk);
        }
        est
    }

    #[test]
    fn clean_tone_offsets_are_within_0_02_hz() {
        let est = run(&tone(FS, OFFSET + CARRIER_REL, 12 * FS as usize));
        assert_eq!(est.segments().len(), 12);
        for (i, s) in est.segments().iter().enumerate() {
            assert!(
                (s.offset_hz - CARRIER_REL).abs() < 0.02,
                "segment {i}: offset {} Hz",
                s.offset_hz
            );
            assert!(
                s.peak_over_floor_db > 30.0,
                "segment {i}: {} dB",
                s.peak_over_floor_db
            );
        }
    }

    #[test]
    fn decimation_rejects_a_strong_out_of_window_signal() {
        let n = 12 * FS as usize;
        let mut iq = tone(FS, OFFSET + CARRIER_REL, n);
        // +20 dB, 2 kHz beyond the window's upper edge.
        add(&mut iq, &tone(FS, OFFSET + W + 2_000.0, n), 10.0);
        let est = run(&iq);
        assert_eq!(est.segments().len(), 12);
        for (i, s) in est.segments().iter().enumerate() {
            assert!(
                (s.offset_hz - CARRIER_REL).abs() < 0.05,
                "segment {i}: offset {} Hz",
                s.offset_hz
            );
        }
    }

    #[test]
    fn noise_only_segments_stay_below_15_db() {
        let est = run(&noise(8 * FS as usize, 1.0, 0x5eed_0127));
        assert_eq!(est.segments().len(), 8);
        for (i, s) in est.segments().iter().enumerate() {
            assert!(
                s.peak_over_floor_db < 15.0,
                "segment {i}: {} dB",
                s.peak_over_floor_db
            );
        }
    }

    #[test]
    fn segment_count_matches_decimated_length() {
        let n = 3 * FS as usize + 12_345;
        let iq = noise(n, 1.0, 7);
        let mut est = CarrierEstimator::new(FS, OFFSET, W).unwrap();
        for chunk in iq.chunks(1_001) {
            est.push(chunk);
        }
        let n_stages = (FS / est.fs_decimated()).log2().round() as u32;
        assert_eq!(FS / 2f64.powi(n_stages as i32), est.fs_decimated());
        // Each stage keeps every other sample starting with the first.
        let mut decimated = n;
        for _ in 0..n_stages {
            decimated = decimated.div_ceil(2);
        }
        let seg_len = est.fs_decimated().round() as usize;
        assert_eq!(est.segments().len(), decimated / seg_len);
    }

    #[test]
    fn discontinuity_drops_the_partial_segment() {
        // Pre-gap: a +10 dB tone at -100 Hz; post-gap: a tone at +60 Hz. A
        // segment spanning the gap would hold 0.5 s of the stronger
        // pre-gap tone and peak there.
        let (pre_hz, post_hz) = (-100.0, 60.0);
        let mut pre = tone(FS, OFFSET + pre_hz, 3 * FS as usize / 2);
        for x in pre.iter_mut() {
            *x *= 10f32.powf(0.5);
        }
        let post = tone(FS, OFFSET + post_hz, 2 * FS as usize);

        let mut est = CarrierEstimator::new(FS, OFFSET, W).unwrap();
        est.push(&pre);
        assert_eq!(est.segments().len(), 1);
        est.discontinuity();
        est.push(&post);
        // 1 s complete before the gap (the 0.5 s partial is dropped), then
        // exactly two complete segments from the 2 s after it.
        let segs = est.segments();
        assert_eq!(segs.len(), 3);
        assert!((segs[0].offset_hz - pre_hz).abs() < 0.1, "{:?}", segs[0]);
        for s in &segs[1..] {
            assert!((s.offset_hz - post_hz).abs() < 0.1, "{s:?}");
        }
    }

    #[test]
    fn two_comparable_carriers_report_a_small_ambiguity_margin() {
        let n = 5 * FS as usize;
        let mut two = tone(FS, OFFSET + CARRIER_REL, n);
        let second_rel = CARRIER_REL + 180.0;
        add(
            &mut two,
            &tone(FS, OFFSET + second_rel, n),
            10f32.powf(-1.9 / 20.0),
        );
        let est = run(&two);
        let margin = est.ambiguity_margin_db().unwrap();
        assert!(margin < 6.0, "two carriers: margin {margin} dB");
        let runner = est.ambiguity_runner_up_offset_hz().unwrap();
        assert!((runner - second_rel).abs() <= 1.0, "runner-up {runner} Hz");

        // One carrier with AM sidebands 100 Hz either side at -12 dB.
        let mut am = tone(FS, OFFSET + CARRIER_REL, n);
        let sb = 10f32.powf(-12.0 / 20.0);
        add(&mut am, &tone(FS, OFFSET + CARRIER_REL - 100.0, n), sb);
        add(&mut am, &tone(FS, OFFSET + CARRIER_REL + 100.0, n), sb);
        let est = run(&am);
        let margin = est.ambiguity_margin_db().unwrap();
        assert!(margin > 6.0, "AM sidebands: margin {margin} dB");
    }

    #[test]
    fn identical_input_gives_bitwise_identical_results() {
        let n = 4 * FS as usize;
        let mut iq = tone(FS, OFFSET + CARRIER_REL, n);
        add(&mut iq, &noise(n, 0.3, 99), 1.0);
        let a = run(&iq);
        let b = run(&iq);
        assert_eq!(a.segments().len(), 4);
        assert_eq!(a.segments().len(), b.segments().len());
        for (x, y) in a.segments().iter().zip(b.segments()) {
            assert_eq!(x.offset_hz.to_bits(), y.offset_hz.to_bits());
            assert_eq!(
                x.peak_over_floor_db.to_bits(),
                y.peak_over_floor_db.to_bits()
            );
        }
        assert_eq!(
            a.ambiguity_margin_db().unwrap().to_bits(),
            b.ambiguity_margin_db().unwrap().to_bits()
        );
        assert_eq!(
            a.ambiguity_runner_up_offset_hz().unwrap().to_bits(),
            b.ambiguity_runner_up_offset_hz().unwrap().to_bits()
        );
    }

    #[test]
    fn new_rejects_bad_parameters() {
        assert!(CarrierEstimator::new(0.0, 0.0, W).is_err());
        assert!(CarrierEstimator::new(-FS, 0.0, W).is_err());
        assert!(CarrierEstimator::new(f64::NAN, 0.0, W).is_err());
        assert!(CarrierEstimator::new(f64::INFINITY, 0.0, W).is_err());
        assert!(CarrierEstimator::new(FS, f64::NAN, W).is_err());
        assert!(CarrierEstimator::new(FS, f64::INFINITY, W).is_err());
        assert!(CarrierEstimator::new(FS, OFFSET, 99.0).is_err());
        assert!(CarrierEstimator::new(FS, OFFSET, f64::NAN).is_err());
        assert!(CarrierEstimator::new(FS, 47_600.0, W).is_err());
        assert!(CarrierEstimator::new(FS, -47_600.0, W).is_err());
        // The window may touch ±fs/2 exactly, and W = MIN_HALF_WIDTH_HZ is
        // accepted.
        assert!(CarrierEstimator::new(FS, 47_500.0, W).is_ok());
        assert!(CarrierEstimator::new(FS, OFFSET, MIN_HALF_WIDTH_HZ).is_ok());
        // An empty estimator reports no ambiguity figures yet.
        let est = CarrierEstimator::new(FS, OFFSET, W).unwrap();
        assert_eq!(est.ambiguity_margin_db(), None);
        assert_eq!(est.ambiguity_runner_up_offset_hz(), None);
    }
}
