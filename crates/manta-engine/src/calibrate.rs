//! Frequency calibration: measure a receiver's ppm error against a carrier
//! whose frequency is known exactly (a time-signal station or an NCDXF
//! beacon), and report the `freq_correction_ppm` that cancels it.
//!
//! Same shape as `doctor`: open a source, read a bounded window, report.
//! The narrowband estimation itself is `manta_dsp::carrier`; this module
//! owns the reference catalogue, the passband planning rule, the bounded
//! read loop and the gates that turn per-segment estimates into a verdict.
//! Deterministic for file input: no parallelism, sequential arithmetic,
//! sorts by `f64::total_cmp`.

// MAN-127. Constants, catalogue and planning rule are recorded in
// docs/DECISIONS/2026-10-11-man127-calibrate-command.md.

use anyhow::{anyhow, bail, Result};
use manta_dsp::carrier::{CarrierEstimator, MIN_HALF_WIDTH_HZ};
use manta_input::IqSource;
use num_complex::Complex32;
use std::time::{Duration, Instant};

/// Shortest accepted measurement.
pub const MIN_DURATION: Duration = Duration::from_secs(10);
/// Longest accepted measurement.
pub const MAX_DURATION: Duration = Duration::from_secs(3600);
/// Default measurement length.
pub const DEFAULT_DURATION: Duration = Duration::from_secs(60);
/// Default search half-width, parts per million of the reference.
pub const DEFAULT_SEARCH_PPM: f64 = 50.0;
/// Accepted `search_ppm` range.
pub const MIN_SEARCH_PPM: f64 = 1.0;
pub const MAX_SEARCH_PPM: f64 = 1000.0;
/// A segment counts as hearing the carrier when its strongest bin is this
/// far over the window's median bin. Noise alone reaches about 10 dB.
pub const MIN_SEGMENT_PEAK_OVER_FLOOR_DB: f64 = 15.0;
/// Valid segments needed before a reference can be measured.
pub const MIN_VALID_SEGMENTS: usize = 10;
/// Below this averaged-spectrum margin the reference is ambiguous.
pub const MIN_AMBIGUITY_MARGIN_DB: f64 = 6.0;
/// Fraction of the delivered passband trimmed from each edge.
pub const PASSBAND_EDGE_FRACTION: f64 = 0.05;
/// Distance kept from an IQ receiver's centre (its DC spur), Hz.
pub const DC_GUARD_HZ: f64 = 25.0;
/// Spread (Hz) above which the caller should warn that the carrier wandered.
pub const SPREAD_WARN_HZ: f64 = 5.0;
/// At most this many references are measured in one run.
pub const MAX_REFERENCES: usize = 3;
/// Wall-clock grace past `duration` before a stalled live source is abandoned.
const STALL_GRACE: Duration = Duration::from_secs(10);
const READ_CHUNK: usize = 8192;

/// What kind of station a reference is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    TimeStandard,
    Beacon,
    Operator,
}

/// A carrier whose frequency is known exactly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reference {
    pub hz: f64,
    pub kind: ReferenceKind,
    pub label: &'static str,
}

const fn time_standard(hz: f64, label: &'static str) -> Reference {
    Reference {
        hz,
        kind: ReferenceKind::TimeStandard,
        label,
    }
}

const fn beacon(hz: f64) -> Reference {
    Reference {
        hz,
        kind: ReferenceKind::Beacon,
        label: "NCDXF beacon",
    }
}

const WWV_WWVH_BPM: &str = "time standard (WWV, WWVH, BPM)";
const WWV: &str = "time standard (WWV)";
const RWM: &str = "time standard (RWM)";
const OPERATOR_LABEL: &str = "the carrier named by --reference-hz";

/// The built-in reference catalogue: time standards first, then the NCDXF
/// beacon frequencies. CHU is absent: it left the air on 2026-06-22.
pub const REFERENCES: &[Reference] = &[
    time_standard(2_500_000.0, WWV_WWVH_BPM),
    time_standard(5_000_000.0, WWV_WWVH_BPM),
    time_standard(10_000_000.0, WWV_WWVH_BPM),
    time_standard(15_000_000.0, WWV_WWVH_BPM),
    time_standard(20_000_000.0, WWV),
    time_standard(25_000_000.0, WWV),
    time_standard(4_996_000.0, RWM),
    time_standard(9_996_000.0, RWM),
    time_standard(14_996_000.0, RWM),
    beacon(14_100_000.0),
    beacon(18_110_000.0),
    beacon(21_150_000.0),
    beacon(24_930_000.0),
    beacon(28_200_000.0),
];

/// Measurement options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CalibrateOptions {
    pub duration: Duration,
    pub search_ppm: f64,
    /// Measure only this carrier, instead of the catalogue.
    pub reference_hz: Option<f64>,
}

