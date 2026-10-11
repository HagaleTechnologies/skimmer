//! Short source measurements using the channelizer and floor estimator only.

use anyhow::{bail, ensure, Context, Result};
use manta_dsp::channelizer::{power_db, Channelizer};
use manta_dsp::floor::FloorBank;
use manta_input::IqSource;
use num_complex::Complex32;
use std::time::{Duration, Instant};

/// Sampling policy. Duration is source time; native reads are not cancellable.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    pub duration_seconds: f64,
    /// Only live audio retries an empty nonblocking callback buffer.
    pub zero_read_is_transient: bool,
    /// Non-secret source kind supplied by the CLI's resolved source spec.
    pub source: String,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            duration_seconds: 3.0,
            zero_read_is_transient: false,
            source: "file".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    SampleWindowComplete,
    EndOfFile,
    SamplingDeadlineReached,
}

/// Raw per-channel lower quartiles summarized over the delivered passband.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct NoiseFloorSummary {
    pub min: f64,
    pub median: f64,
    pub max: f64,
    pub channels: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckReport {
    pub source: String,
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub passband_offsets_hz: [f64; 2],
    pub requested_duration_seconds: f64,
    pub observed_duration_seconds: f64,
    pub samples_received: u64,
    pub input_power_dbfs: Option<f64>,
    pub digital_silence: bool,
    pub noise_floor_dbfs: Option<NoiseFloorSummary>,
    pub stop_reason: StopReason,
}

/// Measure actual delivered IQ without detection, decoding or output services.
///
/// The cooperative deadline is duration + five seconds, checked between reads.
/// A native open or read may block longer; ownership is released on every return.
pub fn check_source(source: Box<dyn IqSource>, options: CheckOptions) -> Result<CheckReport> {
    validate_duration(options.duration_seconds)?;
    let deadline = Duration::from_secs_f64(options.duration_seconds + 5.0);
    check_source_with_deadline(source, options, deadline)
}

fn validate_duration(seconds: f64) -> Result<()> {
    ensure!(
        seconds.is_finite() && (1.0..=60.0).contains(&seconds),
        "duration must be finite and between 1 and 60 seconds"
    );
    Ok(())
}

fn eligible_channels(ch: &Channelizer, center: f64, passband: [f64; 2]) -> Vec<usize> {
    (0..ch.n_channels())
        .filter(|&k| {
            let offset = ch.channel_freq_hz(k) - center;
            offset >= passband[0] && offset < passband[1]
        })
        .collect()
}

fn summarize_floor(bank: &FloorBank, eligible: &[usize]) -> NoiseFloorSummary {
    let mut values: Vec<_> = eligible.iter().map(|&k| bank.channel_floor_db(k)).collect();
    values.sort_by(f64::total_cmp);
    let n = values.len();
    NoiseFloorSummary {
        min: values[0],
        median: if n.is_multiple_of(2) {
            (values[n / 2 - 1] + values[n / 2]) / 2.0
        } else {
            values[n / 2]
        },
        max: values[n - 1],
        channels: n,
    }
}

fn check_source_with_deadline(
    source: Box<dyn IqSource>,
    options: CheckOptions,
    deadline: Duration,
) -> Result<CheckReport> {
    // Start immediately after receiving the opened source, including setup time.
    let started = Instant::now();
    check_source_with_clock(source, options, deadline, || started.elapsed())
}

