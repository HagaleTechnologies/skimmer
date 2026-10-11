//! Open and measure a source without starting the decoding pipeline.

use super::*;
use manta_engine::check::{check_source, CheckOptions, CheckReport, StopReason};

#[derive(clap::Args)]
pub(crate) struct Args {
    /// Audio WAV path, equivalent to --source PATH.
    #[arg(value_name = "SOURCE", group = "source_selector")]
    path: Option<PathBuf>,
    /// Sample window in seconds, from 1 to 60.
    #[arg(long, default_value = "3", value_parser = parse_duration)]
    duration: f64,
    /// Sound-card input device; matched by substring. Defaults to the
    /// system default input.
    #[arg(long, conflicts_with = "source", help_heading = "Audio input")]
    #[arg(group = "source_selector")]
    device: Option<String>,
    /// Replay a WAV file instead of listening to a live device.
    ///
    /// Read as fast as the machine manages, not in real time, so a
    /// 60-second file usually finishes sooner than that. Useful for
    /// demos and repeatable testing.
    #[arg(long, conflicts_with = "device", help_heading = "Audio input")]
    #[arg(group = "source_selector")]
    source: Option<PathBuf>,
    /// KiwiSDR receiver hostname. Requires --kiwi-freq-hz.
    #[arg(help_heading = "KiwiSDR source")]
    #[cfg_attr(all(feature = "hpsdr", feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "soapy_driver"], requires = "kiwi_freq_hz"))]
    #[cfg_attr(all(feature = "hpsdr", not(feature = "soapy")), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host"], requires = "kiwi_freq_hz"))]
    #[cfg_attr(all(not(feature = "hpsdr"), feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "soapy_driver"], requires = "kiwi_freq_hz"))]
    #[cfg_attr(not(any(feature = "hpsdr", feature = "soapy")), arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq_hz"))]
    #[arg(group = "source_selector")]
    kiwi_host: Option<String>,
    /// KiwiSDR receiver port.
    #[arg(
        long,
        default_value = "8073",
        requires = "kiwi_host",
        help_heading = "KiwiSDR source"
    )]
    kiwi_port: u16,
    /// Receiver centre frequency, in Hz. Required with --kiwi-host.
    #[arg(
        long = "kiwi-freq-hz",
        alias = "kiwi-freq",
        requires = "kiwi_host",
        help_heading = "KiwiSDR source"
    )]
    kiwi_freq_hz: Option<f64>,
    /// KiwiSDR password. Leave empty for public receivers that do not
    /// ask for one.
    #[arg(
        long,
        requires = "kiwi_host",
        default_value = "",
        help_heading = "KiwiSDR source"
    )]
    kiwi_password: String,
    /// SoapySDR device arguments, e.g. "driver=rtlsdr".
    ///
    /// Requires --soapy-freq-hz and --soapy-rate-hz. Available only in
    /// builds made with the `soapy` feature.
    #[cfg(feature = "soapy")]
    #[arg(help_heading = "SoapySDR source")]
    #[cfg_attr(feature = "hpsdr", arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "kiwi_host"]))]
    #[cfg_attr(not(feature = "hpsdr"), arg(long, conflicts_with_all = ["device", "source", "kiwi_host"]))]
    #[arg(group = "source_selector")]
    soapy_driver: Option<String>,
    /// Receiver centre frequency, in Hz. Required with --soapy-driver.
    #[cfg(feature = "soapy")]
    #[arg(
        long = "soapy-freq-hz",
        alias = "soapy-freq",
        requires = "soapy_driver",
        help_heading = "SoapySDR source"
    )]
    soapy_freq_hz: Option<f64>,
    /// Sample rate, in Hz. Required with --soapy-driver.
    #[cfg(feature = "soapy")]
    #[arg(
        long = "soapy-rate-hz",
        alias = "soapy-rate",
        requires = "soapy_driver",
        help_heading = "SoapySDR source"
    )]
    soapy_rate_hz: Option<f64>,
    /// Receiver gain, in dB. Omit to let the device use AGC.
    #[cfg(feature = "soapy")]
    #[arg(long, requires = "soapy_driver", help_heading = "SoapySDR source")]
    soapy_gain: Option<f64>,
    /// HPSDR/Hermes device hostname or IP address.
    ///
    /// Requires --hpsdr-freq-hz and --hpsdr-rate-hz. Available only in
    /// builds made with the `hpsdr` feature.
    #[cfg(feature = "hpsdr")]
    #[arg(help_heading = "HPSDR source")]
    #[cfg_attr(feature = "soapy", arg(long, conflicts_with_all = ["device", "source", "kiwi_host", "soapy_driver"]))]
    #[cfg_attr(not(feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "kiwi_host"]))]
    #[arg(group = "source_selector")]
    hpsdr_host: Option<String>,
    /// HPSDR/Hermes control port.
    #[cfg(feature = "hpsdr")]
    #[arg(
        long,
        default_value_t = manta_input::hpsdr::CONTROL_PORT,
        requires = "hpsdr_host",
        help_heading = "HPSDR source"
    )]
    hpsdr_port: u16,
    /// Receiver centre frequency, in Hz. Required with --hpsdr-host.
    #[cfg(feature = "hpsdr")]
    #[arg(
        long = "hpsdr-freq-hz",
        alias = "hpsdr-freq",
        requires = "hpsdr_host",
        value_parser = parse_hpsdr_freq_hz,
        help_heading = "HPSDR source"
    )]
    hpsdr_freq_hz: Option<f64>,
    /// Sample rate, in Hz. Required with --hpsdr-host.
    #[cfg(feature = "hpsdr")]
    #[arg(
        long = "hpsdr-rate-hz",
        alias = "hpsdr-rate",
        requires = "hpsdr_host",
        value_parser = parse_hpsdr_rate_hz,
        help_heading = "HPSDR source"
    )]
    hpsdr_rate_hz: Option<f64>,
    /// Decimate the source to this sample rate, in Hz, before measurement.
    ///
    /// Must divide the source's own rate by a power of two, and the
    /// result must itself be a rate the channelizer supports. Omit to
    /// measure at the source's native rate.
    #[arg(long, value_parser = parse_capture_rate_hz)]
    capture_rate_hz: Option<f64>,
    /// Read --source as a raw complex-IQ recording rather than ordinary
    /// mono audio.
    ///
    /// Without this, --source is taken as mono real audio at 48000 Hz.
    /// A stereo audio recording and a two-channel IQ capture cannot be
    /// told apart from the WAV header alone, so say which you have.
    #[arg(long)]
    source_iq: bool,
    /// Radio dial frequency, in Hz. See `run --dial-freq-hz`; without it
    /// an audio source's reported frequencies, and the report's
    /// `center_freq_hz`, are baseband offsets.
    #[arg(long, value_parser = parse_dial_freq_hz, help_heading = "Audio input")]
    dial_freq_hz: Option<f64>,
    /// TOML config file whose `[input]` table supplies source settings.
    /// All tables are validated; decoding and spot settings are not applied.
    ///
    /// Falls back to $MANTA_CONFIG. Flags override the file, and
    /// MANTA_<TABLE>_<KEY> environment variables sit between the two.
    /// `check` never starts decoding or spot servers.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Print the full report as one JSON object instead of a readable
    /// summary.
    #[arg(long, help_heading = "Output")]
    json: bool,
}