impl Default for CalibrateOptions {
    fn default() -> Self {
        CalibrateOptions {
            duration: DEFAULT_DURATION,
            search_ppm: DEFAULT_SEARCH_PPM,
            reference_hz: None,
        }
    }
}

/// A reference chosen for this receiver, with its search half-width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlannedReference {
    pub reference: Reference,
    pub half_width_hz: f64,
}

/// Rejects a `--duration` outside `MIN_DURATION..=MAX_DURATION`.
pub fn check_duration(duration: Duration) -> Result<()> {
    if duration < MIN_DURATION || duration > MAX_DURATION {
        bail!(
            "--duration must be between {} and {} seconds, got {}",
            MIN_DURATION.as_secs(),
            MAX_DURATION.as_secs(),
            duration.as_secs()
        );
    }
    Ok(())
}

fn check_options(opts: &CalibrateOptions) -> Result<()> {
    if !opts.search_ppm.is_finite() || !(MIN_SEARCH_PPM..=MAX_SEARCH_PPM).contains(&opts.search_ppm)
    {
        bail!(
            "--search-ppm must be between {MIN_SEARCH_PPM} and {MAX_SEARCH_PPM}, got {}",
            opts.search_ppm
        );
    }
    if let Some(hz) = opts.reference_hz {
        if !hz.is_finite() || hz <= 0.0 {
            bail!("--reference-hz must be a positive frequency in Hz, got {hz}");
        }
    }
    Ok(())
}

fn require_absolute_centre(center_hz: f64) -> Result<()> {
    if !center_hz.is_finite() || center_hz <= 0.0 {
        bail!(
            "calibrate needs the receiver's absolute frequency, but the source reports a \
             centre of {center_hz} Hz; pass --dial-freq-hz, or give the recording its \
             centre-frequency sidecar"
        );
    }
    Ok(())
}

/// The search half-width `search_ppm` asks for around `ref_hz`, capped so
/// that no carrier found inside it (allowing half a ~1 Hz bin of
/// interpolation past the edge) needs a correction beyond the +/-1000 ppm
/// `freq_correction_ppm` accepts: a carrier `d` Hz low needs
/// `(r / (r - d) - 1) * 1e6` ppm, which reaches 1000 at `d = r * (1 - 1/1.001)`.
fn ppm_half_width(search_ppm: f64, ref_hz: f64) -> f64 {
    let max_low_hz = ref_hz * (1.0 - 1.0 / (1.0 + MAX_SEARCH_PPM * 1e-6)) - 1.0;
    (search_ppm * ref_hz * 1e-6).min(max_low_hz)
}

/// The half-width the receiver's passband geometry allows for `ref_hz`:
/// inside the inner passband and, for an IQ receiver, away from its centre.
fn geometric_half_width(center_hz: f64, passband: (f64, f64), ref_hz: f64) -> f64 {
    let (lo, hi) = passband;
    let edge = PASSBAND_EDGE_FRACTION * (hi - lo);
    let (lo_in, hi_in) = (lo + edge, hi - edge);
    let off = ref_hz - center_hz;
    let mut w = (off - lo_in).min(hi_in - off);
    if lo < 0.0 && hi > 0.0 {
        w = w.min(off.abs() - DC_GUARD_HZ);
    }
    w
}

/// Search half-width for `reference` on a receiver centred at `center_hz`
/// delivering `passband` (offsets from the centre), or `None` if unusable.
fn half_width(center_hz: f64, passband: (f64, f64), search_ppm: f64, ref_hz: f64) -> Option<f64> {
    let w =
        ppm_half_width(search_ppm, ref_hz).min(geometric_half_width(center_hz, passband, ref_hz));
    (w >= MIN_HALF_WIDTH_HZ).then_some(w)
}

/// The `--search-ppm` that gives `ref_hz` the minimum search half-width.
fn min_search_ppm_for(ref_hz: f64) -> f64 {
    (MIN_HALF_WIDTH_HZ / ref_hz * 1e6).ceil()
}

/// Suggested centre for `--tune-hz` that puts `reference` inside a
/// receiver's window with room for its search half-width.
fn suggested_tune_hz(reference: &Reference, search_ppm: f64) -> f64 {
    let w = search_ppm * reference.hz * 1e-6;
    reference.hz - ((w + 1000.0) / 500.0).ceil() * 500.0
}

