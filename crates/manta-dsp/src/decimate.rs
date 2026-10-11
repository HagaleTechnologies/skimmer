//! Power-of-two decimation stage between an IqSource and the channelizer
//! (issue #169): a cascade of halfband FIR decimate-by-2 stages, letting a
//! capture run at a narrower effective bandwidth than the SDR's native
//! rate while still landing on a channelizer-table-compatible rate
//! (`fs/93.75` a power of two, SPEC §1.1).
//!
//! Each stage's design cutoff is deliberately a bit below the theoretical
//! quarter-band point (see `CUTOFF_FRACTION`), reserving a transition-band
//! margin so full stopband attenuation is actually reached by the new
//! Nyquist rather than only somewhere past it. KNOWN LIMITATION (issue
//! #179): this margin means channels near the decimated Nyquist edge see
//! real, non-negligible attenuation (roughly -6 dB to -22 dB in the last
//! ~1.5 kHz below the edge) while still being exposed to the channelizer
//! as ordinary trackable channels -- a real CW signal landing there can
//! lose enough SNR to go undetected, or a partially-attenuated edge
//! channel can still show some spurious activity. The channelizer has no
//! concept of decimation and does not yet exclude or de-weight these
//! transition-band channels; see issue #179 for the follow-up. Without
//! the margin, a
//! signal just above the new Nyquist could alias into a false in-band
//! track/spot at the mirrored RF frequency.

use crate::proto::{bessel_i0, KAISER_BETA};
use num_complex::Complex32;

const CHANNEL_SPACING_HZ: f64 = 93.75; // SPEC §1.1, same constant as channelizer.rs/single.rs.

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Number of taps in each halfband decimate-by-2 stage. Odd length,
/// Kaiser-windowed at the channelizer prototype's 80 dB stopband target
/// (SPEC §1.2's `KAISER_BETA`) so the decimator isn't the weakest link in
/// the alias-rejection chain.
pub const HALFBAND_TAPS: usize = 127;

/// Design cutoff, as a fraction of `fs_in` (not `fs_in/4`). A Kaiser-
/// windowed FIR's transition band is *centered* on its nominal cutoff, with
/// full width approximately
///
/// ```text
/// Δf_norm ≈ (A - 8) / (2.285 * (N - 1) * 2π)
/// ```
///
/// (A = 80 dB stopband target, matching `KAISER_BETA`'s own derivation in
/// `proto.rs`; N = `HALFBAND_TAPS` = 127) which gives `Δf_norm ≈ 0.0398`.
/// Designing the ideal lowpass at exactly `fc_norm = 0.25` (the new Nyquist
/// after decimate-by-2) therefore leaves the transition band's stopband
/// edge at `0.25 + Δf_norm/2 ≈ 0.270`, not at `0.25` -- a signal just above
/// the new Nyquist is attenuated by only a few dB before decimation folds
/// it back in-band (reported against the channelizer's edge channels: a
/// 127-tap stage with cutoff at exactly fs_in/4 attenuates a tone 500 Hz
/// above the new Nyquist by only ~13 dB before it aliases to within the
/// passband). No tap count fixes this at a cutoff of exactly `fs_in/4`:
/// the transition band straddles the cutoff by construction, so more taps
/// only narrow it, never eliminate the portion past `0.25`.
///
/// The fix is to move the design cutoff *below* `fs_in/4` by roughly half
/// the transition width (`margin = Δf_norm / 2 ≈ 0.0199`, giving
/// `fc_norm ≈ 0.2301`), so the transition band's *upper* edge lands at
/// `fs_in/4` instead of straddling it.
///
/// The clean round value `0.23` (closest to the theoretical `0.2301`) was
/// tried first and rejected: it clears every unit-level stopband/passband
/// test here with a huge margin, but it also breaks
/// `manta-cli`'s `golden_decimated_capture` end-to-end test (192 kHz ->
/// 48 kHz decimate-then-decode of a clean +12.34 kHz-offset CW signal) --
/// bisecting confirmed a real cliff, not test flakiness: `0.233` and above
/// decode cleanly, `0.2325` down to `0.232` blow the test's <=25 Hz
/// frequency-error bound (the frequency estimator is itself sensitive to
/// how close the decimator's cutoff sits to the signal, a real, separate
/// effect from the alias-rejection margin this constant exists for), and
/// `0.23` fails outright (CER far above the 2% bound -- the golden
/// signal's own near-edge content is cut enough to corrupt decode, not
/// just shift the frequency estimate). `0.235` is used instead: comfortably
/// above the observed `0.232`-`0.2325` cliff (empirical safety margin, the
/// same workflow `HALFBAND_TAPS` itself went through), while still an
/// order of magnitude-plus improvement in near-Nyquist attenuation over
/// the old `0.25` design (see `halfband_stopband_near_true_nyquist_db` and
/// `false_track_scenario_tone_near_new_nyquist_is_suppressed`).
pub const CUTOFF_FRACTION: f64 = 0.235;