fn parse_duration(value: &str) -> std::result::Result<f64, String> {
    let seconds: f64 = value
        .parse()
        .map_err(|_| "duration must be a number of seconds")?;
    if !seconds.is_finite() || !(1.0..=60.0).contains(&seconds) {
        return Err("duration must be between 1 and 60 seconds".into());
    }
    Ok(seconds)
}

pub(crate) fn run(args: Args) -> Result<i32> {
    let SourcePrepared { resolved, .. } = prepare_source(
        CliOverrides {
            device: args.device,
            source: args.path.or(args.source),
            source_iq: args.source_iq,
            kiwi: KiwiOpts {
                host: args.kiwi_host,
                port: args.kiwi_port,
                freq: args.kiwi_freq_hz,
                password: args.kiwi_password,
            },
            #[cfg(feature = "soapy")]
            soapy: SoapyOpts {
                driver: args.soapy_driver,
                freq: args.soapy_freq_hz,
                rate: args.soapy_rate_hz,
                gain: args.soapy_gain,
            },
            #[cfg(feature = "hpsdr")]
            hpsdr: HpsdrOpts {
                host: args.hpsdr_host,
                port: args.hpsdr_port,
                freq: args.hpsdr_freq_hz,
                rate: args.hpsdr_rate_hz,
            },
            dial_freq_hz: args.dial_freq_hz,
            capture_rate_hz: args.capture_rate_hz,
            ..CliOverrides::none()
        },
        args.config,
    )?;
    let options = CheckOptions {
        duration_seconds: args.duration,
        zero_read_is_transient: matches!(resolved.spec, LiveSourceSpec::AudioDevice(_)),
        source: resolved.spec.name().to_owned(),
    };
    let source = resolved
        .spec
        .open(resolved.capture_rate_hz, resolved.dial_freq_hz)
        .context("cannot open source")?;
    let report = check_source(source, options).context("source check failed")?;
    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print!("{}", render(&report));
    }
    Ok(exit_code(&report))
}