/// Choose the references a receiver centred at `center_hz` can measure:
/// time standards before beacons, each by descending frequency, at most
/// `MAX_REFERENCES`. With `opts.reference_hz` only that carrier is tried.
/// With none usable, the error suggests a retune.
pub fn plan_references(
    center_hz: f64,
    passband: (f64, f64),
    is_audio: bool,
    opts: &CalibrateOptions,
) -> Result<Vec<PlannedReference>> {
    check_options(opts)?;
    require_absolute_centre(center_hz)?;
    let (lo, hi) = passband;
    let window = format!("{:.1} to {:.1} Hz", center_hz + lo, center_hz + hi);
    if let Some(hz) = opts.reference_hz {
        let reference = Reference {
            hz,
            kind: ReferenceKind::Operator,
            label: OPERATOR_LABEL,
        };
        return match half_width(center_hz, passband, opts.search_ppm, hz) {
            Some(half_width_hz) => Ok(vec![PlannedReference {
                reference,
                half_width_hz,
            }]),
            None if geometric_half_width(center_hz, passband, hz) >= MIN_HALF_WIDTH_HZ => bail!(
                "at --search-ppm {} the search window around --reference-hz {hz:.1} is only \
                 ±{:.1} Hz, under the {MIN_HALF_WIDTH_HZ:.0} Hz minimum; raise --search-ppm to \
                 at least {}",
                opts.search_ppm,
                ppm_half_width(opts.search_ppm, hz),
                min_search_ppm_for(hz)
            ),
            None => bail!(
                "--reference-hz {hz:.1} is not usable in this receiver's passband ({window}); \
                 it must sit inside it, at least {MIN_HALF_WIDTH_HZ:.0} Hz from its edges{}",
                if lo < 0.0 && hi > 0.0 {
                    " and from its centre"
                } else {
                    ""
                }
            ),
        };
    }
    let mut planned: Vec<PlannedReference> = REFERENCES
        .iter()
        .filter_map(|r| {
            half_width(center_hz, passband, opts.search_ppm, r.hz).map(|w| PlannedReference {
                reference: *r,
                half_width_hz: w,
            })
        })
        .collect();
    planned.sort_by(|a, b| {
        let rank = |k: ReferenceKind| match k {
            ReferenceKind::TimeStandard => 0,
            ReferenceKind::Beacon => 1,
            ReferenceKind::Operator => 2,
        };
        rank(a.reference.kind)
            .cmp(&rank(b.reference.kind))
            .then(b.reference.hz.total_cmp(&a.reference.hz))
    });
    planned.truncate(MAX_REFERENCES);
    if planned.is_empty() {
        // References the passband holds but the search window is too narrow
        // for: the fix is --search-ppm, not a retune.
        if let Some(r) = REFERENCES
            .iter()
            .find(|r| geometric_half_width(center_hz, passband, r.hz) >= MIN_HALF_WIDTH_HZ)
        {
            bail!(
                "at --search-ppm {} the search window around {:.1} Hz, {}, is only ±{:.1} Hz, \
                 under the {MIN_HALF_WIDTH_HZ:.0} Hz minimum; raise --search-ppm to at least {}",
                opts.search_ppm,
                r.hz,
                r.label,
                ppm_half_width(opts.search_ppm, r.hz),
                min_search_ppm_for(r.hz)
            );
        }
        bail!(
            "no known reference frequency is usable in this receiver's passband ({window}); {}",
            retune_hint(center_hz, is_audio, opts.search_ppm)
        );
    }
    Ok(planned)
}

fn retune_hint(center_hz: f64, is_audio: bool, search_ppm: f64) -> String {
    let nearest = REFERENCES
        .iter()
        .filter(|r| r.kind == ReferenceKind::TimeStandard)
        .min_by(|a, b| {
            (a.hz - center_hz)
                .abs()
                .total_cmp(&(b.hz - center_hz).abs())
        })
        .copied()
        .unwrap_or(REFERENCES[2]);
    let ten_mhz = REFERENCES[2];
    let mut picks = vec![nearest];
    if nearest.hz != ten_mhz.hz {
        picks.push(ten_mhz);
    }
    let parts: Vec<String> = picks
        .iter()
        .map(|r| {
            if is_audio {
                format!(
                    "--dial-freq-hz {:.0} with the radio tuned there for the {:.1} Hz {}",
                    r.hz - 1650.0,
                    r.hz,
                    r.label
                )
            } else {
                format!(
                    "--tune-hz {:.0} for the {:.1} Hz {}",
                    suggested_tune_hz(r, search_ppm),
                    r.hz,
                    r.label
                )
            }
        })
        .collect();
    if is_audio {
        format!(
            "tune the radio near a reference for the measurement, e.g. {}, or name a carrier \
             you know with --reference-hz",
            parts.join(" or ")
        )
    } else {
        format!(
            "retune it for the measurement, e.g. {}, or name a carrier you know with \
             --reference-hz",
            parts.join(" or ")
        )
    }
}

/// `freq_correction_ppm` that maps `measured_hz` back onto `reference_hz`:
/// the exact inverse of the reported-frequency multiplier `1 + ppm * 1e-6`.
pub fn correction_ppm(reference_hz: f64, measured_hz: f64) -> f64 {
    (reference_hz / measured_hz - 1.0) * 1e6
}

/// Outcome for one reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceStatus {
    Measured,
    TooFewSegments,
    Ambiguous,
}

