//! Synthetic recordings for `manta bench sensitivity` (MAN-116): ten looped CQ
//! stations per recording at one condition/speed/SNR, rendered once at a
//! reference SNR and scaled per sweep point. See
//! docs/DECISIONS/2026-10-10-man116-sensitivity-benchmark.md.
//!
//! Pure and decode-free: everything here is scene construction, so any crate
//! that links the testkit can use it.

use crate::scene::{render_scene, SignalSpec, WattersonFade};
use anyhow::Result;
use coppa_channel::watterson::WattersonPreset;
use num_complex::Complex32;

/// Recording sample rate.
pub const SAMPLE_RATE_HZ: f64 = 96_000.0;
/// Recording centre frequency; station frequencies are this plus their offset.
pub const CENTER_FREQ_HZ: f64 = 14_000_000.0;
/// Stations per recording (D2).
pub const STATIONS_PER_RECORDING: usize = 10;
/// Offset of station 0 from the centre.
pub const FIRST_OFFSET_HZ: f64 = -36_000.0;
/// 8 kHz plus a tenth of a 93.75 Hz channel: ten different in-channel positions.
pub const OFFSET_STEP_HZ: f64 = 8_009.375;
/// Every station is rendered at this SNR (2500 Hz) and scaled per point (D4).
pub const REFERENCE_SNR_2500_DB: f32 = 0.0;
/// What each station keys, looped for the whole recording.
pub const PAYLOAD_TEMPLATE: &str = "CQ CQ DE <call> <call> K";
/// Machine keying: standard weight, gaps and edges, no jitter.
pub const WEIGHT: f32 = 3.0;
pub const CHAR_GAP_UNITS: f32 = 3.0;
pub const WORD_GAP_UNITS: f32 = 7.0;
pub const RISE_MS: f64 = 5.0;

/// A channel condition to sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Condition {
    /// A steady signal in white noise.
    Awgn,
    /// Two-path Watterson fading, coppa's `Good` preset (0.5 ms, 0.1 Hz).
    WattersonGood,
    /// Two-path Watterson fading, coppa's `Poor` preset (2 ms, 1 Hz).
    WattersonPoor,
}

impl Condition {
    /// Canonical sweep order.
    pub const ALL: [Condition; 3] = [
        Condition::Awgn,
        Condition::WattersonGood,
        Condition::WattersonPoor,
    ];

    /// The name `--conditions` takes and JSON carries.
    pub fn name(self) -> &'static str {
        match self {
            Condition::Awgn => "awgn",
            Condition::WattersonGood => "good",
            Condition::WattersonPoor => "poor",
        }
    }

    /// The label tables print.
    pub fn label(self) -> &'static str {
        match self {
            Condition::Awgn => "AWGN",
            Condition::WattersonGood => "Watterson good",
            Condition::WattersonPoor => "Watterson poor",
        }
    }

    /// The coppa fading preset, `None` for AWGN.
    pub fn preset(self) -> Option<WattersonPreset> {
        match self {
            Condition::Awgn => None,
            Condition::WattersonGood => Some(WattersonPreset::Good),
            Condition::WattersonPoor => Some(WattersonPreset::Poor),
        }
    }
}

impl std::str::FromStr for Condition {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Condition::ALL
            .into_iter()
            .find(|c| c.name() == s)
            .ok_or_else(|| format!("unknown condition `{s}` (awgn, good or poor)"))
    }
}

/// SplitMix64: golden-ratio increment, then its finalizer (wrapping ops).
fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// D5: every noise and fading seed comes from (seed, trial, stream) only,
/// never from SNR, speed or condition. Pinned by `seed_derivation_is_pinned`.
pub fn derive_seed(seed: u64, trial: u32, stream: u64) -> u64 {
    mix(mix(mix(seed) ^ trial as u64) ^ stream)
}

/// The trial's noise seed (stream 0).
pub fn noise_seed(seed: u64, trial: u32) -> u64 {
    derive_seed(seed, trial, 0)
}

/// Station `station`'s Watterson seed (stream `station + 1`), shared by good
/// and poor.
pub fn fading_seed(seed: u64, trial: u32, station: usize) -> u64 {
    derive_seed(seed, trial, station as u64 + 1)
}