/// Design one halfband lowpass stage: ideal cutoff at `CUTOFF_FRACTION *
/// fs_in` (deliberately below the theoretical quarter-band point of
/// `fs_in/4`, the new Nyquist after decimate-by-2), Kaiser-windowed. See
/// `CUTOFF_FRACTION`'s doc comment for why: reserving this transition-band
/// margin is what guarantees full stopband attenuation is actually reached
/// by the new Nyquist, rather than only somewhere past it.
///
/// Moving the cutoff off exactly `fs_in/4` gives up the classic halfband
/// property (every even-offset tap except the center exactly zero, which
/// only holds for `fc_norm = 0.25` exactly -- `sinc(k/2) = 0` for even
/// nonzero `k`, but `sinc(0.47*k)` has no such exact zeros). That property
/// was already unexploited by `HalfbandStage::process` (see its doc
/// comment), so this costs no current runtime performance.
pub fn design_halfband(taps: usize) -> Vec<f32> {
    assert!(taps % 2 == 1, "halfband design requires odd length");
    let center = (taps - 1) as f64 / 2.0;
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut h = vec![0.0f64; taps];
    let mut sum = 0.0f64;
    for (i, tap) in h.iter_mut().enumerate() {
        let k = i as f64 - center;
        let ideal = 2.0 * CUTOFF_FRACTION * sinc(2.0 * CUTOFF_FRACTION * k);
        let t = 2.0 * i as f64 / (taps - 1) as f64 - 1.0;
        let w = bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / i0_beta;
        *tap = ideal * w;
        sum += *tap;
    }
    h.iter().map(|&v| (v / sum) as f32).collect()
}

/// One halfband decimate-by-2 stage: direct-form FIR, computed only at
/// kept (every-other) input instants -- never spends a multiply-
/// accumulate on a sample that will be dropped.
///
/// NOTE: this already did not skip the ~half of `taps` that used to be
/// structurally exactly zero (the classic halfband property) -- see
/// `docs/SPEC-decode-core.md` §1.5 and issue #176 for the unrealized 2x
/// MAC saving and the still-missing Pi4 CPU-budget bench for this stage.
/// As of the `CUTOFF_FRACTION` change (see its doc comment), the cutoff no
/// longer sits at exactly `fs_in/4`, so those taps are no longer exactly
/// zero either -- issue #176's skip opportunity no longer applies here.
pub(crate) struct HalfbandStage {
    taps: Vec<f32>,
    hist: std::collections::VecDeque<Complex32>,
    parity: u64,
}

impl HalfbandStage {
    pub(crate) fn new(taps: Vec<f32>) -> Self {
        let len = taps.len();
        HalfbandStage {
            taps,
            hist: std::collections::VecDeque::from(vec![Complex32::new(0.0, 0.0); len]),
            parity: 0,
        }
    }

    /// Clear the filter history and the decimation phase, returning the
    /// stage to its just-constructed state (used across an input
    /// discontinuity so pre-gap samples never leak into post-gap output).
    pub(crate) fn reset(&mut self) {
        for s in self.hist.iter_mut() {
            *s = Complex32::new(0.0, 0.0);
        }
        self.parity = 0;
    }

    pub(crate) fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let mut out = Vec::with_capacity(input.len() / 2 + 1);
        for &x in input {
            self.hist.pop_front();
            self.hist.push_back(x);
            let keep = self.parity.is_multiple_of(2);
            self.parity = self.parity.wrapping_add(1);
            if keep {
                // Sequential f64 accumulation (SPEC §6.4 determinism convention).
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (&h, &s) in self.taps.iter().zip(self.hist.iter()) {
                    re += h as f64 * s.re as f64;
                    im += h as f64 * s.im as f64;
                }
                out.push(Complex32::new(re as f32, im as f32));
            }
        }
        out
    }
}

/// Cascaded halfband decimator: `factor` (a power of two) stages of
/// decimate-by-2, front to back. SPEC-decode-core.md's decimation section.
pub struct Decimator {
    stages: Vec<HalfbandStage>,
    fs_out: f64,
}