/// One reference's measurement.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ReferenceResult {
    pub reference_hz: f64,
    pub label: &'static str,
    pub kind: ReferenceKind,
    pub search_half_width_hz: f64,
    pub status: ReferenceStatus,
    pub segments_total: usize,
    pub segments_valid: usize,
    pub measured_hz: Option<f64>,
    pub error_hz: Option<f64>,
    pub freq_correction_ppm: Option<f64>,
    pub uncertainty_ppm: Option<f64>,
    pub peak_over_floor_db: Option<f64>,
    pub spread_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambiguity_margin_db: Option<f64>,
    /// Offsets (Hz, from the reference) of the two competing carriers when
    /// the reference is ambiguous: the main one and the runner-up.
    #[serde(skip)]
    pub peak_offset_hz: Option<f64>,
    #[serde(skip)]
    pub runner_up_offset_hz: Option<f64>,
}

/// Everything a calibration run measured.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CalibrationReport {
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    /// Source time actually consumed, seconds (shorter than requested when
    /// a recording ends first).
    pub duration_s: f64,
    pub search_ppm: f64,
    pub references: Vec<ReferenceResult>,
}

impl CalibrationReport {
    /// The first reference, in planned order, that was measured.
    pub fn result(&self) -> Option<&ReferenceResult> {
        self.references
            .iter()
            .find(|r| r.status == ReferenceStatus::Measured)
    }
}

fn median_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        0.5 * (sorted[n / 2 - 1] + sorted[n / 2])
    }
}

fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    median_sorted(&v)
}

/// Read up to `opts.duration` of `src` and measure every planned reference.
pub fn calibrate(
    mut src: Box<dyn IqSource>,
    planned: &[PlannedReference],
    opts: &CalibrateOptions,
) -> Result<CalibrationReport> {
    check_duration(opts.duration)?;
    check_options(opts)?;
    let fs = src.sample_rate();
    let center = src.center_freq_hz();
    require_absolute_centre(center)?;
    if planned.is_empty() {
        bail!("no reference to measure");
    }
    let mut estimators = planned
        .iter()
        .map(|p| CarrierEstimator::new(fs, p.reference.hz - center, p.half_width_hz))
        .collect::<Result<Vec<_>, String>>()
        .map_err(|e| anyhow!("cannot measure this reference on this source: {e}"))?;

    let max_samples = (fs * opts.duration.as_secs_f64()).round().max(0.0) as u64;
    let deadline = Instant::now() + opts.duration + STALL_GRACE;
    let mut buf = vec![Complex32::new(0.0, 0.0); READ_CHUNK];
    let mut consumed: u64 = 0;
    while consumed < max_samples {
        // Saturate rather than truncate, as doctor's SampleBoundedSource does.
        let remaining = usize::try_from(max_samples - consumed).unwrap_or(usize::MAX);
        let cap = remaining.min(buf.len());
        let n = src.read(&mut buf[..cap])?;
        if n == 0 {
            break;
        }
        if src.take_discontinuity().is_some() {
            for e in &mut estimators {
                e.discontinuity();
            }
        }
        for e in &mut estimators {
            e.push(&buf[..n]);
        }
        consumed += n as u64;
        if Instant::now() > deadline {
            break;
        }
    }

    let mut references = Vec::with_capacity(planned.len());
    for (p, est) in planned.iter().zip(&estimators) {
        references.push(evaluate(p, est)?);
    }
    Ok(CalibrationReport {
        sample_rate_hz: fs,
        center_freq_hz: center,
        duration_s: consumed as f64 / fs,
        search_ppm: opts.search_ppm,
        references,
    })
}