/// The ten calls of a trial: `pileup_calls()[(trial * 10 + j) mod 50]`.
pub fn series_calls(trial: u32) -> Vec<String> {
    let all = crate::callsigns::pileup_calls();
    (0..STATIONS_PER_RECORDING)
        .map(|j| all[(trial as usize * STATIONS_PER_RECORDING + j) % all.len()].clone())
        .collect()
}

/// Station `j`'s offset from the centre frequency.
pub fn station_offset_hz(j: usize) -> f64 {
    FIRST_OFFSET_HZ + OFFSET_STEP_HZ * j as f64
}

/// The keyed payload for one call.
pub fn payload(call: &str) -> String {
    PAYLOAD_TEMPLATE.replace("<call>", call)
}

/// D2: one series' ten stations, at the reference SNR.
pub fn series_signals(condition: Condition, wpm: f32, trial: u32, seed: u64) -> Vec<SignalSpec> {
    series_calls(trial)
        .iter()
        .enumerate()
        .map(|(j, call)| SignalSpec {
            text: payload(call),
            loop_text: true,
            wpm,
            offset_hz: station_offset_hz(j),
            snr_2500_db: REFERENCE_SNR_2500_DB,
            jitter: None,
            qsb: None,
            watterson: condition.preset().map(|preset| WattersonFade {
                preset,
                seed: fading_seed(seed, trial, j),
            }),
            char_wpm: None,
            weight: WEIGHT,
            char_gap_units: CHAR_GAP_UNITS,
            word_gap_units: WORD_GAP_UNITS,
            rise_ms: RISE_MS,
        })
        .collect()
}

/// One station, no noise. Returns its samples and keyed text.
pub fn render_station(sig: &SignalSpec, duration_s: f64) -> Result<(Vec<Complex32>, String)> {
    let (iq, mut texts) =
        render_scene(std::slice::from_ref(sig), SAMPLE_RATE_HZ, duration_s, None)?;
    Ok((iq, texts.pop().unwrap_or_default()))
}

/// `acc[i] += part[i]`. The one summation every series render uses, so the
/// float order never depends on how stations were scheduled.
pub fn accumulate(acc: &mut [Complex32], part: &[Complex32]) {
    for (a, p) in acc.iter_mut().zip(part) {
        *a += p;
    }
}

/// Sequential reference render: every station, summed in station order.
pub fn render_series(
    signals: &[SignalSpec],
    duration_s: f64,
) -> Result<(Vec<Complex32>, Vec<String>)> {
    let n = (duration_s * SAMPLE_RATE_HZ).round() as usize;
    let mut acc = vec![Complex32::new(0.0, 0.0); n];
    let mut texts = Vec::with_capacity(signals.len());
    for sig in signals {
        let (part, text) = render_station(sig, duration_s)?;
        accumulate(&mut acc, &part);
        texts.push(text);
    }
    Ok((acc, texts))
}

/// D7: [`render_series`] with the stations rendered `chunk` at a time on
/// scoped threads, then summed in station order, so the samples are
/// bit-identical to the sequential reference for any `chunk`.
pub fn render_series_chunked(
    signals: &[SignalSpec],
    duration_s: f64,
    chunk: usize,
) -> Result<(Vec<Complex32>, Vec<String>)> {
    let n = (duration_s * SAMPLE_RATE_HZ).round() as usize;
    let mut acc = vec![Complex32::new(0.0, 0.0); n];
    let mut texts = Vec::with_capacity(signals.len());
    for group in signals.chunks(chunk.max(1)) {
        let parts: Vec<Result<(Vec<Complex32>, String)>> = std::thread::scope(|s| {
            let handles: Vec<_> = group
                .iter()
                .map(|sig| s.spawn(move || render_station(sig, duration_s)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("rendering a station panicked")))
                })
                .collect()
        });
        for part in parts {
            let (buf, text) = part?;
            accumulate(&mut acc, &buf);
            texts.push(text);
        }
    }
    Ok((acc, texts))
}