impl Decimator {
    /// `factor` must be a power of two, and `fs_in / factor` must itself
    /// satisfy the channelizer's `fs/93.75` power-of-two table constraint
    /// (same validation shape as `Channelizer::new`/
    /// `SingleChannelExtractor::new`) -- a bad target rate fails here, at
    /// construction, not silently downstream. Also rejects a channel count
    /// `n < 4`: `Channelizer::new` sets `hop = n / 4`, which is exactly 0
    /// for `n` in `{1, 2}` (both of which are otherwise valid powers of
    /// two) -- `Channelizer::process`'s `self.read += self.hop` loop then
    /// never advances and never terminates, hanging the process on its
    /// first call. `n == 4` gives `hop == 1`, which advances fine, so `n >=
    /// 4` is the exact safe floor, not merely a conservative guess.
    pub fn new(fs_in: f64, factor: usize) -> Result<Self, String> {
        if factor == 0 || !factor.is_power_of_two() {
            return Err(format!("decimation factor {factor} must be a power of two"));
        }
        let fs_out = fs_in / factor as f64;
        let nf = fs_out / CHANNEL_SPACING_HZ;
        let n = nf.round() as usize;
        if (nf - n as f64).abs() > 1e-9 || !n.is_power_of_two() || n < 4 {
            return Err(format!(
                "unsupported target rate {fs_out}: fs/93.75 must be a power of two >= 4 \
                 (a channelizer with fewer than 4 channels has hop=0 and hangs)"
            ));
        }
        let n_stages = factor.trailing_zeros() as usize;
        let taps = design_halfband(HALFBAND_TAPS);
        let stages = (0..n_stages)
            .map(|_| HalfbandStage::new(taps.clone()))
            .collect();
        Ok(Decimator { stages, fs_out })
    }

    /// The decimated output rate, Hz.
    pub fn fs_out(&self) -> f64 {
        self.fs_out
    }

    /// Feed input IQ at fs_in; returns however many decimated samples
    /// became available (possibly 0, if too few input samples have
    /// accumulated through every cascade stage so far).
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<Complex32> {
        let mut cur = iq.to_vec();
        for stage in &mut self.stages {
            cur = stage.process(&cur);
        }
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// |H(f)| in dB at frequency f (Hz), direct DTFT -- same helper shape
    /// as proto.rs's `response_db` (test-only, exact not FFT-approximated).
    fn response_db(h: &[f32], f_hz: f64, fs: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &tap) in h.iter().enumerate() {
            let phi = -2.0 * std::f64::consts::PI * f_hz * i as f64 / fs;
            re += tap as f64 * phi.cos();
            im += tap as f64 * phi.sin();
        }
        10.0 * (re * re + im * im).log10()
    }

    #[test]
    fn halfband_is_symmetric_and_unity_dc() {
        let h = design_halfband(HALFBAND_TAPS);
        assert_eq!(h.len(), HALFBAND_TAPS);
        for i in 0..h.len() / 2 {
            assert_eq!(h[i], h[h.len() - 1 - i], "tap {i} asymmetric");
        }
        let sum: f64 = h.iter().map(|&x| x as f64).sum();
        assert!((sum - 1.0).abs() < 1e-6, "DC gain {sum}");
    }

    // NOTE: there used to be a `halfband_even_offset_taps_are_zero` test
    // here, asserting the classic halfband property (every even-offset tap
    // from the center exactly zero). That property is unique to a design
    // cutoff of exactly `fc_norm = 0.25` (`sinc(k/2) = 0` for even nonzero
    // `k`); it no longer holds now that `CUTOFF_FRACTION` (0.235) moves the
    // cutoff off `0.25` by design, to reserve a transition-band margin
    // (see `CUTOFF_FRACTION`'s doc comment). Deleted rather than reworded
    // to "merely small": at the new cutoff there's no particular reason
    // for taps near the old zero-crossing offsets to be small in any
    // testable sense, so a reworded version wouldn't assert anything
    // meaningful about the filter design.

    #[test]
    fn halfband_stopband_at_least_78_db() {
        // fs = 192 kHz -> new Nyquist (cutoff) at 48 kHz. Check well past
        // the cutoff, out to the original Nyquist (96 kHz).
        let fs = 192_000.0;
        let h = design_halfband(HALFBAND_TAPS);
        let mut worst = -300.0f64;
        let mut f = 60_000.0; // guard band above the 48 kHz cutoff
        while f < 96_000.0 {
            worst = worst.max(response_db(&h, f, fs));
            f += 500.0;
        }
        assert!(
            worst <= -78.0,
            "worst stopband {worst} dB (raise HALFBAND_TAPS if this fails)"
        );
    }