fn exit_code(report: &CheckReport) -> i32 {
    i32::from(
        report.noise_floor_dbfs.is_none()
            || report.stop_reason == StopReason::SamplingDeadlineReached,
    )
}

fn render(report: &CheckReport) -> String {
    use std::fmt::Write;
    let mut out = format!(
        "Source: {}\nStream sample rate: {} Hz\n",
        report.source, report.sample_rate_hz
    );
    if report.center_freq_hz == 0.0 {
        out.push_str("Center frequency: unknown (baseband)\n");
    } else {
        writeln!(out, "Center frequency: {} Hz", report.center_freq_hz).unwrap();
    }
    writeln!(
        out,
        "Passband offsets: {} to {} Hz",
        report.passband_offsets_hz[0], report.passband_offsets_hz[1]
    )
    .unwrap();
    writeln!(
        out,
        "Samples received: {} ({:.3} s)",
        report.samples_received, report.observed_duration_seconds
    )
    .unwrap();
    match report.input_power_dbfs {
        Some(power) => writeln!(out, "Input power: {power:.1} dBFS").unwrap(),
        None if report.digital_silence => out.push_str("Input power: digital silence\n"),
        None => out.push_str("Input power: unavailable\n"),
    }
    if let Some(floor) = &report.noise_floor_dbfs {
        writeln!(
            out,
            "Noise floor: {:.1} dBFS median; {:.1} to {:.1} dBFS range across {} channels",
            floor.median, floor.min, floor.max, floor.channels
        )
        .unwrap();
        out.push_str(
            "Floor estimate: brief, per-channel lower quartile; not calibrated RF power.\n",
        );
    } else {
        out.push_str("Noise floor: unavailable (not enough samples)\n");
    }
    let reason = match report.stop_reason {
        StopReason::SampleWindowComplete => "sample window complete",
        StopReason::EndOfFile => "end of file",
        StopReason::SamplingDeadlineReached => "sampling deadline reached",
    };
    writeln!(out, "Stopped: {reason}").unwrap();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_engine::check::NoiseFloorSummary;

    fn report() -> CheckReport {
        CheckReport {
            source: "file".into(),
            sample_rate_hz: 96000.0,
            center_freq_hz: 0.0,
            passband_offsets_hz: [-48000.0, 48000.0],
            requested_duration_seconds: 3.0,
            observed_duration_seconds: 0.0,
            samples_received: 0,
            input_power_dbfs: None,
            digital_silence: false,
            noise_floor_dbfs: None,
            stop_reason: StopReason::EndOfFile,
        }
    }

    #[test]
    fn empty_report_is_complete_and_unsuccessful() {
        let r = report();
        assert_eq!(exit_code(&r), 1);
        assert_eq!(render(&r), "Source: file\nStream sample rate: 96000 Hz\nCenter frequency: unknown (baseband)\nPassband offsets: -48000 to 48000 Hz\nSamples received: 0 (0.000 s)\nInput power: unavailable\nNoise floor: unavailable (not enough samples)\nStopped: end of file\n");
    }

    #[test]
    fn measured_silence_and_deadline_have_distinct_statuses() {
        let mut r = report();
        r.samples_received = 288000;
        r.observed_duration_seconds = 3.0;
        r.digital_silence = true;
        r.noise_floor_dbfs = Some(NoiseFloorSummary {
            min: -139.75,
            median: -139.75,
            max: -139.75,
            channels: 1024,
        });
        r.stop_reason = StopReason::SampleWindowComplete;
        assert_eq!(exit_code(&r), 0);
        let text = render(&r);
        assert!(text.contains("Input power: digital silence\n"));
        assert!(text.contains("-139.8 dBFS median"));
        assert!(text.ends_with("Stopped: sample window complete\n"));
        r.stop_reason = StopReason::SamplingDeadlineReached;
        assert_eq!(exit_code(&r), 1);
        assert!(render(&r).ends_with("Stopped: sampling deadline reached\n"));
        r.stop_reason = StopReason::EndOfFile;
        assert_eq!(exit_code(&r), 0);
    }

    #[test]
    fn duration_requires_finite_bounded_seconds() {
        for v in ["NaN", "inf", "-inf", "0", "0.9", "60.1", "bad"] {
            assert!(parse_duration(v).is_err(), "{v}");
        }
        for v in ["1", "3", "1.5", "60"] {
            assert!(parse_duration(v).is_ok(), "{v}");
        }
    }
}