/// The trial's noise, alone.
pub fn render_noise(duration_s: f64, seed: u64, trial: u32) -> Result<Vec<Complex32>> {
    Ok(render_scene(
        &[],
        SAMPLE_RATE_HZ,
        duration_s,
        Some(noise_seed(seed, trial)),
    )?
    .0)
}

/// Amplitude gain that moves a reference render to `snr_2500_db`.
pub fn gain_for_snr_2500(snr_2500_db: f64) -> f32 {
    10f64.powf((snr_2500_db - REFERENCE_SNR_2500_DB as f64) / 20.0) as f32
}

/// D4: `clean * gain + noise`, sample by sample.
pub fn compose(clean: &[Complex32], noise: &[Complex32], gain: f32) -> Vec<Complex32> {
    clean.iter().zip(noise).map(|(c, n)| c * gain + n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_names_parse_and_label() {
        for (s, c, label) in [
            ("awgn", Condition::Awgn, "AWGN"),
            ("good", Condition::WattersonGood, "Watterson good"),
            ("poor", Condition::WattersonPoor, "Watterson poor"),
        ] {
            assert_eq!(s.parse::<Condition>().unwrap(), c);
            assert_eq!(c.name(), s);
            assert_eq!(c.label(), label);
        }
        let err = "moderate".parse::<Condition>().unwrap_err();
        assert_eq!(err, "unknown condition `moderate` (awgn, good or poor)");
        assert_eq!(
            Condition::ALL,
            [
                Condition::Awgn,
                Condition::WattersonGood,
                Condition::WattersonPoor
            ]
        );
    }

    #[test]
    fn series_is_ten_looped_cq_stations_spread_across_the_passband() {
        let s = series_signals(Condition::Awgn, 25.0, 0, 1);
        assert_eq!(s.len(), STATIONS_PER_RECORDING);
        assert_eq!(STATIONS_PER_RECORDING, 10);
        let calls = series_calls(0);
        for (j, sig) in s.iter().enumerate() {
            assert_eq!(sig.text, format!("CQ CQ DE {0} {0} K", calls[j]));
            assert!(
                sig.loop_text
                    && sig.jitter.is_none()
                    && sig.qsb.is_none()
                    && sig.char_wpm.is_none()
            );
            assert_eq!(
                (
                    sig.wpm,
                    sig.weight,
                    sig.char_gap_units,
                    sig.word_gap_units,
                    sig.rise_ms
                ),
                (25.0, 3.0, 3.0, 7.0, 5.0)
            );
            assert_eq!(sig.snr_2500_db, REFERENCE_SNR_2500_DB);
            assert_eq!(REFERENCE_SNR_2500_DB, 0.0);
            assert_eq!(sig.offset_hz, -36_000.0 + 8_009.375 * j as f64);
            assert!(sig.offset_hz.abs() <= 40_000.0 && sig.offset_hz.abs() >= 3_900.0);
        }
        // distinct calls, distinct channel positions
        let uniq: std::collections::BTreeSet<_> = calls.iter().collect();
        assert_eq!(uniq.len(), 10);
    }

    #[test]
    fn fading_only_for_watterson_and_seeds_are_per_station_not_per_speed() {
        let seeds_of = |v: &[SignalSpec]| -> Vec<u64> {
            v.iter().map(|s| s.watterson.unwrap().seed).collect()
        };
        assert!(series_signals(Condition::Awgn, 25.0, 0, 1)
            .iter()
            .all(|s| s.watterson.is_none()));
        let g15 = series_signals(Condition::WattersonGood, 15.0, 0, 1);
        let g35 = series_signals(Condition::WattersonGood, 35.0, 0, 1);
        let p15 = series_signals(Condition::WattersonPoor, 15.0, 0, 1);
        let seeds = seeds_of(&g15);
        // common random numbers across speeds, and across good/poor
        assert_eq!(seeds, seeds_of(&g35));
        assert_eq!(seeds, seeds_of(&p15));
        assert!(g15
            .iter()
            .all(|s| matches!(s.watterson.unwrap().preset, WattersonPreset::Good)));
        assert!(p15
            .iter()
            .all(|s| matches!(s.watterson.unwrap().preset, WattersonPreset::Poor)));
        let uniq: std::collections::BTreeSet<_> = seeds.iter().collect();
        assert_eq!(uniq.len(), 10);
        assert_ne!(
            seeds,
            seeds_of(&series_signals(Condition::WattersonGood, 15.0, 1, 1))
        );
    }

    #[test]
    fn calls_rotate_through_the_fifty_fixture_calls_by_trial() {
        let all = crate::callsigns::pileup_calls();
        assert_eq!(series_calls(0), all[0..10].to_vec());
        assert_eq!(series_calls(1), all[10..20].to_vec());
        assert_eq!(series_calls(5), all[0..10].to_vec()); // wraps after 50
    }

    #[test]
    fn seed_derivation_is_pinned() {
        // Changing these moves every published sensitivity number; see the
        // MAN-116 decision record.
        assert_eq!(derive_seed(1, 0, 0), 12_793_040_940_332_582_595);
        assert_eq!(derive_seed(1, 0, 1), 7_806_873_273_932_414_515);
        assert_eq!(derive_seed(1, 0, 10), 7_446_594_392_015_701_318);
        assert_eq!(derive_seed(1, 1, 0), 6_301_985_355_436_268_297);
        assert_eq!(derive_seed(2, 0, 0), 1_825_907_084_063_272_085);
        assert_eq!(noise_seed(1, 0), derive_seed(1, 0, 0));
        assert_eq!(fading_seed(1, 0, 9), derive_seed(1, 0, 10));
    }

    #[test]
    fn gain_is_amplitude_for_a_2500hz_snr_above_the_reference() {
        assert_eq!(gain_for_snr_2500(0.0), 1.0);
        assert!((gain_for_snr_2500(20.0) - 10.0).abs() < 1e-5);
        assert!((gain_for_snr_2500(-6.020_6) - 0.5).abs() < 1e-5);
    }

    /// D4: one reference render scaled per SNR must equal a direct render at
    /// that SNR, for both the NCO path (AWGN) and the Watterson + Hilbert
    /// path. 2 stations and 3 s keep this fast in the dev profile.
    #[test]
    fn composed_recording_matches_a_direct_render() {
        for cond in [Condition::Awgn, Condition::WattersonPoor] {
            let reference: Vec<SignalSpec> = series_signals(cond, 25.0, 0, 1)
                .into_iter()
                .take(2)
                .collect();
            let (clean, texts_ref) = render_series(&reference, 3.0).unwrap(); // no noise
            let noise = render_noise(3.0, 1, 0).unwrap();
            for snr_2500 in [-5.0f64, 10.0, 23.0] {
                let composed = compose(&clean, &noise, gain_for_snr_2500(snr_2500));
                let at_snr: Vec<SignalSpec> = reference
                    .iter()
                    .cloned()
                    .map(|mut s| {
                        s.snr_2500_db = snr_2500 as f32;
                        s
                    })
                    .collect();
                let (direct, texts) =
                    render_scene(&at_snr, SAMPLE_RATE_HZ, 3.0, Some(noise_seed(1, 0))).unwrap();
                assert_eq!(texts, texts_ref);
                assert_eq!(composed.len(), direct.len());
                let peak = direct.iter().map(|c| c.norm()).fold(0.0f32, f32::max);
                let worst = composed
                    .iter()
                    .zip(&direct)
                    .map(|(a, b)| (a - b).norm())
                    .fold(0.0f32, f32::max);
                assert!(
                    worst <= 1e-5 * peak,
                    "{cond:?} {snr_2500} dB: {worst} vs peak {peak}"
                );
            }
        }
    }

    /// The CLI renders stations on parallel threads, `--jobs` at a time, but
    /// must add them in station order; this pins that the chunked, threaded
    /// render is bit-identical to the sequential reference.
    #[test]
    fn render_series_sums_stations_in_order_regardless_of_chunking() {
        let signals: Vec<SignalSpec> = series_signals(Condition::Awgn, 25.0, 0, 1);
        let reference = render_series(&signals, 1.0).unwrap();
        for chunk in [1usize, 3, 10] {
            let chunked = render_series_chunked(&signals, 1.0, chunk).unwrap();
            assert_eq!(chunked, reference, "chunk {chunk}");
        }
    }
}