    #[test]
    fn halfband_stopband_near_true_nyquist_db() {
        // Regression for the Codex P1 finding on decimate.rs:39: with the
        // old fc_norm = 0.25 design, a tone at fs_in/4 + 500 Hz (24.5 kHz
        // at 192 kHz input) was attenuated by only ~13 dB before folding
        // in-band. Unlike `halfband_stopband_at_least_78_db` (which only
        // checks a 12 kHz guard band above the cutoff, i.e. exactly the
        // gap Codex found), this sweeps right from the true output
        // Nyquist boundary (fs_in/4 = 48 kHz) out a couple of kHz, where
        // the old design's attenuation was weakest.
        let fs = 192_000.0;
        let h = design_halfband(HALFBAND_TAPS);
        let true_nyquist = fs / 4.0;
        let mut worst = -300.0f64;
        let mut f = true_nyquist;
        while f <= true_nyquist + 2_000.0 {
            worst = worst.max(response_db(&h, f, fs));
            f += 50.0;
        }
        // Kaiser windowing's own finite steepness means the exact boundary
        // point can't hit the full 78 dB stopband target -- and
        // CUTOFF_FRACTION is deliberately not pushed as low as the raw
        // margin formula would allow (see its doc comment: 0.23 breaks the
        // golden_decimated_capture end-to-end decode test). Measured worst
        // case in this sweep at CUTOFF_FRACTION = 0.235 is ~-39 dB -- still
        // a huge, real margin over the ~13 dB (-8.9 dB at this exact sweep
        // start) Codex reported for the old fc_norm = 0.25 design.
        assert!(
            worst <= -30.0,
            "worst response near true Nyquist {worst} dB (want a large margin over the reported ~13 dB)"
        );
    }

    #[test]
    fn halfband_passband_is_flat_near_dc() {
        let fs = 192_000.0;
        let h = design_halfband(HALFBAND_TAPS);
        // Well inside the new (0.235 * fs = 45.12 kHz) cutoff; DC-normalized
        // gain should sit close to 0 dB.
        let gain_db = response_db(&h, 5_000.0, fs);
        assert!(gain_db.abs() < 0.5, "passband gain {gain_db} dB");
    }

    #[test]
    fn halfband_passband_is_flat_near_new_cutoff_edge() {
        // Confirms the passband stays flat well inside the new, slightly
        // narrower cutoff (CUTOFF_FRACTION * fs = 45.12 kHz at 192 kHz),
        // not just far below it near DC.
        let fs = 192_000.0;
        let h = design_halfband(HALFBAND_TAPS);
        let gain_db = response_db(&h, 40_000.0, fs);
        assert!(gain_db.abs() < 0.5, "passband gain {gain_db} dB");
    }