fn check_source_with_clock(
    mut source: Box<dyn IqSource>,
    options: CheckOptions,
    deadline: Duration,
    mut elapsed: impl FnMut() -> Duration,
) -> Result<CheckReport> {
    validate_duration(options.duration_seconds)?;
    let fs = source.sample_rate();
    ensure!(
        fs.is_finite() && fs > 0.0,
        "invalid stream sample rate {fs}"
    );
    let budget = (options.duration_seconds * fs).ceil();
    ensure!(
        budget.is_finite() && budget >= 1.0 && budget < u64::MAX as f64,
        "sample budget overflow"
    );
    let budget = budget as u64;
    let center = source.center_freq_hz();
    ensure!(center.is_finite(), "invalid center frequency {center}");
    let (lo, hi) = source.rf_passband_hz();
    ensure!(
        lo.is_finite() && hi.is_finite() && lo < hi,
        "invalid source passband"
    );
    let passband = [lo.max(-fs / 2.0), hi.min(fs / 2.0)];
    ensure!(
        passband[0] < passband[1],
        "source passband does not intersect Nyquist interval"
    );
    // Channelizer's general power-of-two check also accepts N=1 and N=2,
    // whose N/4 hop is zero. Reject these before constructing it.
    ensure!(
        fs >= 4.0 * 93.75,
        "stream sample rate requires fewer than four channels"
    );
    let mut ch = Channelizer::new(fs, center)
        .map_err(anyhow::Error::msg)
        .context("channelizer")?;
    let eligible = eligible_channels(&ch, center, passband);
    ensure!(
        !eligible.is_empty(),
        "source passband contains no channel centers"
    );
    let mut floor = FloorBank::new(ch.n_channels());
    let mut db = vec![0.0; ch.n_channels()];
    let mut buffer = vec![Complex32::new(0.0, 0.0); 4096];
    let mut samples_received = 0u64;
    let mut power_sum = 0.0f64;
    let mut any_hop = false;
    let mut all_zero = true;
    let stop_reason = loop {
        if elapsed() >= deadline {
            break StopReason::SamplingDeadlineReached;
        }
        if samples_received == budget {
            break StopReason::SampleWindowComplete;
        }
        let request = (budget - samples_received).min(buffer.len() as u64) as usize;
        let n = source
            .read(&mut buffer[..request])
            .context("reading source samples")?;
        let deadline_reached = elapsed() >= deadline;
        ensure!(n <= request, "source returned more samples than requested");
        if let Some(missed) = source.take_discontinuity() {
            bail!("source discontinuity ({missed} missing samples)");
        }
        for sample in &buffer[..n] {
            ensure!(
                sample.re.is_finite() && sample.im.is_finite(),
                "nonfinite input sample"
            );
            all_zero &= sample.re == 0.0 && sample.im == 0.0;
            let re = f64::from(sample.re);
            let im = f64::from(sample.im);
            power_sum += re * re + im * im;
        }
        samples_received += n as u64;
        for hop in ch.process(&buffer[..n]) {
            for (value, &power) in db.iter_mut().zip(&hop.power) {
                ensure!(power.is_finite(), "nonfinite channel power");
                *value = power_db(power);
            }
            floor.update(&db);
            any_hop = true;
        }
        if deadline_reached {
            break StopReason::SamplingDeadlineReached;
        }
        if n == 0 {
            if !options.zero_read_is_transient {
                break StopReason::EndOfFile;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    Ok(CheckReport {
        source: options.source,
        sample_rate_hz: fs,
        center_freq_hz: center,
        passband_offsets_hz: passband,
        requested_duration_seconds: options.duration_seconds,
        observed_duration_seconds: samples_received as f64 / fs,
        samples_received,
        input_power_dbfs: (power_sum > 0.0)
            .then(|| 10.0 * (power_sum / samples_received as f64).log10()),
        digital_silence: samples_received > 0 && all_zero,
        noise_floor_dbfs: any_hop.then(|| summarize_floor(&floor, &eligible)),
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const FS: f64 = 12_000.0;

    #[derive(Default)]
    struct State {
        requests: Vec<usize>,
        delivered: usize,
        dropped: bool,
    }

    struct Source {
        samples: Vec<Complex32>,
        rate: f64,
        center: f64,
        passband: (f64, f64),
        chunk: usize,
        zero_reads: usize,
        error: bool,
        error_after: Option<usize>,
        gap: bool,
        state: Rc<RefCell<State>>,
    }

    impl Source {
        fn new(samples: Vec<Complex32>) -> Self {
            Self {
                samples,
                rate: FS,
                center: 7_030_000.0,
                passband: (-FS / 2.0, FS / 2.0),
                chunk: usize::MAX,
                zero_reads: 0,
                error: false,
                error_after: None,
                gap: false,
                state: Rc::default(),
            }
        }
    }

    impl IqSource for Source {
        fn sample_rate(&self) -> f64 {
            self.rate
        }
        fn center_freq_hz(&self) -> f64 {
            self.center
        }
        fn rf_passband_hz(&self) -> (f64, f64) {
            self.passband
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let mut state = self.state.borrow_mut();
            state.requests.push(buf.len());
            if self.error
                || self
                    .error_after
                    .is_some_and(|after| state.delivered >= after)
            {
                bail!("scripted read failure");
            }
            if self.zero_reads > 0 {
                self.zero_reads -= 1;
                return Ok(0);
            }
            let n = self
                .chunk
                .min(buf.len())
                .min(self.samples.len() - state.delivered);
            buf[..n].copy_from_slice(&self.samples[state.delivered..state.delivered + n]);
            state.delivered += n;
            Ok(n)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.gap.then_some(100)
        }
    }

    impl Drop for Source {
        fn drop(&mut self) {
            self.state.borrow_mut().dropped = true;
        }
    }

    fn options() -> CheckOptions {
        CheckOptions {
            duration_seconds: 1.0,
            ..Default::default()
        }
    }

    fn noise(n: usize) -> Vec<Complex32> {
        let mut seed = 125u32;
        (0..n)
            .map(|_| {
                let mut next = || {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    ((seed >> 8) as f32 / 16_777_216.0 - 0.5) * 0.1
                };
                Complex32::new(next(), next())
            })
            .collect()
    }

    #[test]
    fn uses_source_metadata_caps_sample_budget_and_releases_source() {
        let source = Source::new(vec![Complex32::new(0.3, 0.4); 24_000]);
        let state = source.state.clone();
        let report = check_source(Box::new(source), options()).unwrap();
        assert_eq!(report.sample_rate_hz, FS);
        assert_eq!(report.center_freq_hz, 7_030_000.0);
        assert_eq!(report.passband_offsets_hz, [-6000.0, 6000.0]);
        assert_eq!(report.samples_received, 12_000);
        assert_eq!(report.observed_duration_seconds, 1.0);
        assert!((report.input_power_dbfs.unwrap() - 10.0 * 0.25f64.log10()).abs() < 1e-6);
        assert_eq!(report.stop_reason, StopReason::SampleWindowComplete);
        let state = state.borrow();
        assert!(state.dropped);
        assert_eq!(state.delivered, 12_000);
        assert_eq!(state.requests.iter().sum::<usize>(), 12_000);
        assert!(state.requests.iter().all(|&n| n <= 4096));
    }

    #[test]
    fn noise_scaling_changes_input_power_and_floor_by_six_db() {
        let samples = noise(12_000);
        let doubled = samples.iter().map(|s| *s * 2.0).collect();
        let a = check_source(Box::new(Source::new(samples)), options()).unwrap();
        let b = check_source(Box::new(Source::new(doubled)), options()).unwrap();
        assert!(
            (b.input_power_dbfs.unwrap() - a.input_power_dbfs.unwrap() - 6.020599913).abs() < 1e-7
        );
        assert!(
            (b.noise_floor_dbfs.unwrap().median - a.noise_floor_dbfs.unwrap().median - 6.0).abs()
                <= 0.5
        );
    }

    #[test]
    fn excluded_channels_do_not_change_raw_floor_across_block_boundary() {
        let ch = Channelizer::new(FS, 0.0).unwrap();
        let eligible = eligible_channels(&ch, 0.0, [31.0 * 93.75, 33.0 * 93.75]);
        assert_eq!(eligible, vec![31, 32]);
        let mut quiet = vec![-130.0; 128];
        quiet[31] = -40.0;
        quiet[32] = -50.0;
        let mut loud = vec![-10.0; 128];
        loud[31] = -40.0;
        loud[32] = -50.0;
        let mut a = FloorBank::new(128);
        let mut b = FloorBank::new(128);
        a.update(&quiet);
        b.update(&loud);
        let summary = summarize_floor(&a, &eligible);
        assert_eq!(summary, summarize_floor(&b, &eligible));
        assert_eq!(
            summary,
            NoiseFloorSummary {
                min: -49.75,
                median: -44.75,
                max: -39.75,
                channels: 2
            }
        );
        assert_ne!(a.effective_floor_db(31), b.effective_floor_db(31));
    }

    #[test]
    fn narrow_positive_iq_band_reports_in_band_power_despite_finite_leakage() {
        let samples = (0..12_000)
            .map(|i| {
                let phase = 2.0 * std::f64::consts::PI * 32.0 * 93.75 * i as f64 / FS;
                Complex32::new(0.1 * phase.cos() as f32, 0.1 * phase.sin() as f32)
            })
            .collect();
        let mut source = Source::new(samples);
        source.passband = (31.5 * 93.75, 32.5 * 93.75);
        let report = check_source(Box::new(source), options()).unwrap();
        let floor = report.noise_floor_dbfs.unwrap();
        assert_eq!(floor.channels, 1);
        assert!((floor.median + 20.0).abs() < 1.0, "{floor:?}");
    }

    #[test]
    fn silence_uses_actual_histogram_floor_but_empty_and_short_input_do_not() {
        for count in [0, 1023, 12_000] {
            let source = Source::new(vec![Complex32::new(0.0, 0.0); count]);
            let state = source.state.clone();
            let report = check_source(Box::new(source), options()).unwrap();
            assert_eq!(report.digital_silence, count > 0);
            assert_eq!(report.input_power_dbfs, None);
            assert_eq!(report.samples_received, count as u64);
            if count == 12_000 {
                assert_eq!(report.noise_floor_dbfs.unwrap().median, -139.75);
            } else {
                assert_eq!(report.noise_floor_dbfs, None);
                assert_eq!(report.stop_reason, StopReason::EndOfFile);
            }
            assert!(state.borrow().dropped);
        }
        let report = check_source(
            Box::new(Source::new(vec![Complex32::new(1.0, 0.0); 100])),
            options(),
        )
        .unwrap();
        assert_eq!(report.input_power_dbfs, Some(0.0));
        assert_eq!(report.noise_floor_dbfs, None);
    }

    #[test]
    fn transient_zero_reads_retry_but_file_eof_is_terminal() {
        for transient in [false, true] {
            let mut source = Source::new(noise(12_000));
            source.zero_reads = 2;
            let state = source.state.clone();
            let report = check_source(
                Box::new(source),
                CheckOptions {
                    zero_read_is_transient: transient,
                    ..options()
                },
            )
            .unwrap();
            assert_eq!(report.samples_received, if transient { 12_000 } else { 0 });
            assert_eq!(state.borrow().requests.len(), if transient { 5 } else { 1 });
        }
    }

    #[test]
    fn real_deadline_stops_empty_audio_without_busy_spinning() {
        let source = Source::new(vec![]);
        let state = source.state.clone();
        let start = Instant::now();
        let report = check_source_with_deadline(
            Box::new(source),
            CheckOptions {
                zero_read_is_transient: true,
                ..options()
            },
            Duration::from_millis(25),
        )
        .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(25));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(state.borrow().requests.len() <= 3);
        assert!(state.borrow().dropped);
        assert_eq!(report.stop_reason, StopReason::SamplingDeadlineReached);
        assert_eq!(report.samples_received, 0);
        assert_eq!(report.noise_floor_dbfs, None);
    }

    #[test]
    fn deadline_after_read_retains_received_measurement() {
        let source = Source::new(noise(12_000));
        // Clock advances across read(), independent of host scheduling/setup.
        let mut checks = 0;
        let elapsed = || {
            checks += 1;
            if checks == 1 {
                Duration::ZERO
            } else {
                Duration::from_millis(20)
            }
        };
        let report = check_source_with_clock(
            Box::new(source),
            options(),
            Duration::from_millis(10),
            elapsed,
        )
        .unwrap();
        assert_eq!(report.stop_reason, StopReason::SamplingDeadlineReached);
        assert_eq!(report.samples_received, 4096);
        assert!(report.noise_floor_dbfs.is_some());
    }

    #[test]
    fn short_eof_reports_delivered_duration_and_releases_source() {
        let source = Source::new(noise(6000));
        let state = source.state.clone();
        let report = check_source(Box::new(source), options()).unwrap();
        assert_eq!(report.samples_received, 6000);
        assert_eq!(report.observed_duration_seconds, 0.5);
        assert_eq!(report.stop_reason, StopReason::EndOfFile);
        assert!(report.noise_floor_dbfs.is_some());
        assert!(state.borrow().dropped);
    }

    #[test]
    fn read_errors_discontinuities_and_nonfinite_samples_fail_and_release_source() {
        for mode in 0..4 {
            let mut source = Source::new(noise(12000));
            match mode {
                0 => source.error = true,
                1 => source.gap = true,
                2 => source.samples[10].re = f32::NAN,
                _ => source.samples[10].im = f32::INFINITY,
            }
            let state = source.state.clone();
            assert!(check_source(Box::new(source), options()).is_err());
            assert!(state.borrow().dropped);
        }
    }

    #[test]
    fn a_read_error_after_measured_samples_does_not_return_a_report() {
        let mut source = Source::new(noise(12_000));
        source.error_after = Some(4096);
        let state = source.state.clone();
        let error = check_source(Box::new(source), options()).unwrap_err();
        assert!(format!("{error:#}").contains("scripted read failure"));
        assert_eq!(state.borrow().delivered, 4096);
        assert!(state.borrow().dropped);
    }

    #[test]
    fn elapsed_deadline_is_checked_before_the_first_read() {
        let source = Source::new(noise(12_000));
        let state = source.state.clone();
        let report =
            check_source_with_deadline(Box::new(source), options(), Duration::ZERO).unwrap();
        assert_eq!(report.stop_reason, StopReason::SamplingDeadlineReached);
        assert_eq!(report.samples_received, 0);
        assert!(state.borrow().requests.is_empty());
        assert!(state.borrow().dropped);
    }

    #[test]
    fn invalid_metadata_and_duration_fail_before_reading() {
        for rate in [
            0.0,
            -FS,
            f64::NAN,
            f64::INFINITY,
            44_100.0,
            93.75,
            187.5,
            f64::MAX,
        ] {
            let mut source = Source::new(vec![]);
            source.rate = rate;
            let state = source.state.clone();
            assert!(
                check_source(Box::new(source), options()).is_err(),
                "rate {rate}"
            );
            assert!(state.borrow().requests.is_empty());
            assert!(state.borrow().dropped);
        }
        for band in [
            (0.0, 0.0),
            (100.0, 10.0),
            (f64::NAN, 100.0),
            (0.0, f64::INFINITY),
            (7000.0, 8000.0),
            (1.0, 2.0),
        ] {
            let mut source = Source::new(vec![]);
            source.passband = band;
            assert!(
                check_source(Box::new(source), options()).is_err(),
                "band {band:?}"
            );
        }
        for duration in [0.0, 0.5, 61.0, f64::NAN, f64::INFINITY] {
            assert!(check_source(
                Box::new(Source::new(vec![])),
                CheckOptions {
                    duration_seconds: duration,
                    ..options()
                }
            )
            .is_err());
        }
        let mut source = Source::new(vec![]);
        source.center = f64::INFINITY;
        assert!(check_source(Box::new(source), options()).is_err());
    }

    #[test]
    fn passband_is_clipped_to_nyquist_and_upper_boundary_excluded() {
        let mut source = Source::new(noise(12_000));
        source.passband = (-20_000.0, 93.75);
        let report = check_source(Box::new(source), options()).unwrap();
        assert_eq!(report.passband_offsets_hz, [-6000.0, 93.75]);
        assert_eq!(report.noise_floor_dbfs.unwrap().channels, 65);
    }

    #[test]
    fn read_chunk_size_preserves_sample_count_power_and_floor() {
        let mut reports = vec![];
        for chunk in [137, 1024, 4096] {
            let mut source = Source::new(noise(24_000));
            source.chunk = chunk;
            reports.push(check_source(Box::new(source), options()).unwrap());
        }
        for report in &reports[1..] {
            assert_eq!(report.samples_received, reports[0].samples_received);
            assert_eq!(report.noise_floor_dbfs, reports[0].noise_floor_dbfs);
            assert!(
                (report.input_power_dbfs.unwrap() - reports[0].input_power_dbfs.unwrap()).abs()
                    < 1e-12
            );
        }
    }
}