fn evaluate(p: &PlannedReference, est: &CarrierEstimator) -> Result<ReferenceResult> {
    let segs = est.segments();
    let valid: Vec<_> = segs
        .iter()
        .filter(|s| s.peak_over_floor_db >= MIN_SEGMENT_PEAK_OVER_FLOOR_DB)
        .collect();
    let mut out = ReferenceResult {
        reference_hz: p.reference.hz,
        label: p.reference.label,
        kind: p.reference.kind,
        search_half_width_hz: p.half_width_hz,
        status: ReferenceStatus::TooFewSegments,
        segments_total: segs.len(),
        segments_valid: valid.len(),
        measured_hz: None,
        error_hz: None,
        freq_correction_ppm: None,
        uncertainty_ppm: None,
        peak_over_floor_db: None,
        spread_hz: None,
        ambiguity_margin_db: None,
        peak_offset_hz: None,
        runner_up_offset_hz: None,
    };
    if valid.len() < MIN_VALID_SEGMENTS {
        return Ok(out);
    }
    let offsets: Vec<f64> = valid.iter().map(|s| s.offset_hz).collect();
    let offset = median(&offsets);
    let margin = est.ambiguity_margin_db();
    out.ambiguity_margin_db = margin;
    if margin.is_none_or(|m| m < MIN_AMBIGUITY_MARGIN_DB) {
        out.status = ReferenceStatus::Ambiguous;
        out.peak_offset_hz = Some(offset);
        out.runner_up_offset_hz = est.ambiguity_runner_up_offset_hz();
        return Ok(out);
    }
    let deviations: Vec<f64> = offsets.iter().map(|o| (o - offset).abs()).collect();
    let spread = 1.4826 * median(&deviations);
    let measured = p.reference.hz + offset;
    let ppm = correction_ppm(p.reference.hz, measured);
    manta_spot::calibration_factor_from_ppm(ppm).map_err(|e| {
        anyhow!(
            "the {:.1} Hz reference measured {ppm:.2} ppm, outside what freq_correction_ppm \
             accepts: {e}",
            p.reference.hz
        )
    })?;
    let floors: Vec<f64> = valid.iter().map(|s| s.peak_over_floor_db).collect();
    out.status = ReferenceStatus::Measured;
    out.measured_hz = Some(measured);
    out.error_hz = Some(offset);
    out.freq_correction_ppm = Some(ppm);
    out.uncertainty_ppm =
        Some(1.2533 * spread / (valid.len() as f64).sqrt() / p.reference.hz * 1e6);
    out.peak_over_floor_db = Some(median(&floors));
    out.spread_hz = Some(spread);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// In-memory source, like doctor's `RawIqSource`, plus an optional
    /// injected gap: the read that starts at `gap_at` reports a
    /// discontinuity.
    struct VecSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        centre: f64,
        passband: (f64, f64),
        gap_at: Option<usize>,
        pending_gap: bool,
    }

    impl IqSource for VecSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.centre
        }
        fn rf_passband_hz(&self) -> (f64, f64) {
            self.passband
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let mut n = buf.len().min(self.samples.len() - self.cursor);
            if let Some(g) = self.gap_at {
                if self.cursor < g {
                    n = n.min(g - self.cursor);
                } else if self.cursor == g {
                    self.pending_gap = true;
                    self.gap_at = None;
                }
            }
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            std::mem::take(&mut self.pending_gap).then_some(4800)
        }
    }

    const FS: f64 = 48_000.0;
    const CENTRE: f64 = 9_998_500.0;

    fn source(samples: Vec<Complex32>, centre: f64) -> Box<dyn IqSource> {
        Box::new(VecSource {
            samples,
            cursor: 0,
            fs: FS,
            centre,
            passband: (-FS / 2.0, FS / 2.0),
            gap_at: None,
            pending_gap: false,
        })
    }

    /// Seeded xorshift64 + Box-Muller complex Gaussian noise, sigma per axis.
    fn noise(n: usize, sigma: f64, seed: u64) -> Vec<Complex32> {
        let mut s = seed.max(1);
        let mut uniform = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        (0..n)
            .map(|_| {
                let r = (-2.0 * uniform().ln()).sqrt() * sigma;
                let t = 2.0 * PI * uniform();
                Complex32::new((r * t.cos()) as f32, (r * t.sin()) as f32)
            })
            .collect()
    }

    /// Carrier at absolute `hz` seen by a receiver centred at `centre`,
    /// with amplitude envelope `env(t)`.
    fn carrier(hz: f64, centre: f64, secs: f64, env: impl Fn(f64) -> f64) -> Vec<Complex32> {
        let n = (secs * FS) as usize;
        let f = hz - centre;
        (0..n)
            .map(|i| {
                let t = i as f64 / FS;
                let a = env(t);
                let phi = 2.0 * PI * f * t;
                Complex32::new((a * phi.cos()) as f32, (a * phi.sin()) as f32)
            })
            .collect()
    }

    fn add(a: &mut [Complex32], b: &[Complex32]) {
        for (x, y) in a.iter_mut().zip(b) {
            *x += *y;
        }
    }

    /// Noise at SNR +10 dB in 2500 Hz for a unit carrier.
    fn with_noise(mut s: Vec<Complex32>, seed: u64) -> Vec<Complex32> {
        let sigma = (0.1 * FS / 2500.0 / 2.0f64).sqrt();
        let n = noise(s.len(), sigma, seed);
        add(&mut s, &n);
        s
    }

    fn opts(secs: u64) -> CalibrateOptions {
        CalibrateOptions {
            duration: Duration::from_secs(secs),
            ..CalibrateOptions::default()
        }
    }

    fn only(hz: f64, kind: ReferenceKind) -> Vec<PlannedReference> {
        let reference = REFERENCES
            .iter()
            .find(|r| r.hz == hz)
            .copied()
            .unwrap_or(Reference {
                hz,
                kind,
                label: OPERATOR_LABEL,
            });
        vec![PlannedReference {
            reference,
            half_width_hz: 500.0,
        }]
    }

    const PPM_2_5_HZ: f64 = 10e6 / (1.0 + 2.5e-6);

    #[test]
    fn correction_ppm_is_the_inverse_of_the_correction_factor() {
        assert!((correction_ppm(1e7, 1e7 / (1.0 + 2.5e-6)) - 2.5).abs() < 1e-9);
        assert!((correction_ppm(14e6, 14e6 + 20.0) - (-1.4286)).abs() < 1e-3);
        for (r, m) in [
            (1e7, 1e7 + 14.2),
            (14.1e6, 14.1e6 - 3.3),
            (2.5e6, 2.5e6 + 0.7),
        ] {
            let back = (1.0 + correction_ppm(r, m) * 1e-6) * m;
            assert!((back - r).abs() < 1e-6, "{r} {m} {back}");
        }
    }

    #[test]
    fn catalogue_has_no_chu_and_orders_time_standards_first() {
        for chu in [3_330_000.0, 7_850_000.0, 14_670_000.0] {
            assert!(REFERENCES.iter().all(|r| (r.hz - chu).abs() > 1.0));
        }
        let first_beacon = REFERENCES
            .iter()
            .position(|r| r.kind == ReferenceKind::Beacon)
            .unwrap();
        assert!(REFERENCES[..first_beacon]
            .iter()
            .all(|r| r.kind == ReferenceKind::TimeStandard));
        assert!(REFERENCES[first_beacon..]
            .iter()
            .all(|r| r.kind == ReferenceKind::Beacon));
        assert_eq!(REFERENCES.len(), 14);
    }

    #[test]
    fn plan_picks_ncdxf_in_a_20m_passband() {
        let p = plan_references(14_050_000.0, (-96_000.0, 96_000.0), false, &opts(60)).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].reference.hz, 14_100_000.0);
        assert_eq!(p[0].reference.kind, ReferenceKind::Beacon);
        assert!((p[0].half_width_hz - 705.0).abs() < 1e-6);
    }

    #[test]
    fn plan_excludes_a_reference_at_the_centre() {
        // A KiwiSDR window centred on 10 MHz: the 10 MHz carrier sits on
        // the DC spur and is excluded; RWM's 9.996 MHz is still inside.
        let p = plan_references(10_000_000.0, (-5000.0, 5000.0), false, &opts(60)).unwrap();
        assert!(p.iter().all(|r| r.reference.hz != 10_000_000.0));
        assert_eq!(p[0].reference.hz, 9_996_000.0);
        // A narrower window holds only the centre carrier: refused, with a
        // retune suggestion for the 10 MHz time signal.
        let e = plan_references(10_000_000.0, (-3000.0, 3000.0), false, &opts(60))
            .unwrap_err()
            .to_string();
        assert!(e.contains("--tune-hz 9998500"), "{e}");
    }

    #[test]
    fn plan_orders_and_caps_a_wide_passband() {
        let p =
            plan_references(15_000_000.0, (-1_200_000.0, 1_200_000.0), false, &opts(60)).unwrap();
        let hz: Vec<f64> = p.iter().map(|r| r.reference.hz).collect();
        assert_eq!(hz, vec![14_996_000.0, 14_100_000.0]);
        // A passband holding many references keeps at most MAX_REFERENCES,
        // time standards first by descending frequency.
        let p = plan_references(10_000_000.0, (-8e6, 8e6), false, &opts(60)).unwrap();
        let hz: Vec<f64> = p.iter().map(|r| r.reference.hz).collect();
        assert_eq!(hz, vec![15_000_000.0, 14_996_000.0, 9_996_000.0]);
    }

    #[test]
    fn plan_clips_w_to_an_audio_passband() {
        let p = plan_references(28_200_000.0 - 1650.0, (300.0, 3000.0), true, &opts(60)).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].reference.hz, 28_200_000.0);
        assert!(
            (p[0].half_width_hz - 1215.0).abs() < 1e-6,
            "{}",
            p[0].half_width_hz
        );
    }

    #[test]
    fn plan_uses_only_reference_hz_when_given() {
        let o = CalibrateOptions {
            reference_hz: Some(10_001_000.0),
            ..opts(60)
        };
        let p = plan_references(CENTRE, (-24_000.0, 24_000.0), false, &o).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].reference.hz, 10_001_000.0);
        assert_eq!(p[0].reference.kind, ReferenceKind::Operator);
    }

    #[test]
    fn plan_rejects_non_positive_or_nan_reference_hz() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let o = CalibrateOptions {
                reference_hz: Some(bad),
                ..opts(60)
            };
            assert!(plan_references(CENTRE, (-24_000.0, 24_000.0), false, &o).is_err());
        }
    }

    #[test]
    fn plan_names_search_ppm_when_the_window_is_too_narrow() {
        let o = CalibrateOptions {
            reference_hz: Some(1_000_000.0),
            ..opts(60)
        };
        let e = plan_references(1_001_500.0, (-24_000.0, 24_000.0), false, &o)
            .unwrap_err()
            .to_string();
        assert!(e.contains("raise --search-ppm to at least 100"), "{e}");
        let o = CalibrateOptions {
            search_ppm: 1.0,
            ..opts(60)
        };
        let e = plan_references(CENTRE, (-24_000.0, 24_000.0), false, &o)
            .unwrap_err()
            .to_string();
        assert!(e.contains("raise --search-ppm to at least 10"), "{e}");
    }

    #[test]
    fn search_window_never_allows_a_correction_beyond_1000_ppm() {
        for hz in [200_000.0, 2_500_000.0, 10e6, 28.2e6] {
            let w = ppm_half_width(MAX_SEARCH_PPM, hz);
            // Half a bin of interpolation past the low edge.
            let ppm = correction_ppm(hz, hz - w - 0.5);
            assert!(
                manta_spot::calibration_factor_from_ppm(ppm).is_ok(),
                "{hz} {ppm}"
            );
            assert!((ppm_half_width(50.0, hz) - 50e-6 * hz).abs() < 1e-9);
        }
    }

    #[test]
    fn plan_rejects_search_ppm_out_of_range() {
        for bad in [0.5, 1000.5, f64::NAN, -50.0] {
            let o = CalibrateOptions {
                search_ppm: bad,
                ..opts(60)
            };
            let e = plan_references(CENTRE, (-24_000.0, 24_000.0), false, &o).unwrap_err();
            assert!(e.to_string().contains("--search-ppm"), "{e}");
        }
    }

    #[test]
    fn calibrate_rejects_duration_out_of_range() {
        for secs in [5, 9, 3601] {
            let e = calibrate(
                source(vec![], CENTRE),
                &only(10e6, ReferenceKind::TimeStandard),
                &opts(secs),
            )
            .unwrap_err();
            assert!(e.to_string().contains("--duration"), "{e}");
        }
        assert!(check_duration(Duration::from_secs(10)).is_ok());
        assert!(check_duration(Duration::from_secs(3600)).is_ok());
    }

    #[test]
    fn measures_a_2_5_ppm_receiver() {
        let s = with_noise(carrier(PPM_2_5_HZ, CENTRE, 12.0, |_| 1.0), 1);
        let planned = plan_references(CENTRE, (-FS / 2.0, FS / 2.0), false, &opts(12)).unwrap();
        assert_eq!(planned[0].reference.hz, 10e6);
        let report = calibrate(source(s, CENTRE), &planned, &opts(12)).unwrap();
        let r = &report.references[0];
        assert_eq!(r.status, ReferenceStatus::Measured);
        let ppm = r.freq_correction_ppm.unwrap();
        assert!((ppm - 2.5).abs() < 0.02, "{ppm}");
        assert_eq!(report.result(), Some(r));
    }

    #[test]
    fn measures_through_am_modulation() {
        let s = with_noise(
            carrier(PPM_2_5_HZ, CENTRE, 12.0, |t| {
                1.0 + 0.5 * (2.0 * PI * 500.0 * t).cos() + 0.5 * (2.0 * PI * 100.0 * t).cos()
            }),
            2,
        );
        let report = calibrate(
            source(s, CENTRE),
            &only(10e6, ReferenceKind::TimeStandard),
            &opts(12),
        )
        .unwrap();
        let r = report.result().expect("measured");
        assert!((r.freq_correction_ppm.unwrap() - 2.5).abs() < 0.02);
    }

    #[test]
    fn measures_a_keyed_beacon_with_quiet_slots() {
        // 10 s slots: audible, silent, audible; inside an audible slot the
        // carrier is keyed with 50 ms dits.
        let beacon_hz = 14_100_000.0 / (1.0 + 1.1e-6);
        let centre = 14_098_500.0;
        let s = with_noise(
            carrier(beacon_hz, centre, 30.0, |t| {
                let slot_on = ((t / 10.0) as u64).is_multiple_of(2);
                let dit_on = ((t / 0.05) as u64).is_multiple_of(2);
                if slot_on && (t % 10.0 > 5.0 || dit_on) {
                    1.0
                } else {
                    0.0
                }
            }),
            3,
        );
        let report = calibrate(
            source(s, centre),
            &only(14_100_000.0, ReferenceKind::Beacon),
            &opts(30),
        )
        .unwrap();
        let r = &report.references[0];
        assert_eq!(r.status, ReferenceStatus::Measured, "{r:?}");
        assert!(r.segments_valid < r.segments_total, "{r:?}");
        assert!((r.freq_correction_ppm.unwrap() - 1.1).abs() < 0.02, "{r:?}");
    }

    #[test]
    fn noise_only_is_too_few_segments() {
        let s = noise((12.0 * FS) as usize, 1.0, 4);
        let report = calibrate(
            source(s, CENTRE),
            &only(10e6, ReferenceKind::TimeStandard),
            &opts(12),
        )
        .unwrap();
        let r = &report.references[0];
        assert_eq!(r.status, ReferenceStatus::TooFewSegments);
        assert!(r.segments_valid < MIN_VALID_SEGMENTS);
        assert!(r.measured_hz.is_none() && r.ambiguity_margin_db.is_none());
        assert_eq!(report.result(), None);
    }

    #[test]
    fn two_carriers_are_ambiguous() {
        let mut s = carrier(PPM_2_5_HZ, CENTRE, 12.0, |_| 1.0);
        let other = carrier(PPM_2_5_HZ + 180.0, CENTRE, 12.0, |_| {
            10f64.powf(-1.9 / 20.0)
        });
        add(&mut s, &other);
        let report = calibrate(
            source(with_noise(s, 5), CENTRE),
            &only(10e6, ReferenceKind::TimeStandard),
            &opts(12),
        )
        .unwrap();
        let r = &report.references[0];
        assert_eq!(r.status, ReferenceStatus::Ambiguous, "{r:?}");
        assert!(r.ambiguity_margin_db.unwrap() < MIN_AMBIGUITY_MARGIN_DB);
        assert!(r.runner_up_offset_hz.is_some());
        assert_eq!(report.result(), None);
    }

    #[test]
    fn falls_back_to_the_next_reference() {
        let s = with_noise(
            carrier(9_996_000.0 / (1.0 + 2.5e-6), CENTRE, 12.0, |_| 1.0),
            6,
        );
        let planned = plan_references(CENTRE, (-FS / 2.0, FS / 2.0), false, &opts(12)).unwrap();
        let hz: Vec<f64> = planned.iter().map(|r| r.reference.hz).collect();
        assert_eq!(hz, vec![10_000_000.0, 9_996_000.0]);
        let report = calibrate(source(s, CENTRE), &planned, &opts(12)).unwrap();
        assert_eq!(report.references[0].status, ReferenceStatus::TooFewSegments);
        let r = report.result().expect("measured");
        assert_eq!(r.reference_hz, 9_996_000.0);
        assert!((r.freq_correction_ppm.unwrap() - 2.5).abs() < 0.02);
    }

    #[test]
    fn discontinuity_discards_the_straddling_segment() {
        let s = with_noise(carrier(PPM_2_5_HZ, CENTRE, 12.0, |_| 1.0), 7);
        let plain = calibrate(
            source(s.clone(), CENTRE),
            &only(10e6, ReferenceKind::TimeStandard),
            &opts(12),
        )
        .unwrap();
        let gapped = calibrate(
            Box::new(VecSource {
                samples: s,
                cursor: 0,
                fs: FS,
                centre: CENTRE,
                passband: (-FS / 2.0, FS / 2.0),
                gap_at: Some((5.5 * FS) as usize),
                pending_gap: false,
            }),
            &only(10e6, ReferenceKind::TimeStandard),
            &opts(12),
        )
        .unwrap();
        assert_eq!(plain.references[0].segments_total, 12);
        // 5 whole segments before the gap (the half segment is dropped),
        // 6 after it.
        assert_eq!(gapped.references[0].segments_total, 11);
        assert_eq!(gapped.references[0].status, ReferenceStatus::Measured);
    }

    #[test]
    fn short_file_reports_observed_duration() {
        let s = with_noise(carrier(PPM_2_5_HZ, CENTRE, 12.0, |_| 1.0), 8);
        let t0 = Instant::now();
        let report = calibrate(
            source(s, CENTRE),
            &only(10e6, ReferenceKind::TimeStandard),
            &opts(60),
        )
        .unwrap();
        assert!(
            (report.duration_s - 12.0).abs() < 1e-9,
            "{}",
            report.duration_s
        );
        assert!(t0.elapsed() < Duration::from_secs(60));
    }

    #[test]
    fn report_is_deterministic() {
        let run = || {
            let s = with_noise(carrier(PPM_2_5_HZ, CENTRE, 12.0, |_| 1.0), 9);
            let planned = plan_references(CENTRE, (-FS / 2.0, FS / 2.0), false, &opts(12)).unwrap();
            calibrate(source(s, CENTRE), &planned, &opts(12)).unwrap()
        };
        let (a, b) = (run(), run());
        // Debug prints every f64 in its shortest round-trip form, so equal
        // text means bit-identical values.
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert_eq!(a, b);
    }

    #[test]
    fn zero_centre_is_rejected() {
        let e = calibrate(
            source(vec![Complex32::new(0.0, 0.0); 1000], 0.0),
            &only(10e6, ReferenceKind::TimeStandard),
            &opts(12),
        )
        .unwrap_err();
        assert!(e.to_string().contains("absolute frequency"), "{e}");
        let e = plan_references(0.0, (-24_000.0, 24_000.0), false, &opts(12)).unwrap_err();
        assert!(e.to_string().contains("absolute frequency"), "{e}");
    }
}