    fn tone(freq: f64, n: usize, amp: f32, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn rejects_non_power_of_two_factor() {
        assert!(Decimator::new(192_000.0, 3).is_err());
        assert!(Decimator::new(192_000.0, 4).is_ok());
    }

    #[test]
    fn rejects_target_rate_failing_channelizer_table_constraint() {
        // fs_in=100_000, factor=2 (a power of two) -> fs_out=50_000, whose
        // fs/93.75 = 533.33.. is not an integer, let alone a power of two.
        assert!(Decimator::new(100_000.0, 2).is_err());
    }

    #[test]
    fn rejects_degenerate_channel_count_that_would_hang_the_channelizer() {
        // fs_in=48_000, factor=256 (a power of two) -> fs_out=187.5, whose
        // fs/93.75 = 2 IS a power of two, so the old check alone would
        // accept this -- but Channelizer::new's hop = n/4 = 0 for n=2,
        // which hangs Channelizer::process's read-advancing loop forever.
        // Must be rejected at construction, not hang the process later.
        assert!(Decimator::new(48_000.0, 256).is_err());
        // n=1 (fs_out=93.75) is equally degenerate.
        assert!(Decimator::new(375.0, 4).is_err());
        // n=4 (fs_out=375, hop=1) is the smallest table size that is safe.
        assert!(Decimator::new(1_500.0, 4).is_ok());
    }

    #[test]
    fn fs_out_matches_factor() {
        let d = Decimator::new(192_000.0, 4).unwrap();
        assert_eq!(d.fs_out(), 48_000.0);
    }

    #[test]
    fn dc_tone_passes_through_decimation_at_near_unity() {
        let fs_in = 192_000.0;
        let mut d = Decimator::new(fs_in, 4).unwrap();
        // A tone well inside the final 48 kHz/2=24 kHz Nyquist.
        let iq = tone(2_000.0, 20_000, 1.0, fs_in);
        let out = d.process(&iq);
        // Skip transient: two cascaded HALFBAND_TAPS-length filters need
        // roughly HALFBAND_TAPS output samples (post-decimation) to settle.
        let warmup = HALFBAND_TAPS;
        assert!(out.len() > warmup + 50, "only {} output samples", out.len());
        for s in &out[warmup..] {
            assert!((s.norm() - 1.0).abs() < 0.1, "mag {}", s.norm());
        }
    }

    #[test]
    fn tone_above_new_nyquist_is_rejected() {
        let fs_in = 192_000.0;
        let mut d = Decimator::new(fs_in, 4).unwrap();
        // 30 kHz is above the final 24 kHz Nyquist -- must be suppressed,
        // not aliased through as a spurious in-band tone.
        let iq = tone(30_000.0, 20_000, 1.0, fs_in);
        let out = d.process(&iq);
        let warmup = HALFBAND_TAPS;
        for s in &out[warmup..] {
            assert!(s.norm() < 0.05, "aliased mag {}", s.norm());
        }
    }

    #[test]
    fn false_track_scenario_tone_near_new_nyquist_is_suppressed() {
        // Direct regression for Codex's exact reported scenario on
        // decimate.rs:39: "when decimating 192 kS/s to 48 kS/s, a signal
        // immediately above the requested +/-24 kHz band reaches a final
        // stage whose cutoff is exactly 24 kHz; this 127-tap window
        // attenuates a 24.5 kHz tone by only about 13 dB before
        // downsampling folds it to -23.5 kHz" -- which could produce a
        // false in-band track/spot at the mirrored RF frequency.
        //
        // Every cascade stage uses the same normalized design, so this
        // generalizes to any fs_in/factor combination; isolating a single
        // stage via Decimator::new(96_000.0, 2) (fs_in/4 = 24 kHz, same
        // relative offset Codex described) tests the same filter margin
        // more cleanly than reproducing the full 192k->48k cascade.
        let fs_in = 96_000.0;
        let mut d = Decimator::new(fs_in, 2).unwrap();
        assert_eq!(d.fs_out(), 48_000.0);
        // fs_in/4 + 500 Hz, matching Codex's "24.5 kHz" offset above the
        // 24 kHz cutoff.
        let iq = tone(24_500.0, 20_000, 1.0, fs_in);
        let out = d.process(&iq);
        let warmup = HALFBAND_TAPS;
        assert!(out.len() > warmup + 50, "only {} output samples", out.len());
        for s in &out[warmup..] {
            // Genuinely low, not just "improved from 13 dB" -- roughly
            // matching (tighter than) `tone_above_new_nyquist_is_rejected`'s
            // 0.05 bound for a tone well clear of the transition band.
            assert!(s.norm() < 0.01, "aliased mag {} (want << 1.0)", s.norm());
        }
    }

    #[test]
    fn is_deterministic() {
        let fs_in = 192_000.0;
        let iq = tone(2_000.0, 40_000, 1.0, fs_in);
        let mut d_a = Decimator::new(fs_in, 4).unwrap();
        let mut d_b = Decimator::new(fs_in, 4).unwrap();
        let out_a = d_a.process(&iq);
        let out_b = d_b.process(&iq);
        assert_eq!(out_a.len(), out_b.len());
        for (a, b) in out_a.iter().zip(out_b.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }

    #[test]
    fn process_across_multiple_calls_matches_one_call() {
        let fs_in = 192_000.0;
        let iq = tone(2_000.0, 20_000, 1.0, fs_in);
        let mut whole = Decimator::new(fs_in, 4).unwrap();
        let out_whole = whole.process(&iq);

        let mut chunked = Decimator::new(fs_in, 4).unwrap();
        let mut out_chunked = Vec::new();
        for chunk in iq.chunks(137) {
            out_chunked.extend(chunked.process(chunk));
        }
        assert_eq!(out_whole.len(), out_chunked.len());
        for (a, b) in out_whole.iter().zip(out_chunked.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }
}
