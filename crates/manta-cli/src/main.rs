//! `manta` CLI: decode CW from a recorded file or a live receiver, and run
//! the spotting daemon.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use manta_decode::decoder::Engine;
use manta_engine::{decode_wav, PipelineConfig};
use manta_input::IqSource;
use std::path::{Path, PathBuf};

mod bench;
mod build_info;
mod config;
mod config_cmd;
mod devices;
mod reconnect;
mod source_check;
mod text_lines;
use reconnect::ReconnectingSource;

#[derive(Parser)]
#[command(
    name = "manta",
    // Without an explicit bin_name, clap falls back to the runtime argv[0]
    // for "Usage: ..." lines -- on Windows that's "manta.exe" (the actual
    // executable filename), not "manta". Pin it so help/usage text (and
    // tests that assert against it, e.g. crates/manta-cli/tests/cli.rs) is
    // identical across platforms.
    bin_name = "manta",
    // MAN-83: commit + features, so a support request needs nothing else
    version = build_info::VERSION_LINE,
    about = "Open-source wideband CW skimmer: every CW signal in an SDR passband, decoded at once, emitted as RBN-compatible spots",
    after_help = "\
Examples:
  manta gen v1 --out /tmp/v1          Make a synthetic test recording
  manta decode /tmp/v1/v1.wav         Decode it, no radio needed
  manta listen                        Copy from the default sound card
  manta listen --kiwi-host rx.example.org --kiwi-freq-hz 7030000
                                      Copy from a public KiwiSDR on 40 m

Run `manta <command> --help` for a command's full options."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List audio inputs and available SDR devices.
    Devices {
        /// Print one JSON report.
        #[arg(long)]
        json: bool,
    },
    /// Read a short source sample and report its level without decoding.
    #[command(
        long_about = "Read a short source sample and report its level without decoding. SOURCE is an audio WAV path, equivalent to --source PATH; use --source-iq for complex IQ. Without a selector, use the configured input or default audio input. The sampling deadline is duration + 5 seconds after opening, checked between reads. Native open/read calls can exceed it."
    )]
    Check(source_check::Args),
    /// Decode CW from a recorded IQ WAV file.
    ///
    /// Reads a stereo WAV file (channel 0 = I, channel 1 = Q), decodes the
    /// CW in it, and prints the copied text plus a spot summary. Fully
    /// deterministic: the same file always produces the same output.
    Decode {
        /// Stereo IQ WAV file. Its centre frequency is read from the
        /// matching `<name>.json` sidecar next to it, unless
        /// --center-freq-hz is given.
        path: PathBuf,
        /// RF centre frequency of the recording, in Hz, overriding its sidecar.
        ///
        /// Without this flag or a sidecar, reported frequencies are baseband
        /// offsets from the recording's centre, and manta prints a warning.
        #[arg(long, value_name = "HZ", value_parser = parse_center_freq_hz)]
        center_freq_hz: Option<f64>,
        /// Print the full decode report as one JSON object.
        #[arg(long, help_heading = "Output")]
        json: bool,
        /// TOML config file; the whole file is validated.
        ///
        /// Its `[decode]` values are the baseline; an explicit --engine
        /// overrides just the engine setting. The environment (MANTA_CONFIG,
        /// MANTA_*) is not read here, so output never depends on it.
        // `--server-config` stays as a hidden alias for the flag's old
        // name; help advertises the canonical spelling only (D11/MAN-77).
        #[arg(long, alias = "server-config")]
        config: Option<PathBuf>,
        /// Decode engine: `legacy` (the default), `edge-legacy`, or `hsmm`.
        ///
        /// `hsmm` is experimental: it was tested and did not pass the accuracy
        /// and CPU-cost checks needed to become the default. Left unset
        /// rather than defaulting, so that an explicit choice can be told
        /// apart from no choice at all: when --config also names an engine,
        /// this flag wins if given, and the file's value is used otherwise.
        // hsmm's promotion gate was measured and failed on 2026-09-09, on both
        // accuracy and CPU budget:
        // docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
        #[arg(long, value_parser = parse_engine)]
        engine: Option<Engine>,
        #[command(flatten)]
        filters: FilterOpts,
    },
    /// Score the decoder against a reference recording and a spot log.
    ///
    /// Decodes each station a reference skimmer reported, one channel at a
    /// time with the tracker bypassed, and reports how many callsigns came
    /// back. For measuring decode quality, not for day-to-day operating.
    Oracle {
        /// Stereo IQ WAV file. Its centre frequency is read from the
        /// matching `<name>.json` sidecar next to it.
        path: PathBuf,
        /// Reverse Beacon Network daily-dump CSV, pre-filtered to the
        /// recording's time window.
        rbn_csv: PathBuf,
        /// Spotter callsign whose spots are the reference set -- normally
        /// the skimmer that sat next to this receiver.
        #[arg(long, default_value = "K5TR")]
        spotter: String,
        /// When the recording starts, as ISO-8601 UTC (for example
        /// 2025-11-29T00:00:00Z).
        ///
        /// Anchors the reference spot times to the recording instead of
        /// assuming it began exactly on the hour.
        #[arg(long, value_parser = parse_capture_start)]
        capture_start: i64,
        /// How many seconds around each reference spot to decode.
        #[arg(long, default_value_t = 40.0, value_parser = parse_window_s)]
        window_s: f64,
        /// TOML config file; the whole file is validated.
        ///
        /// Its `[decode]` values are the baseline; an explicit --engine
        /// overrides just the engine setting. The environment (MANTA_CONFIG,
        /// MANTA_*) is not read here, so output never depends on it.
        // `--server-config` stays as a hidden alias for the flag's old
        // name; help advertises the canonical spelling only (D11/MAN-77).
        #[arg(long, alias = "server-config")]
        config: Option<PathBuf>,
        /// Decode engine: `legacy` (the default), `edge-legacy`, or `hsmm`.
        ///
        /// `hsmm` is experimental: it was tested and did not pass the accuracy
        /// and CPU-cost checks needed to become the default. Left unset
        /// rather than defaulting, so that an explicit choice can be told
        /// apart from no choice at all: when --config also names an engine,
        /// this flag wins if given, and the file's value is used otherwise.
        // hsmm's promotion gate was measured and failed on 2026-09-09, on both
        // accuracy and CPU budget:
        // docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
        #[arg(long, value_parser = parse_engine)]
        engine: Option<Engine>,
        /// Write one JSON object per reference spot to this file. The
        /// summary always goes to stdout as well.
        #[arg(long)]
        jsonl: Option<PathBuf>,
    },
    /// Generate a synthetic CW test recording.
    ///
    /// Writes a WAV file plus its sidecar and manifest, for testing a
    /// decode without any radio hardware. Each named recording is a fixed,
    /// reproducible scenario -- clean signal, weak signal, fading, high
    /// speed, two stations sharing one channel -- so the same name always
    /// produces the same file.
    Gen {
        /// Which test recording to generate: v1 to v6, vr1 to vr5, vr6a,
        /// vr6b, vr7 or vr8.
        vector: String,
        /// Directory to write `<name>.wav`, `<name>.json` and
        /// `<name>.manifest.json` into.
        #[arg(long)]
        out: PathBuf,
    },
    /// Decode CW live from a receiver or sound card, or run the spotting
    /// daemon.
    ///
    /// Prints a SPOT: line on stdout for each confirmed spot, and decoded
    /// text on stderr, one line per track. With --config, also runs the
    /// full spotting daemon: the telnet cluster server, the JSON Lines /
    /// WebSocket stream, and the metrics endpoint; decoded text is then
    /// off unless --decoded-text is given.
    // `run` is the daemon entry point; `listen` stays as a visible alias
    // for ad hoc audio/dev testing (D11/MAN-77, see
    // docs/DECISIONS/2026-09-06-broad-review-decisions.md).
    #[command(visible_alias = "listen")]
    Run {
        /// Sound-card input device; matched by substring. Defaults to the
        /// system default input.
        #[arg(long, conflicts_with = "source", help_heading = "Audio input")]
        device: Option<String>,
        /// Replay a WAV file instead of listening to a live device.
        ///
        /// Decoded as fast as the machine manages, not in real time, so a
        /// 60-second file usually finishes sooner than that. Useful for
        /// demos and repeatable testing.
        #[arg(long, conflicts_with = "device", help_heading = "Audio input")]
        source: Option<PathBuf>,
        /// KiwiSDR receiver hostname. Requires --kiwi-freq-hz.
        #[arg(help_heading = "KiwiSDR source")]
        #[cfg_attr(all(feature = "hpsdr", feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "soapy_driver"], requires = "kiwi_freq_hz"))]
        #[cfg_attr(all(feature = "hpsdr", not(feature = "soapy")), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host"], requires = "kiwi_freq_hz"))]
        #[cfg_attr(all(not(feature = "hpsdr"), feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "soapy_driver"], requires = "kiwi_freq_hz"))]
        #[cfg_attr(not(any(feature = "hpsdr", feature = "soapy")), arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq_hz"))]
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
        /// Decimate the source to this sample rate, in Hz, before decoding.
        ///
        /// Must divide the source's own rate by a power of two, and the
        /// result must itself be a rate the channelizer supports. Omit to
        /// decode at the source's native rate.
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
        /// Print each decode as a JSON object, one per line, instead of
        /// plain text.
        #[arg(long, help_heading = "Output")]
        json: bool,
        /// Also print decoded text while the spotting daemon runs.
        ///
        /// Decoded text goes to stderr, one line per track, labelled with
        /// the track number, frequency and speed. It is printed by default,
        /// except when a `[server]` table starts the spotting daemon, so
        /// that a service log holds spots and diagnostics only. This flag
        /// prints it there too.
        #[arg(long, conflicts_with = "json", help_heading = "Output")]
        decoded_text: bool,
        /// TOML config file that turns this into the full spotting daemon.
        ///
        /// Needs a `[server]` table with the station callsign and ports.
        /// Starts the telnet cluster server, the JSON Lines / WebSocket
        /// stream, and the metrics endpoint alongside the decode loop.
        // `--server-config` stays as a hidden alias for the flag's old
        // name; help advertises the canonical spelling only (D11/MAN-77).
        #[arg(long, alias = "server-config", help_heading = "Server")]
        config: Option<PathBuf>,
        /// Radio dial frequency, in Hz.
        ///
        /// For a sound card or an ordinary (real-audio) --source WAV this
        /// becomes the source's centre frequency; for KiwiSDR, SoapySDR and
        /// HPSDR inputs, and for a --source-iq recording whose sidecar already
        /// carries a centre frequency, it overrides what the source reports.
        /// Required with --config when the input is a sound card or a
        /// real-audio --source WAV, because neither knows what frequency the
        /// radio was on -- without it, spots would be published at the audio
        /// tone frequency (for example 700 Hz) instead of the real one. A
        /// --source-iq recording with a sidecar centre frequency does not need
        /// it. Outside --config it is optional, but omitting it for an audio
        /// source prints a warning and reported frequencies stay baseband
        /// offsets.
        ///
        /// Enter the suppressed-carrier (USB) dial reading: manta adds the
        /// decoded tone offset to this value as-is. On a rig in CW mode the
        /// displayed dial frequency is usually already offset by your sidetone
        /// pitch -- subtract it (for example 700-800 Hz) first, or spots read
        /// high by that amount (CW-R inverts the sign and doubles the error).
        #[arg(long, value_parser = parse_dial_freq_hz, help_heading = "Server")]
        dial_freq_hz: Option<f64>,
        /// Timestamp to treat as the start of a replayed file, in Unix
        /// seconds.
        ///
        /// Only meaningful with --source and --config; ignored for a live
        /// input. Without it, the file's modification time is used, which
        /// is stable for an untouched file but changes when the file is
        /// copied or downloaded. Set it explicitly when spot timestamps
        /// must be identical across machines.
        #[arg(long, value_parser = parse_replay_epoch, help_heading = "Server")]
        replay_epoch: Option<i64>,
        /// Decode engine: `legacy` (the default), `edge-legacy`, or `hsmm`.
        ///
        /// `hsmm` is experimental: it was tested and did not pass the accuracy
        /// and CPU-cost checks needed to become the default. Left unset
        /// rather than defaulting, so that an explicit choice can be told
        /// apart from no choice at all: when --config also names an engine,
        /// this flag wins if given, and the file's value is used otherwise.
        // hsmm's promotion gate was measured and failed on 2026-09-09, on both
        // accuracy and CPU budget:
        // docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
        #[arg(long, value_parser = parse_engine)]
        engine: Option<Engine>,
        #[command(flatten)]
        filters: FilterOpts,
    },
    /// Run the live decoder for a fixed duration as a stability check.
    ///
    /// Reads from the same sources `listen` does, decodes for --duration
    /// seconds, and exits non-zero if the pipeline panics or its memory
    /// use keeps growing. Intended for unattended stability runs, not for
    /// day-to-day operating.
    Soak {
        /// How long to run, in seconds.
        #[arg(long)]
        duration: u64,
        /// Sound-card input device; matched by substring. Defaults to the
        /// system default input.
        #[arg(long, conflicts_with = "source", help_heading = "Audio input")]
        device: Option<String>,
        /// Replay a WAV file instead of listening to a live device.
        ///
        /// Decoded as fast as the machine manages, not in real time, so a
        /// 60-second file usually finishes sooner than that. Useful for
        /// demos and repeatable testing.
        #[arg(long, conflicts_with = "device", help_heading = "Audio input")]
        source: Option<PathBuf>,
        /// KiwiSDR receiver hostname. Requires --kiwi-freq-hz.
        #[arg(help_heading = "KiwiSDR source")]
        #[cfg_attr(all(feature = "hpsdr", feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "soapy_driver"], requires = "kiwi_freq_hz"))]
        #[cfg_attr(all(feature = "hpsdr", not(feature = "soapy")), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host"], requires = "kiwi_freq_hz"))]
        #[cfg_attr(all(not(feature = "hpsdr"), feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "soapy_driver"], requires = "kiwi_freq_hz"))]
        #[cfg_attr(not(any(feature = "hpsdr", feature = "soapy")), arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq_hz"))]
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
        /// Decimate the source to this sample rate, in Hz, before decoding.
        ///
        /// Must divide the source's own rate by a power of two, and the
        /// result must itself be a rate the channelizer supports. Omit to
        /// decode at the source's native rate.
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
        #[command(flatten)]
        filters: FilterOpts,
        /// Radio dial frequency, in Hz. See `run --dial-freq-hz`; without it
        /// an audio source's reported frequencies are baseband offsets.
        #[arg(long, value_parser = parse_dial_freq_hz, help_heading = "Audio input")]
        dial_freq_hz: Option<f64>,
        /// TOML config file whose `[input]`, `[spot]`, `[detector]` and
        /// `[decode]` tables supply what the flags leave unset.
        ///
        /// Falls back to $MANTA_CONFIG. Flags override the file, and
        /// MANTA_<TABLE>_<KEY> environment variables sit between the two.
        /// `soak` never starts the spot servers.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Ask a running daemon whether its RBN uplink is healthy.
    ///
    // MAN-44.
    /// Reads the `GET /status` document the daemon's metrics listener
    /// serves and prints a one-screen summary, or the raw JSON
    /// with --json. Exits 0 when every enabled uplink target is connected
    /// (or none is configured), 1 when any is down or flapping, and 2 when
    /// the daemon cannot be reached or its answer cannot be read.
    Status {
        /// Daemon config to read the metrics `bind_addr`/`metrics_port`
        /// from, with the same `MANTA_*` overlay `run` applies (falls back
        /// to `MANTA_CONFIG`). A wildcard `bind_addr` (`0.0.0.0`/`::`)
        /// dials loopback.
        // `--server-config` stays a hidden alias, as on `run`: the
        // deprecation notice names `--config`, so `status` must accept it.
        #[arg(long, alias = "server-config", conflicts_with = "addr")]
        config: Option<PathBuf>,
        /// Explicit `host:port` of the daemon's metrics listener (default
        /// 127.0.0.1:7302, the documented default metrics port).
        #[arg(long)]
        addr: Option<String>,
        /// Print the raw status JSON instead of the human-readable summary.
        #[arg(long)]
        json: bool,
        /// Give up after this many seconds if the daemon doesn't answer.
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
    },
    /// Check whether a source is hearing anything, and say what it found.
    ///
    /// Runs the real decode pipeline for --duration seconds, then reports
    /// track, SNR and spot counts plus a verdict. Tells "nothing is coming
    /// in" apart from "signal is coming in but nothing decodes" and from
    /// "working end to end" -- which a quiet `listen` run cannot.
    Doctor {
        /// How long to listen for, in seconds (3 to 3600).
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// Sound-card input device; matched by substring. Defaults to the
        /// system default input.
        #[arg(long, conflicts_with = "source", help_heading = "Audio input")]
        device: Option<String>,
        /// Replay a WAV file instead of listening to a live device.
        ///
        /// Decoded as fast as the machine manages, not in real time, so a
        /// 60-second file usually finishes sooner than that. Useful for
        /// demos and repeatable testing.
        #[arg(long, conflicts_with = "device", help_heading = "Audio input")]
        source: Option<PathBuf>,
        /// KiwiSDR receiver hostname. Requires --kiwi-freq-hz.
        #[arg(help_heading = "KiwiSDR source")]
        #[cfg_attr(all(feature = "hpsdr", feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "soapy_driver"], requires = "kiwi_freq_hz"))]
        #[cfg_attr(all(feature = "hpsdr", not(feature = "soapy")), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host"], requires = "kiwi_freq_hz"))]
        #[cfg_attr(all(not(feature = "hpsdr"), feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "soapy_driver"], requires = "kiwi_freq_hz"))]
        #[cfg_attr(not(any(feature = "hpsdr", feature = "soapy")), arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq_hz"))]
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
        /// Decimate the source to this sample rate, in Hz, before decoding.
        ///
        /// Must divide the source's own rate by a power of two, and the
        /// result must itself be a rate the channelizer supports. Omit to
        /// decode at the source's native rate.
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
        /// TOML config file whose `[input]`, `[spot]`, `[detector]` and
        /// `[decode]` tables supply what the flags leave unset.
        ///
        /// Falls back to $MANTA_CONFIG. Flags override the file, and
        /// MANTA_<TABLE>_<KEY> environment variables sit between the two.
        /// `doctor` never starts the spot servers.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Print the full report as one JSON object instead of a readable
        /// summary.
        #[arg(long, help_heading = "Output")]
        json: bool,
        #[command(flatten)]
        filters: FilterOpts,
    },
    /// Check or create a config file, without starting anything.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Measure decode quality on synthetic signals, no radio needed.
    #[command(subcommand)]
    Bench(BenchCommand),
}

// MAN-116: `manta bench sensitivity`; see bench.rs and
// docs/DECISIONS/2026-10-10-man116-sensitivity-benchmark.md.
#[derive(Subcommand)]
enum BenchCommand {
    /// Measure how recall and copy accuracy fall off as SNR drops.
    ///
    /// Generates synthetic CW recordings for every combination of channel
    /// condition, speed and SNR, decodes each one with the same pipeline
    /// `manta decode` uses, and prints a recall and character-error-rate
    /// table. Each recording holds ten stations sending "CQ CQ DE <call>
    /// <call> K" for its whole length. The same manta version and flags
    /// always print the same table, so a published result can be
    /// regenerated and checked.
    ///
    /// SNR is quoted the way RBN and CW Skimmer quote it: the transmitted
    /// carrier against the noise in a 500 Hz bandwidth.
    Sensitivity(bench::SensitivityArgs),
}

// MAN-76: `manta config check` / `manta config init`; see config_cmd.rs and
// docs/DECISIONS/2026-10-07-man76-config-check-init.md.
#[derive(Subcommand)]
enum ConfigCommand {
    /// Validate a config file and print the settings it resolves to.
    ///
    /// Runs every check `manta run` applies to its config, including the
    /// MANTA_* environment variables, without opening the receiver,
    /// binding a port or starting a server. Exits 0 when the config is
    /// valid and 1, naming the setting and the problem, when it is not.
    Check {
        /// Config file to check. Defaults to $MANTA_CONFIG, then to
        /// manta.toml in the current directory.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Write a new config file listing every setting at its default.
    ///
    /// Every setting is commented out, so the new file changes nothing
    /// until you edit it. Refuses to replace an existing file without
    /// --force.
    Init {
        /// Where to write the file; `-` prints it instead.
        #[arg(long, default_value = config_cmd::DEFAULT_PATH)]
        out: PathBuf,
        /// Replace the file if it already exists.
        #[arg(long)]
        force: bool,
    },
}

// Provenance for the flags below (contributor-facing, deliberately `//` and
// not `///`: clap republishes `///` verbatim into operator-facing `--help`,
// and MAN-135 is specifically about not doing that):
//   - freq_correction_ppm: config key `input.freq_correction_ppm`,
//     SPEC-decode-core.md §1.4. Legacy precedent is CW Skimmer/SkimSrv's
//     `FreqCalibration=` .ini key, which is a raw multiplier; this flag is
//     ppm per the spec's contract.
//   - allowlist: the Operator Watch List of ARCHITECTURE §6 / MAN-28.
//     Legacy precedent: CW Skimmer's Watch List (Aggregator manual App. A2).
//   - blocklist / notch: MAN-31. cty / scp: MAN-79.
/// Flags that decide which decodes become spots. Shared by `decode`,
/// `listen` and `soak`.
#[derive(clap::Args, Clone, Debug)]
#[command(next_help_heading = "Filtering")]
struct FilterOpts {
    /// Correct a receiver whose clock reads off-frequency, in parts per
    /// million.
    ///
    /// Decoded and spotted frequencies are scaled by 1 + ppm/1000000, so
    /// a receiver reading about 20 Hz high on 14 MHz is corrected with
    /// roughly -1.4. 0 disables the correction. `doctor` still prints the
    /// source's own centre frequency uncorrected; the correction only
    /// affects the frequencies it validates. This is the same idea as CW
    /// Skimmer's FreqCalibration setting, expressed in ppm rather than as
    /// a raw multiplier.
    ///
    /// Defaults to 0, or to `input.freq_correction_ppm` from --config.
    #[arg(
        long,
        value_parser = parse_freq_correction_ppm,
        allow_negative_numbers = true
    )]
    freq_correction_ppm: Option<f64>,
    /// Spot this callsign even when the usual validity checks reject it.
    ///
    /// Repeat the flag for more than one call. Use it for a station you
    /// know is on the air but that manta keeps rejecting -- an unusual
    /// prefix, a special-event call. Bypasses callsign-grammar and
    /// country checks and the repeat-before-spotting rule. --blocklist and
    /// --notch still win: a call named by both is not spotted.
    #[arg(long)]
    allowlist: Vec<String>,
    /// File of callsigns never to spot, one per line.
    #[arg(long)]
    blocklist: Option<PathBuf>,
    /// File of frequency ranges never to spot, one `low_hz-high_hz` range
    /// per line.
    #[arg(long)]
    notch: Option<PathBuf>,
    /// Country-prefix file (AD1C's cty.dat) to check callsigns against, in place of the copy built into manta.
    ///
    /// A call whose prefix this file does not list is not spotted unless --allowlist names it,
    /// so a new prefix or DXpedition is rejected until the file lists it. Download the current
    /// file from https://www.country-files.com/cty/cty.dat to accept such calls without waiting
    /// for a new manta release. Defaults to the built-in copy, or to `spot.cty_path` from --config.
    #[arg(long)]
    cty: Option<PathBuf>,
    /// Known-callsign list (MASTER.SCP) to use in place of the copy built into manta.
    ///
    /// A call on the list is spotted with more confidence; a call missing from it can still
    /// be spotted. Download the current list from https://www.supercheckpartial.com/MASTER.SCP.
    /// Defaults to the built-in copy, or to `spot.scp_path` from --config.
    #[arg(long)]
    scp: Option<PathBuf>,
}

/// KiwiSDR connection flags, grouped to keep `open_source`'s arity down.
/// `Clone`: MAN-73's `LiveSourceSpec` carries one of these so a live source
/// can be re-opened by `ReconnectingSource` after a loss.
#[derive(Clone)]
struct KiwiOpts {
    host: Option<String>,
    port: u16,
    freq: Option<f64>,
    password: String,
}

/// SoapySDR connection flags (feature `soapy`), grouped for the same reason.
#[cfg(feature = "soapy")]
#[derive(Clone)]
struct SoapyOpts {
    driver: Option<String>,
    freq: Option<f64>,
    rate: Option<f64>,
    gain: Option<f64>,
}

/// HPSDR/Hermes connection flags (feature `hpsdr`), grouped for the same reason.
#[cfg(feature = "hpsdr")]
#[derive(Clone)]
struct HpsdrOpts {
    host: Option<String>,
    port: u16,
    freq: Option<f64>,
    rate: Option<f64>,
}

/// Everything needed to (re)open the configured live input. MAN-73: a
/// reconnectable kind (every variant but `File`) is re-opened by
/// `ReconnectingSource` after a loss; `File` (deterministic replay) never
/// is. `name()` must keep returning the same label `set_source_health`
/// always used for that kind, since dashboards/scrapes key on it.
#[derive(Clone)]
enum LiveSourceSpec {
    Kiwi(KiwiOpts),
    #[cfg(feature = "soapy")]
    Soapy(SoapyOpts),
    #[cfg(feature = "hpsdr")]
    Hpsdr(HpsdrOpts),
    AudioDevice(Option<String>),
    /// `source_iq` selects `WavIqSource` over `AudioIqSource`; see
    /// `open_audio_source`.
    File {
        path: PathBuf,
        source_iq: bool,
    },
}

impl LiveSourceSpec {
    fn name(&self) -> &'static str {
        match self {
            LiveSourceSpec::Kiwi(_) => "kiwi",
            #[cfg(feature = "soapy")]
            LiveSourceSpec::Soapy(_) => "soapy",
            #[cfg(feature = "hpsdr")]
            LiveSourceSpec::Hpsdr(_) => "hpsdr",
            LiveSourceSpec::AudioDevice(_) => "audio",
            LiveSourceSpec::File { .. } => "file",
        }
    }

    /// Kiwi/Soapy/Hpsdr, or an IQ WAV whose sidecar reports a real (> 0)
    /// center frequency: a source that already knows its own tuned
    /// frequency, so `--dial-freq-hz` is an override and not a requirement.
    fn is_rf_aware(&self) -> bool {
        match self {
            LiveSourceSpec::AudioDevice(_) => false,
            LiveSourceSpec::File { path, source_iq } => {
                source_iq_has_real_rf_center(path, *source_iq)
            }
            _ => true,
        }
    }

    /// Whether a loss of this source should be retried by
    /// `ReconnectingSource` (MAN-73) rather than ending the daemon. File
    /// replay's errors and EOF are deterministic and must reach `listen()`
    /// unchanged -- see AGENTS.md's byte-identical-replay requirement.
    fn is_reconnectable(&self) -> bool {
        !matches!(self, LiveSourceSpec::File { .. })
    }

    /// Open (or re-open) the described source, applying `maybe_decimate`
    /// for `capture_rate_hz` and then `FixedCenterFreqSource` when
    /// `dial_freq_hz` is set -- so every call through this one method
    /// produces a fully-composed source and nothing above it (in particular
    /// `ReconnectingSource`) needs to know about either wrapper. A reopen
    /// therefore also starts a fresh decimator, with no filter state carried
    /// across the outage.
    fn open(
        &self,
        capture_rate_hz: Option<f64>,
        dial_freq_hz: Option<f64>,
    ) -> Result<Box<dyn IqSource>> {
        let src: Box<dyn IqSource> = match self {
            LiveSourceSpec::Kiwi(kiwi) => {
                let host = kiwi
                    .host
                    .as_deref()
                    .expect("LiveSourceSpec::Kiwi always carries a host");
                let freq = kiwi
                    .freq
                    .ok_or_else(|| anyhow!("--kiwi-freq-hz is required with --kiwi-host"))?;
                Box::new(manta_input::kiwi::KiwiIqSource::connect(
                    host,
                    kiwi.port,
                    freq,
                    &kiwi.password,
                )?)
            }
            #[cfg(feature = "soapy")]
            LiveSourceSpec::Soapy(soapy) => {
                let driver = soapy
                    .driver
                    .as_deref()
                    .expect("LiveSourceSpec::Soapy always carries a driver");
                let freq = soapy
                    .freq
                    .ok_or_else(|| anyhow!("--soapy-freq-hz is required with --soapy-driver"))?;
                let rate = soapy
                    .rate
                    .ok_or_else(|| anyhow!("--soapy-rate-hz is required with --soapy-driver"))?;
                Box::new(manta_input::soapy::SoapySdrIqSource::open(
                    driver, rate, freq, soapy.gain,
                )?)
            }
            #[cfg(feature = "hpsdr")]
            LiveSourceSpec::Hpsdr(hpsdr) => {
                let host = hpsdr
                    .host
                    .clone()
                    .expect("LiveSourceSpec::Hpsdr always carries a host");
                let freq = hpsdr
                    .freq
                    .ok_or_else(|| anyhow!("--hpsdr-freq-hz is required with --hpsdr-host"))?;
                let rate = hpsdr
                    .rate
                    .ok_or_else(|| anyhow!("--hpsdr-rate-hz is required with --hpsdr-host"))?;
                let cfg = manta_input::hpsdr::HpsdrConfig {
                    host,
                    port: hpsdr.port,
                    ddc_count: 1,
                    sample_rate_hz: rate,
                    center_freq_hz: vec![freq],
                };
                let mut sources = manta_input::hpsdr::HpsdrDevice::open(cfg)?;
                Box::new(sources.remove(0))
            }
            // Each (re)open gets its own one-shot silence notice (MAN-131).
            LiveSourceSpec::AudioDevice(device) => Box::new(
                manta_input::AudioIqSource::from_device(device.as_deref())?.with_silence_notice(
                    Box::new(|device| eprintln!("{}", audio_silence_warning(device))),
                ),
            ),
            LiveSourceSpec::File { path, source_iq } => {
                open_audio_source(None, Some(path.clone()), *source_iq, None)?
            }
        };
        let src = maybe_decimate(src, capture_rate_hz)?;
        Ok(match dial_freq_hz {
            Some(freq_hz) => Box::new(FixedCenterFreqSource {
                inner: src,
                freq_hz,
            }),
            None => src,
        })
    }
}

/// `--source <path>.wav` covers two distinct file formats sharing the same
/// flag: a mono real-audio recording (M1 "Audio passband" input, e.g.
/// captured from a rig's RX line-out -- decoded via `AudioIqSource`'s
/// Hilbert transform, hard-pinned to 48000 Hz) and a 2-channel raw complex-
/// IQ recording at any rate (`WavIqSource`, the same format `decode`/
/// `oracle` already read directly, and the only format that can feed
/// `--capture-rate-hz` decimation for file replay -- MAN-169 round-2 Codex
/// finding: routing every `--source` WAV through `AudioIqSource`
/// unconditionally meant a 96/192 kS/s IQ replay could never reach here,
/// since `AudioIqSource::from_wav_file` rejects every rate but 48000).
/// Disambiguated by the explicit `--source-iq` flag, not channel count
/// (MAN-169 round-4 Codex finding: a 2-channel WAV is ambiguous between a
/// genuine IQ capture and an ordinary stereo real-audio recording -- header
/// shape alone can't tell them apart, so guessing from it silently
/// misinterpreted stereo audio as IQ). `--source-iq` set routes through
/// `WavIqSource`; unset (the default, matching this flag's pre-round-2
/// behavior exactly) always falls through to `AudioIqSource::from_wav_file`,
/// so its own existing validation error (not a new one invented here) is
/// what the operator sees for a rate/format mismatch.
fn open_audio_source(
    device: Option<String>,
    source: Option<PathBuf>,
    source_iq: bool,
    dial_freq_hz: Option<f64>,
) -> Result<Box<dyn IqSource>> {
    Ok(match source {
        // A raw-IQ WAV is a `WavIqSource`, not an `AudioIqSource`, so it
        // has no `with_center_freq_hz`: `--dial-freq-hz` is applied with
        // the same override wrapper RF-aware sources use. (MAN-34 moved
        // audio sources to a native dial frequency; this keeps the IQ
        // replay path honoring `--dial-freq-hz` exactly as before.)
        Some(path) if source_iq => {
            let src: Box<dyn IqSource> = Box::new(manta_input::WavIqSource::open(&path)?);
            match dial_freq_hz {
                Some(freq_hz) => Box::new(FixedCenterFreqSource {
                    inner: src,
                    freq_hz,
                }),
                None => src,
            }
        }
        Some(path) => audio_with_dial(
            manta_input::AudioIqSource::from_wav_file(&path)?,
            dial_freq_hz,
        )?,
        None => audio_with_dial(
            manta_input::AudioIqSource::from_device(device.as_deref())?,
            dial_freq_hz,
        )?,
    })
}

/// Gives an `AudioIqSource` the operator's dial frequency natively
/// (`AudioIqSource::with_center_freq_hz`, MAN-34), or returns it unchanged.
fn audio_with_dial(
    src: manta_input::AudioIqSource,
    dial_freq_hz: Option<f64>,
) -> Result<Box<dyn IqSource>> {
    Ok(Box::new(match dial_freq_hz {
        Some(hz) => src.with_center_freq_hz(hz)?,
        None => src,
    }))
}

/// Whether `source` (only meaningful when `source_iq` is set -- see
/// `open_audio_source`) carries a real RF center frequency once actually
/// opened and its sidecar parsed, not merely because a `<stem>.json` file
/// happens to exist (MAN-169 round-4 Codex finding: a sidecar existing
/// with `center_freq_hz: 0.0` -- IqSource's own "unknown center" sentinel
/// -- is indistinguishable from "no sidecar" once parsed, so existence
/// alone isn't enough to bypass the --dial-freq-hz guard below). Requires
/// strictly positive, not just nonzero (MAN-169 round-5 Codex finding: a
/// negative `center_freq_hz` passed the old `!= 0.0` check and would have
/// published negative/invalid RF frequencies through spot outputs) -- an
/// RF dial frequency in this domain is never zero or negative.
fn source_iq_has_real_rf_center(path: &Path, source_iq: bool) -> bool {
    if !source_iq {
        return false;
    }
    manta_input::WavIqSource::open(path)
        .map(|src| src.center_freq_hz() > 0.0)
        .unwrap_or(false)
}

/// Overrides an inner source's `center_freq_hz()` with a fixed value.
/// Audio sources now carry the operator's dial frequency natively
/// (`open_audio_source` -> `AudioIqSource::with_center_freq_hz`, MAN-34);
/// this decorator remains for the RF-aware sources (KiwiSDR/SoapySDR/HPSDR,
/// and IQ WAVs with a real sidecar frequency), which already report a tuned
/// frequency of their own that `--dial-freq-hz` is allowed to supersede, and
/// for raw-IQ WAV replay, which has no native dial setter. See
/// `--dial-freq-hz`.
struct FixedCenterFreqSource {
    inner: Box<dyn IqSource>,
    freq_hz: f64,
}

impl IqSource for FixedCenterFreqSource {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.freq_hz
    }

    /// Only the centre frequency is overridden -- the wrapped source's own
    /// RF passband must still reach `SKIMMER/SETT`, or wrapping a
    /// resampling source (KiwiSDR) or a rig-audio source in
    /// `--dial-freq-hz` would silently re-introduce the "advertise the
    /// processing rate" bug this method exists to prevent (MAN-86 review).
    /// The wrapped bounds are offsets from the centre, so overriding the
    /// centre alone relocates them correctly.
    fn rf_passband_hz(&self) -> (f64, f64) {
        self.inner.rf_passband_hz()
    }

    fn read(&mut self, buf: &mut [num_complex::Complex32]) -> Result<usize> {
        self.inner.read(buf)
    }

    fn confirmed_live_handle(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.inner.take_discontinuity()
    }

    fn health_counters(&self) -> Option<std::sync::Arc<manta_input::InputHealthCounters>> {
        self.inner.health_counters()
    }
}

/// Warn on stderr when the opened source is audio-derived (not RF-aware)
/// and no `--dial-freq-hz` was given -- MAN-34. A missing dial frequency in
/// this case means every downstream frequency is a bare baseband offset,
/// not an absolute RF frequency. This is a warning, not a `bail!`: a bare
/// `manta listen --device` watching decoded text locally, with no interest
/// in frequency, is a legitimate and documented use (README.md, the M1
/// manual-acceptance runbook). The harm this ticket names -- a wrong
/// frequency reaching the network -- is already a hard error via
/// `--config`'s own check; this covers the local case without
/// regressing it.
fn warn_if_audio_source_has_no_rf_reference(has_rf_aware_source: bool, dial_freq_hz: Option<f64>) {
    if !has_rf_aware_source && dial_freq_hz.is_none() {
        eprintln!(
            "warning: no --dial-freq-hz given for an audio source -- \
             reported frequencies are baseband offsets within the \
             audio passband, not absolute RF frequencies. Pass the \
             rig's dial frequency, e.g. --dial-freq-hz 14030000."
        );
    }
}

/// MAN-131: a live audio input delivering exact zeros is, on macOS, the
/// usual sign of a denied microphone permission (cpal cannot see TCC; the
/// stream opens and the callback hands over silence). A warning, not an
/// error: a muted input is a legitimate state, and `doctor` still reports
/// NO_SIGNAL on its own.
fn audio_silence_warning(device: &str) -> String {
    format!(
        "warning: audio input {device} is delivering digital silence at {} Hz \
         (every sample is exactly zero) -- {}",
        manta_input::TARGET_RATE_HZ,
        manta_input::audio::audio_input_hint()
    )
}

/// MAN-131: `decode`'s analogue of `warn_if_audio_source_has_no_rf_reference`.
/// `sidecar_center_hz` is `None` when `<stem>.json` does not exist.
fn recording_center_warning(wav: &Path, sidecar_center_hz: Option<f64>) -> Option<String> {
    let sidecar = manta_input::sidecar_path(wav);
    let why = match sidecar_center_hz {
        Some(hz) if hz > 0.0 => return None,
        Some(hz) => format!(
            "sidecar {} gives center_freq_hz = {hz}, not an RF frequency",
            sidecar.display()
        ),
        None => format!("no sidecar {} next to {}", sidecar.display(), wav.display()),
    };
    Some(format!(
        "warning: {why} -- reported frequencies are baseband offsets from the \
         recording's centre, not absolute RF frequencies. Pass the centre \
         frequency, e.g. --center-freq-hz 14000000."
    ))
}

/// Wrap `src` in a `DecimatingSource` targeting `capture_rate_hz`, unless
/// it's `None` or already matches the source's native rate (a no-op in
/// either case -- omitting `--capture-rate-hz` reproduces today's exact
/// behavior). Applied uniformly regardless of source type (kiwi/soapy/
/// hpsdr/audio/file replay), mirroring how `dial_freq_hz`'s
/// `FixedCenterFreqSource` wrap is already applied uniformly below.
fn maybe_decimate(
    src: Box<dyn IqSource>,
    capture_rate_hz: Option<f64>,
) -> Result<Box<dyn IqSource>> {
    match capture_rate_hz {
        Some(target) if (target - src.sample_rate()).abs() > 1e-6 => {
            Ok(Box::new(manta_input::DecimatingSource::new(src, target)?))
        }
        _ => Ok(src),
    }
}

/// Lower bound for `--capture-rate-hz`. `manta_dsp::decimate::Decimator::
/// new` rejects any target rate whose channel count (`fs_out/93.75`) is
/// below 4 (a `Channelizer` with `hop = n/4 == 0` never terminates its
/// read-advancing loop -- MAN-169 whole-branch review finding). That floor
/// alone is 4*93.75 = 375 Hz, but this constant is set well above it (same
/// 1000 Hz floor `--hpsdr-rate` already uses via `MIN_HPSDR_RATE_HZ`) as a
/// second, earlier layer of defense: rejecting a degenerate
/// `--capture-rate-hz` here, at CLI-parse time, fails before any live SDR
/// device is opened/activated and produces a clearer error message than
/// the DSP-layer construction error would.
const MIN_CAPTURE_RATE_HZ: f64 = 1_000.0;

/// Clap value parser for `--capture-rate-hz`: rejects non-finite (NaN/
/// infinity) and implausibly small values at CLI-parse time, before any
/// live SDR device is opened -- matching `parse_hpsdr_rate_hz`'s pattern.
/// The full validation (evenly divides the source's native rate by a power
/// of two, and the result is itself a valid channelizer table rate) still
/// happens later in `DecimatingSource::new`/`Decimator::new`, once the
/// source's actual native rate is known; this is a cheap, early rejection
/// of obviously-bad input (e.g. a negative or degenerately tiny value that
/// would otherwise reach `Channelizer::new` with `hop == 0` and hang).
fn parse_capture_rate_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e| format!("invalid --capture-rate-hz {s:?}: {e}"))?;
    check_capture_rate_hz("--capture-rate-hz", hz)
}

/// `parse_capture_rate_hz`'s check, shared with `input.capture_rate_hz`
/// (MAN-261); `name` is the flag or config key the message cites.
fn check_capture_rate_hz(name: &str, hz: f64) -> std::result::Result<f64, String> {
    if !hz.is_finite() || hz < MIN_CAPTURE_RATE_HZ {
        return Err(format!(
            "{name} must be a finite number of Hz >= {MIN_CAPTURE_RATE_HZ}, got {hz}"
        ));
    }
    Ok(hz)
}

/// Clap value parser for `--freq-correction-ppm`: fails at CLI-parse time
/// (before opening any source) rather than deep in the pipeline, using the
/// same validation `manta_spot::calibration_factor_from_ppm` applies
/// (MAN-29 review).
fn parse_freq_correction_ppm(s: &str) -> std::result::Result<f64, String> {
    let ppm: f64 = s
        .parse()
        .map_err(|e| format!("invalid --freq-correction-ppm {s:?}: {e}"))?;
    check_freq_correction_ppm(ppm)
}

/// `parse_freq_correction_ppm`'s check, shared with
/// `input.freq_correction_ppm` (MAN-261). The message already names
/// `freq_correction_ppm`, so it serves both spellings unchanged.
fn check_freq_correction_ppm(ppm: f64) -> std::result::Result<f64, String> {
    manta_spot::calibration_factor_from_ppm(ppm).map_err(|e| e.to_string())?;
    Ok(ppm)
}

/// `Engine::Hsmm` (Task 8: `TrackDecoder::push_hop_hsmm`) is a real, fully
/// implemented and reviewed engine as of Task 11 -- it parses through like
/// `legacy`/`edge-legacy`. It remains experimental (its stage-2 gate was
/// measured and failed, docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md),
/// but that's a deployment/support-posture question for operators choosing
/// `--engine hsmm` explicitly, not a reason to reject it at the CLI.
fn parse_engine(s: &str) -> std::result::Result<Engine, String> {
    s.parse()
}

/// Derives the replay session's wall-clock epoch (fed to `SpotBus`, and
/// from there into every JSON `timestamp`/RBN Zulu field a client
/// observes) from the replayed file's own filesystem modification time.
/// This satisfies two constraints an earlier version traded off against
/// each other across several review rounds: it must be a GENUINE
/// wall-clock instant (a file-content hash reinterpreted as nanoseconds
/// produced technically-unique but fabricated dates spanning 1970-2554 --
/// round 5), and it must be STABLE across reruns of the same replay file
/// (unconditionally using `SystemTime::now()` made every rerun's JSON/RBN
/// output non-reproducible -- round 6's finding). A file's mtime is a real
/// system fact -- not perfect (it's "when this file was last written,"
/// not "when the recording happened"), but honest and non-arbitrary,
/// unlike either prior approach -- and it doesn't change between two
/// reads of the same untouched file. Session-identity uniqueness (the
/// separate concern that originally motivated the content hash) is
/// handled by `session_nonce_for_replay_path` below, independently of
/// this epoch.
fn epoch_for_replay_path(path: &std::path::Path) -> Result<std::time::SystemTime> {
    let mtime = std::fs::metadata(path)
        .with_context(|| {
            format!(
                "reading metadata for {} to derive its replay epoch",
                path.display()
            )
        })?
        .modified()
        .with_context(|| {
            format!(
                "{} has no modification time on this platform",
                path.display()
            )
        })?;
    // A Unix filesystem can represent a pre-1970 mtime. Left unvalidated,
    // this SystemTime flows all the way to SpotBus::unix_ts_for, whose
    // `.duration_since(UNIX_EPOCH).expect(...)` panics on the very first
    // spot delivered to any client -- reject it here, at startup, with a
    // clear error instead (round-8 review finding).
    if mtime < std::time::SystemTime::UNIX_EPOCH {
        bail!(
            "{} has a modification time before the Unix epoch (1970-01-01), which can't be used \
             as a replay epoch -- pass --replay-epoch explicitly instead",
            path.display()
        );
    }
    Ok(mtime)
}

/// Resolves the wall-clock epoch fed to `SpotBus`: an explicit
/// `--replay-epoch` wins when given AND this is a replay session (the
/// escape hatch for a copy/download that didn't preserve the file's
/// mtime); a replay session with no explicit epoch falls back to the
/// file's own mtime; a live session (`replay_path` is `None`) always uses
/// the current time, ignoring `replay_epoch` entirely -- matching the
/// flag's own documented "ignored for a live source" contract. Applying it
/// to a live session (round-8 review finding) would publish spots with a
/// fabricated historical timestamp and derive the live session_nonce from
/// that same fixed value, breaking the "two live sessions started within
/// the same wall-clock second don't collide" guarantee.
fn resolve_epoch(
    replay_path: Option<&std::path::Path>,
    replay_epoch: Option<i64>,
) -> Result<std::time::SystemTime> {
    match (replay_path, replay_epoch) {
        (Some(_), Some(secs)) => {
            Ok(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
        }
        (Some(path), None) => epoch_for_replay_path(path),
        (None, _) => Ok(std::time::SystemTime::now()),
    }
}

/// Derives a stable, recording-specific session nonce from a WAV file's
/// CONTENT (not its path): same bytes -> same nonce on every run
/// (deterministic spot `id`s across reruns) *regardless of where the file
/// lives* -- a different checkout, mount point, rename, or machine must
/// not change it, since it's the same recording. Two different recordings
/// hash to (almost certainly) different nonces, so their spots don't
/// collide in JSON `id` even at the same track/sample position. Uses
/// FNV-1a-64 (`hash = (hash XOR byte) * FNV_PRIME`, from the published
/// offset basis) -- a small, independently specified, versioned algorithm
/// with no dependency on any std or compiler internals -- NOT `std`'s
/// `DefaultHasher`, whose own docs disclaim any stability guarantee across
/// Rust releases (round-12 review finding: the same replay file could
/// hash differently across builds/toolchains, and this value feeds every
/// JSON spot `id`). This keeps the nonce stable across different
/// builds/toolchains too, not just within one binary -- the same
/// determinism guarantee this repo's own "3 runs, same binary ->
/// identical output" CI rule already relies on (that rule covers `manta
/// decode --json`'s sample-relative Spot output, which never carries a
/// wall-clock field at all -- see wiki/pages/determinism.md; it does not
/// extend to manta-server's live wall-clock `timestamp`/RBN Zulu fields,
/// which SpotBus's `epoch` -- always real `SystemTime::now()`, see
/// `start_spot_server` -- covers separately and deliberately does NOT
/// reproduce across reruns).
///
/// This value is ONLY a session nonce (`SpotBus::session_nonce`), never
/// fed into `SpotBus::epoch`/`unix_ts_for` -- an earlier version derived
/// both from this same hash, which meant a replayed file's JSON
/// `timestamp`/RBN Zulu time was a fabricated date with no relation to
/// real time (nanoseconds-since-Unix-epoch reinterpreted as a wall clock).
/// A network client's `timestamp` must always be truthful.
fn session_nonce_for_replay_path(path: &std::path::Path) -> Result<u128> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to derive its replay identity", path.display()))?;
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {} to derive its replay identity", path.display()))?;
        if n == 0 {
            break;
        }
        for &byte in &buf[..n] {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
    }
    Ok(hash as u128)
}

/// Clap value parser for `--dial-freq-hz`: rejects non-finite (NaN/infinity)
/// and non-positive values at CLI-parse time, before they're baked into
/// `FixedCenterFreqSource` and silently propagate into malformed RBN/JSON
/// frequency fields (e.g. a literal `NaN`, or a "0"/`band: "unknown"` from
/// a zero or negative dial frequency).
/// `--capture-start` for `Command::Oracle`: ISO-8601 UTC (e.g.
/// "2025-11-29T00:00:00Z"), parsed to Unix epoch seconds via
/// `manta_testkit::oracle::parse_utc_timestamp` -- anchors RBN spot times
/// to the actual recording start (Codex review, PR #161).
fn parse_capture_start(s: &str) -> std::result::Result<i64, String> {
    manta_testkit::oracle::parse_utc_timestamp(s)
        .map_err(|e| format!("invalid --capture-start {s:?}: {e}"))
}

// Codex review, PR #161 round 2: a negative --window-s makes run_oracle's
// s1 < s0, panicking on the iq[s0..s1] slice; zero/NaN silently produces an
// empty window; infinity decodes the whole capture per spot. Reject at the
// CLI boundary too (run_oracle itself now also validates -- see that
// function's doc comment -- but a CLI-level rejection gives a clap usage
// error instead of a bail! from inside the command's execution path).
fn parse_window_s(s: &str) -> std::result::Result<f64, String> {
    let window_s: f64 = s
        .parse()
        .map_err(|e| format!("invalid --window-s {s:?}: {e}"))?;
    if !window_s.is_finite() || window_s <= 0.0 {
        return Err(format!(
            "--window-s must be finite and positive, got {window_s}"
        ));
    }
    Ok(window_s)
}

fn parse_dial_freq_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e| format!("invalid --dial-freq-hz {s:?}: {e}"))?;
    check_dial_freq_hz("--dial-freq-hz", hz)
}

/// `decode --center-freq-hz` (MAN-131). The parse error is just the
/// `ParseFloatError` text: clap already prints the flag and the value.
fn parse_center_freq_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    check_dial_freq_hz("--center-freq-hz", hz)
}

/// `parse_dial_freq_hz`'s check, shared with `input.center_freq_hz`
/// (MAN-261) and `decode --center-freq-hz` (MAN-131); `name` is the flag
/// or config key the message cites.
fn check_dial_freq_hz(name: &str, hz: f64) -> std::result::Result<f64, String> {
    if !hz.is_finite() || hz <= 0.0 {
        return Err(format!(
            "{name} must be a finite, positive number of Hz, got {hz}"
        ));
    }
    Ok(hz)
}

/// Lower bound for `--hpsdr-rate-hz`: comfortably below every real HPSDR/
/// Hermes sample rate (48 kHz-1.536 MHz) while still guaranteeing
/// `GapDetector::new`'s `Duration::from_secs_f64(126.0 / sample_rate_hz)`
/// (126 = `USB_FRAMES_PER_PACKET * samples_per_usb_frame(1)`, this CLI's
/// fixed single-DDC case) stays far inside `Duration`'s representable range
/// -- a finite, positive but tiny rate like `1e-20` still overflows it and
/// panics (round-2 review finding: the round-1 fix rejected NaN/inf/<=0 but
/// not an unrealistically small positive value).
const MIN_HPSDR_RATE_HZ: f64 = 1_000.0;
/// Upper bound for `--hpsdr-rate-hz`: generous headroom above any real
/// HPSDR/Hermes rate, purely to keep the range symmetric and reject
/// obviously-wrong input (e.g. a value with stray zeros) rather than to
/// pin an exact hardware ceiling this CLI layer has no authority over.
const MAX_HPSDR_RATE_HZ: f64 = 10_000_000.0;

/// Default HPSDR/Hermes control port for `[input] type = "hpsdr"`, defined
/// on every build so the table validates identically with or without
/// `--features hpsdr` (MAN-261); pinned to manta-input's own constant below.
const HPSDR_CONTROL_PORT: u16 = 1024;
#[cfg(feature = "hpsdr")]
const _: () = assert!(HPSDR_CONTROL_PORT == manta_input::hpsdr::CONTROL_PORT);

/// Clap value parser for `--hpsdr-rate-hz`: rejects non-finite (NaN/infinity)
/// and out-of-range values at CLI-parse time. `HpsdrConfig::validate`'s own
/// `validate_ddc_config` bandwidth check silently passes a NaN rate
/// (comparisons against NaN are always false), and the value then reaches
/// `GapDetector::new`'s `Duration::from_secs_f64(samples_per_packet as f64
/// / sample_rate_hz)`, which panics on NaN or an unrepresentable Duration
/// -- caught here instead, before any source is opened, matching
/// `parse_dial_freq_hz`'s pattern.
#[cfg(feature = "hpsdr")]
fn parse_hpsdr_rate_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e| format!("invalid --hpsdr-rate-hz {s:?}: {e}"))?;
    check_hpsdr_rate_hz("--hpsdr-rate-hz", hz)
}

/// `parse_hpsdr_rate_hz`'s check, un-gated so `[input] type = "hpsdr"`
/// validates identically on every build (MAN-261).
fn check_hpsdr_rate_hz(name: &str, hz: f64) -> std::result::Result<f64, String> {
    if !hz.is_finite() || !(MIN_HPSDR_RATE_HZ..=MAX_HPSDR_RATE_HZ).contains(&hz) {
        return Err(format!(
            "{name} must be a finite number of Hz between {MIN_HPSDR_RATE_HZ} and \
             {MAX_HPSDR_RATE_HZ}, got {hz}"
        ));
    }
    Ok(hz)
}

/// Clap value parser for `--hpsdr-freq-hz`: rejects non-finite (NaN/infinity)
/// and non-positive values at CLI-parse time, matching
/// `parse_dial_freq_hz`'s pattern (round-2 review finding: an unvalidated
/// `--hpsdr-freq-hz NaN`/`inf` reaches `HpsdrConfig.center_freq_hz`, which
/// is only length-checked, not value-checked, and then propagates into
/// every emitted spot's frequency field).
#[cfg(feature = "hpsdr")]
fn parse_hpsdr_freq_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e| format!("invalid --hpsdr-freq-hz {s:?}: {e}"))?;
    check_hpsdr_freq_hz("--hpsdr-freq-hz", hz)
}

/// `parse_hpsdr_freq_hz`'s check, un-gated like `check_hpsdr_rate_hz`.
fn check_hpsdr_freq_hz(name: &str, hz: f64) -> std::result::Result<f64, String> {
    if !hz.is_finite() || hz <= 0.0 {
        return Err(format!(
            "{name} must be a finite, positive number of Hz, got {hz}"
        ));
    }
    Ok(hz)
}

/// Upper bound for `--replay-epoch`: 2100-01-01T00:00:00Z in Unix seconds.
/// No real recording needs an epoch beyond this; the bound exists purely
/// to keep `secs` far away from the range where `SpotBus::unix_ts_for`'s
/// `epoch + elapsed` (`SystemTime` arithmetic) could overflow and panic on
/// the first spot delivered to any client (round-9 review finding) --
/// generous, not tight, since the actual overflow point depends on the
/// platform's `SystemTime` representation and isn't worth pinning exactly.
const MAX_REPLAY_EPOCH_SECS: i64 = 4_102_444_800;

/// Clap value parser for `--replay-epoch`: Unix seconds, bounded to a
/// plausible calendar range (non-negative, before `MAX_REPLAY_EPOCH_SECS`)
/// -- a `SystemTime` before `UNIX_EPOCH` isn't representable via the
/// `UNIX_EPOCH + Duration` construction this flag feeds, and an
/// unrealistically large value risks overflowing later `SystemTime`
/// arithmetic instead of failing cleanly here. Deliberately a plain
/// integer, not RFC3339 or similar -- avoids pulling in a date/time-
/// parsing dependency for one CLI flag; any real timestamp source (a
/// recording tool's own metadata, `date +%s`) can produce Unix seconds
/// directly.
fn parse_replay_epoch(s: &str) -> std::result::Result<i64, String> {
    let secs: i64 = s
        .parse()
        .map_err(|e| format!("invalid --replay-epoch {s:?}: {e}"))?;
    check_replay_epoch("--replay-epoch", secs)
}

/// `parse_replay_epoch`'s check, shared with `input.replay_epoch`
/// (MAN-261); `name` is the flag or config key the message cites.
fn check_replay_epoch(name: &str, secs: i64) -> std::result::Result<i64, String> {
    if !(0..=MAX_REPLAY_EPOCH_SECS).contains(&secs) {
        return Err(format!(
            "{name} must be Unix seconds between 0 and {MAX_REPLAY_EPOCH_SECS} \
             (2100-01-01), got {secs}"
        ));
    }
    Ok(secs)
}

/// Strips a leading UTF-8 BOM (`\u{feff}`), common in Windows-authored text
/// files -- `str::trim` does not remove it, so left unstripped it corrupts
/// the first line's parse (a blocklist callsign that never matches, or a
/// notch range silently rejected).
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Builds a `PipelineConfig` from the CLI's shared flags: the MAN-29
/// frequency-calibration correction, the MAN-28 operator Watch List, and
/// the MAN-31 operator suppression lists. Each is optional/repeatable; an
/// absent one leaves that list empty, matching `PipelineConfig`'s own
/// defaults.
fn build_pipeline_config(
    freq_correction_ppm: f64,
    spot: &SpotResolved,
    detector: manta_engine::DetectorConfig,
    engine: Engine,
) -> Result<PipelineConfig> {
    let mut cfg = PipelineConfig {
        freq_correction_ppm,
        allowlist: spot.allowlist.clone(),
        detector,
        ..Default::default()
    };
    cfg.decode.engine = engine;
    if let Some(path) = &spot.blocklist {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading blocklist file {}", path.display()))?;
        cfg.blocklist = manta_engine::Blocklist::parse(strip_bom(&text));
    }
    if let Some(path) = &spot.notch {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading notch file {}", path.display()))?;
        cfg.notch = manta_engine::NotchList::parse(strip_bom(&text));
    }
    if let Some(path) = &spot.cty {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading cty.dat file {}", path.display()))?;
        let table = manta_spot::cty::Table::parse(strip_bom(&text));
        if table.is_empty() {
            bail!("cty.dat file {} lists no callsign prefixes; is it AD1C's cty.dat (not cty.csv or a saved web page)?", path.display());
        }
        cfg.cty = Some(std::sync::Arc::new(table));
    }
    if let Some(path) = &spot.scp {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading master.scp file {}", path.display()))?;
        let set = manta_spot::scp::Set::parse(strip_bom(&text));
        if set.is_empty() {
            bail!("master.scp file {} lists no callsigns", path.display());
        }
        cfg.scp = Some(std::sync::Arc::new(set));
    }
    Ok(cfg)
}

/// Range-checks a `[decode]` table (SPEC v2 §7) loaded by `config::load`
/// from `origin` (the config file's path, or "environment"). Every message
/// names `origin`; `config::load` adds no context layer on top.
fn validate_decode_config(
    cfg: manta_decode::decoder::DecodeConfig,
    origin: &str,
) -> Result<manta_decode::decoder::DecodeConfig> {
    // Codex review, PR #161: `fallback_hops = 0` deserializes successfully (it's a
    // plain u32 with no serde-level range check) but Evidence::push's anchor
    // computation divides by it (`self.hop_out % self.cfg.fallback_hops as u64`),
    // panicking on the first hop for both edge-legacy and hsmm. Reject it here,
    // at the one place a `[decode]` table from disk enters the process, rather
    // than scattering a zero-guard into the hot per-hop path.
    if cfg.evidence.fallback_hops == 0 {
        bail!(
            "[decode] fallback_hops must be nonzero in {} (0 would divide by zero on the \
             first evidence hop)",
            origin
        );
    }
    // Codex review, PR #161 round 2: `tau_hi_bounds_ms = [400, 100]` (or any
    // non-finite/inverted pair) deserializes successfully, but
    // `Demod::set_dit_ms` later passes it straight to `f64::clamp`, which
    // panics on `min > max` and takes down the whole decode/listen/run
    // process on its first speed update. Reject an invalid bound here,
    // same reasoning as the fallback_hops check above.
    let (tau_hi_lo, tau_hi_hi) = cfg.demod.tau_hi_bounds_ms;
    if !tau_hi_lo.is_finite() || !tau_hi_hi.is_finite() || tau_hi_lo > tau_hi_hi || tau_hi_lo <= 0.0
    {
        bail!(
            "[decode] tau_hi_bounds_ms must be a finite [low, high] pair with 0 < low <= high \
             in {} (got [{tau_hi_lo}, {tau_hi_hi}], which would panic in f64::clamp on the \
             first speed update)",
            origin
        );
    }
    if !cfg.demod.tau_lo_ms.is_finite() || cfg.demod.tau_lo_ms <= 0.0 {
        bail!(
            "[decode] tau_lo_ms must be a finite, positive number of milliseconds in {} \
             (got {})",
            origin,
            cfg.demod.tau_lo_ms
        );
    }
    // Codex review, PR #161 rounds 3 and 5: `timing_sigma = 0` (or NaN)
    // reaches beam::log_likelihood's `2 * sigma * sigma` denominator,
    // producing infinite/NaN confidence scores instead of a load-time
    // error. Round 3's `> 0.0` check alone isn't a strong enough floor: an
    // f32-representable but tiny value (e.g. 1e-30) still passes it, yet
    // `2.0 * sigma * sigma` underflows to exactly 0.0 in f32 (f32's
    // smallest positive normal is ~1.18e-38, so anything with
    // sigma^2 below ~5.9e-39 flushes to zero), giving 0.0/0.0 = NaN for
    // any candidate with a perfectly-matched duration. 1e-3 (0.1% relative
    // timing tolerance) is nowhere near that underflow threshold and is
    // already far stricter than any real keying signal's timing jitter --
    // SPEC v2's own default is 0.25 -- so it rejects only configs that
    // could never usefully decode real audio, not legitimate tuning.
    const MIN_TIMING_SIGMA: f32 = 1e-3;
    if !cfg.beam.sigma.is_finite() || cfg.beam.sigma < MIN_TIMING_SIGMA {
        bail!(
            "[decode] timing_sigma must be finite and >= {MIN_TIMING_SIGMA} in {} (got {}; \
             smaller values can underflow log_likelihood's denominator to a NaN score)",
            origin,
            cfg.beam.sigma
        );
    }
    if cfg.beam.width == 0 {
        bail!(
            "[decode] beam_width must be nonzero in {} (0 disables the beam decoder entirely)",
            origin
        );
    }
    // Codex review, PR #161 round 3: `[decode] beam = 0` (the hsmm
    // engine's own beam size, SPEC v2 §4 -- distinct from the legacy
    // `beam_width` checked above) deserializes and passes every check
    // above, but `HsmmDecoder::push`'s `merged.truncate(0)` then
    // permanently empties the live hypothesis set on the very first
    // anchor step -- the command silently emits no decoded text or
    // spots, no error. Same class of gap as `beam_width`, just the other
    // engine's beam.
    if cfg.hsmm.beam == 0 {
        bail!(
            "[decode] hsmm beam must be nonzero in {} (0 empties the live hypothesis set on the \
             first anchor step, silently emitting no decoded text)",
            origin
        );
    }
    // Codex review, PR #161 round 4: `sigma_u = 0` puts a hop exactly on
    // the normalized half-amplitude decision surface at `0 / 0`, making
    // the LLR (and every downstream accumulated prefix) permanently NaN;
    // a non-finite value corrupts every present hop the same way. Both
    // edge-legacy and hsmm then silently stop decoding or propagate NaN
    // scores.
    // Codex review, PR #161 round 20: fresh evidence beyond the zero-value
    // case above -- a finite-but-tiny sigma_u (e.g. 1e-30) still passes
    // `> 0.0`, yet `sigma_u * sigma_u` underflows to exactly 0.0 in f32,
    // permanently poisoning the evidence prefix with NaN. Same underflow
    // class as dur_sigma's MIN_DUR_SIGMA floor.
    const MIN_SIGMA_U: f32 = 1e-3;
    if !cfg.evidence.sigma_u.is_finite() || cfg.evidence.sigma_u < MIN_SIGMA_U {
        bail!(
            "[decode] sigma_u must be finite and >= {MIN_SIGMA_U} in {} (got {}; smaller values \
             can underflow the LLR denominator to 0/0)",
            origin,
            cfg.evidence.sigma_u
        );
    }
    // Codex review, PR #161 round 4: an empty `seed_units_hops` list
    // deserializes and passes every check above, but every keying onset
    // then seeds zero tokens -- candidate generation stays empty forever
    // and the command silently emits no decoded text or spots. Require at
    // least one seed unit, and that every seed is itself finite and
    // positive (a bad seed is exactly as silently broken as an empty
    // list, just one hypothesis worth instead of all of them).
    if cfg.hsmm.seed_units_hops.is_empty() {
        bail!(
            "[decode] seed_units_hops must have at least one entry in {} (an empty list seeds \
             zero tokens at every keying onset, silently emitting no decoded text)",
            origin
        );
    }
    if let Some(bad) = cfg
        .hsmm
        .seed_units_hops
        .iter()
        .find(|u| !u.is_finite() || **u <= 0.0)
    {
        bail!(
            "[decode] every seed_units_hops entry must be finite and positive in {} (got {bad})",
            origin
        );
    }
    // Codex review, PR #161 round 18: a seed outside the decoder's
    // supported [u_min, u_max] speed range (7.5..=56 hops/dit) is finite
    // and positive and so passed the check above, but can't produce a
    // valid initial mark transition for any real 8-60 WPM signal -- a
    // seed list containing only such values silently emits no decoded
    // text.
    if let Some(bad) = cfg
        .hsmm
        .seed_units_hops
        .iter()
        .find(|u| **u < cfg.hsmm.u_min || **u > cfg.hsmm.u_max)
    {
        bail!(
            "[decode] every seed_units_hops entry must be within the supported speed range \
             [{}, {}] hops/dit in {} (got {bad}; outside that range, the seed can't produce a \
             valid initial mark transition for any real signal)",
            cfg.hsmm.u_min,
            cfg.hsmm.u_max,
            origin
        );
    }
    // Codex review, PR #161 round 4: `conf_kappa = 0` makes the common
    // no-competing-hypothesis path (s_alt == best.score) compute `0 / 0`
    // in `margin`'s confidence sigmoid, emitting a NaN character/word-
    // boundary confidence that then contaminates every downstream spot-
    // confidence calculation and JSON report.
    if !cfg.hsmm.conf_kappa.is_finite() || cfg.hsmm.conf_kappa <= 0.0 {
        bail!(
            "[decode] conf_kappa must be finite and strictly positive in {} (got {}; 0 makes the \
             no-competing-hypothesis confidence path compute 0/0)",
            origin,
            cfg.hsmm.conf_kappa
        );
    }
    // Codex review, PR #161 rounds 5 and 16: `dur_sigma = 0` reaches
    // `log_dur_prior`'s `2 * dur_sigma * dur_sigma` denominator -- an
    // exactly-nominal-duration segment computes 0/0, and every other
    // segment computes an infinite (non-nominal) score, corrupting
    // pruning and every downstream confidence. Round 5's `> 0.0` check
    // alone isn't a strong enough floor: a finite-but-tiny value (e.g.
    // 1e-30) still passes it, yet `2.0 * dur_sigma * dur_sigma`
    // underflows to exactly 0.0 in f32 -- same underflow class as
    // timing_sigma's `MIN_TIMING_SIGMA` floor below.
    const MIN_DUR_SIGMA: f32 = 1e-3;
    if !cfg.hsmm.dur_sigma.is_finite() || cfg.hsmm.dur_sigma < MIN_DUR_SIGMA {
        bail!(
            "[decode] dur_sigma must be finite and >= {MIN_DUR_SIGMA} in {} (got {}; smaller \
             values can underflow log_dur_prior's denominator to 0/0)",
            origin,
            cfg.hsmm.dur_sigma
        );
    }
    // Codex review, PR #161 round 6: a large but individually-plausible
    // `hold_dits` (e.g. 300) pushes `Evidence`'s hold-window width `h`
    // (`hold_dits * u_max`) past `MAX_RETAIN` -- a debug build panics on
    // the internal `debug_assert!`, a release build silently caps the
    // delay line and discards centers with no error, decoding only the
    // retained tail at EOF. `cfg.hsmm.u_max` is the largest `u_ref` the
    // live speed-feedback loop can ever request (`Token::successor`
    // clamps every speed update to `[u_min, u_max]`), so that's the
    // correct worst case to bound against -- not just the config's
    // initial `u_init_hops`.
    let max_h = cfg.evidence.hold_dits as f64 * cfg.hsmm.u_max as f64;
    if !cfg.evidence.hold_dits.is_finite() || cfg.evidence.hold_dits <= 0.0 || !max_h.is_finite() {
        bail!(
            "[decode] hold_dits must be finite and positive in {} (got {})",
            origin,
            cfg.evidence.hold_dits
        );
    }
    // Codex review, PR #161 round 20: fresh evidence beyond the earlier
    // retention-bound fix -- `Evidence::set_u_ref` rounds `hold_dits *
    // u_ref` before enforcing `h < MAX_RETAIN` (`.round().max(1.0)`), so
    // comparing the unrounded product here can accept a value that
    // rounds UP into the cap once actually used (e.g. hold_dits=73.14 at
    // u_max=56 gives 4095.84, accepted here, but rounds to 4096).
    // Compare the same rounded value Evidence itself uses.
    let max_h_rounded = max_h.round().max(1.0);
    if max_h_rounded >= manta_decode::evidence::MAX_RETAIN as f64 {
        bail!(
            "[decode] hold_dits={} is too large in {}: at the configured u_max={}, the rounded \
             hold window (round(hold_dits * u_max) = {max_h_rounded}) would reach or exceed \
             Evidence's internal retention cap ({}), silently truncating the delay line and \
             discarding evidence centers",
            cfg.evidence.hold_dits,
            origin,
            cfg.hsmm.u_max,
            manta_decode::evidence::MAX_RETAIN,
        );
    }
    // Codex review, PR #161 round 8: `speed_alpha = nan` deserializes and
    // passes every check above; the first duration update
    // (`u += speed_alpha * (target - u)`) then makes `u` NaN, and every
    // subsequent duration prior/score derived from it goes NaN too --
    // beam ordering can then retain those hypotheses independently of
    // real evidence, silently corrupting or emptying the output.
    if !cfg.hsmm.speed_alpha.is_finite() {
        bail!(
            "[decode] speed_alpha must be finite in {} (got {}; a non-finite value poisons \
             every subsequent speed update and duration prior with NaN)",
            origin,
            cfg.hsmm.speed_alpha
        );
    }
    // Codex review, PR #161 round 17: a finite but NEGATIVE speed_alpha
    // moves `u += speed_alpha * (target - u)` away from the observed
    // segment duration instead of toward it -- repeated short segments
    // then drive `u` toward a clamp boundary, producing incorrect WPM
    // reports and killing otherwise-valid duration hypotheses.
    if cfg.hsmm.speed_alpha < 0.0 {
        bail!(
            "[decode] speed_alpha must be nonnegative in {} (got {}; a negative gain moves the \
             speed estimate away from observed durations instead of toward them)",
            origin,
            cfg.hsmm.speed_alpha
        );
    }
    // Codex review, PR #161 round 10: `mark_insert_penalty = nan` reaches
    // `SegType::log_type_prior`; the first Dit/Dah transition then gives
    // every candidate a NaN score, so beam ordering no longer reflects
    // the evidence and emitted confidence can also become NaN.
    if !cfg.hsmm.mark_insert_penalty.is_finite() {
        bail!(
            "[decode] mark_insert_penalty must be finite in {} (got {}; a non-finite value \
             poisons every Dit/Dah transition's score with NaN)",
            origin,
            cfg.hsmm.mark_insert_penalty
        );
    }
    // Codex review, PR #161 round 12: `noise_min_bias_db = inf` (used by
    // both edge-legacy and hsmm) makes `NoiseTracker::new`'s `b_min`
    // infinite; every temporal noise estimate then becomes infinite and
    // the evidence gate stays closed forever, silently emitting nothing.
    if !cfg.noise.noise_min_bias_db.is_finite() {
        bail!(
            "[decode] noise_min_bias_db must be finite in {} (got {}; a non-finite value makes \
             every temporal noise estimate infinite, silently closing the evidence gate)",
            origin,
            cfg.noise.noise_min_bias_db
        );
    }
    // Codex review, PR #178: MAN-168's engine wiring is what gives
    // `NoiseTracker::push`'s spectral-reference branch its first real
    // (non-`None`) input, so `spectral_beta`/`spectral_min_bias_db =
    // inf` -- previously harmless dead config, since the branch never
    // activated -- now makes `spectral_beta * b_spec * r` infinite the
    // same way `noise_min_bias_db = inf` does above, permanently closing
    // the keying-present gate.
    if !cfg.noise.spectral_min_bias_db.is_finite() {
        bail!(
            "[decode] spectral_min_bias_db must be finite in {} (got {}; a non-finite value \
             makes every spectral noise estimate infinite, silently closing the evidence gate)",
            origin,
            cfg.noise.spectral_min_bias_db
        );
    }
    if !cfg.noise.spectral_beta.is_finite() {
        bail!(
            "[decode] spectral_beta must be finite in {} (got {}; a non-finite value makes \
             every spectral noise estimate infinite, silently closing the evidence gate)",
            origin,
            cfg.noise.spectral_beta
        );
    }
    // Codex review, PR #178 round 4: a finite NEGATIVE spectral_beta
    // (e.g. a `-0.5` sign typo) makes NoiseTracker::push's spectral term
    // (`beta * b_spec * r`) negative, so `max(n_temp, ...)` always
    // discards it -- silently disabling the QRM/click discount just
    // wired in, the same way `speed_alpha < 0.0` silently broke the
    // speed estimate elsewhere in this file. 0.0 stays legal: it's a
    // valid, explicit "no spectral discount" value, not a sign error.
    if cfg.noise.spectral_beta < 0.0 {
        bail!(
            "[decode] spectral_beta must be nonnegative in {} (got {}; a negative value makes \
             the spectral term always lose to max(), silently disabling the spectral \
             noise discount)",
            origin,
            cfg.noise.spectral_beta
        );
    }
    // Codex review, PR #161 round 13: a negative `lookahead_dits` makes
    // every non-consensus history entry's nonnegative age always exceed
    // the (negative) forced-commit threshold, reducing the HSMM to
    // greedy commits and producing misleading confidence/decoded text; a
    // NaN or infinite value disables forced commits entirely.
    if !cfg.hsmm.lookahead_dits.is_finite() || cfg.hsmm.lookahead_dits < 0.0 {
        bail!(
            "[decode] lookahead_dits must be finite and nonnegative in {} (got {}; a negative \
             value forces every non-consensus entry immediately, and a non-finite value \
             disables forced commits entirely)",
            origin,
            cfg.hsmm.lookahead_dits
        );
    }
    // Codex review, PR #161 round 14: `noise_window_ms` <= 0 or NaN casts
    // to zero in `ms_to_hops`, silently reducing the minimum-statistics
    // window to one hop (keyed power itself becomes the noise floor,
    // suppressing EdgeLegacy/HSMM output); infinity becomes `u32::MAX`,
    // letting each track's deque grow for effectively the process
    // lifetime.
    if !cfg.noise.noise_window_ms.is_finite() || cfg.noise.noise_window_ms <= 0.0 {
        bail!(
            "[decode] noise_window_ms must be finite and positive in {} (got {})",
            origin,
            cfg.noise.noise_window_ms
        );
    }
    // Codex review, PR #161 round 4: the remaining newly-exposed v1 §9
    // fields have the same class of gap -- `hyst_frac` reaching
    // `Demod::decision_band`'s `hyst_frac * (e_hi - lo)` with NaN makes
    // every keying comparison false (Legacy silently emits nothing at all,
    // no error, no panic); a non-positive or too-wide band (hyst_frac <= 0
    // collapses the band to a single point, no hysteresis at all;
    // hyst_frac >= 0.5 pushes the band's outer edges to or past the rails
    // themselves) breaks the open/close asymmetry hysteresis exists for
    // (MAN-103 replaced the multiplicative `hyst_up`/`hyst_down` pair with
    // this single additive fraction; see `envelope.rs`'s
    // `DemodConfig::hyst_frac` doc comment); `flush_gap_dits <= 0` forces
    // an instant/premature word flush on every hop. Validate all of them
    // here too, for the same reason as every check above: this is the one
    // place a `[decode]` table from disk enters the process.
    if !cfg.demod.hyst_frac.is_finite() {
        bail!(
            "[decode] hyst_frac must be finite in {} (got {})",
            origin,
            cfg.demod.hyst_frac
        );
    }
    if cfg.demod.hyst_frac <= 0.0 || cfg.demod.hyst_frac >= 0.5 {
        bail!(
            "[decode] hyst_frac must be > 0.0 and < 0.5 in {} (got {})",
            origin,
            cfg.demod.hyst_frac
        );
    }
    if !cfg.demod.debounce_ms.is_finite() || cfg.demod.debounce_ms <= 0.0 {
        bail!(
            "[decode] debounce_ms must be finite and positive in {} (got {})",
            origin,
            cfg.demod.debounce_ms
        );
    }
    if !cfg.flush_gap_dits.is_finite() || cfg.flush_gap_dits <= 0.0 {
        bail!(
            "[decode] flush_gap_dits must be finite and positive in {} (got {})",
            origin,
            cfg.flush_gap_dits
        );
    }
    // Codex review, PR #161 round 2: a negative or non-finite `llr_clip`
    // reaches `f32::clamp(-llr_clip, llr_clip)` on the first keying-present
    // evidence hop in both edge-legacy and hsmm, panicking on inverted or
    // NaN bounds exactly like the tau_hi_bounds_ms case above.
    if !cfg.evidence.llr_clip.is_finite() || cfg.evidence.llr_clip <= 0.0 {
        bail!(
            "[decode] llr_clip must be finite and positive in {} (got {}; f32::clamp panics on \
             a negative or non-finite bound)",
            origin,
            cfg.evidence.llr_clip
        );
    }
    // Codex review, MAN-168: `refine_bw_hz` is a newly-activated (default
    // 0.0/disabled) setting -- a NaN or infinite value is neither `<=
    // 0.0` (so refinement isn't bypassed) nor a usable bandwidth,
    // reaching `Refiner::new`'s own `debug_assert!(bw_hz > 0.0)` (a debug
    // panic; a release build instead designs an all-NaN/degenerate FIR
    // that then poisons every refined amplitude). 0.0 itself must stay
    // legal -- it's the documented "disabled" sentinel, not an error.
    //
    // Codex review, PR #178: a NEGATIVE value (e.g. a `-30` sign typo)
    // also isn't `> 0.0`, so `decoder_input`'s own `refine_bw_hz <= 0.0`
    // check silently treats it as the disabled bypass instead of
    // reporting the operator's config error -- the documented disabled
    // sentinel is specifically `0.0`, not "anything non-positive".
    if !cfg.refine_bw_hz.is_finite() || cfg.refine_bw_hz < 0.0 {
        bail!(
            "[decode] refine_bw_hz must be finite and nonnegative in {} (got {}; use 0.0 to \
             disable refinement, not a negative value)",
            origin,
            cfg.refine_bw_hz
        );
    }
    Ok(cfg)
}

/// Test-only entry point over the production loader (`config::load`), kept
/// so the `[decode]` validation tests exercise the real path unmodified.
#[cfg(test)]
fn load_decode_config_file(config: Option<&Path>) -> Result<manta_decode::decoder::DecodeConfig> {
    Ok(config::load(config, config::Env::Ignore)?.decode)
}

/// SPEC v2 §7: an explicit `--engine` flag overrides the `[decode]` table's
/// `engine` key; the file's value (or `Engine::Legacy` if there's no
/// `--server-config`/no `[decode]` table) is the baseline otherwise. Every
/// other `DecodeConfig` field always comes from `file_decode` (i.e. from
/// the file, or its defaults) -- there is no CLI flag for them.
fn merge_cli_engine(
    cli_engine: Option<Engine>,
    mut file_decode: manta_decode::decoder::DecodeConfig,
) -> manta_decode::decoder::DecodeConfig {
    if let Some(engine) = cli_engine {
        file_decode.engine = engine;
    }
    file_decode
}

/// Handles what the `Run` on-spot closure needs to feed a running spot server.
struct SpotServer {
    bus: std::sync::Arc<manta_server::bus::SpotBus>,
    metrics: std::sync::Arc<manta_server::metrics::Metrics>,
    /// Signals the telnet/JSON/WS client tasks to drain their already-
    /// queued spots and exit, instead of being forcibly cut off by
    /// `Runtime::shutdown_timeout`'s raw deadline with no chance to finish
    /// an in-flight write. Call `.send(true)` before shutting the runtime
    /// down.
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Every spawned telnet/JSON/WS per-client connection task, tracked so
    /// shutdown can genuinely AWAIT their completion (bounded by
    /// `SHUTDOWN_DRAIN_DEADLINE`) instead of guessing a fixed sleep
    /// duration -- see `shutdown_runtime_after_drain`.
    tasks: manta_server::tasks::ClientTasks,
    /// The run's resolved table, shared with the validator and `JsonStreamConfig`,
    /// kept here too so the publish callback can check resolvability once
    /// per spot for `manta_spots_unresolved_geography_total` -- checking
    /// inside `SpotMessage::from_spot` would scale with connected client
    /// count instead of spot count.
    cty: std::sync::Arc<manta_spot::cty::Table>,
    /// Whether the operator's OWN station callsign (config, not decoder
    /// output -- and not required to be cty-resolvable) already forces the
    /// de-side `UNKNOWN_*` sentinels. Resolved ONCE at `start_spot_server`
    /// time rather than per spot: `station_callsign` cannot change for the
    /// life of the process, so re-running the same binary search on every
    /// spot only re-derives a constant.
    station_geography_unresolved: bool,
    /// MAN-122 review round 4 (P2): the periodic status task's `JoinHandle`
    /// is RETAINED, not discarded, so the shutdown sequence can AWAIT the
    /// task's actual exit right after `shutdown_tx.send(true)` and before
    /// the client drain begins. The task's own two shutdown guards (a
    /// `biased` select and a re-borrow after the sleep) still leave a
    /// check-to-log window: shutdown can be signalled between the second
    /// borrow returning `false` and the `tracing::info!` that follows, so a
    /// status line could still land in the middle of the drain. Awaiting
    /// the handle here is what makes shutdown and emission mutually
    /// ordered -- once the join returns, the task is gone and no further
    /// line can be emitted. `None` when the status line is disabled
    /// (`status_interval_secs = 0`), in which case there is nothing to
    /// await. `Mutex<Option<..>>` because shutdown only ever holds
    /// `&SpotServer` and must `take()` the handle to join it.
    status_line: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The metrics/status HTTP listener's real bound address (MAN-44),
    /// from `TcpListener::local_addr()`, so a port-0 bind is reachable by
    /// tests without guessing or binding a fixed port.
    #[cfg_attr(not(test), allow(dead_code))]
    metrics_addr: std::net::SocketAddr,
}

// MAN-89 (PR #131 review, round 7): the "would `from_spot` emit the
// `UNKNOWN_*` sentinels?" predicate that
// `manta_spots_unresolved_geography_total` is defined over used to be
// defined HERE, one crate away from the `SpotMessage::from_spot` it has to
// agree with -- and it drifted: the station side was classified from the raw
// configured `station_callsign` while `from_spot` resolved the SSID-stripped
// one. It now lives beside that code in `manta-server::spot_message`, with
// the station-side entry point doing the stripping itself so no call site
// can forget it.
use manta_server::spot_message::{geography_is_unresolved, station_geography_unresolved};

/// Starts the telnet/JSON-Lines-and-WebSocket/metrics servers on their own
/// tokio runtime (ARCHITECTURE §7-§8). The returned `Runtime` must be kept
/// alive for the servers to keep running -- dropping it stops them.
/// Before exit: send `true` on `SpotServer::shutdown_tx` so client tasks
/// get a chance to drain (e.g. spots from `TrackManager::finish()`), THEN
/// call `Runtime::shutdown_timeout` as the bounded safety net.
///
/// `epoch` is the bus's real wall-clock session start (see `SpotBus::new`)
/// -- always pass `SystemTime::now()` (this daemon's actual start time),
/// live or replay: it feeds every client-observed `timestamp`/RBN Zulu
/// field, which must stay truthful. `session_nonce` is the separate,
/// spot-`id`-uniqueness-only value -- pass a fixed one (e.g.
/// `session_nonce_for_replay_path`) when replaying a file, or two runs of
/// the same fixture emit colliding spot `id`s.
/// Upper bound on how long `shutdown_runtime_after_drain` waits for
/// spawned client-connection tasks to actually finish draining before
/// falling through to `Runtime::shutdown_timeout`'s hard cutoff. Unlike a
/// fixed sleep, this is a ceiling, not a guess that's always fully paid --
/// `tasks::await_all` returns as soon as every tracked task completes, so
/// shutdown with zero (or quickly-finishing) clients is fast regardless of
/// this value; it only matters when a task is genuinely still writing.
///
/// Must stay comfortably >= the worst-case time a SINGLE legitimately-slow
/// client's final drain write is itself permitted to take, or this deadline
/// cuts a write off before it could ever finish even under its own
/// individual timeout -- not a lagged/dead client, just an ordinary slow
/// one. `telnet::handle_client`'s drain loop writes each spot via TWO
/// separately-timed `write_with_timeout` calls (the RBN line, then
/// `\r\n`), each up to telnet's own `WRITE_TIMEOUT` (10s) -- up to ~20s for
/// one spot. The previous 2s value was shorter than even a single one of
/// those 10s writes, so a genuinely slow-but-completing client was
/// routinely cut off mid-drain for no reason (round-15 review finding).
///
/// MAN-45 (round-16 finding): as of this change, the value that actually
/// bounds ONE client's drain is `manta_server::tasks::CLIENT_DRAIN_DEADLINE`
/// -- each of the three per-client drain loops (telnet's, json_stream's TCP
/// and WS) now enforces its own inner deadline and counts whatever it
/// abandons when that fires, so a healthy handler always returns from
/// `await_all` well within its own budget. This constant is now a
/// registry-wide *scheduling backstop* above that per-client bound (see the
/// `the_outer_shutdown_deadline_outlives_every_handlers_own_drain_deadline`
/// test below) -- it no longer needs sizing against any particular spot
/// count, only against `CLIENT_DRAIN_DEADLINE` plus scheduling margin.
///
/// MAN-45 remediate (round-16 P1, finding 2): "scheduling margin" above
/// CLIENT_DRAIN_DEADLINE isn't the whole story -- `CLIENT_DRAIN_DEADLINE`
/// only bounds a handler's OWN `_ = shutdown.changed() =>` branch body.
/// `tokio::select!` doesn't poll that branch again until whichever OTHER
/// branch is currently running resolves, so a handler already mid-write
/// when shutdown fires can burn up to its own current branch's full
/// worst-case time BEFORE it even reaches the drain branch and starts
/// that 20s clock. The largest such branch across all three handlers is
/// telnet's live-spot write (`manta_server::telnet::WRITE_TIMEOUT`, TWO
/// separately-timed writes per spot) -- json_stream's TCP/WS write and
/// Pong-reply arms are each a single `WRITE_TIMEOUT`, strictly smaller.
/// So the true worst case this deadline must outlive is `2 *
/// telnet::WRITE_TIMEOUT + CLIENT_DRAIN_DEADLINE`, not
/// `CLIENT_DRAIN_DEADLINE` alone (asserted directly by
/// `the_outer_shutdown_deadline_outlives_every_handlers_own_drain_deadline`
/// below). telnet's `sh/dx` replay loop is bounded to the SAME worst case
/// as the live-write arm rather than its own unbounded backlog depth: it
/// re-checks `shutdown.has_changed()` before every history entry and, the
/// moment it's observed, `break`s back to the `select!` loop's own drain
/// branch to deliver the live `rx` backlog with that branch's full unused
/// budget, rather than abandoning it (validation round 17, CR-2/CR-3 --
/// the remaining history replay itself is simply not re-attempted, since
/// those entries were already published and counted once).
///
/// Validation round 17 (CR-1): this model above only accounts for
/// branches INSIDE the `select!` loop -- it does NOT need to also budget
/// for `telnet::handle_client`'s pre-loop login handshake (prompt write,
/// login-line read, banner write; up to `WRITE_TIMEOUT +
/// bounded_io::IDLE_READ_TIMEOUT + WRITE_TIMEOUT` = 50s) because that
/// handshake itself now races `shutdown.changed()` at every step and
/// bails out (counting its subscribed `rx` backlog) the moment shutdown
/// fires, instead of running any of those three waits to completion
/// first. A stalled pre-login client therefore contributes close to zero
/// to shutdown latency, not up to 50s -- if a future change ever makes
/// that handshake NOT shutdown-aware again, this deadline's true worst
/// case would need to grow to include it.
///
/// MAN-45 remediate (code-review round 18, finding 3): the same was true,
/// but NOT yet fixed, of `json_stream::serve`'s pre-loop phase --
/// `looks_like_websocket_handshake`'s classifying peek (up to
/// `PEEK_TIMEOUT`, or `HANDSHAKE_TIMEOUT` once any byte had arrived) and
/// `handle_ws_client`'s own `accept_async_with_config` step (up to another
/// `HANDSHAKE_TIMEOUT`) previously never observed `shutdown` either. Both
/// now race `shutdown.changed()` the same way telnet's handshake does, so
/// this deadline's safety margin no longer rests on the coincidence that
/// json_stream's *unraced* worst case (20s) happened to be smaller than
/// telnet's live-write branch (`2 * telnet::WRITE_TIMEOUT` = 20s) already
/// budgeted for above -- it now holds because BOTH pre-loop phases are
/// shutdown-aware by design, matching this deadline's own model.
///
/// MAN-45 remediate (code-review round 19, P1): the "at most ONE in-flight
/// branch body precedes the drain" step of that model is now ENFORCED, not
/// assumed. `tokio::select!` picks a random ready arm, so a client with a
/// backlog could previously win the live-spot arm repeatedly after shutdown
/// was signalled -- an unbounded number of `2 * WRITE_TIMEOUT` writes
/// before its own `CLIENT_DRAIN_DEADLINE` clock ever started, which this
/// deadline cannot cover at any constant value. Every client-write-capable
/// arm in all three handler loops (`telnet::handle_client`'s live-spot and
/// command-read arms, `json_stream`'s TCP live-spot and socket-read arms,
/// and its WS live-spot and frame arms) now carries an
/// `if !shutdown.has_changed()` precondition, so once shutdown is pending
/// the drain arm is the only arm those loops can still select. The worst
/// case therefore really is one already-selected branch body plus
/// `CLIENT_DRAIN_DEADLINE`, which is what the value below is sized for.
///
/// MAN-45 remediate (round-19 P1, re-raised against an earlier head): the
/// "one branch body" half of that budget is now also asserted END-TO-END,
/// not only arithmetically here --
/// `telnet_acceptance::shutdown_bounds_live_writes_to_at_most_one_before_the_drain`
/// queues a backlog, signals shutdown before the client task can wake, and
/// asserts across repeated trials that at most ONE live spot write precedes
/// the drain and that every queued spot is then delivered or counted. "At
/// most one", not zero, is deliberate: a handler already parked in
/// `select!` when shutdown fires evaluated its preconditions before the
/// flag was set, so it can still take the live-spot arm once -- which is
/// precisely the single branch body this deadline budgets for, above. See
/// that test's own doc comment for what it does and does not prove (with
/// fast localhost writes the unguarded build stays inside the bound too;
/// exceeding it needs a client that has stopped reading, so each write runs
/// the full `WRITE_TIMEOUT`).
/// MAN-45 remediate (code-review round 19, P1): **changing this value is
/// not self-contained** -- it is the floor for the CALLER-side stop grace
/// period an operator must configure, and two documents state that period
/// as a literal number: `README.md`'s Docker install section (`docker stop
/// -t 60`) and `Dockerfile`'s STOPSIGNAL comment block. Both said 30s,
/// sized against the pre-MAN-45 25s value; against 50s here, a 30s
/// container timeout SIGKILLs the daemon partway through the very drain
/// this constant exists to allow, before it can record the abandoned
/// backlog on `manta_spots_dropped_write_failed_total` (the counter each
/// handler's drain loop charges when its own `CLIENT_DRAIN_DEADLINE`
/// expires -- `manta_spots_dropped_shutdown_total` covers only a client
/// still in pre-login/handshake, whose backlog had not been offered for
/// delivery yet; that is not the same as the connection having written
/// nothing, since the telnet banner and WS-accept branches are reached
/// after the login prompt / part of the 101 response is already on the
/// wire) --
/// recreating the silent truncation the drain work removed. Both are now
/// 60s, leaving margin over this deadline. If this constant grows again,
/// raise them with it.
const SHUTDOWN_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(50);

/// How often the server runtime copies the engine's live track count into
/// the `manta_active_tracks` gauge. The decode loop runs on the MAIN
/// thread, outside the tokio runtime that owns `Metrics`, so a poller is
/// the bridge -- the same shape MAN-55's `confirmed_live_handle` watcher
/// already uses. 4 Hz is far finer than any Prometheus scrape interval and
/// costs one relaxed atomic load per tick.
const ACTIVE_TRACKS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// MAN-73: the `manta_source_health` sink a `ReconnectingSource` reports
/// through. The unhealthy transition also zeroes the shared active-track
/// gauge (PR #207 review): for the whole outage `listen()` is blocked in
/// the reconnecting `read()`, so it can't publish a count itself, and the
/// poller above would keep exporting the pre-drop tracks -- MAN-45's ghost
/// count. `listen()` publishes the real count again after its first
/// post-reconnect chunk.
fn source_health_sink(
    name: &'static str,
    metrics: Option<std::sync::Arc<manta_server::metrics::Metrics>>,
    active_tracks: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
) -> reconnect::HealthSink {
    Box::new(move |healthy| {
        if !healthy {
            if let Some(gauge) = &active_tracks {
                gauge.store(0, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(metrics) = &metrics {
            metrics.set_source_health(name, healthy);
        }
    })
}
/// How often the daemon copies the engine's decode-latency observer into
/// `Metrics` (MAN-128). Same bridge shape and tuning rationale as
/// `ACTIVE_TRACKS_POLL_INTERVAL`.
const DECODE_LATENCY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Shuts down `rt`, first AWAITING (not just giving scheduler time to)
/// every spawned client-connection task tracked in `tasks`, bounded by
/// `SHUTDOWN_DRAIN_DEADLINE`. `Runtime::shutdown_timeout`'s `duration`
/// parameter does not do this on its own -- verified against tokio
/// 1.53.1's source (`runtime/runtime.rs`): `shutdown_timeout` calls
/// `self.handle.inner.shutdown()` synchronously and IMMEDIATELY, tearing
/// down the async executor and dropping in-flight tasks the moment they
/// next yield; its `duration` argument bounds only the SEPARATE blocking-
/// thread-pool's shutdown. An earlier version of this function papered
/// over that with a fixed blind sleep before `shutdown_timeout` -- real
/// scheduler time, but no guarantee the tasks actually FINISHED before the
/// sleep elapsed and `shutdown_timeout` tore things down anyway (round-10
/// review finding). Awaiting `tasks::await_all` instead genuinely waits
/// for completion, up to the deadline, then still falls through to
/// `shutdown_timeout` as a final hard backstop for anything left running
/// past it.
fn shutdown_runtime_after_drain(
    rt: tokio::runtime::Runtime,
    tasks: &manta_server::tasks::ClientTasks,
) {
    rt.block_on(manta_server::tasks::await_all(
        tasks,
        SHUTDOWN_DRAIN_DEADLINE,
    ));
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
}

/// MAN-64: the source-health FAILURE transition for a fatal `listen` exit.
/// Since MAN-73, a reconnectable source's read error no longer reaches
/// here -- `ReconnectingSource` retries it and reports `false`/`true`
/// itself through `source_health_sink` -- so `manta_engine::listen`
/// returning `Err` now means file replay failed, a reopened live source
/// came back with a different sample rate or centre frequency, or the
/// pipeline behind the source failed. Either way the process is about to
/// exit; recording it here -- BEFORE `shutdown_tx.send(true)` -- gives a
/// scraper a chance to see `manta_source_health{...} 0` before the daemon
/// is gone, PROVIDED the metrics listener is still alive to answer it.
/// That is conditional, not guaranteed (validate-plan round, V-1):
/// `metrics_http::serve` runs outside `ClientTasks` with no shutdown watch,
/// so it survives past this point only for as long as
/// `shutdown_runtime_after_drain`'s `tasks::await_all` keeps the runtime
/// alive -- the whole `SHUTDOWN_DRAIN_DEADLINE` window when a
/// telnet/JSON/WS client is genuinely still draining, but almost no time
/// (the runtime is torn down microseconds later) when no such client is
/// connected, the ordinary state for a scrape-only deployment. Written
/// through `set_source_health_terminal` (round 7, V-2), not the regular
/// `set_source_health`, so no later regular write can flip it back; see
/// `SourceHealthEntry` in `manta-server/src/metrics.rs` for which writers
/// exist today.
///
/// Deliberately silent on the `Ok` path: a clean end of stream (file replay
/// finished, operator Ctrl-C) is normal termination, not a source failure,
/// and reporting it as unhealthy would make the gauge lie in the opposite
/// direction. See ARCHITECTURE.md §8 and
/// `docs/DECISIONS/2026-09-04-man64-metrics-request-rate-and-source-health.md`.
fn record_terminal_source_health(
    metrics: &manta_server::metrics::Metrics,
    source_name: &str,
    listen_result: &Result<()>,
) {
    if listen_result.is_err() {
        metrics.set_source_health_terminal(source_name, false);
    }
}

/// What the MAN-122 startup banner names about the live source; carried as
/// one struct so `start_spot_server`'s argument list stays at four.
struct SourceInfo<'a> {
    name: &'a str,
    sample_rate_hz: f64,
    /// Also the RF centre the MAN-86 `SKIMMER/SETT` segments are derived
    /// around (`IqSource::center_freq_hz`, after any `--dial-freq-hz`
    /// override).
    dial_freq_hz: f64,
    /// MAN-86 review: deliberately SEPARATE from `sample_rate_hz`, not
    /// derived from it. `sample_rate_hz` is the per-sample timing quantity
    /// `SpotBus` needs to turn a sample index into wall clock;
    /// `rf_passband_hz` is the `(lo, hi)` offsets from `dial_freq_hz` of
    /// the spectrum the receiver actually delivers, and only that may be
    /// advertised to Aggregator as decodable coverage. The two differ for
    /// any resampling source, and the passband is not even symmetric for a
    /// rig-audio source -- see `IqSource::rf_passband_hz`.
    rf_passband_hz: (f64, f64),
    /// The same multiplicative factor `manta-engine::listen` applies to
    /// every emitted spot frequency (`--freq-correction-ppm`). MAN-86
    /// review: the advertised segments have to move with the spots, or at
    /// the supported +/-1000 ppm limit manta advertises bounds that exclude
    /// frequencies from its own spot stream.
    freq_calibration: f64,
}

/// MAN-128: the engine's `DecodeLatencySnapshot` and `manta-server`'s
/// `LatencyHistogram` are deliberately disjoint types (the same dependency-
/// free-`manta-server` boundary `input_health_of` crosses above) -- this is
/// where the engine's bucket bounds get attached to its own snapshot.
fn latency_histogram_of(
    s: &manta_engine::DecodeLatencySnapshot,
) -> manta_server::metrics::LatencyHistogram {
    manta_server::metrics::LatencyHistogram {
        bounds_seconds: manta_engine::DECODE_LATENCY_BUCKETS_SECONDS.to_vec(),
        bucket_counts: s.bucket_counts.clone(),
        sum_seconds: s.sum_seconds,
    }
}

/// How often the daemon samples an input source's `InputHealthCounters`
/// into `Metrics` (MAN-56). An order of magnitude below any realistic
/// Prometheus scrape interval, so a scrape never sees more than ~1 s of
/// staleness; the tick itself is three relaxed atomic loads and one
/// `BTreeMap` insert. Deliberately slower than the `confirmed_live` poll
/// (200 ms, see the `confirmed_live_handle` wiring below), which is tuned
/// for a single startup transition rather than a forever-loop.
const INPUT_HEALTH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Snapshot `manta-input`'s counters into `manta-server`'s own,
/// dependency-free mirror struct. The two crates deliberately share no
/// type -- that disjointness is what keeps `manta-server` free of any
/// `manta-input` dependency (ARCHITECTURE §3/§8) -- so `manta-cli`, which
/// depends on both, is where the translation belongs.
fn input_health_of(
    counters: &manta_input::InputHealthCounters,
) -> manta_server::metrics::InputHealth {
    manta_server::metrics::InputHealth {
        dropped_packets: counters.dropped_packets(),
        gaps_detected: counters.gaps_detected(),
        malformed_packets: counters.malformed_packets(),
    }
}

/// MAN-128: feeds `manta_build_info`. `git_sha` comes from `build.rs`
/// (`MANTA_GIT_SHA`, "unknown" with no `.git` -- e.g. a Docker build
/// context, which `.dockerignore` excludes it from). MAN-83: the values are
/// `build_info`'s constants, the same ones `--version` and the JSON
/// stream's `decoderVersion` read, so the gauge, the version line and the
/// spots always name the same build.
fn daemon_build_info() -> manta_server::metrics::BuildInfo {
    manta_server::metrics::BuildInfo {
        version: build_info::VERSION.to_string(),
        git_sha: build_info::GIT_SHA.to_string(),
        features: build_info::FEATURES.to_string(),
    }
}

/// The identity and `SKIMMER/SETT` settings every telnet client is told
/// about this station. Split out of `start_spot_server` (which can only be
/// exercised through a real bound listener) so the source-to-SETT wiring
/// itself is unit-testable -- MAN-86 review found two ways for it to
/// advertise coverage manta cannot actually hear, and neither was visible
/// from `sett.rs`'s own tests.
fn station_profile(
    cfg: &manta_server::config::ServerConfig,
    center_freq_hz: f64,
    rf_passband_hz: (f64, f64),
    freq_calibration: f64,
) -> manta_server::telnet::StationProfile {
    manta_server::telnet::StationProfile {
        call: cfg.station_callsign.clone(),
        operator_name: cfg.operator_name.clone(),
        operator_qth: cfg.operator_qth.clone(),
        operator_grid: cfg.operator_grid.clone(),
        sett: manta_server::sett::SettSettings {
            validation_level: manta_server::sett::ValidationLevel::Normal,
            cq_only: false,
            segments: manta_server::sett::segments_for_passband(
                center_freq_hz,
                rf_passband_hz,
                freq_calibration,
            ),
        },
    }
}

/// The three listener names `/healthz`/`manta_listener_up` track (MAN-128)
/// -- named constants so `set_listener_up`'s and `spawn_tracked_listener`'s
/// call sites below can't drift apart via a typo in one of the two paired
/// string literals (code-review finding, round 1).
const LISTENER_TELNET: &str = "telnet";
const LISTENER_JSON: &str = "json";
const LISTENER_METRICS: &str = "metrics";

/// Spawns `fut` as a tracked listener task: `metrics` reports `name` down
/// the moment that task ends, whether by panic or by returning (the
/// daemon's own listener loops only return via `shutdown_tx`, but a
/// probe reading `manta_listener_up`/`/healthz` during the drain that
/// follows should see the true state, not a stale "up"). `name` is
/// `&'static str` because it's always one of the three fixed listener
/// names below, never operator-supplied.
fn spawn_tracked_listener(
    name: &'static str,
    metrics: std::sync::Arc<manta_server::metrics::Metrics>,
    fut: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let handle = tokio::spawn(fut);
    tokio::spawn(async move {
        if let Err(err) = handle.await {
            tracing::error!(listener = name, error = %err, "listener task panicked");
        }
        metrics.set_listener_up(name, false);
    });
}

fn start_spot_server(
    cfg: manta_server::config::ServerConfig,
    rbn_uplink_cfgs: Vec<manta_server::config::RbnUplinkConfig>,
    source: SourceInfo<'_>,
    epoch: std::time::SystemTime,
    session_nonce: u128,
    cty: std::sync::Arc<manta_spot::cty::Table>,
) -> Result<(tokio::runtime::Runtime, SpotServer)> {
    // MAN-59: the daemon's only durable record of connection events/
    // rejections was the live Prometheus counters (no history, reset on
    // restart) -- nothing to reconstruct WHAT happened or FROM WHERE
    // after an abuse incident. `try_init` (not `init`, which panics on a
    // second call) since this function is the sole place the daemon's
    // Tokio runtime is constructed, but a defensive no-op on an
    // already-initialized global subscriber costs nothing. `RUST_LOG`
    // overrides; unset defaults to `info` -- connection/rejection events
    // below are logged at `info`/`warn`, so an operator gets useful
    // output with zero configuration, and can raise verbosity for deeper
    // debugging without a code change.
    //
    // MAN-59 review round 6 (P1): `fmt()` writes to stdout by default,
    // but `Command::Run --json` ALSO writes DecoderEvents/spots as
    // JSON Lines to stdout (below) -- AGENTS.md's "file input ->
    // byte-identical spot logs" hard requirement means any interleaved
    // non-JSON tracing line corrupts that machine-readable stream for
    // real consumers and breaks deterministic-replay byte-identity.
    // stderr is a separate stream a JSON-Lines consumer never reads.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // MAN-261: `[server]`/`[[rbn_uplink]]` arrive already parsed and
    // validated by `config::load`, before any source was opened.
    let bus = std::sync::Arc::new(manta_server::bus::SpotBus::new(
        source.sample_rate_hz,
        epoch,
        session_nonce,
    ));
    let metrics = std::sync::Arc::new(manta_server::metrics::Metrics::new());
    metrics.set_build_info(daemon_build_info());
    // MAN-83: the commit rides as SemVer build metadata, so every spot names
    // the binary that produced it (`manta-<version>+<sha>`). Same-binary
    // byte-identity (SPEC §6 item 4) holds because this is a compile-time
    // constant, and the outputs CI hashes (`decode --json`, `run --json`)
    // never carry it. Supersedes MAN-128's note keeping this string free of
    // the commit; see
    // docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md.
    let decoder_version = build_info::DECODER_VERSION.to_string();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tasks = manta_server::tasks::new_client_tasks();

    let rt = tokio::runtime::Runtime::new()?;
    let (status_line, metrics_addr) = rt.block_on(async {
        // MAN-132: metrics binds its own `metrics_bind_addr` (loopback by
        // default), never `bind_addr`. With two configurable addresses
        // "which listener failed?" is a real question, so each bind names
        // its listener, address key and port.
        let telnet_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.telnet_port))
                .await
                .with_context(|| {
                    format!(
                        "binding the telnet server (bind_addr = {:?}, telnet_port = {})",
                        cfg.bind_addr, cfg.telnet_port
                    )
                })?;
        let json_listener = tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.json_port))
            .await
            .with_context(|| {
                format!(
                    "binding the JSON server (bind_addr = {:?}, json_port = {})",
                    cfg.bind_addr, cfg.json_port
                )
            })?;
        let metrics_listener =
            tokio::net::TcpListener::bind((cfg.metrics_bind_addr.as_str(), cfg.metrics_port))
                .await
                .with_context(|| {
                    format!(
                        "binding the metrics server (metrics_bind_addr = {:?}, metrics_port = {})",
                        cfg.metrics_bind_addr, cfg.metrics_port
                    )
                })?;
        let metrics_addr = metrics_listener.local_addr()?;

        // MAN-122 scenario 1. Emitted after every bind succeeds (so a bind
        // failure never produces a banner at all) but BEFORE any listener task is
        // spawned -- on a multi-thread runtime a spawned accept loop can admit a
        // client immediately, and its per-connection line would otherwise be able
        // to land ahead of the banner. `local_addr()`, not the configured port,
        // so a `*_port = 0` (ephemeral) config still names the real address.
        //
        // Review round 2: this line says `listening:`, not `ready:`. Three
        // bound sockets are not evidence that anything will ever be decoded --
        // a replay shorter than `manta_engine`'s two-second calibration window,
        // or a live source that fails its first reads, exits here with the
        // pipeline never having started. The readiness event proper is emitted
        // from the decode loop's first batch (see `format_pipeline_ready`'s
        // call site).
        tracing::info!(
            "{}",
            manta_server::status::format_startup_banner(&manta_server::status::StartupInfo {
                version: env!("CARGO_PKG_VERSION"),
                source: source.name,
                sample_rate_hz: source.sample_rate_hz,
                dial_freq_hz: source.dial_freq_hz,
                station_callsign: &cfg.station_callsign,
                telnet_addr: telnet_listener.local_addr()?,
                json_addr: json_listener.local_addr()?,
                metrics_addr: metrics_listener.local_addr()?,
            })
        );

        // MAN-128: all three binds above already succeeded (a failed bind
        // returns via `?` before this point), so every listener starts
        // "up" -- `spawn_tracked_listener` below flips each back to "down"
        // the moment its own task actually ends.
        metrics.set_listener_up(LISTENER_TELNET, true);
        metrics.set_listener_up(LISTENER_JSON, true);
        metrics.set_listener_up(LISTENER_METRICS, true);

        let telnet_ip_command_limiter = manta_server::rate_limit::IpRateLimiter::new_with_override(
            manta_server::telnet::MAX_TELNET_COMMANDS,
            manta_server::telnet::COMMAND_RATE_WINDOW,
            cfg.telnet_max_commands_per_ip,
        );
        manta_server::rate_limit::spawn_stale_entry_reaper(telnet_ip_command_limiter.clone());
        // MAN-86: Aggregator will not forward spots from a source that
        // never answers SKIMMER/SETT (Aggregator manual v6.0 §9.2) --
        // `profile` carries the operator identity for the greeting banner
        // and the live-passband segments for the SETT reply.
        if cfg.operator_grid.is_none() || cfg.operator_qth.is_none() {
            tracing::warn!(
                "telnet greeting will omit QTH/grid -- set [server].operator_qth and \
                 operator_grid so RBN Aggregator can record this node's location \
                 (Aggregator manual v6.0 §9.2)"
            );
        }
        let profile = std::sync::Arc::new(station_profile(
            &cfg,
            source.dial_freq_hz,
            source.rf_passband_hz,
            source.freq_calibration,
        ));
        spawn_tracked_listener(
            LISTENER_TELNET,
            metrics.clone(),
            manta_server::telnet::serve(
                telnet_listener,
                bus.clone(),
                metrics.clone(),
                profile,
                shutdown_rx.clone(),
                tasks.clone(),
                manta_server::tasks::new_connection_limiter(
                    manta_server::telnet::MAX_TELNET_CONNECTIONS,
                ),
                manta_server::tasks::IpQuota::new_with_override(
                    manta_server::telnet::MAX_TELNET_CONNECTIONS_PER_IP,
                    cfg.telnet_max_connections_per_ip,
                ),
                telnet_ip_command_limiter,
                manta_server::tasks::CLIENT_DRAIN_DEADLINE,
                cfg.line_format,
            ),
        );
        let json_ip_ping_limiter = manta_server::rate_limit::IpRateLimiter::new_with_override(
            manta_server::json_stream::MAX_INBOUND_PINGS,
            manta_server::json_stream::PING_RATE_WINDOW,
            cfg.json_max_pings_per_ip,
        );
        manta_server::rate_limit::spawn_stale_entry_reaper(json_ip_ping_limiter.clone());
        spawn_tracked_listener(
            LISTENER_JSON,
            metrics.clone(),
            manta_server::json_stream::serve(
                json_listener,
                manta_server::json_stream::JsonStreamConfig {
                    bus: bus.clone(),
                    metrics: metrics.clone(),
                    cty: cty.clone(),
                    station_call: cfg.station_callsign.clone(),
                    decoder_version,
                    // .clone(): MAN-32/MAN-42's uplink::serve spawns below also
                    // need shutdown_rx -- can't let this be the moving consumer
                    // anymore now that there are more consumers.
                    shutdown: shutdown_rx.clone(),
                    drain_deadline: manta_server::tasks::CLIENT_DRAIN_DEADLINE,
                },
                tasks.clone(),
                manta_server::tasks::new_connection_limiter(
                    manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
                ),
                manta_server::tasks::IpQuota::new_with_override(
                    manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS_PER_IP,
                    cfg.json_max_connections_per_ip,
                ),
                json_ip_ping_limiter,
            ),
        );
        // Reaps completed per-client tasks continuously, independent of
        // shutdown -- without this, `tasks` only ever shrinks at
        // shutdown_runtime_after_drain's one-time `await_all`, so ordinary
        // connect/disconnect churn grows it without bound for the life of
        // the process (round-11 review finding).
        manta_server::tasks::spawn_reaper(tasks.clone());
        // MAN-44: the WHOLE uplink registry is published before the
        // metrics/status endpoint is spawned below. On this multi-thread
        // runtime that endpoint accepts on another worker the instant its
        // task is spawned, so a probe racing a later registration loop
        // could read a half-registered (or empty) registry and be told
        // `disabled` -- `manta status` exit 0 -- for a daemon whose
        // configured targets are in fact down. Registered-but-unconnected
        // reads as down, the honest startup answer. `target_labels`
        // assigns the Prometheus label (`host:port`, `#N`-suffixed only on
        // an exact duplicate; MAN-128 D7).
        let uplink_labels = manta_server::uplink::target_labels(&rbn_uplink_cfgs);
        let enabled_uplinks = rbn_uplink_cfgs.iter().filter(|u| u.enabled).count();
        let uplink_tasks: Vec<_> = rbn_uplink_cfgs
            .into_iter()
            .zip(uplink_labels)
            .map(|(uplink_cfg, label)| {
                let target = metrics.register_uplink_target(label, uplink_cfg.enabled);
                (uplink_cfg, target)
            })
            .collect();
        let metrics_ip_request_limiter = manta_server::rate_limit::IpRateLimiter::new_with_override(
            manta_server::metrics_http::MAX_METRICS_REQUESTS_PER_IP,
            manta_server::metrics_http::METRICS_REQUEST_RATE_WINDOW,
            cfg.metrics_max_requests_per_ip,
        );
        manta_server::rate_limit::spawn_stale_entry_reaper(metrics_ip_request_limiter.clone());
        spawn_tracked_listener(
            LISTENER_METRICS,
            metrics.clone(),
            manta_server::metrics_http::serve(
                metrics_listener,
                metrics.clone(),
                manta_server::tasks::new_connection_limiter(
                    manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
                ),
                manta_server::tasks::IpQuota::new_with_override(
                    manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
                    cfg.metrics_max_connections_per_ip,
                ),
                metrics_ip_request_limiter,
            ),
        );
        // MAN-32/MAN-42: one independent uplink::serve task per configured
        // [[rbn_uplink]] entry -- the common case for existing single-node
        // operators is no [[rbn_uplink]] tables at all (empty Vec, loop
        // body never runs), and uplink::serve itself also no-ops when
        // `enabled = false` (belt-and-suspenders, not a duplicate check:
        // this loop additionally avoids spawning a task at all when the
        // Vec is empty). Each task owns its own SpotBus subscription and
        // backoff state, so one target being down never affects another's
        // delivery or retry timing.
        // MAN-128 D6/D7: each target was registered above, before both its
        // `serve` task and the metrics endpoint, so a scrape landing before
        // the first connect attempt still sees it.
        for (uplink_cfg, target) in uplink_tasks {
            tokio::spawn(manta_server::uplink::serve(
                uplink_cfg,
                cfg.station_callsign.clone(),
                bus.clone(),
                target,
                shutdown_rx.clone(),
            ));
        }

        // MAN-122 scenario 2. The handle travels out of this block and
        // into `SpotServer::status_line` so shutdown can join the task --
        // see that field's doc comment.
        let status_line = manta_server::status::spawn_status_line(
            metrics.clone(),
            cfg.status_interval_secs
                .map_or(manta_server::status::DEFAULT_STATUS_INTERVAL, |secs| {
                    std::time::Duration::from_secs(secs)
                }),
            enabled_uplinks,
            shutdown_rx.clone(),
        );

        anyhow::Ok((status_line, metrics_addr))
    })?;

    Ok((
        rt,
        SpotServer {
            bus,
            metrics,
            shutdown_tx,
            tasks,
            // MAN-89: `station_geography_unresolved` strips the RBN `-N`
            // per-band SSID itself, so this flag is classified through the
            // SAME string `SpotMessage::from_spot` resolves the de side
            // through -- see its doc comment for what an unstripped
            // classification costs.
            station_geography_unresolved: station_geography_unresolved(&cty, &cfg.station_callsign),
            cty,
            status_line: std::sync::Mutex::new(status_line),
            metrics_addr,
        },
    ))
}

/// The command line's say in source/input/spot selection, before it is
/// merged over the config file and environment (`resolve`, MAN-261).
struct CliOverrides {
    device: Option<String>,
    source: Option<PathBuf>,
    source_iq: bool,
    kiwi: KiwiOpts,
    #[cfg(feature = "soapy")]
    soapy: SoapyOpts,
    #[cfg(feature = "hpsdr")]
    hpsdr: HpsdrOpts,
    freq_correction_ppm: Option<f64>,
    dial_freq_hz: Option<f64>,
    capture_rate_hz: Option<f64>,
    replay_epoch: Option<i64>,
    allowlist: Vec<String>,
    blocklist: Option<PathBuf>,
    notch: Option<PathBuf>,
    cty: Option<PathBuf>,
    scp: Option<PathBuf>,
}

impl CliOverrides {
    /// No command-line say at all: what `manta config check` resolves with,
    /// so it validates exactly what the file and environment give `run`.
    fn none() -> Self {
        CliOverrides {
            device: None,
            source: None,
            source_iq: false,
            kiwi: KiwiOpts {
                host: None,
                port: 8073,
                freq: None,
                password: String::new(),
            },
            #[cfg(feature = "soapy")]
            soapy: SoapyOpts {
                driver: None,
                freq: None,
                rate: None,
                gain: None,
            },
            #[cfg(feature = "hpsdr")]
            hpsdr: HpsdrOpts {
                host: None,
                port: manta_input::hpsdr::CONTROL_PORT,
                freq: None,
                rate: None,
            },
            freq_correction_ppm: None,
            dial_freq_hz: None,
            capture_rate_hz: None,
            replay_epoch: None,
            allowlist: Vec::new(),
            blocklist: None,
            notch: None,
            cty: None,
            scp: None,
        }
    }

    /// The flag that selects a source, if any: given one, the command line
    /// defines the whole source (D6).
    fn source_selector(&self) -> Option<&'static str> {
        #[cfg(feature = "hpsdr")]
        if self.hpsdr.host.is_some() {
            return Some("--hpsdr-host");
        }
        #[cfg(feature = "soapy")]
        if self.soapy.driver.is_some() {
            return Some("--soapy-driver");
        }
        if self.kiwi.host.is_some() {
            Some("--kiwi-host")
        } else if self.source.is_some() {
            Some("--source")
        } else if self.device.is_some() {
            Some("--device")
        } else {
            None
        }
    }

    /// The source the flags describe, in today's priority order: hpsdr,
    /// then kiwi, then soapy, then a WAV file or audio device.
    fn into_spec(self) -> LiveSourceSpec {
        #[cfg(feature = "hpsdr")]
        if self.hpsdr.host.is_some() {
            return LiveSourceSpec::Hpsdr(self.hpsdr);
        }
        if self.kiwi.host.is_some() {
            return LiveSourceSpec::Kiwi(self.kiwi);
        }
        #[cfg(feature = "soapy")]
        if self.soapy.driver.is_some() {
            return LiveSourceSpec::Soapy(self.soapy);
        }
        match self.source {
            Some(path) => LiveSourceSpec::File {
                path,
                source_iq: self.source_iq,
            },
            None => LiveSourceSpec::AudioDevice(self.device),
        }
    }
}

/// `[spot]` after the CLI is merged over the file/env layer (D6).
#[derive(Debug, PartialEq)]
struct SpotResolved {
    allowlist: Vec<String>,
    blocklist: Option<PathBuf>,
    notch: Option<PathBuf>,
    cty: Option<PathBuf>,
    scp: Option<PathBuf>,
}

/// A non-empty `--allowlist` replaces the file's list (never concatenated);
/// `--blocklist`/`--notch` beat `blocklist_path`/`notch_path`.
fn resolve_spot(
    cli_allowlist: Vec<String>,
    cli_blocklist: Option<PathBuf>,
    cli_notch: Option<PathBuf>,
    cli_cty: Option<PathBuf>,
    cli_scp: Option<PathBuf>,
    spot: &config::SpotFile,
) -> SpotResolved {
    SpotResolved {
        allowlist: if cli_allowlist.is_empty() {
            spot.allowlist.clone()
        } else {
            cli_allowlist
        },
        blocklist: cli_blocklist.or_else(|| spot.blocklist_path.clone()),
        notch: cli_notch.or_else(|| spot.notch_path.clone()),
        cty: cli_cty.or_else(|| spot.cty_path.clone()),
        scp: cli_scp.or_else(|| spot.scp_path.clone()),
    }
}

/// Everything a live command needs after CLI > env > file > default.
struct Resolved {
    spec: LiveSourceSpec,
    freq_correction_ppm: f64,
    dial_freq_hz: Option<f64>,
    capture_rate_hz: Option<f64>,
    replay_epoch: Option<i64>,
    spot: SpotResolved,
    /// Printed to stderr by the caller, never stdout.
    notes: Vec<String>,
}

/// D6: merges the command line over the loaded file + environment layer.
fn resolve(cli: CliOverrides, loaded: &config::Loaded) -> Result<Resolved> {
    let mut notes = Vec::new();
    let spot = resolve_spot(
        cli.allowlist.clone(),
        cli.blocklist.clone(),
        cli.notch.clone(),
        cli.cty.clone(),
        cli.scp.clone(),
        &loaded.spot,
    );
    let mut shared = loaded.input.shared.clone();
    let (freq_correction_ppm, dial_freq_hz, capture_rate_hz, replay_epoch) = (
        cli.freq_correction_ppm,
        cli.dial_freq_hz,
        cli.capture_rate_hz,
        cli.replay_epoch,
    );
    let spec = match (cli.source_selector(), &loaded.input.source) {
        (Some(flag), Some(file_source)) => {
            // A typed [input] describes one receiver: its ppm and dial
            // belong to it, not to whatever the command line names instead.
            notes.push(format!(
                "note: {flag} selects the source; ignoring [input] (type = \"{}\") from {}, \
                 including its freq_correction_ppm/center_freq_hz",
                file_source.kind().name(),
                loaded.origin
            ));
            shared = config::SharedInput::default();
            cli.into_spec()
        }
        (Some(_), None) | (None, None) => cli.into_spec(),
        (None, Some(file_source)) => spec_from_file(file_source, cli.source_iq)?,
    };
    Ok(Resolved {
        spec,
        freq_correction_ppm: freq_correction_ppm
            .or(shared.freq_correction_ppm)
            .unwrap_or(0.0),
        dial_freq_hz: dial_freq_hz.or(shared.center_freq_hz),
        capture_rate_hz: capture_rate_hz.or(shared.capture_rate_hz),
        replay_epoch: replay_epoch.or(shared.replay_epoch),
        spot,
        notes,
    })
}

/// The `LiveSourceSpec` a typed `[input]` describes. `--source-iq` without
/// `--source` sets `iq` on a `type = "file"` source (D6).
fn spec_from_file(source: &config::SourceFromFile, cli_source_iq: bool) -> Result<LiveSourceSpec> {
    Ok(match source {
        config::SourceFromFile::Audio { device } => LiveSourceSpec::AudioDevice(device.clone()),
        config::SourceFromFile::File { path, iq } => LiveSourceSpec::File {
            path: path.clone(),
            source_iq: *iq || cli_source_iq,
        },
        config::SourceFromFile::Kiwi {
            host,
            port,
            freq_hz,
            password,
        } => LiveSourceSpec::Kiwi(KiwiOpts {
            host: Some(host.clone()),
            port: *port,
            freq: Some(*freq_hz),
            password: password.clone(),
        }),
        #[cfg(feature = "soapy")]
        config::SourceFromFile::Soapy {
            driver,
            freq_hz,
            rate_hz,
            gain_db,
        } => LiveSourceSpec::Soapy(SoapyOpts {
            driver: Some(driver.clone()),
            freq: Some(*freq_hz),
            rate: Some(*rate_hz),
            gain: *gain_db,
        }),
        #[cfg(not(feature = "soapy"))]
        config::SourceFromFile::Soapy { .. } => {
            bail!("input.type = \"soapy\" needs a manta built with --features soapy")
        }
        #[cfg(feature = "hpsdr")]
        config::SourceFromFile::Hpsdr {
            host,
            port,
            freq_hz,
            rate_hz,
        } => LiveSourceSpec::Hpsdr(HpsdrOpts {
            host: Some(host.clone()),
            port: *port,
            freq: Some(*freq_hz),
            rate: Some(*rate_hz),
        }),
        #[cfg(not(feature = "hpsdr"))]
        config::SourceFromFile::Hpsdr { .. } => {
            bail!("input.type = \"hpsdr\" needs a manta built with --features hpsdr")
        }
    })
}

/// `run`/`soak`/`doctor`'s shared start: load the config (`--config`, else
/// `MANTA_CONFIG`) with the `MANTA_*` overlay, merge the CLI over it, print
/// the merge notes, and build the pipeline config. The environment is read
/// here and only here, with `vars_os` so a non-UTF-8 variable is never a
/// panic (MAN-261 scenario 4).
struct Prepared {
    config_path: Option<PathBuf>,
    loaded: config::Loaded,
    resolved: Resolved,
    pipeline: PipelineConfig,
}

/// MAN-79 scenario 2: one stderr line when the built-in cty.dat is in use
/// and older than manta_spot::vintage::CTY_DAT_STALE_AFTER_DAYS. `now` is a
/// parameter so tests never depend on today's date.
fn bundled_cty_warning(cty_overridden: bool, now: std::time::SystemTime) -> Option<String> {
    if cty_overridden {
        return None;
    }
    let days = manta_spot::vintage::stale_cty_dat_age_days(now)?;
    Some(format!(
        "warning: the built-in cty.dat is {days} days old (retrieved {}), so calls from \
         prefixes allocated since then are not spotted. Download the current file from \
         https://www.country-files.com/cty/cty.dat and pass it with --cty or set spot.cty_path.",
        manta_spot::vintage::CTY_DAT_RETRIEVED
    ))
}

/// Shared typed config and source resolution, with no pipeline assets or I/O.
struct SourcePrepared {
    config_path: Option<PathBuf>,
    loaded: config::Loaded,
    resolved: Resolved,
}

fn prepare_source(cli: CliOverrides, config_flag: Option<PathBuf>) -> Result<SourcePrepared> {
    let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let config_path = config_flag.or_else(|| config::config_path_from_env(&vars));
    let loaded = config::load(config_path.as_deref(), config::Env::Read(&vars))?;
    let resolved = resolve(cli, &loaded)?;
    for note in &resolved.notes {
        eprintln!("{note}");
    }
    Ok(SourcePrepared {
        config_path,
        loaded,
        resolved,
    })
}

fn prepare_live(
    cli: CliOverrides,
    config_flag: Option<PathBuf>,
    cli_engine: Option<Engine>,
) -> Result<Prepared> {
    let SourcePrepared {
        config_path,
        loaded,
        resolved,
    } = prepare_source(cli, config_flag)?;
    let decode = merge_cli_engine(cli_engine, loaded.decode.clone());
    let mut pipeline = build_pipeline_config(
        resolved.freq_correction_ppm,
        &resolved.spot,
        loaded.detector,
        decode.engine,
    )?;
    pipeline.decode = decode;
    if let Some(warning) = bundled_cty_warning(pipeline.cty.is_some(), std::time::SystemTime::now())
    {
        eprintln!("{warning}");
    }
    Ok(Prepared {
        config_path,
        loaded,
        resolved,
        pipeline,
    })
}

/// `decode`/`oracle`: one stderr note naming the tables present in the file
/// that the command does not apply (D7).
fn note_ignored_tables(loaded: &config::Loaded, command: &str, applied: &[&str]) {
    if let Some(note) = ignored_tables_note(loaded, command, applied) {
        eprintln!("{note}");
    }
}

/// The text of `note_ignored_tables`'s note, or `None` when every present
/// table applies (`bench sensitivity` returns it instead of printing it).
fn ignored_tables_note(loaded: &config::Loaded, command: &str, applied: &[&str]) -> Option<String> {
    let ignored: Vec<&str> = loaded
        .present
        .iter()
        .map(String::as_str)
        .filter(|t| !applied.contains(t))
        .collect();
    (!ignored.is_empty()).then(|| {
        format!(
            "note: {command} ignores [{}] from {}",
            ignored.join("], ["),
            loaded.origin
        )
    })
}

/// Resolves the address(es) `manta status` should DIAL to reach a running
/// daemon's metrics/status listener (MAN-44). An explicit `--addr` always
/// wins; otherwise a `--config` file's `[server]` table supplies the
/// port, with its `metrics_bind_addr` (MAN-132: the metrics listener's own
/// address, loopback by default -- never `bind_addr`, which only moves
/// telnet/JSON) translated to a real dialable address --
/// `0.0.0.0`/`::` mean "listening on every interface," which isn't itself
/// something a client can connect TO, so those collapse to loopback (the
/// one address guaranteed to reach a same-host daemon). With neither,
/// falls back to the documented default metrics port on loopback.
///
/// Returns every address a hostname resolves to, not just the first
/// (code-review fix): `ToSocketAddrs` on a hostname can return several
/// candidates in resolver-dependent order -- e.g. `localhost` resolving
/// `::1` before `127.0.0.1` on a dual-stack host -- and a daemon bound to
/// `0.0.0.0` (or the default `127.0.0.1`) only listens on IPv4. Keeping just
/// `.next()` picked whichever candidate the resolver happened to list
/// first, reporting a healthy daemon as unreachable whenever that guess
/// was wrong. `fetch_status` tries every returned address in turn (same
/// precedent as `uplink::connect_first_reachable`).
fn resolve_status_addr(
    addr: Option<&str>,
    server: Option<&manta_server::config::ServerConfig>,
) -> Result<Vec<std::net::SocketAddr>> {
    if let Some(addr) = addr {
        if let Ok(sock) = addr.parse() {
            return Ok(vec![sock]);
        }
        // CR-B applies equally here: the daemon accepts a hostname in its
        // own `bind_addr` (resolved via `ToSocketAddrs` in
        // `start_spot_server`), and the runbook tells operators to reach a
        // remote daemon with `--addr <host>:<metrics_port>` -- rejecting a
        // literal-IP-only `--addr` would contradict both.
        use std::net::ToSocketAddrs;
        let addrs: Vec<_> = addr
            .to_socket_addrs()
            .with_context(|| format!("invalid --addr {addr:?}"))?
            .collect();
        if addrs.is_empty() {
            bail!("--addr {addr:?} resolved to no addresses");
        }
        return Ok(addrs);
    }
    let Some(server) = server else {
        return Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], 7302))]);
    };
    match server.metrics_bind_addr.as_str() {
        "0.0.0.0" => Ok(vec![std::net::SocketAddr::new(
            std::net::Ipv4Addr::LOCALHOST.into(),
            server.metrics_port,
        )]),
        "::" => Ok(vec![std::net::SocketAddr::new(
            std::net::Ipv6Addr::LOCALHOST.into(),
            server.metrics_port,
        )]),
        other => match other.parse::<std::net::IpAddr>() {
            Ok(ip) => Ok(vec![std::net::SocketAddr::new(ip, server.metrics_port)]),
            // CR-B: the daemon itself binds `metrics_bind_addr` through
            // `TcpListener::bind((host, port))`, which resolves a
            // hostname via `ToSocketAddrs` (main.rs's `start_spot_server`)
            // rather than requiring a literal IP -- so `metrics_bind_addr =
            // "localhost"` is a config the daemon happily runs on. `manta
            // status` must resolve the same way instead of rejecting a
            // config the daemon itself accepts.
            Err(_) => {
                use std::net::ToSocketAddrs;
                let addrs: Vec<_> = (other, server.metrics_port)
                    .to_socket_addrs()
                    .with_context(|| format!("resolving server.metrics_bind_addr {other:?}"))?
                    .collect();
                if addrs.is_empty() {
                    bail!("server.metrics_bind_addr {other:?} resolved to no addresses");
                }
                Ok(addrs)
            }
        },
    }
}

/// Renders a list of candidate addresses for an error message.
fn format_addrs(addrs: &[std::net::SocketAddr]) -> String {
    addrs
        .iter()
        .map(std::net::SocketAddr::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The full pre-render `manta status` flow: read/parse an optional
/// `--config` (a.k.a. the deprecated `--server-config` alias), resolve
/// the dial address, and fetch+parse the daemon's `/status` document.
/// Extracted so every failure along this path -- not just
/// `fetch_status`'s -- goes through the same exit-2 handling (CR-A).
///
/// `resolve_status_addr` runs INSIDE the tokio runtime, on the blocking
/// pool (MAN-44 code review CR-2): the previous version called it before
/// the runtime -- and therefore before any timer -- existed, so
/// `--timeout-secs` bounded connect+read but not the blocking
/// `ToSocketAddrs` lookup a hostname `--addr` or `bind_addr` triggers. An
/// unreachable or slow resolver then blocked for the OS's own
/// `resolv.conf` budget (commonly 10-40s) regardless of what the operator
/// asked for.
///
/// Resolution and the fetch share ONE end-to-end deadline rather than a
/// timeout window each (see `deadline`/`remaining` in the body): giving
/// each leg its own full `timeout` would let `--timeout-secs 5` take
/// nearly ten seconds, twice the give-up bound the flag advertises.
fn run_status(
    server_config: Option<&std::path::Path>,
    addr: Option<&str>,
    timeout_secs: u64,
) -> Result<manta_server::status_doc::StatusDoc> {
    // MAN-261: the same loader and `MANTA_*` overlay `run` uses, so the
    // port `status` dials is the port the daemon bound -- including a
    // `MANTA_SERVER_METRICS_PORT` override and the `MANTA_CONFIG` fallback.
    // An explicit `--addr` needs no config at all.
    let server = if addr.is_some() {
        None
    } else {
        let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
        let config_path = server_config
            .map(std::path::Path::to_path_buf)
            .or_else(|| config::config_path_from_env(&vars));
        match config_path {
            Some(path) => config::load(Some(&path), config::Env::Read(&vars))?.server,
            None => None,
        }
    };
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let addr_owned = addr.map(str::to_string);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let outcome = rt.block_on(async move {
        // MAN-44 review: ONE end-to-end deadline spans resolution and the
        // fetch. Giving each leg its own full `timeout` let a lookup that
        // finished just under the wire be followed by a fresh, full-length
        // connect/read window, so `--timeout-secs 5` could take nearly ten
        // seconds -- twice the give-up bound the flag advertises, and twice
        // what a cron/Nagios check budgeted for.
        let deadline = tokio::time::Instant::now() + timeout;
        let targets = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || {
                resolve_status_addr(addr_owned.as_deref(), server.as_ref())
            }),
        )
        .await
        .map_err(|_| anyhow!("resolving the daemon's address timed out"))?
        .context("resolving the daemon's address panicked")??;
        // Only what's LEFT of the deadline goes to the fetch. A zero
        // remainder is not special-cased: `fetch_status` bounds itself with
        // this duration and reports the same "timed out talking to ..."
        // error it would for any other exhausted budget.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        fetch_status(&targets, remaining)
            .await
            .with_context(|| format!("could not reach daemon at {}", format_addrs(&targets)))
    });
    // MAN-44 review: the timeout above only DROPS the JoinHandle -- a
    // `spawn_blocking` task cannot be aborted once it is running, so a
    // wedged `ToSocketAddrs` keeps occupying a blocking-pool thread after
    // `--timeout-secs` has already elapsed. Letting `rt` drop here would
    // then block the caller a second time, for the resolver's own
    // `resolv.conf` budget, which is exactly the wait `--timeout-secs`
    // exists to bound: the timeout would fire and the command would still
    // hang, wedging a monitoring invocation. `shutdown_background`
    // detaches the runtime instead of joining it, so the bound the
    // operator asked for is the bound they get; the orphaned lookup is
    // pure-read, owns no caller-visible state, and dies with the process
    // (which `Command::Status` reaches immediately after this returns).
    rt.shutdown_background();
    outcome
}

/// Exit code contract for scripting (cron/Nagios-style): `0` when every
/// enabled uplink target is connected, or there's no uplink configured at
/// all; `1` when the daemon was reached but the uplink is unhealthy. (A
/// third case -- `2`, "could not reach or parse the daemon's status at
/// all" -- is returned directly by `Command::Status`'s handler, since it
/// never gets as far as a `StatusDoc` to pass here.)
fn status_exit_code(doc: &manta_server::status_doc::StatusDoc) -> i32 {
    use manta_server::metrics::OverallUplinkHealth;
    match doc.uplink.health {
        OverallUplinkHealth::Ok | OverallUplinkHealth::Disabled => 0,
        OverallUplinkHealth::Degraded | OverallUplinkHealth::Down => 1,
    }
}

/// What `Command::Status` prints to stderr before exiting 2. Not escaped
/// here as a whole: local diagnostics such as a `--config` TOML error carry
/// a multi-line source snippet that must stay readable. Peer-supplied text
/// is escaped where it enters the error instead -- the status line in
/// `fetch_status_inner`, serde_json's quoted values in `parse_status_doc`
/// (Codex review, PR #95).
fn status_failure_message(e: &anyhow::Error) -> String {
    format!("manta status: {e:#}")
}

/// Bounds a fetched status document's body size (MAN-44): a real status
/// document is kilobytes at most, but a wrong or hostile endpoint
/// answering `GET /status` must not be able to make this allocate without
/// bound.
const MAX_STATUS_BODY_BYTES: u64 = 256 * 1024;
/// Bounds the status line plus header block the same way
/// `manta_server::metrics_http`'s own `MAX_HEADER_LINES` bounds its side of
/// the identical parsing job (CR-E) -- without this, an unbounded
/// `read_line`-per-line loop against a hostile or misbehaving peer that
/// never terminates a line, or never ends its header block, could grow a
/// buffer or spin without bound; the body already had `MAX_STATUS_BODY_BYTES`
/// but the status line and headers did not.
const MAX_STATUS_HEADER_LINES: usize = 100;

/// Fetches and parses `GET /status` from a running daemon's metrics
/// listener (MAN-44) -- a small hand-rolled HTTP/1.1 GET, matching
/// `manta_server::metrics_http`'s own hand-rolled precedent rather than
/// adding an HTTP client dependency for one request. The whole operation
/// (connect, write, read) is bounded by `timeout` so a silent or
/// half-open peer can't hang `manta status` indefinitely.
async fn fetch_status(
    addrs: &[std::net::SocketAddr],
    timeout: std::time::Duration,
) -> Result<manta_server::status_doc::StatusDoc> {
    tokio::time::timeout(timeout, fetch_status_inner(addrs, timeout))
        .await
        .map_err(|_| anyhow!("timed out talking to {}", format_addrs(addrs)))?
}

/// Tries every candidate in turn, returning the first that accepts a TCP
/// connection, or the last error if all of them fail (code-review fix --
/// see `resolve_status_addr`). Same precedent as
/// `uplink::connect_first_reachable`: a hostname resolving to more than
/// one address must not make the CLI give up after the first, resolver-
/// order-dependent candidate.
async fn connect_any(
    addrs: &[std::net::SocketAddr],
    overall_timeout: std::time::Duration,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    connect_any_bounded(addrs, overall_timeout, tokio::net::TcpStream::connect).await
}

/// `connect_any`'s real logic, with the per-candidate connect operation
/// injectable (MAN-44 code review CR-1) so the fall-through-past-a-stalled-
/// candidate behavior is unit-testable with a fake, instantly-controllable
/// "hangs forever" attempt under paused tokio time -- same precedent as
/// `uplink::connect_first_reachable_bounded`. Each candidate gets its own
/// slice of `overall_timeout` -- the time still left before the deadline,
/// split evenly across the candidates not yet tried, so a candidate that
/// fails fast passes its unused time on -- rather than a bare, unbounded
/// `TcpStream::connect`: without a per-candidate
/// bound, a first address that silently black-holes SYNs (a firewall drop,
/// not a refusal) consumed `fetch_status`'s ENTIRE outer timeout before a
/// later, live candidate was ever dialled -- the exact failure this
/// function exists to prevent, defeated by having no bound of its own.
async fn connect_any_bounded<T, F, Fut>(
    addrs: &[std::net::SocketAddr],
    overall_timeout: std::time::Duration,
    connect: F,
) -> std::io::Result<(T, std::net::SocketAddr)>
where
    F: Fn(std::net::SocketAddr) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + overall_timeout;
    let mut last_err = None;
    for (i, &addr) in addrs.iter().enumerate() {
        // Re-split what is LEFT of the deadline across the candidates not
        // yet tried (Codex review, PR #95): a fixed up-front even split let
        // fast refusals strand their unused time, so a live last candidate
        // could time out with most of the budget unspent.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let per_addr_timeout = remaining / ((addrs.len() - i) as u32);
        match tokio::time::timeout(per_addr_timeout, connect(addr)).await {
            Ok(Ok(stream)) => return Ok((stream, addr)),
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => {
                last_err = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("connect to {addr} timed out"),
                ));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no addresses to try")
    }))
}

async fn fetch_status_inner(
    addrs: &[std::net::SocketAddr],
    timeout: std::time::Duration,
) -> Result<manta_server::status_doc::StatusDoc> {
    use manta_server::bounded_io::read_line_bounded;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    let (mut stream, addr) = connect_any(addrs, timeout)
        .await
        .with_context(|| format!("connecting to {}", format_addrs(addrs)))?;
    stream
        .write_all(
            format!("GET /status HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .context("writing the status request")?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    read_line_bounded(&mut reader, &mut status_line)
        .await
        .context("reading the status line")?;
    if !status_line.starts_with("HTTP/1.1 200") {
        bail!(
            "daemon returned {}",
            manta_server::status_doc::escape_for_terminal(status_line.trim_end())
        );
    }

    for _ in 0..MAX_STATUS_HEADER_LINES {
        let mut line = String::new();
        let n = read_line_bounded(&mut reader, &mut line)
            .await
            .context("reading response headers")?;
        if n == 0 || line == "\r\n" {
            break;
        }
    }

    let mut body = Vec::new();
    reader
        .take(MAX_STATUS_BODY_BYTES)
        .read_to_end(&mut body)
        .await
        .context("reading the status body")?;
    let body = String::from_utf8(body).context("status body was not valid UTF-8")?;
    parse_status_doc(&body)
}

/// Parses a `/status` body AND enforces its `schema_version` (Codex
/// review, PR #95).
///
/// `schema_version` only earns its place in the document if someone
/// checks it: a newer daemon that removed or re-meaning'd a field can
/// still deserialize structurally into this build's `StatusDoc` -- serde
/// ignores unknown keys and every field this build requires may well
/// still be present -- and `manta status` would then render it, and pick
/// an exit code from it, under semantics that no longer hold. An operator
/// running a monitoring one-liner would get a confident `0`/`1` computed
/// from a document this build cannot actually interpret.
///
/// So a version this build does not understand is a hard error, which
/// `Command::Status` turns into the documented exit 2 ("could not reach
/// or parse the daemon's status at all", `docs/RUNBOOKS/uplink-health.md`)
/// -- deliberately NOT exit 1, which means "asked, and the uplink is
/// unhealthy". Version skew is a tooling problem, not an uplink problem.
///
/// The version is read BEFORE the document is deserialized into
/// `StatusDoc`, not after: an incompatible future schema is exactly the
/// one that may have dropped or renamed a field this build requires, and
/// deserializing first would then fail with "not a valid status document"
/// -- reporting a malformed daemon when the real cause is version skew,
/// and hiding the one message that tells an operator which side to
/// upgrade. Reading the version off a `serde_json::Value` first makes the
/// check hold for EVERY document carrying a version this build does not
/// understand, structurally compatible or not. (The extra intermediate
/// parse costs nothing worth counting: one kilobytes-sized document, once,
/// per `manta status` invocation.)
///
/// A body with no `schema_version` at all -- or a non-integer one -- is
/// deliberately NOT a version error: it is a wrong endpoint answering
/// `GET /status`, and it falls through to the structural parse so it
/// fails on "not a valid status document" instead.
///
/// Separated from `fetch_status_inner` so this is testable without a
/// socket; `fetch_status_inner` has no other post-read logic to keep.
fn parse_status_doc(body: &str) -> Result<manta_server::status_doc::StatusDoc> {
    use manta_server::status_doc::STATUS_SCHEMA_VERSION;

    let value: serde_json::Value = serde_json::from_str(body).map_err(invalid_status_doc)?;
    if let Some(version) = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    {
        if version != u64::from(STATUS_SCHEMA_VERSION) {
            bail!(
                "daemon reports status schema_version {} but this manta build understands only {} \
                 -- upgrade whichever of the daemon and the CLI is older",
                version,
                STATUS_SCHEMA_VERSION
            );
        }
    }
    let doc: manta_server::status_doc::StatusDoc =
        serde_json::from_value(value).map_err(invalid_status_doc)?;
    Ok(doc)
}

/// serde_json's data errors quote the offending value ("unknown variant
/// `...`", "invalid type: string ..."), and that value comes from the
/// peer, so it is escaped before it can reach the operator's terminal
/// (Codex review, PR #95).
fn invalid_status_doc(e: serde_json::Error) -> anyhow::Error {
    anyhow!(
        "status body was not a valid status document: {}",
        manta_server::status_doc::escape_for_terminal(&e.to_string())
    )
}

fn main() -> Result<()> {
    warn_deprecations();
    match Cli::parse().command {
        Command::Devices { json } => std::process::exit(devices::run(json)?),
        Command::Check(args) => {
            let code = match source_check::run(args) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("error: {error:#}");
                    1
                }
            };
            std::process::exit(code);
        }
        Command::Decode {
            path,
            center_freq_hz,
            json,
            filters,
            config,
            engine,
        } => {
            let FilterOpts {
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                cty,
                scp,
            } = filters;
            // D7/D8: the whole file is validated, the environment is never
            // read; [decode], [detector], [spot] and input.freq_correction_ppm
            // apply, with the CLI over the file.
            let loaded = config::load(config.as_deref(), config::Env::Ignore)?;
            note_ignored_tables(&loaded, "decode", &["decode", "detector", "spot", "input"]);
            let shared = &loaded.input.shared;
            if loaded.input.source.is_some()
                || shared.center_freq_hz.is_some()
                || shared.capture_rate_hz.is_some()
                || shared.replay_epoch.is_some()
            {
                eprintln!(
                    "note: decode applies only input.freq_correction_ppm from [input] in {}",
                    loaded.origin
                );
            }
            let spot = resolve_spot(allowlist, blocklist, notch, cty, scp, &loaded.spot);
            let decode_cfg = merge_cli_engine(engine, loaded.decode.clone());
            let mut cfg = build_pipeline_config(
                freq_correction_ppm
                    .or(loaded.input.shared.freq_correction_ppm)
                    .unwrap_or(0.0),
                &spot,
                loaded.detector,
                decode_cfg.engine,
            )?;
            cfg.decode = decode_cfg;
            // MAN-131: say so before decoding when the reported frequencies
            // will be baseband offsets. Only for an existing file, so a
            // missing WAV still fails with just its `open WAV` error.
            if center_freq_hz.is_none() && path.is_file() {
                let sidecar = manta_input::read_sidecar(&path)?;
                if let Some(warning) =
                    recording_center_warning(&path, sidecar.map(|sc| sc.center_freq_hz))
                {
                    eprintln!("{warning}");
                }
            }
            let report = decode_wav(&path, center_freq_hz, &cfg)?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("{}", report.text);
                eprintln!("freq_hz: {:.1}  wpm: {:?}", report.freq_hz, report.wpm);
                eprintln!("spots: {}", report.spots.len());
            }
        }
        Command::Oracle {
            path,
            rbn_csv,
            spotter,
            capture_start,
            window_s,
            config,
            engine,
            jsonl,
        } => {
            let loaded = config::load(config.as_deref(), config::Env::Ignore)?;
            note_ignored_tables(&loaded, "oracle", &["decode"]);
            let mut src = manta_input::WavIqSource::open(&path)?;
            let (fs, center) = (src.sample_rate(), src.center_freq_hz());
            let iq = manta_input::read_all(&mut src)?;
            let spots = manta_testkit::oracle::parse_rbn_spots(&rbn_csv, &spotter, capture_start)?;
            let cfg = merge_cli_engine(engine, loaded.decode);
            let (results, summary) =
                manta_testkit::oracle::run_oracle(&iq, fs, center, &spots, window_s, &cfg)?;
            if let Some(p) = jsonl {
                let mut w = std::io::BufWriter::new(std::fs::File::create(p)?);
                for r in &results {
                    use std::io::Write;
                    writeln!(w, "{}", serde_json::to_string(r)?)?;
                }
            }
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Command::Gen { vector, out } => {
            let spec = match vector.as_str() {
                "v1" => manta_testkit::vectors::v1(),
                "v2" => manta_testkit::vectors::v2(),
                "v3" => manta_testkit::vectors::v3(),
                "v4" => manta_testkit::vectors::v4(),
                "v5" => manta_testkit::vectors::v5(),
                "v6" => manta_testkit::vectors::v6(),
                "vr1" => manta_testkit::vectors::vr1(),
                "vr2" => manta_testkit::vectors::vr2(),
                "vr3" => manta_testkit::vectors::vr3(),
                "vr4" => manta_testkit::vectors::vr4(),
                "vr5" => manta_testkit::vectors::vr5(),
                "vr6a" => manta_testkit::vectors::vr6a(),
                "vr6b" => manta_testkit::vectors::vr6b(),
                "vr7" => manta_testkit::vectors::vr7(),
                "vr8" => manta_testkit::vectors::vr8(),
                other => bail!(
                    "unknown vector {other:?} (available: v1-v6, vr1-vr5, vr6a, vr6b, vr7-vr8)"
                ),
            };
            std::fs::create_dir_all(&out)?;
            let manifest = manta_testkit::vectors::write_fixture_set(&spec, &out)?;
            eprintln!(
                "wrote {}/{{{}.wav,{}.json,{}.manifest.json}} (expected freq {:.1} Hz)",
                out.display(),
                spec.name,
                spec.name,
                spec.name,
                manifest.expected_freq_hz
            );
        }
        Command::Run {
            device,
            source,
            kiwi_host,
            kiwi_port,
            kiwi_freq_hz,
            kiwi_password,
            #[cfg(feature = "soapy")]
            soapy_driver,
            #[cfg(feature = "soapy")]
            soapy_freq_hz,
            #[cfg(feature = "soapy")]
            soapy_rate_hz,
            #[cfg(feature = "soapy")]
            soapy_gain,
            #[cfg(feature = "hpsdr")]
            hpsdr_host,
            #[cfg(feature = "hpsdr")]
            hpsdr_port,
            #[cfg(feature = "hpsdr")]
            hpsdr_freq_hz,
            #[cfg(feature = "hpsdr")]
            hpsdr_rate_hz,
            capture_rate_hz,
            source_iq,
            filters,
            json,
            decoded_text,
            config,
            dial_freq_hz,
            replay_epoch,
            engine,
        } => {
            let FilterOpts {
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                cty,
                scp,
            } = filters;
            let Prepared {
                config_path,
                loaded,
                resolved,
                pipeline: cfg,
            } = prepare_live(
                CliOverrides {
                    device,
                    source,
                    source_iq,
                    kiwi: KiwiOpts {
                        host: kiwi_host,
                        port: kiwi_port,
                        freq: kiwi_freq_hz,
                        password: kiwi_password,
                    },
                    #[cfg(feature = "soapy")]
                    soapy: SoapyOpts {
                        driver: soapy_driver,
                        freq: soapy_freq_hz,
                        rate: soapy_rate_hz,
                        gain: soapy_gain,
                    },
                    #[cfg(feature = "hpsdr")]
                    hpsdr: HpsdrOpts {
                        host: hpsdr_host,
                        port: hpsdr_port,
                        freq: hpsdr_freq_hz,
                        rate: hpsdr_rate_hz,
                    },
                    freq_correction_ppm,
                    dial_freq_hz,
                    capture_rate_hz,
                    replay_epoch,
                    allowlist,
                    blocklist,
                    notch,
                    cty,
                    scp,
                },
                config,
                engine,
            )?;
            // MAN-268: an unedited manta.example.toml must not start a
            // daemon that spots, or logs in to a collector, as N0CALL.
            // Checked first, on the identities after the MANTA_* overlay:
            // before `is_rf_aware()` (which opens IQ WAVs), the
            // dial-frequency guard, and any source or listener. Callsigns
            // only; `config check` keeps the broad placeholder scan. See
            // docs/DECISIONS/2026-10-10-man268-unattended-packaging.md.
            config_cmd::reject_example_callsigns(&loaded)?;
            let spec = &resolved.spec;
            let dial_freq_hz = resolved.dial_freq_hz;
            // Needed to derive a recording-specific replay epoch/nonce.
            let replay_path = match spec {
                LiveSourceSpec::File { path, .. } => Some(path.clone()),
                _ => None,
            };
            let has_rf_aware_source = spec.is_rf_aware();
            let source_name = spec.name();

            // Servers start iff the resolved config has a [server] table
            // (D7); the guard runs after the load and before any source I/O.
            if loaded.server.is_some() && !has_rf_aware_source && dial_freq_hz.is_none() {
                bail!(
                    "--dial-freq-hz is required with --config when using a plain \
                     audio device or --source WAV file -- neither reports a real RF \
                     frequency (KiwiSDR/SoapySDR already know theirs from \
                     --kiwi-freq/--soapy-freq); set it with --dial-freq-hz, \
                     MANTA_INPUT_CENTER_FREQ_HZ, or input.center_freq_hz"
                );
            }
            if config_path.is_some() && loaded.server.is_none() {
                eprintln!(
                    "note: {} has no [server] table; the telnet/JSON/metrics servers are not \
                     started",
                    loaded.origin
                );
            }
            warn_if_audio_source_has_no_rf_reference(has_rf_aware_source, dial_freq_hz);
            let first = spec.open(resolved.capture_rate_hz, dial_freq_hz)?;
            let replay_epoch = resolved.replay_epoch;

            // MAN-122 review round 6 (P2): installed HERE -- before
            // `start_spot_server` binds the sockets and logs the
            // `listening:` startup banner naming them -- and not after it,
            // as an earlier revision did. That banner is an advertisement:
            // a supervisor or operator that reacts to it by immediately
            // sending SIGINT/SIGTERM must not hit the signal's DEFAULT
            // disposition, which terminates the daemon outright and skips
            // the client/status shutdown drain that MAN-85's handler
            // exists to guarantee. Installing the handler first makes the
            // whole observable window -- banner included -- covered by the
            // drain. A signal landing before the decode loop starts is
            // observed by `listen_with_observers` before its next startup-
            // calibration read (review round 7; before that round it was
            // observed only once the two-second calibration buffer had
            // filled); it processes only what it has already read and
            // returns `Ok` without reading again. A read already in flight is not
            // interrupted: it returns with whatever the source yields next,
            // or fails after that source's own stall bound, and the shutdown
            // sequence below runs on both the `Ok` and the error path,
            // exactly as it does for a signal arriving mid-decode. The
            // banner itself still precedes the listener
            // tasks (see `start_spot_server`), so its ordering against
            // per-connection lines is unchanged.
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_handler = stop.clone();
            ctrlc::set_handler(move || {
                stop_handler.store(true, std::sync::atomic::Ordering::Relaxed);
            })?;

            // Kept alive for the process lifetime: dropping it would stop
            // the spawned server tasks. `None` when --config wasn't
            // given, in which case `spot_server` stays None too. `epoch`/
            // `session_nonce` are deliberately computed IN this branch, not
            // above it -- `--source`-only replay (no --config) never
            // consumes either, and computing `session_nonce` means hashing
            // the entire replayed file a second time after it's already
            // been opened; skip that full-file pass entirely when nothing
            // downstream needs it (round-7 review finding).
            let (
                server_runtime,
                spot_server,
                active_tracks,
                mut active_tracks_poller,
                decode_latency,
                mut decode_latency_poller,
            ) = match loaded.server.clone() {
                Some(server_cfg) => {
                    // `epoch` feeds SpotBus's wall-clock conversion (every
                    // JSON `timestamp`/RBN Zulu field a client observes) --
                    // a live session's epoch is this process's real start
                    // time; a replay session's defaults to the replayed
                    // file's own mtime, a genuine timestamp that's stable
                    // across reruns of the SAME untouched file, but changes
                    // across a copy/download/restore that doesn't preserve
                    // filesystem metadata even though the recording's
                    // content is identical -- pass --replay-epoch to pin an
                    // exact value when that matters more than "whatever
                    // this machine's copy says" (round-7 review finding;
                    // see the flag's own doc comment for the full
                    // rationale, and `epoch_for_replay_path`'s for why
                    // neither "always now()" nor a content-hash alone was
                    // right before this flag existed). `session_nonce` is
                    // the separate, spot-id-uniqueness-only value:
                    // recording-content-derived for file replay (so
                    // different recordings never collide on id even at the
                    // same track/sample position), nanosecond-precision-now
                    // for a live session (so two live sessions started
                    // within the same wall-clock second don't collide
                    // either).
                    let epoch = resolve_epoch(replay_path.as_deref(), replay_epoch)?;
                    let session_nonce: u128 = match &replay_path {
                        Some(replay_path) => session_nonce_for_replay_path(replay_path)?,
                        // Live session: `epoch` above is already SystemTime::now().
                        None => epoch
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .expect("epoch predates the Unix epoch")
                            .as_nanos(),
                    };

                    // The resolved (CLI > env > file, MAN-261) value, already
                    // validated by `check_freq_correction_ppm`; re-derived
                    // here because the factor, not the ppm, is what the
                    // advertised SETT bounds are scaled by (MAN-86 review).
                    let freq_calibration =
                        manta_spot::calibration_factor_from_ppm(resolved.freq_correction_ppm)
                            .map_err(|e| anyhow!(e))?;
                    // MAN-122: the banner `start_spot_server` logs names
                    // the source, its sample rate and its dial frequency,
                    // so all three are read HERE, from `first` (the
                    // startup connection), before it is moved into the
                    // pipeline below.
                    let (rt, server) = start_spot_server(
                        server_cfg,
                        loaded.rbn_uplink.clone(),
                        SourceInfo {
                            name: source_name,
                            sample_rate_hz: first.sample_rate(),
                            dial_freq_hz: first.center_freq_hz(),
                            rf_passband_hz: first.rf_passband_hz(),
                            freq_calibration,
                        },
                        epoch,
                        session_nonce,
                        cfg.cty_table(),
                    )?;
                    // MAN-45 (round-9 finding): the daemon's own copy of the
                    // gauge `manta_engine::listen_with_observers` updates as
                    // it runs (on the MAIN thread, outside this tokio
                    // runtime) -- polled into `Metrics` below, the same
                    // bridge shape MAN-55's `confirmed_live_handle` watcher
                    // uses for source liveness. MAN-122 additionally takes
                    // the same count synchronously off `listen`'s per-batch
                    // `on_tracks` callback, for the status line's
                    // decode-progress heartbeat, which a polled gauge value
                    // cannot express (a steady count and a wedged decode
                    // loop look identical through the atomic alone).
                    let active_tracks = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                    // MAN-45 remediate (code-review finding 1): the
                    // `JoinHandle` is kept, not discarded, so the shutdown
                    // sequence below can abort this poller and WAIT for it
                    // to actually stop before writing the deterministic
                    // zero -- otherwise a tick already in flight can read
                    // the still-stale gauge and write it right back after
                    // the zero, undoing it.
                    let active_tracks_poller = {
                        let gauge = active_tracks.clone();
                        let track_metrics = server.metrics.clone();
                        rt.spawn(async move {
                            loop {
                                track_metrics.set_active_tracks(
                                    gauge.load(std::sync::atomic::Ordering::Relaxed),
                                );
                                tokio::time::sleep(ACTIVE_TRACKS_POLL_INTERVAL).await;
                            }
                        })
                    };
                    // MAN-128: same bridge shape as `active_tracks`
                    // above -- the engine's decode loop runs on the
                    // main thread, outside this tokio runtime, so a
                    // poller copies its lock-free observer into
                    // `Metrics` on a timer.
                    let decode_latency =
                        std::sync::Arc::new(manta_engine::DecodeLatencyObserver::new());
                    server
                        .metrics
                        .set_decode_latency(latency_histogram_of(&decode_latency.snapshot()));
                    let decode_latency_poller = {
                        let obs = decode_latency.clone();
                        let latency_metrics = server.metrics.clone();
                        rt.spawn(async move {
                            loop {
                                tokio::time::sleep(DECODE_LATENCY_POLL_INTERVAL).await;
                                latency_metrics
                                    .set_decode_latency(latency_histogram_of(&obs.snapshot()));
                            }
                        })
                    };
                    // MAN-128: armed right after the listeners are up
                    // and before the decode loop starts below, so
                    // `/healthz`'s decode check is live for the entire
                    // run -- `mark_decode_stopped()` on the shutdown
                    // path (below) is what makes it catch a source
                    // that has died during the drain that follows.
                    server.metrics.arm_decode_watchdog();
                    // MAN-73: for every reconnectable source kind, ALL
                    // health reporting (including this initial value) is
                    // now owned by `ReconnectingSource` below, which reads
                    // the same `confirmed_live_handle()` signal the old
                    // MAN-55 watcher task used to, and additionally flips
                    // health false/true across every later reconnect, not
                    // just the first connection. File replay is the only
                    // kind that bypasses `ReconnectingSource` entirely
                    // (determinism; see `is_reconnectable`'s doc comment),
                    // so its health is set true here, once, immediately.
                    if !spec.is_reconnectable() {
                        server.metrics.set_source_health(source_name, true);
                    }

                    (
                        Some(rt),
                        Some(server),
                        Some(active_tracks),
                        Some(active_tracks_poller),
                        Some(decode_latency),
                        Some(decode_latency_poller),
                    )
                }
                None => (None, None, None, None, None, None),
            };

            // MAN-73: wrap every reconnectable source so a later read
            // error/EOF is retried with `manta-server::backoff`'s policy
            // instead of propagating out of `listen()` and ending the
            // process. File replay is passed through unwrapped -- its
            // errors and EOF must reach `listen()` unchanged for
            // byte-identical replay.
            let input_health: Option<std::sync::Arc<reconnect::InputHealthTotals>>;
            let src: Box<dyn IqSource> = if spec.is_reconnectable() {
                let initial_healthy = first.confirmed_live_handle().is_none();
                let name = spec.name();
                let on_health = source_health_sink(
                    name,
                    spot_server.as_ref().map(|server| server.metrics.clone()),
                    active_tracks.clone(),
                );
                let reopen_spec = spec.clone();
                let wrapped = ReconnectingSource::new(
                    name,
                    first,
                    Box::new(move || reopen_spec.open(capture_rate_hz, dial_freq_hz)),
                    stop.clone(),
                    initial_healthy,
                    on_health,
                );
                input_health = wrapped.input_health();
                Box::new(wrapped)
            } else {
                input_health = reconnect::InputHealthTotals::for_source(first.as_ref());
                first
            };

            // MAN-56: a source's packet loss/malformed counters are
            // input-layer state manta-server cannot compute itself (it
            // has no manta-input dependency). Sample them into Metrics on
            // a timer, the same wiring-layer-injection shape
            // `set_source_health` uses -- and take the handle HERE, before
            // `listen(src, ..)` below takes ownership of the source for
            // the rest of the run. MAN-228: the handle is the
            // `InputHealthTotals` summed over every connection, so the
            // series keep counting across a MAN-73 reconnect instead of
            // freezing at the startup connection's values. Sources with
            // no wire-packet loss model publish no series at all, which
            // is deliberate: a permanently-zero counter reads as "no
            // loss" rather than "not measured" (ARCHITECTURE §8's
            // "absent means not measured" distinction).
            if let (Some(rt), Some(server), Some(counters)) =
                (&server_runtime, &spot_server, input_health)
            {
                let metrics = server.metrics.clone();
                // Published once eagerly so the series exists (at 0) from
                // the very first scrape rather than only after one poll
                // interval.
                metrics.set_input_health(source_name, counters.snapshot());
                rt.spawn(async move {
                    loop {
                        tokio::time::sleep(INPUT_HEALTH_POLL_INTERVAL).await;
                        metrics.set_input_health(source_name, counters.snapshot());
                    }
                });
            }

            // Printed via `eprintln!` rather than `tracing::info!` because
            // the subscriber is only initialized inside
            // `start_spot_server` -- a plain `listen` (no --server-config)
            // has no subscriber at all. Two jobs: `listen` otherwise prints
            // nothing at startup (2026-09-05 review, lens 1 #4/#7), and it
            // is the readiness handshake `tests/signal_shutdown.rs` waits
            // for. It remains AFTER `ctrlc::set_handler`, which as of
            // review round 6 runs further above (before `start_spot_server`
            // and its `listening:` banner), so every startup line this
            // daemon emits -- this marker and the banner alike -- is now
            // published with the handler already installed; no line
            // advertises a daemon that would still die on the signal's
            // default disposition. If the fuller startup banner (lens 1 #7)
            // ever replaces this line, `READY_MARKER` must be updated to
            // match. stdout stays pure JSON under `--json` (MAN-59
            // round 6); this goes to stderr.
            eprintln!("manta: listening; send SIGINT or SIGTERM to stop");
            // Captured before `src` is moved into the pipeline, for the
            // readiness event below.
            let source_sample_rate_hz = src.sample_rate();
            let mut pipeline_ready_logged = false;
            // MAN-122 review round 5 (P2): `stop` is moved into
            // `listen_with_observers` below, so the readiness observer needs
            // its own handle to read the cancellation flag. The engine runs
            // the padding and calibration `on_tracks` callbacks BEFORE its
            // loop first examines `stop` (manta-engine/src/listen.rs: the
            // two `on_tracks(n_tracks)` calls above `loop { if
            // stop.load(..) { break } }`) -- also when a stop request cut
            // the calibration fill short (review round 7), since what was
            // read is still processed -- so a SIGINT arriving during the
            // two-second startup calibration would otherwise publish
            // `ready: decoding` for a run that shuts down without ever
            // decoding a chunk.
            let stop_ready = stop.clone();
            // MAN-123: decoded text is grouped per track (text_lines.rs) and
            // goes to stderr; stdout carries spots. A daemon -- servers
            // started from a [server] table -- logs no decoded text unless
            // asked, so its journal holds spots and diagnostics only.
            let print_text = !json && (spot_server.is_none() || decoded_text);
            let mut text_lines = text_lines::TrackLines::default();
            let listen_result = manta_engine::listen_with_observers(
                src,
                &cfg,
                stop,
                manta_engine::ListenObservers {
                    active_tracks: active_tracks.clone(),
                    decode_latency: decode_latency.clone(),
                },
                |ev| {
                    if json {
                        println!("{}", serde_json::to_string(ev).unwrap());
                        return;
                    }
                    if !print_text {
                        return;
                    }
                    if let Some(line) = text_lines.ingest(ev) {
                        eprintln!("{line}");
                    }
                },
                // Provisional CLI-debugging text/JSON printed below is NOT
                // the ecosystem wire contract -- that's `spot_server`
                // (manta-server's telnet/JSON-Lines/WebSocket fan-out,
                // ARCHITECTURE §7), fed here when --config is set. The
                // human `SPOT:` line is stdout's product in text mode
                // (MAN-123); decoded text and diagnostics go to stderr.
                |spot| {
                    if let Some(server) = &spot_server {
                        server.bus.publish(spot.clone());
                        server.metrics.record_spot(spot);
                        // MAN-136/MAN-45: counted ONCE per spot here, NOT
                        // inside `SpotMessage::from_spot` -- that runs once
                        // per connected JSON/WS client (json_stream.rs:126),
                        // so counting there would scale with client count
                        // instead of spot count. Checks BOTH sides: the
                        // operator's own station_callsign is config, not
                        // decoder output, and isn't required to resolve --
                        // but it also never changes, so its side is
                        // resolved once at start_spot_server time.
                        if geography_is_unresolved(&server.cty, &spot.callsign)
                            || server.station_geography_unresolved
                        {
                            server.metrics.record_unresolved_geography();
                        }
                    }
                    if json {
                        println!("{}", serde_json::json!({ "spot": spot }));
                        return;
                    }
                    println!(
                        "SPOT: {} ({:?}) {:.1} Hz {:.0} dB {:.0} wpm conf={:.2}",
                        spot.callsign,
                        spot.spot_type,
                        spot.freq_hz,
                        spot.snr_db,
                        spot.wpm,
                        spot.confidence
                    );
                },
                // MAN-122 review round 1: the live track gauge comes from
                // `TrackManager`'s own lifecycle -- how many tracks are
                // promoted and holding a decoder right now -- not from the
                // `DecoderEvent` stream above. A promoted track whose
                // demodulator has not latched emits nothing for up to the
                // ~30 s silent-GC window, so an event-derived count reports
                // `tracks=0` on a node that is genuinely decoding weak
                // signals.
                //
                // Review round 2: this observer fires once per processed
                // batch (repeats included), so it is also the daemon's
                // decode-progress heartbeat. Two relaxed atomics per
                // ~43 ms chunk, immediately after that chunk's channelizer
                // + TrackManager work -- unmeasurable against it, and the
                // only thing that lets the status line say "stalled"
                // instead of republishing a frozen `tracks=N` forever.
                |n_tracks| {
                    if let Some(server) = &spot_server {
                        server.metrics.set_active_tracks(n_tracks as u64);
                        server.metrics.record_pipeline_batch();
                        // The first batch is the earliest moment the daemon
                        // can honestly claim to be decoding: `listen`'s
                        // calibration read has returned, the channelizer is
                        // built and the TrackManager has processed real
                        // hops. The startup banner above only ever claimed
                        // bound sockets (review round 2).
                        // Checked here rather than only at first-batch
                        // time so a cancelled startup cannot publish a
                        // false readiness event: `stop` is already `true`
                        // by the time the calibration callbacks run, and
                        // the decode loop breaks out immediately after
                        // them. Not latching `pipeline_ready_logged` on
                        // this path is deliberate -- the flag means "we
                        // have claimed readiness", and no claim was made.
                        if !pipeline_ready_logged
                            && !stop_ready.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            pipeline_ready_logged = true;
                            tracing::info!(
                                "{}",
                                manta_server::status::format_pipeline_ready(
                                    env!("CARGO_PKG_VERSION"),
                                    source_name,
                                    source_sample_rate_hz,
                                )
                            );
                        }
                    }
                },
            );

            // A source read error returns from listen without closing its
            // tracks, so their partial lines are printed here, on both paths.
            if print_text {
                for line in text_lines.finish() {
                    eprintln!("{line}");
                }
            }

            // Run the same server-shutdown sequence on BOTH the success and
            // error paths -- an SDR disconnect or WAV read failure from
            // `listen` must not skip draining already-published spots or
            // abort in-flight client writes with no chance to finish, which
            // a bare `listen(...)?` before this block used to do on any
            // error (round-7 review finding). Explicitly signal the client
            // tasks to drain (e.g. spots from TrackManager::finish() just
            // before `listen` returned) before tearing the runtime down.
            if let Some(server) = &spot_server {
                // MAN-128: `listen_with_observers` has just returned, which
                // means the decode loop has genuinely stopped (success or
                // error alike -- `listen_result` is captured above rather
                // than `?`-ed for exactly this reason). This is the first
                // thing done in this block, before touching any poller or
                // sending `shutdown_tx`, so `/healthz` reports unhealthy for
                // the entire up-to-`SHUTDOWN_DRAIN_DEADLINE` drain that
                // follows -- the window in which `source_health` alone
                // would otherwise still read healthy for a source that has
                // already died. One final snapshot is published first so a
                // scrape during the drain sees the decode loop's true last
                // latency distribution rather than a stale mid-run one.
                if let Some(obs) = &decode_latency {
                    server
                        .metrics
                        .set_decode_latency(latency_histogram_of(&obs.snapshot()));
                }
                server.metrics.mark_decode_stopped();
                if let Some(poller) = decode_latency_poller.take() {
                    poller.abort();
                    if let Some(rt) = server_runtime.as_ref() {
                        let _ = rt.block_on(poller);
                    }
                }
                // MAN-45 remediate (code-review finding 1): the engine's
                // own gauge is already 0 on the SUCCESS path
                // (TrackManager::finish() closed every track before
                // `listen_with_observers` returned), but NOT on the ERROR
                // path -- `listen_result` above is deliberately captured
                // rather than `?`-ed so an SDR disconnect or WAV read
                // failure still runs this drain sequence (round-7 finding),
                // and on that path `listen_with_observers` returns before
                // reaching `tm.finish()`'s trailing zero, leaving the
                // shared `AtomicU64` at the last processed chunk's nonzero
                // count. The still-running poller reads that stale value
                // every `ACTIVE_TRACKS_POLL_INTERVAL` and would overwrite
                // the deterministic zero below within one tick if left
                // running -- abort it and AWAIT its actual termination
                // first (not just issue the abort and hope), so no
                // in-flight tick can race the zero-write below. A metrics
                // scrape landing anywhere in the `SHUTDOWN_DRAIN_DEADLINE`
                // window that follows must never see a stale nonzero
                // count for a daemon with no live tracks.
                if let Some(poller) = active_tracks_poller.take() {
                    poller.abort();
                    if let Some(rt) = server_runtime.as_ref() {
                        let _ = rt.block_on(poller);
                    }
                }
                server.metrics.set_active_tracks(0);
                // Before `shutdown_tx.send(true)`, not after: ordering is
                // what makes this observable at all (MAN-64) -- writing it
                // after the drain would guarantee no scraper still
                // connected to the (by-then torn-down) metrics
                // listener could ever see it. Even before the drain, this
                // is a best-effort window, not a guarantee: the metrics
                // listener only outlives this call for as long as a
                // telnet/JSON/WS client is genuinely draining underneath
                // `shutdown_runtime_after_drain`'s `await_all` (up to
                // `SHUTDOWN_DRAIN_DEADLINE`); with none connected the
                // runtime tears down microseconds later and no scrape can
                // land (see `record_terminal_source_health`'s doc comment).
                record_terminal_source_health(&server.metrics, source_name, &listen_result);
                let _ = server.shutdown_tx.send(true);
                // MAN-122 review round 4 (P2): shutdown is signalled ABOVE
                // and the periodic status task is joined HERE, before the
                // client drain below starts -- the task's own guards order
                // shutdown against its two `select!` poll points, but not
                // against the wall clock between its last `shutdown.borrow()`
                // and the `tracing::info!` that follows, so a line could
                // otherwise still be emitted into the middle of the drain.
                // Joining makes the two mutually ordered: `spawn_status_line`
                // returns from its `shutdown.changed()` arm as soon as the
                // send above is observed, so this wait is bounded by one
                // scheduler poll, not by `status_interval_secs`.
                if let Some(status_line) = server
                    .status_line
                    .lock()
                    .expect("status-line handle mutex poisoned")
                    .take()
                {
                    if let Some(rt) = server_runtime.as_ref() {
                        let _ = rt.block_on(status_line);
                    }
                }
            }
            // `server_runtime`/`spot_server` are always constructed as a
            // matched pair (both `Some` or both `None`, see their
            // construction above) -- `zip` makes that invariant explicit
            // instead of a defensive branch for a case that can't happen.
            if let Some((rt, server)) = server_runtime.zip(spot_server.as_ref()) {
                shutdown_runtime_after_drain(rt, &server.tasks);
            }
            listen_result?;
        }
        Command::Soak {
            duration,
            device,
            source,
            kiwi_host,
            kiwi_port,
            kiwi_freq_hz,
            kiwi_password,
            #[cfg(feature = "soapy")]
            soapy_driver,
            #[cfg(feature = "soapy")]
            soapy_freq_hz,
            #[cfg(feature = "soapy")]
            soapy_rate_hz,
            #[cfg(feature = "soapy")]
            soapy_gain,
            #[cfg(feature = "hpsdr")]
            hpsdr_host,
            #[cfg(feature = "hpsdr")]
            hpsdr_port,
            #[cfg(feature = "hpsdr")]
            hpsdr_freq_hz,
            #[cfg(feature = "hpsdr")]
            hpsdr_rate_hz,
            capture_rate_hz,
            source_iq,
            filters,
            dial_freq_hz,
            config,
        } => {
            let FilterOpts {
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                cty,
                scp,
            } = filters;
            let Prepared {
                loaded,
                resolved,
                pipeline: cfg,
                ..
            } = prepare_live(
                CliOverrides {
                    device,
                    source,
                    source_iq,
                    kiwi: KiwiOpts {
                        host: kiwi_host,
                        port: kiwi_port,
                        freq: kiwi_freq_hz,
                        password: kiwi_password,
                    },
                    #[cfg(feature = "soapy")]
                    soapy: SoapyOpts {
                        driver: soapy_driver,
                        freq: soapy_freq_hz,
                        rate: soapy_rate_hz,
                        gain: soapy_gain,
                    },
                    #[cfg(feature = "hpsdr")]
                    hpsdr: HpsdrOpts {
                        host: hpsdr_host,
                        port: hpsdr_port,
                        freq: hpsdr_freq_hz,
                        rate: hpsdr_rate_hz,
                    },
                    freq_correction_ppm,
                    dial_freq_hz,
                    capture_rate_hz,
                    replay_epoch: None,
                    allowlist,
                    blocklist,
                    notch,
                    cty,
                    scp,
                },
                config,
                None,
            )?;
            if loaded.server.is_some() {
                eprintln!(
                    "note: soak does not start the spot servers; ignoring [server] from {}",
                    loaded.origin
                );
            }
            let spec = &resolved.spec;
            warn_if_audio_source_has_no_rf_reference(spec.is_rf_aware(), resolved.dial_freq_hz);
            let src: Box<dyn IqSource> =
                spec.open(resolved.capture_rate_hz, resolved.dial_freq_hz)?;
            let report = manta_engine::soak(src, &cfg, std::time::Duration::from_secs(duration))?;
            eprintln!("{report:?}");
            if !manta_engine::soak_passed(&report) {
                std::process::exit(1);
            }
        }
        Command::Status {
            config,
            addr,
            json,
            timeout_secs,
        } => {
            // CR-A: EVERY failure short of a parsed StatusDoc -- an
            // unreadable/unparseable --config, a bad --addr, or a
            // daemon that couldn't be reached -- exits 2 ("couldn't ask"),
            // distinct from exit 1 ("asked, the uplink is unhealthy",
            // `status_exit_code`). Letting the first two `?`-propagate out
            // of `main` used to exit 1 for those cases too, which is the
            // documented "uplink unhealthy" code
            // (`docs/RUNBOOKS/uplink-health.md`'s exit-code table) -- a
            // typo'd config path was indistinguishable from a genuinely
            // degraded uplink.
            let doc = match run_status(config.as_deref(), addr.as_deref(), timeout_secs) {
                Ok(doc) => doc,
                Err(e) => {
                    eprintln!("{}", status_failure_message(&e));
                    std::process::exit(2);
                }
            };
            if json {
                print!("{}", doc.to_json());
            } else {
                print!("{}", manta_server::status_doc::render_human(&doc));
            }
            std::process::exit(status_exit_code(&doc));
        }
        Command::Doctor {
            duration,
            device,
            source,
            kiwi_host,
            kiwi_port,
            kiwi_freq_hz,
            kiwi_password,
            filters,
            #[cfg(feature = "soapy")]
            soapy_driver,
            #[cfg(feature = "soapy")]
            soapy_freq_hz,
            #[cfg(feature = "soapy")]
            soapy_rate_hz,
            #[cfg(feature = "soapy")]
            soapy_gain,
            #[cfg(feature = "hpsdr")]
            hpsdr_host,
            #[cfg(feature = "hpsdr")]
            hpsdr_port,
            #[cfg(feature = "hpsdr")]
            hpsdr_freq_hz,
            #[cfg(feature = "hpsdr")]
            hpsdr_rate_hz,
            capture_rate_hz,
            source_iq,
            dial_freq_hz,
            config,
            json,
        } => {
            let FilterOpts {
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                cty,
                scp,
            } = filters;
            // Checked before any source is opened -- otherwise an invalid
            // --duration only surfaces after a KiwiSDR/SoapySDR/HPSDR
            // connect/activate already spent real time (or hung/failed for
            // an unrelated hardware reason), and the user never sees the
            // actual duration error at all (round-5 review finding).
            let duration_secs = duration;
            if !(manta_engine::MIN_DURATION.as_secs()..=manta_engine::MAX_DURATION.as_secs())
                .contains(&duration_secs)
            {
                bail!(
                    "--duration must be between {} and {} seconds, got {duration_secs}",
                    manta_engine::MIN_DURATION.as_secs(),
                    manta_engine::MAX_DURATION.as_secs()
                );
            }
            let Prepared {
                loaded,
                resolved,
                pipeline: cfg,
                ..
            } = prepare_live(
                CliOverrides {
                    device,
                    source,
                    source_iq,
                    kiwi: KiwiOpts {
                        host: kiwi_host,
                        port: kiwi_port,
                        freq: kiwi_freq_hz,
                        password: kiwi_password,
                    },
                    #[cfg(feature = "soapy")]
                    soapy: SoapyOpts {
                        driver: soapy_driver,
                        freq: soapy_freq_hz,
                        rate: soapy_rate_hz,
                        gain: soapy_gain,
                    },
                    #[cfg(feature = "hpsdr")]
                    hpsdr: HpsdrOpts {
                        host: hpsdr_host,
                        port: hpsdr_port,
                        freq: hpsdr_freq_hz,
                        rate: hpsdr_rate_hz,
                    },
                    freq_correction_ppm,
                    dial_freq_hz,
                    capture_rate_hz,
                    replay_epoch: None,
                    allowlist,
                    blocklist,
                    notch,
                    cty,
                    scp,
                },
                config,
                None,
            )?;
            if loaded.server.is_some() {
                eprintln!(
                    "note: doctor does not start the spot servers; ignoring [server] from {}",
                    loaded.origin
                );
            }
            let spec = &resolved.spec;
            warn_if_audio_source_has_no_rf_reference(spec.is_rf_aware(), resolved.dial_freq_hz);
            let src: Box<dyn IqSource> =
                spec.open(resolved.capture_rate_hz, resolved.dial_freq_hz)?;
            let report = manta_engine::doctor(src, &cfg, std::time::Duration::from_secs(duration))?;
            if json {
                // `verdict()` is computed, not a stored field, so a plain
                // `serde_json::to_string(&report)` omits the command's
                // primary health classification entirely -- merge it in as
                // an extra key rather than making JSON consumers duplicate
                // the classification policy themselves.
                let mut value = serde_json::to_value(&report)?;
                if let serde_json::Value::Object(ref mut map) = value {
                    map.insert(
                        "verdict".to_string(),
                        serde_json::to_value(report.verdict())?,
                    );
                }
                println!("{}", serde_json::to_string(&value)?);
            } else {
                print_doctor_report(&report);
            }
        }
        Command::Config(ConfigCommand::Check { config }) => config_cmd::check(config)?,
        Command::Config(ConfigCommand::Init { out, force }) => config_cmd::init(&out, force)?,
        Command::Bench(BenchCommand::Sensitivity(args)) => bench::sensitivity(args)?,
    }
    Ok(())
}

/// Which replaced CLI spelling the operator typed, if any.
///
/// D11/MAN-77 promoted `listen --server-config` to `run --config`. clap
/// cannot answer this: an alias is normalized to the subcommand's canonical
/// name inside `Parser::possible_subcommand` before `ArgMatches` ever sees
/// it, and `MatchedArg` records no alias spelling for flags either. So the
/// only source of truth is raw argv, read before `Cli::parse()`.
#[derive(Debug, PartialEq, Eq)]
enum Deprecation {
    /// `listen` used to start the daemon (i.e. with a config file). Plain
    /// `listen --device`/`--kiwi-host` is NOT deprecated -- MAN-77's title
    /// keeps `listen` for ad hoc audio/dev testing.
    ListenVerb,
    /// `--server-config`, under either verb.
    ServerConfigFlag,
}

fn deprecations<I: IntoIterator<Item = String>>(args: I) -> Vec<Deprecation> {
    let argv: Vec<String> = args.into_iter().collect();
    let is_flag = |name: &str| {
        argv.iter()
            .any(|a| a == name || a.strip_prefix(name).is_some_and(|r| r.starts_with('=')))
    };
    let mut out = Vec::new();
    let has_config = is_flag("--config") || is_flag("--server-config");
    if argv.get(1).map(String::as_str) == Some("listen") && has_config {
        out.push(Deprecation::ListenVerb);
    }
    if is_flag("--server-config") {
        out.push(Deprecation::ServerConfigFlag);
    }
    out
}

/// stderr, not `tracing::warn!`: the only `tracing_subscriber` init in this
/// binary lives inside `start_spot_server`, so a parse-time `warn!` would be
/// dropped. stderr is also the stream `--json`'s JSON Lines consumer never
/// reads (see `start_spot_server`'s MAN-59 round-6 note), so this cannot
/// corrupt the byte-identical spot log AGENTS.md requires.
///
/// Known, accepted limitation: a flag *value* that is literally the string
/// `--server-config` (e.g. a blocklist path so named) would trigger a
/// spurious notice, because the scan is positional-unaware by design -- it
/// runs before clap, so it cannot know which tokens are values. The failure
/// mode is one extra stderr line, never a wrong exit code or a changed
/// behavior.
fn warn_deprecations() {
    let argv = std::env::args_os().map(|a| a.to_string_lossy().into_owned());
    for d in deprecations(argv) {
        match d {
            Deprecation::ListenVerb => eprintln!(
                "warning: starting the daemon with `manta listen` is deprecated and will be \
                 removed in a future release; use `manta run --config` instead."
            ),
            Deprecation::ServerConfigFlag => eprintln!(
                "warning: `--server-config` is deprecated and will be removed in a future \
                 release; use `--config` instead."
            ),
        }
    }
}

/// Human-readable `manta doctor` summary. `--json` bypasses this entirely
/// in favor of the raw `DoctorReport`.
fn print_doctor_report(report: &manta_engine::DoctorReport) {
    println!(
        "source: {:.0} Hz sample rate, {:.1} Hz center, observed for {:.1}s",
        report.sample_rate_hz,
        report.center_freq_hz,
        report.duration.as_secs_f64()
    );
    println!(
        "tracks: {} promoted, {} TrackMeta updates, {} closed",
        report.tracks_promoted, report.track_meta_count, report.tracks_closed
    );
    match (report.snr_db_min, report.snr_db_median, report.snr_db_max) {
        (Some(min), Some(median), Some(max)) => {
            println!("snr_2500_db: min={min:.1} median={median:.1} max={max:.1}");
        }
        // `tracks_promoted` is verdict()'s own authoritative signal for
        // "did anything really happen" -- match its logic exactly rather
        // than re-deriving it from a different combination of fields.
        _ if report.tracks_promoted == 0 => {
            println!("snr_2500_db: no TrackMeta events -- no track ever promoted")
        }
        _ => println!(
            "snr_2500_db: a track was promoted but no TrackMeta ever landed for it before this \
             run ended"
        ),
    }
    println!(
        "decode: {} chars ({} distinct), {} confirmed spots",
        report.chars_decoded, report.distinct_chars, report.spots_confirmed
    );
    println!("verdict: {}", report.verdict().summary());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    // Derived from manta_spot's constant, never hardcoded, so a cty.dat refresh
    // does not break these tests.
    fn at_day(d: u64) -> SystemTime {
        manta_spot::vintage::cty_dat_retrieved_at().unwrap() + Duration::from_secs(d * 86_400)
    }

    #[test]
    fn audio_silence_warning_names_the_device_the_rate_and_the_hint() {
        let w = audio_silence_warning("\"Test Mic\"");
        assert!(
            w.starts_with(
                "warning: audio input \"Test Mic\" is delivering digital silence at 48000 Hz \
                 (every sample is exactly zero) -- "
            ),
            "{w}"
        );
        assert!(w.ends_with(manta_input::audio::audio_input_hint()), "{w}");
    }

    #[test]
    fn recording_center_warning_covers_a_missing_and_a_non_positive_sidecar() {
        let wav = Path::new("/rec/x.wav");
        let missing = recording_center_warning(wav, None).unwrap();
        assert!(
            missing.starts_with("warning: no sidecar /rec/x.json next to /rec/x.wav -- "),
            "{missing}"
        );
        let zero = recording_center_warning(wav, Some(0.0)).unwrap();
        for w in [&missing, &zero] {
            assert!(
                w.contains("baseband offsets") && w.contains("--center-freq-hz 14000000"),
                "{w}"
            );
        }
        assert!(
            zero.starts_with(
                "warning: sidecar /rec/x.json gives center_freq_hz = 0, not an RF frequency -- "
            ),
            "{zero}"
        );
        assert!(recording_center_warning(wav, Some(-5.0)).is_some());
        assert!(recording_center_warning(wav, Some(14_000_000.0)).is_none());
    }

    #[test]
    fn bundled_cty_warning_names_the_age_and_the_fix() {
        let w = bundled_cty_warning(false, at_day(200)).expect("stale");
        let head = format!(
            "warning: the built-in cty.dat is 200 days old (retrieved {})",
            manta_spot::vintage::CTY_DAT_RETRIEVED
        );
        assert!(w.starts_with(&head), "{w}");
        for needle in [
            "https://www.country-files.com/cty/cty.dat",
            "--cty",
            "spot.cty_path",
        ] {
            assert!(w.contains(needle), "{needle}: {w}");
        }
        assert!(!w.contains('\n'), "one line: {w}");
        // Other suites assert these never appear on these commands' stderr.
        for absent in [
            "deprecated",
            "--dial-freq-hz",
            "telnet=",
            "listening:",
            "station_callsign",
            "(os error 2)",
        ] {
            assert!(!w.contains(absent), "{absent}: {w}");
        }
    }

    #[test]
    fn no_warning_at_or_below_the_threshold() {
        assert_eq!(bundled_cty_warning(false, at_day(180)), None);
        assert_eq!(bundled_cty_warning(false, at_day(0)), None);
    }

    #[test]
    fn no_warning_with_an_override() {
        assert_eq!(bundled_cty_warning(true, at_day(10_000)), None);
    }

    #[test]
    fn no_warning_when_the_clock_reads_before_the_retrieval_date() {
        assert_eq!(bundled_cty_warning(false, UNIX_EPOCH), None);
    }

    const QQ9_ENTITY: &str = "Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n";

    #[test]
    fn cli_cty_beats_cty_path() {
        let spot = config::SpotFile {
            cty_path: Some("file.dat".into()),
            scp_path: Some("file.scp".into()),
            ..Default::default()
        };
        let r = resolve_spot(vec![], None, None, Some("cli.dat".into()), None, &spot);
        assert_eq!(r.cty, Some("cli.dat".into()));
        assert_eq!(r.scp, Some("file.scp".into()));
    }
    #[test]
    fn cli_scp_beats_scp_path() {
        let spot = config::SpotFile {
            cty_path: Some("file.dat".into()),
            scp_path: Some("file.scp".into()),
            ..Default::default()
        };
        let r = resolve_spot(vec![], None, None, None, Some("cli.scp".into()), &spot);
        assert_eq!(r.cty, Some("file.dat".into()));
        assert_eq!(r.scp, Some("cli.scp".into()));
    }
    #[test]
    fn unset_cty_and_scp_stay_built_in() {
        let r = resolve_spot(vec![], None, None, None, None, &config::SpotFile::default());
        let cfg = build_pipeline_config(0.0, &r, Default::default(), Engine::Legacy).unwrap();
        assert!(cfg.cty.is_none() && cfg.scp.is_none());
    }
    fn tables_config(cty: Option<PathBuf>, scp: Option<PathBuf>) -> Result<PipelineConfig> {
        let spot = resolve_spot(vec![], None, None, cty, scp, &config::SpotFile::default());
        build_pipeline_config(0.0, &spot, Default::default(), Engine::Legacy)
    }
    #[test]
    fn build_pipeline_config_loads_an_override_cty_and_scp() {
        let cty = write_temp_file(format!("{}{QQ9_ENTITY}", manta_spot::CTY_DAT).as_bytes());
        let scp = write_temp_file(b"QQ9ZZZ\nW1AW\n");
        let cfg = tables_config(Some(cty.path().into()), Some(scp.path().into())).unwrap();
        assert!(cfg.cty_table().is_allocated("QQ9ZZZ"));
        assert!(cfg.scp_set().contains("QQ9ZZZ"));
        assert_eq!(cfg.scp_set().len(), 2);
    }
    #[test]
    fn build_pipeline_config_rejects_a_cty_file_with_no_prefixes() {
        let cty =
            write_temp_file(b"1A,Sov Mil Order of Malta,246,EU,15,28,41.90,-12.43,-1.0,1A;\n");
        let err = tables_config(Some(cty.path().into()), None)
            .unwrap_err()
            .to_string();
        for needle in [
            "cty.dat file",
            cty.path().to_str().unwrap(),
            "lists no callsign prefixes",
        ] {
            assert!(err.contains(needle), "{err}");
        }
    }
    #[test]
    fn build_pipeline_config_rejects_an_scp_file_with_no_callsigns() {
        let scp = write_temp_file(b"# comment\n!!header\n");
        let err = tables_config(None, Some(scp.path().into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("master.scp file") && err.contains("lists no callsigns"),
            "{err}"
        );
    }
    #[test]
    fn build_pipeline_config_reports_a_missing_cty_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = tables_config(Some(dir.path().join("missing.dat")), None)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("reading cty.dat file"), "{err}");
    }
    #[test]
    fn build_pipeline_config_strips_a_bom_from_cty() {
        let cty = write_temp_file(format!("\u{feff}{QQ9_ENTITY}").as_bytes());
        assert!(tables_config(Some(cty.path().into()), None)
            .unwrap()
            .cty_table()
            .is_allocated("QQ9ZZZ"));
    }
    #[test]
    fn build_pipeline_config_rejects_non_utf8_tables() {
        let file = write_temp_file(&[0xff, 0xfe]);
        for (cty, scp, label) in [
            (Some(file.path().into()), None, "cty.dat"),
            (None, Some(file.path().into()), "master.scp"),
        ] {
            let err = tables_config(cty, scp).unwrap_err().to_string();
            assert!(err.contains(&format!("reading {label} file")), "{err}");
            assert!(err.contains(file.path().to_str().unwrap()), "{err}");
        }
    }
    #[test]
    fn start_spot_server_uses_the_cty_table_it_is_given() {
        let table = std::sync::Arc::new(manta_spot::cty::Table::parse_with_dxcc(
            &GEOGRAPHY_CTY_FIXTURE.replace("K,W,N;", "K,W,N,QQ9;"),
            GEOGRAPHY_DXCC_FIXTURE,
        ));
        let loaded = loaded_from("[server]\nstation_callsign = 'W1AW'\nbind_addr = '127.0.0.1'\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n");
        let (test_runtime, server) = start_spot_server(
            loaded.server.unwrap(),
            vec![],
            SourceInfo {
                name: "file",
                sample_rate_hz: 96_000.0,
                dial_freq_hz: 14_000_000.0,
                rf_passband_hz: (-48_000.0, 48_000.0),
                freq_calibration: 1.0,
            },
            std::time::UNIX_EPOCH,
            0,
            table.clone(),
        )
        .unwrap();
        assert!(std::sync::Arc::ptr_eq(&server.cty, &table));
        assert!(!geography_is_unresolved(&server.cty, "QQ9ZZZ"));
        let entry = server.cty.lookup("QQ9ZZZ").unwrap();
        assert_eq!(
            (&*entry.continent, entry.cq_zone, entry.lat, entry.lon),
            ("NA", 5, 40.0, -75.0)
        );
        let _ = server.shutdown_tx.send(true);
        test_runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    }
    #[test]
    fn help_lists_cty_and_scp_for_every_validating_command() {
        use clap::CommandFactory;
        let cli = Cli::command();
        for name in ["decode", "run", "listen", "soak", "doctor"] {
            let command = cli.find_subcommand(name).unwrap();
            for flag in ["cty", "scp"] {
                assert!(
                    command
                        .get_arguments()
                        .any(|arg| arg.get_long() == Some(flag)),
                    "{name} --{flag}"
                );
            }
        }
    }

    fn write_temp_file(contents: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents).unwrap();
        f.flush().unwrap();
        f
    }

    /// Walk every command/subcommand and every argument, collecting
    /// (path, flag-or-<command>, help text) for each help string clap would
    /// actually show an operator. `Arg::get_id()` is the Rust field name, so
    /// use `get_long()` for the spelling a user types.
    fn help_strings(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String, String)>) {
        let mut push = |who: String, text: Option<&clap::builder::StyledStr>| {
            if let Some(t) = text {
                out.push((path.to_string(), who, t.to_string()));
            }
        };
        push("<command>".into(), cmd.get_about());
        push("<command-long>".into(), cmd.get_long_about());
        push("<after-help>".into(), cmd.get_after_help());
        push("<after-long-help>".into(), cmd.get_after_long_help());
        for a in cmd.get_arguments() {
            let name = a
                .get_long()
                .map(|l| format!("--{l}"))
                .unwrap_or_else(|| format!("<{}>", a.get_id()));
            push(name.clone(), a.get_help());
            push(name, a.get_long_help());
        }
        for sub in cmd.get_subcommands() {
            help_strings(sub, &format!("{path} {}", sub.get_name()), out);
        }
    }

    /// Words an operator who has never seen this repo's specs, roadmap or
    /// ticket tracker must not meet. Case-insensitive and whole-word: `SPEC`
    /// and `spec` are the same leak to an operator, but "special-event
    /// call" is ordinary English and must not trip the guard.
    const JARGON: &[&str] = &[
        r"(?i)\bspec\b",
        r"(?i)\barchitecture\b",
        r"(?i)\broadmap\b",
        r"(?i)\bappendix\b",
        r"\u{a7}",
        r"\bM[0-4]\b",
        r"(?i)\bMAN-\d+",
    ];

    fn jargon_res() -> Vec<regex::Regex> {
        JARGON
            .iter()
            .map(|p| regex::Regex::new(p).unwrap())
            .collect()
    }

    /// MAN-135: `--help` is read by operators who have never seen this
    /// repo's specs, roadmap, or ticket tracker. Contributor-facing
    /// provenance belongs in `//` comments, which clap never republishes;
    /// `///` on a clap field or variant IS user-facing copy.
    #[test]
    fn help_text_is_free_of_internal_process_jargon() {
        use clap::CommandFactory as _;
        let res = jargon_res();

        let mut out = Vec::new();
        help_strings(&Cli::command(), "manta", &mut out);

        let mut bad = Vec::new();
        for (path, who, text) in out {
            for re in &res {
                if let Some(m) = re.find(&text) {
                    bad.push(format!("{path} {who}: {:?} -- {text}", m.as_str()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "internal jargon in --help:\n{}",
            bad.join("\n")
        );
    }

    /// MAN-76: `manta config init` writes the scaffold into an operator's
    /// own file, so it is operator-facing copy just as `--help` is.
    #[test]
    fn scaffold_is_free_of_internal_process_jargon() {
        let res = jargon_res();
        let mut bad = Vec::new();
        for (n, line) in config_cmd::SCAFFOLD.lines().enumerate() {
            for re in &res {
                if let Some(m) = re.find(line) {
                    bad.push(format!("line {}: {:?} -- {line}", n + 1, m.as_str()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "internal jargon in the config scaffold:\n{}",
            bad.join("\n")
        );
    }

    /// `hsmm` was measured against `legacy` and failed its promotion gate
    /// (docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md), so no
    /// `--engine` help may tell an operator it is unmeasured.
    #[test]
    fn engine_help_does_not_call_hsmm_unmeasured() {
        use clap::CommandFactory as _;
        let re = regex::Regex::new(r"(?i)unmeasured|not\s+(yet\s+)?measured").unwrap();
        let mut out = Vec::new();
        help_strings(&Cli::command(), "manta", &mut out);
        let engine: Vec<_> = out.iter().filter(|(_, who, _)| who == "--engine").collect();
        // decode, oracle and run each take --engine; an empty walk would
        // pass vacuously. Count commands, not strings: each arg yields both
        // a short and a long help string.
        let commands: std::collections::BTreeSet<_> =
            engine.iter().map(|(path, _, _)| path).collect();
        assert!(
            commands.len() >= 3,
            "found --engine on {commands:?}, expected decode, oracle and run"
        );
        let bad: Vec<String> = engine
            .iter()
            .filter(|(_, _, text)| re.is_match(text))
            .map(|(path, _, text)| format!("{path}: {text}"))
            .collect();
        assert!(
            bad.is_empty(),
            "--engine help calls hsmm unmeasured:\n{}",
            bad.join("\n")
        );
    }

    /// MAN-135: a flag with no `///` comment renders as a blank line under
    /// `--help`, which tells an operator nothing at all.
    #[test]
    fn every_flag_has_non_empty_help() {
        use clap::CommandFactory as _;
        fn walk(cmd: &clap::Command, path: &str, bad: &mut Vec<String>) {
            for a in cmd.get_arguments() {
                let text = a
                    .get_help()
                    .or_else(|| a.get_long_help())
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    // `get_id()` is the Rust field name (`kiwi_freq_hz`);
                    // report the spelling an operator actually types.
                    let name = a
                        .get_long()
                        .map(|l| format!("--{l}"))
                        .unwrap_or_else(|| format!("<{}>", a.get_id()));
                    bad.push(format!("{path} {name}"));
                }
            }
            for sub in cmd.get_subcommands() {
                walk(sub, &format!("{path} {}", sub.get_name()), bad);
            }
        }
        let mut bad = Vec::new();
        walk(&Cli::command(), "manta", &mut bad);
        assert!(bad.is_empty(), "flags with empty help:\n{}", bad.join("\n"));
    }

    /// MAN-135: frequencies and sample rates should end in `-hz`; ports and
    /// gains should not. Catches the next `--foo-rate` before review does.
    #[test]
    fn every_hz_valued_flag_is_named_hz() {
        use clap::CommandFactory as _;
        // A frequency-ish name that already carries a DIFFERENT explicit unit
        // is self-describing and must not be forced to `-hz`:
        // `--freq-correction-ppm` is parts-per-million, not hertz. Only
        // unitless frequency/rate names are ambiguous to an operator.
        const OTHER_UNIT_SUFFIXES: [&str; 1] = ["-ppm"];
        fn walk(cmd: &clap::Command, path: &str, bad: &mut Vec<String>) {
            for a in cmd.get_arguments() {
                if let Some(long) = a.get_long() {
                    let is_freq_or_rate = long.contains("freq") || long.contains("rate");
                    let has_explicit_unit = long.ends_with("-hz")
                        || OTHER_UNIT_SUFFIXES.iter().any(|s| long.ends_with(s));
                    if is_freq_or_rate && !has_explicit_unit {
                        bad.push(format!("{path} --{long}"));
                    }
                }
            }
            for sub in cmd.get_subcommands() {
                walk(sub, &format!("{path} {}", sub.get_name()), bad);
            }
        }
        let mut bad = Vec::new();
        walk(&Cli::command(), "manta", &mut bad);
        assert!(
            bad.is_empty(),
            "frequency/rate flags without a -hz suffix:\n{}",
            bad.join("\n")
        );
    }

    /// MAN-45 (PR #63 round-16 finding): the outer registry-wide deadline
    /// must never fire before a handler's own drain deadline, or
    /// `shutdown_timeout` aborts a drain that was still inside its budget
    /// and the abandoned queue goes uncounted again -- the exact failure
    /// rounds 15 and 16 both landed on from different directions. The
    /// margin covers task scheduling, not another spot's write.
    ///
    /// MAN-45 remediate (round-16 P1, finding 2): `CLIENT_DRAIN_DEADLINE`
    /// alone under-counts the true worst case -- a handler already mid-
    /// write when shutdown fires doesn't even START its own drain
    /// deadline until that in-progress `select!` branch resolves. Asserts
    /// the FULL relationship (`2 * telnet::WRITE_TIMEOUT +
    /// CLIENT_DRAIN_DEADLINE`, telnet's live-spot write being the largest
    /// such in-progress branch across all three handlers), not just the
    /// drain deadline in isolation -- see `SHUTDOWN_DRAIN_DEADLINE`'s own
    /// doc comment for the full argument.
    #[test]
    fn the_outer_shutdown_deadline_outlives_every_handlers_own_drain_deadline() {
        let worst_case_before_drain_starts = 2 * manta_server::telnet::WRITE_TIMEOUT;
        let true_worst_case =
            worst_case_before_drain_starts + manta_server::tasks::CLIENT_DRAIN_DEADLINE;
        assert!(
            SHUTDOWN_DRAIN_DEADLINE > true_worst_case,
            "SHUTDOWN_DRAIN_DEADLINE ({SHUTDOWN_DRAIN_DEADLINE:?}) must exceed the true \
             worst case of {true_worst_case:?} (2 * telnet::WRITE_TIMEOUT = \
             {worst_case_before_drain_starts:?}, the largest in-progress `select!` branch \
             a handler can already be running when shutdown fires, plus \
             CLIENT_DRAIN_DEADLINE = {:?} for its own drain loop once it gets there)",
            manta_server::tasks::CLIENT_DRAIN_DEADLINE,
        );
    }

    /// MAN-122 review round 4 (P2): `spawn_status_line`'s in-task guards
    /// cannot close the window between its post-sleep `shutdown.borrow()`
    /// and its `tracing::info!`, so the daemon must order the two from the
    /// outside -- retain the task's `JoinHandle` and AWAIT it right after
    /// signalling shutdown, before the client drain starts. This pins both
    /// halves of that: the handle really is retained on `SpotServer` for a
    /// config that enables the status line (a discarded handle is
    /// unjoinable and the ordering is unenforceable), and joining it after
    /// `shutdown_tx.send(true)` completes promptly rather than blocking for
    /// a whole `status_interval_secs`.
    #[test]
    fn the_status_line_task_is_retained_and_joins_promptly_on_shutdown() {
        let cfg_file = write_temp_file(
            br#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0
                status_interval_secs = 3600
                "#,
        );

        let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
        let (rt, server) = start_spot_server(
            loaded.server.unwrap(),
            loaded.rbn_uplink,
            SourceInfo {
                name: "file",
                sample_rate_hz: 96_000.0,
                dial_freq_hz: 14_000_000.0,
                rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
                freq_calibration: 1.0,                 // --freq-correction-ppm 0
            },
            std::time::SystemTime::UNIX_EPOCH,
            0,
            std::sync::Arc::new(manta_spot::cty::Table::bundled()),
        )
        .unwrap();

        let handle =
            server.status_line.lock().unwrap().take().expect(
                "a non-zero status_interval_secs must leave a joinable handle on SpotServer",
            );

        let _ = server.shutdown_tx.send(true);
        // One hour of configured interval against a one-second budget: this
        // can only pass because the task's `shutdown.changed()` arm is
        // `biased`-first and returns without waiting for the next tick.
        let joined = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), handle).await
        });
        assert!(
            joined.is_ok(),
            "the status task must be joinable within a scheduler poll of shutdown, \
             not at the next status_interval_secs tick"
        );
        assert!(
            joined.unwrap().is_ok(),
            "the status task must exit cleanly, not panic"
        );
    }

    #[test]
    fn merge_cli_engine_prefers_the_explicit_cli_flag_over_the_file() {
        // SPEC v2 §7: --engine overrides [decode]'s engine key when both
        // are given -- this is the exact precedence rule Command::Run's
        // (and, since MAN-166's final-review fix batch, Decode's/Oracle's)
        // handler relies on `merge_cli_engine` for.
        let file_decode = manta_decode::decoder::DecodeConfig {
            engine: Engine::Legacy,
            ..manta_decode::decoder::DecodeConfig::default()
        };
        let resolved = merge_cli_engine(Some(Engine::EdgeLegacy), file_decode);
        assert_eq!(resolved.engine, Engine::EdgeLegacy);
    }

    #[test]
    fn merge_cli_engine_falls_back_to_the_file_engine_when_the_flag_is_absent() {
        let file_decode = manta_decode::decoder::DecodeConfig {
            engine: Engine::EdgeLegacy,
            ..manta_decode::decoder::DecodeConfig::default()
        };
        let resolved = merge_cli_engine(None, file_decode);
        assert_eq!(resolved.engine, Engine::EdgeLegacy);
    }

    #[test]
    fn merge_cli_engine_leaves_every_other_decode_field_from_the_file_untouched() {
        // The CLI has no flag for sigma_u/beam/etc. -- only `engine` may be
        // overridden; everything else must come through verbatim from the
        // file-derived DecodeConfig.
        let mut file_decode = manta_decode::decoder::DecodeConfig::default();
        file_decode.evidence.sigma_u = 0.31;
        file_decode.hsmm.beam = 10;
        let resolved = merge_cli_engine(Some(Engine::EdgeLegacy), file_decode);
        assert_eq!(resolved.evidence.sigma_u, 0.31);
        assert_eq!(resolved.hsmm.beam, 10);
    }

    #[test]
    fn load_decode_config_file_defaults_when_no_server_config_given() {
        let cfg = load_decode_config_file(None).unwrap();
        assert_eq!(cfg.engine, Engine::Legacy);
        assert_eq!(
            cfg.evidence.sigma_u,
            manta_decode::evidence::EvidenceConfig::default().sigma_u
        );
    }

    #[test]
    fn load_decode_config_file_reads_the_decode_table_from_server_config() {
        let f = write_temp_file(
            br#"
            [server]
            station_callsign = "W3XYZ"
            [decode]
            engine = "edge-legacy"
            sigma_u = 0.31
            beam = 10
            "#,
        );
        let cfg = load_decode_config_file(Some(f.path())).unwrap();
        assert_eq!(cfg.engine, Engine::EdgeLegacy);
        assert_eq!(cfg.evidence.sigma_u, 0.31);
        assert_eq!(cfg.hsmm.beam, 10);
    }

    #[test]
    fn load_decode_config_file_rejects_zero_fallback_hops() {
        let f = write_temp_file(b"[decode]\nfallback_hops = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_inverted_tau_hi_bounds() {
        // Codex review, PR #161 round 2: this would otherwise panic in
        // f64::clamp on the first speed update instead of failing to load.
        let f = write_temp_file(b"[decode]\ntau_hi_bounds_ms = [400.0, 100.0]\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_tau_lo_ms() {
        let f = write_temp_file(b"[decode]\ntau_lo_ms = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
        let f = write_temp_file(b"[decode]\ntau_lo_ms = -5.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_accepts_valid_tau_bounds() {
        let f =
            write_temp_file(b"[decode]\ntau_lo_ms = 450.0\ntau_hi_bounds_ms = [120.0, 380.0]\n");
        let cfg = load_decode_config_file(Some(f.path())).unwrap();
        assert_eq!(cfg.demod.tau_lo_ms, 450.0);
        assert_eq!(cfg.demod.tau_hi_bounds_ms, (120.0, 380.0));
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_timing_sigma() {
        // Codex review, PR #161 round 3: this would otherwise produce
        // infinite/NaN confidence scores instead of failing to load.
        let f = write_temp_file(b"[decode]\ntiming_sigma = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
        let f = write_temp_file(b"[decode]\ntiming_sigma = -0.5\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_timing_sigma_too_small_to_avoid_underflow() {
        // Codex review, PR #161 round 5: 1e-30 is finite and > 0.0 (so
        // round 3's original check alone would accept it), but
        // beam::log_likelihood's `2.0 * sigma * sigma` underflows to
        // exactly 0.0 in f32, giving NaN confidence for a perfectly-timed
        // candidate.
        let f = write_temp_file(b"[decode]\ntiming_sigma = 1e-30\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_beam_width() {
        let f = write_temp_file(b"[decode]\nbeam_width = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_hsmm_beam() {
        let f = write_temp_file(b"[decode]\nbeam = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_sigma_u() {
        let f = write_temp_file(b"[decode]\nsigma_u = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_sigma_u() {
        let f = write_temp_file(b"[decode]\nsigma_u = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_sigma_u_too_small_to_avoid_underflow() {
        // Codex review, PR #161 round 20: 1e-30 is finite and > 0.0 (so
        // the original check alone passed it), but sigma_u * sigma_u
        // underflows to exactly 0.0 in f32.
        let f = write_temp_file(b"[decode]\nsigma_u = 1e-30\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_empty_seed_units_hops() {
        let f = write_temp_file(b"[decode]\nseed_units_hops = []\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_nonpositive_seed_unit() {
        let f = write_temp_file(b"[decode]\nseed_units_hops = [9.0, 0.0, 18.0]\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_seed_unit_below_u_min() {
        let f = write_temp_file(b"[decode]\nseed_units_hops = [1.0]\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_seed_unit_above_u_max() {
        let f = write_temp_file(b"[decode]\nseed_units_hops = [100.0]\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_conf_kappa() {
        let f = write_temp_file(b"[decode]\nconf_kappa = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_dur_sigma() {
        let f = write_temp_file(b"[decode]\ndur_sigma = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_dur_sigma_too_small_to_avoid_underflow() {
        // Codex review, PR #161 round 16: 1e-30 is finite and > 0.0 (so
        // round 5's original check alone passed it), but 2*dur_sigma^2
        // underflows to exactly 0.0 in f32.
        let f = write_temp_file(b"[decode]\ndur_sigma = 1e-30\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_hold_dits() {
        let f = write_temp_file(b"[decode]\nhold_dits = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_hold_dits_that_overflows_max_retain() {
        // Codex review, PR #161 round 6: a large but individually-plausible
        // hold_dits (300) at the default u_max (56.0) computes h = 16800,
        // far past Evidence's MAX_RETAIN (4096).
        let f = write_temp_file(b"[decode]\nhold_dits = 300\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_hold_dits_that_only_overflows_after_rounding() {
        // Codex review, PR #161 round 20: hold_dits=73.14 at the default
        // u_max (56.0) computes an UNROUNDED product of 4095.84 -- under
        // MAX_RETAIN (4096) -- but Evidence::set_u_ref rounds before
        // enforcing the cap, and round(4095.84) = 4096, which is not
        // "well under" MAX_RETAIN.
        let f = write_temp_file(b"[decode]\nhold_dits = 73.14\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_accepts_the_default_hold_dits() {
        let f = write_temp_file(b"[decode]\n");
        assert!(load_decode_config_file(Some(f.path())).is_ok());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_speed_alpha() {
        let f = write_temp_file(b"[decode]\nspeed_alpha = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_negative_speed_alpha() {
        let f = write_temp_file(b"[decode]\nspeed_alpha = -0.1\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_mark_insert_penalty() {
        let f = write_temp_file(b"[decode]\nmark_insert_penalty = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_infinite_noise_min_bias_db() {
        let f = write_temp_file(b"[decode]\nnoise_min_bias_db = inf\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_infinite_spectral_min_bias_db() {
        let f = write_temp_file(b"[decode]\nspectral_min_bias_db = inf\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_infinite_spectral_beta() {
        let f = write_temp_file(b"[decode]\nspectral_beta = inf\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_negative_spectral_beta() {
        // Codex review, PR #178 round 4: a negative value makes
        // NoiseTracker's max() always discard the spectral term,
        // silently disabling the discount instead of reporting the
        // operator's sign-typo config error.
        let f = write_temp_file(b"[decode]\nspectral_beta = -0.5\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_accepts_zero_spectral_beta() {
        // 0.0 is a valid, explicit "no spectral discount" value, not a
        // sign error -- must stay legal.
        let f = write_temp_file(b"[decode]\nspectral_beta = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_ok());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_refine_bw_hz() {
        let f = write_temp_file(b"[decode]\nrefine_bw_hz = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_infinite_refine_bw_hz() {
        let f = write_temp_file(b"[decode]\nrefine_bw_hz = inf\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_accepts_zero_refine_bw_hz() {
        // 0.0 is the documented "disabled" sentinel, not an error.
        let f = write_temp_file(b"[decode]\nrefine_bw_hz = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_ok());
    }

    #[test]
    fn load_decode_config_file_accepts_a_positive_refine_bw_hz() {
        let f = write_temp_file(b"[decode]\nrefine_bw_hz = 30.0\n");
        let cfg = load_decode_config_file(Some(f.path())).unwrap();
        assert_eq!(cfg.refine_bw_hz, 30.0);
    }

    #[test]
    fn load_decode_config_file_rejects_negative_refine_bw_hz() {
        // Codex review, PR #178: a negative value (e.g. a `-30` sign
        // typo) isn't `> 0.0`, so decoder_input's own bypass check would
        // silently treat it as disabled instead of reporting the error --
        // the documented disabled sentinel is specifically 0.0.
        let f = write_temp_file(b"[decode]\nrefine_bw_hz = -30.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_negative_lookahead_dits() {
        let f = write_temp_file(b"[decode]\nlookahead_dits = -1.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_lookahead_dits() {
        let f = write_temp_file(b"[decode]\nlookahead_dits = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_accepts_zero_lookahead_dits() {
        let f = write_temp_file(b"[decode]\nlookahead_dits = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_ok());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_noise_window_ms() {
        let f = write_temp_file(b"[decode]\nnoise_window_ms = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_infinite_noise_window_ms() {
        let f = write_temp_file(b"[decode]\nnoise_window_ms = inf\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_hysteresis() {
        // Codex review, PR #161 round 4: NaN makes every `Demod::step`
        // comparison false, silently disabling Legacy's decode entirely.
        let f = write_temp_file(b"[decode]\nhyst_frac = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_out_of_range_hysteresis() {
        // hyst_frac >= 0.5 pushes the band's outer edges to or past the
        // rails; <= 0.0 collapses it to a single point (no hysteresis).
        let f = write_temp_file(b"[decode]\nhyst_frac = 0.5\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
        let f = write_temp_file(b"[decode]\nhyst_frac = -1.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_debounce_ms() {
        let f = write_temp_file(b"[decode]\ndebounce_ms = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_flush_gap_dits() {
        // Codex review, PR #161 round 4: <= 0 forces an instant/premature
        // word flush on every hop.
        let f = write_temp_file(b"[decode]\nflush_gap_dits = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    /// Regression: an earlier version of this task rejected `engine =
    /// "hsmm"` at TOML-deserialize time (inside `load_decode_config_file`,
    /// i.e. BEFORE `merge_cli_engine` ever runs), which meant an explicit
    /// `--engine legacy` could never override a config file staging
    /// `engine = "hsmm"` -- the file's parse error fired first, and the
    /// override never got a chance to apply. As of Task 11, `hsmm` is no
    /// longer rejected anywhere in this path, but the precedence rule this
    /// test protects (an explicit `--engine` beats the file's `engine` key)
    /// still matters, so it's kept with `hsmm` as the file-staged value to
    /// prove `merge_cli_engine` reads the CLI override, not the file, when
    /// both are given.
    #[test]
    fn cli_engine_override_beats_a_hsmm_staged_file() {
        let f = write_temp_file(
            br#"
            [server]
            station_callsign = "W3XYZ"
            [decode]
            engine = "hsmm"
            "#,
        );
        let file_decode = load_decode_config_file(Some(f.path())).unwrap();
        assert_eq!(
            file_decode.engine,
            Engine::Hsmm,
            "the file's own value must still be hsmm going into the merge"
        );
        let result = merge_cli_engine(Some(Engine::Legacy), file_decode);
        assert_eq!(
            result.engine,
            Engine::Legacy,
            "an explicit --engine must override a hsmm-staged file"
        );
    }

    /// The other direction: with NO CLI override, a file staging `engine =
    /// "hsmm"` is honored (not rejected) -- `hsmm` is a fully implemented,
    /// reviewed engine (Task 8) with no CLI-level gate as of Task 11.
    #[test]
    fn hsmm_staged_file_is_honored_without_a_cli_override() {
        let f = write_temp_file(
            br#"
            [decode]
            engine = "hsmm"
            "#,
        );
        let file_decode = load_decode_config_file(Some(f.path())).unwrap();
        let result = merge_cli_engine(None, file_decode);
        assert_eq!(result.engine, Engine::Hsmm);
    }

    #[test]
    fn deprecation_notices_fire_only_for_the_replaced_spellings() {
        fn notices(argv: &[&str]) -> Vec<Deprecation> {
            deprecations(argv.iter().map(|s| s.to_string()))
        }
        // D-3: the daemon-via-listen path is deprecated ...
        assert_eq!(
            notices(&["manta", "listen", "--server-config", "m.toml"]),
            vec![Deprecation::ListenVerb, Deprecation::ServerConfigFlag]
        );
        assert_eq!(
            notices(&["manta", "listen", "--config", "m.toml"]),
            vec![Deprecation::ListenVerb]
        );
        // ... the ad hoc audio path the ticket title preserves is NOT.
        assert_eq!(notices(&["manta", "listen", "--device", "hw:1"]), vec![]);
        assert_eq!(notices(&["manta", "listen"]), vec![]);
        // The flag is deprecated under either verb.
        assert_eq!(
            notices(&["manta", "run", "--server-config", "m.toml"]),
            vec![Deprecation::ServerConfigFlag]
        );
        // `--flag=value` form must be caught too.
        assert_eq!(
            notices(&["manta", "run", "--server-config=m.toml"]),
            vec![Deprecation::ServerConfigFlag]
        );
        // The canonical spelling is silent.
        assert_eq!(notices(&["manta", "run", "--config", "m.toml"]), vec![]);
        assert_eq!(notices(&["manta", "run", "--device", "hw:1"]), vec![]);
        // Other subcommands are never implicated.
        assert_eq!(notices(&["manta", "decode", "/tmp/v1.wav"]), vec![]);
        assert_eq!(notices(&["manta", "soak", "--duration", "10"]), vec![]);
        // `status` is scanned like any other verb -- which is only
        // honest because `status` now ACCEPTS the flag the notice tells
        // the operator to switch to (Codex review, PR #95); see
        // `status_accepts_the_canonical_config_flag_the_deprecation_notice_names`.
        assert_eq!(
            notices(&["manta", "status", "--server-config", "m.toml"]),
            vec![Deprecation::ServerConfigFlag]
        );
        assert_eq!(notices(&["manta", "status", "--config", "m.toml"]), vec![]);
    }

    /// Codex review, PR #95: `warn_deprecations` scans raw argv without
    /// knowing the verb, so `manta status --server-config m.toml` printed
    /// "use `--config` instead" while `status` accepted no `--config` at
    /// all -- the notice pointed operators at a spelling clap rejected.
    /// Both spellings must now parse to the same field.

    #[test]
    fn shutdown_runtime_after_drain_awaits_a_tracked_task_to_completion() {
        // Regression (round-9/round-10 review, verified against tokio
        // 1.53's own source): `Runtime::shutdown_timeout`'s `duration`
        // parameter only bounds the BLOCKING thread pool's shutdown -- the
        // async executor itself is torn down synchronously and
        // immediately via `self.handle.inner.shutdown()`. An earlier
        // version of this function papered over that with a fixed blind
        // `sleep` before `shutdown_timeout`: real scheduler time, but no
        // guarantee the task actually FINISHED before the sleep elapsed.
        // This test spawns a task INTO the tracked `ClientTasks` registry
        // that only sets a flag after being signaled AND doing a small
        // amount of real async work (standing in for a socket write) --
        // proving `shutdown_runtime_after_drain` genuinely awaits its
        // completion rather than guessing a duration.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let drained = Arc::new(AtomicBool::new(false));
        let drained_task = drained.clone();
        let tasks = manta_server::tasks::new_client_tasks();

        rt.block_on({
            let tasks = tasks.clone();
            async move {
                tasks.lock().await.spawn(async move {
                    let _ = rx.changed().await;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    drained_task.store(true, Ordering::SeqCst);
                });
            }
        });

        let _ = tx.send(true);
        shutdown_runtime_after_drain(rt, &tasks);

        assert!(
            drained.load(Ordering::SeqCst),
            "the tracked task must have been genuinely awaited to completion before the runtime shut down"
        );
    }

    /// MAN-64 (PR #76 review round 7): `manta_source_health` had no failure
    /// transition at all -- a fatal source read tore the daemon down with
    /// the gauge still reading 1. The metrics listener is spawned bare (it
    /// takes neither `ClientTasks` nor the shutdown watch), so writing 0
    /// here, BEFORE the drain signal, is observable only for whatever
    /// fraction of the `SHUTDOWN_DRAIN_DEADLINE` window
    /// `shutdown_runtime_after_drain`'s `await_all` happens to keep the
    /// runtime alive -- the full window when a telnet/JSON/WS client is
    /// genuinely still draining, but effectively zero time in the ordinary
    /// scrape-only deployment, where `await_all` returns almost immediately.
    /// See `record_terminal_source_health`'s own doc comment for the full
    /// corrected claim.
    #[test]
    fn fatal_listen_error_flips_source_health_to_zero() {
        let metrics = manta_server::metrics::Metrics::new();
        metrics.set_source_health("hpsdr", true);
        record_terminal_source_health(&metrics, "hpsdr", &Err(anyhow!("source read failed")));
        assert!(
            metrics
                .render_prometheus_text()
                .contains("manta_source_health{source=\"hpsdr\"} 0"),
            "a fatal listen error must be reported as unhealthy"
        );
    }

    /// A clean end of stream is NOT a source failure: file replay reaching
    /// EOF and a Ctrl-C stop both return `Ok(())`, and reporting those as
    /// unhealthy would make the gauge lie in the opposite direction.
    #[test]
    fn clean_listen_completion_leaves_source_health_untouched() {
        let metrics = manta_server::metrics::Metrics::new();
        metrics.set_source_health("file", true);
        record_terminal_source_health(&metrics, "file", &Ok(()));
        assert!(metrics
            .render_prometheus_text()
            .contains("manta_source_health{source=\"file\"} 1"));
    }

    #[test]
    fn epoch_for_replay_path_rejects_a_pre_unix_epoch_mtime() {
        // Regression (round-8 review): a Unix filesystem can represent an
        // mtime before 1970 (rare, but real). Left unvalidated, that
        // SystemTime flows all the way to SpotBus::unix_ts_for, whose
        // `.duration_since(UNIX_EPOCH).expect(...)` panics on the very
        // first spot delivered to any client -- a crash discovered at
        // spot-delivery time instead of a clean error at startup.
        let f = write_temp_file(b"pre-epoch mtime fixture");
        let pre_epoch = std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
        // Windows requires FILE_WRITE_ATTRIBUTES on the handle to call
        // SetFileTime; a read-only `File::open` handle (as used previously)
        // fails with PermissionDenied there regardless of the target time
        // value -- open with write access instead.
        std::fs::OpenOptions::new()
            .write(true)
            .open(f.path())
            .unwrap()
            .set_modified(pre_epoch)
            .expect("this platform must support setting mtime for the test to be meaningful");

        let result = epoch_for_replay_path(f.path());
        assert!(
            result.is_err(),
            "a pre-1970 mtime must be rejected at startup, not deferred to a later panic"
        );
    }

    #[test]
    fn resolve_epoch_ignores_replay_epoch_for_a_live_session() {
        // Regression (round-8 review): --replay-epoch's own doc comment
        // says "ignored for a live source," but the old match applied it
        // unconditionally on `Some(secs)` regardless of `replay_path`. A
        // live session given the flag would then publish spots with a
        // fabricated historical timestamp AND derive its session_nonce
        // from that same fixed value instead of a fresh nanosecond-
        // precision now -- breaking the "two live sessions started within
        // the same wall-clock second don't collide" guarantee entirely,
        // since every live start with the same flag value would collide.
        let before = std::time::SystemTime::now();
        let epoch = resolve_epoch(None, Some(1_751_635_200)).unwrap();
        let after = std::time::SystemTime::now();
        assert!(
            epoch >= before && epoch <= after,
            "a live session (no replay_path) must ignore --replay-epoch and use now(), got {epoch:?}"
        );
    }

    #[test]
    fn resolve_epoch_prefers_an_explicit_replay_epoch_over_file_mtime() {
        // --replay-epoch must win even when a replay path is also given --
        // it's the escape hatch for exactly the case where mtime isn't
        // trustworthy (a copy/download that didn't preserve it).
        let f = write_temp_file(b"resolve_epoch fixture");
        let explicit =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_751_635_200);
        assert_eq!(
            resolve_epoch(Some(f.path()), Some(1_751_635_200)).unwrap(),
            explicit
        );
    }

    #[test]
    fn resolve_epoch_falls_back_to_file_mtime_for_replay_without_an_explicit_epoch() {
        let f = write_temp_file(b"resolve_epoch fixture");
        assert_eq!(
            resolve_epoch(Some(f.path()), None).unwrap(),
            epoch_for_replay_path(f.path()).unwrap()
        );
    }

    #[test]
    fn resolve_epoch_is_now_for_a_live_session_with_no_replay_path() {
        let before = std::time::SystemTime::now();
        let epoch = resolve_epoch(None, None).unwrap();
        let after = std::time::SystemTime::now();
        assert!(epoch >= before && epoch <= after);
    }

    #[test]
    fn parse_replay_epoch_accepts_a_unix_seconds_value() {
        assert_eq!(parse_replay_epoch("1751635200").unwrap(), 1_751_635_200);
    }

    #[test]
    fn parse_replay_epoch_rejects_negative_and_non_numeric_values() {
        assert!(parse_replay_epoch("-1").is_err());
        assert!(parse_replay_epoch("not-a-number").is_err());
        assert!(parse_replay_epoch("2026-07-04T12:00:00Z").is_err());
    }

    #[test]
    fn parse_replay_epoch_rejects_unrealistic_far_future_values() {
        // Regression (round-9 review): an unbounded upper end let
        // i64::MAX (or anything close to it) through, which later
        // overflows SystemTime arithmetic in SpotBus::unix_ts_for
        // (`epoch + elapsed`) and panics on the very first spot delivered
        // to any client -- reject it here, at CLI-parse time, with a
        // clear error instead.
        assert!(parse_replay_epoch(&i64::MAX.to_string()).is_err());
        // A plausible near-future value must still be accepted.
        assert!(parse_replay_epoch("2000000000").is_ok());
    }

    #[test]
    fn epoch_for_replay_path_is_a_real_deterministic_timestamp() {
        // Regression (round-6 review): the epoch fed into SpotBus (and
        // from there into every JSON `timestamp`/RBN Zulu field) must be
        // BOTH a genuine wall-clock instant (not a content-hash reinterpreted
        // as nanoseconds, which produced dates spanning 1970-2554) AND
        // stable across reruns of the same replay file (a fresh
        // SystemTime::now() every run broke reproducible replay output,
        // the specific regression this round's finding flagged). A file's
        // own mtime satisfies both: it's a real filesystem fact, and it
        // doesn't change between two reads of the same untouched file.
        let f = write_temp_file(b"replay epoch fixture");
        let a = epoch_for_replay_path(f.path()).unwrap();
        let b = epoch_for_replay_path(f.path()).unwrap();
        assert_eq!(a, b, "must be stable across reruns of the same file");

        let now = std::time::SystemTime::now();
        let drift = now
            .duration_since(a)
            .or_else(|_| a.duration_since(now))
            .unwrap();
        assert!(
            drift < std::time::Duration::from_secs(60),
            "must be a genuine near-present timestamp, not a fabricated far date; drift was {drift:?}"
        );
    }

    #[test]
    fn session_nonce_for_replay_path_matches_the_published_fnv_1a_algorithm() {
        // Regression (round-12 review): std::collections::hash_map::
        // DefaultHasher's algorithm is explicitly documented as
        // UNSPECIFIED across Rust releases, so the same replay file could
        // hash differently across builds/toolchains -- and this value
        // feeds every JSON spot `id`. FNV-1a-64 is a small, independently
        // published, versioned algorithm with no dependency on any std or
        // compiler internals: `hash = (hash XOR byte) * FNV_PRIME`,
        // starting from the published offset basis. Recomputed here from
        // the same published constants via a separate expression (not
        // just self-consistency) to pin the implementation against the
        // actual formula, catching e.g. an accidentally swapped
        // XOR/multiply order or wrong constant.
        const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let expected_empty = OFFSET_BASIS as u128;
        let expected_a = (OFFSET_BASIS ^ 0x61u64).wrapping_mul(PRIME) as u128;
        let expected_ab =
            (((OFFSET_BASIS ^ 0x61u64).wrapping_mul(PRIME) ^ 0x62u64).wrapping_mul(PRIME)) as u128;

        assert_eq!(
            session_nonce_for_replay_path(write_temp_file(b"").path()).unwrap(),
            expected_empty
        );
        assert_eq!(
            session_nonce_for_replay_path(write_temp_file(b"a").path()).unwrap(),
            expected_a
        );
        assert_eq!(
            session_nonce_for_replay_path(write_temp_file(b"ab").path()).unwrap(),
            expected_ab
        );
    }

    #[test]
    fn session_nonce_for_replay_path_is_deterministic_for_the_same_content() {
        let f = write_temp_file(b"same recording bytes");
        assert_eq!(
            session_nonce_for_replay_path(f.path()).unwrap(),
            session_nonce_for_replay_path(f.path()).unwrap()
        );
    }

    #[test]
    fn session_nonce_for_replay_path_is_stable_across_different_paths_for_the_same_content() {
        // The exact bug this fix exists to prevent: the same recording,
        // re-read from a different path (a rename, a different mount, a
        // different checkout) must derive the SAME replay session nonce.
        let a = write_temp_file(b"identical recording bytes");
        let b = write_temp_file(b"identical recording bytes");
        assert_eq!(
            session_nonce_for_replay_path(a.path()).unwrap(),
            session_nonce_for_replay_path(b.path()).unwrap(),
            "the same content at two different paths must derive the same nonce"
        );
    }

    #[test]
    fn session_nonce_for_replay_path_differs_across_different_recordings() {
        let a = session_nonce_for_replay_path(write_temp_file(b"contest-weekend bytes").path())
            .unwrap();
        let b = session_nonce_for_replay_path(write_temp_file(b"quiet-weeknight bytes").path())
            .unwrap();
        assert_ne!(
            a, b,
            "two different recordings must not collide on the same replay session nonce"
        );
    }

    // MAN-32/MAN-42: start_spot_server spawns one RBN uplink task per
    // configured [[rbn_uplink]] target, only for those that are enabled.

    #[test]
    fn disabled_uplink_makes_no_connection_attempt_from_the_daemon() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_port = target.local_addr().unwrap().port();

        let cfg_file = write_temp_file(
            format!(
                r#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0

                [[rbn_uplink]]
                enabled = false
                target_host = "127.0.0.1"
                target_port = {target_port}
                "#
            )
            .as_bytes(),
        );

        let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
        let (rt, _server) = start_spot_server(
            loaded.server.unwrap(),
            loaded.rbn_uplink,
            SourceInfo {
                name: "file",
                sample_rate_hz: 96_000.0,
                dial_freq_hz: 14_000_000.0,
                rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
                freq_calibration: 1.0,                 // --freq-correction-ppm 0
            },
            std::time::SystemTime::UNIX_EPOCH,
            0,
            std::sync::Arc::new(manta_spot::cty::Table::bundled()),
        )
        .unwrap();

        let accepted = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(300), async {
                loop {
                    if target.accept().is_ok() {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
        });
        assert!(
            accepted.is_err(),
            "enabled=false must never attempt a connection"
        );
    }

    #[test]
    fn enabled_uplink_connects_to_its_configured_target() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_port = target.local_addr().unwrap().port();

        let cfg_file = write_temp_file(
            format!(
                r#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0

                [[rbn_uplink]]
                enabled = true
                target_host = "127.0.0.1"
                target_port = {target_port}
                "#
            )
            .as_bytes(),
        );

        let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
        let (rt, _server) = start_spot_server(
            loaded.server.unwrap(),
            loaded.rbn_uplink,
            SourceInfo {
                name: "file",
                sample_rate_hz: 96_000.0,
                dial_freq_hz: 14_000_000.0,
                rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
                freq_calibration: 1.0,                 // --freq-correction-ppm 0
            },
            std::time::SystemTime::UNIX_EPOCH,
            0,
            std::sync::Arc::new(manta_spot::cty::Table::bundled()),
        )
        .unwrap();

        let accepted = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if target.accept().is_ok() {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
        });
        assert!(
            accepted.unwrap_or(false),
            "enabled=true must connect to the configured target"
        );
    }

    // MAN-86: the three new [server] operator-identity keys must load
    // through the real config path (`config::load` + `start_spot_server`), not
    // just through manta-server's own unit tests. Additive on a
    // deny_unknown_fields struct, so a config WITHOUT them (every other
    // test in this module) must keep loading too.
    #[test]
    fn server_config_accepts_the_operator_identity_keys() {
        let cfg_file = write_temp_file(
            r#"
            [server]
            station_callsign = "HB9H"
            bind_addr = "127.0.0.1"
            telnet_port = 0
            json_port = 0
            metrics_port = 0
            operator_name = "Art"
            operator_qth = "Switzerland"
            operator_grid = "JN46la"
            "#
            .as_bytes(),
        );

        let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore)
            .expect("a config with the operator-identity keys present must load");
        let result = start_spot_server(
            loaded.server.unwrap(),
            loaded.rbn_uplink,
            SourceInfo {
                name: "file",
                sample_rate_hz: 96_000.0,
                dial_freq_hz: 14_040_000.0,
                rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
                freq_calibration: 1.0,                 // --freq-correction-ppm 0
            },
            std::time::SystemTime::UNIX_EPOCH,
            0,
            std::sync::Arc::new(manta_spot::cty::Table::bundled()),
        );
        assert!(
            result.is_ok(),
            "a config with the operator-identity keys present must still start: {:?}",
            result.err()
        );
    }

    #[test]
    fn two_enabled_uplink_targets_each_independently_connect() {
        let target1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target1.set_nonblocking(true).unwrap();
        let target1_port = target1.local_addr().unwrap().port();

        let target2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target2.set_nonblocking(true).unwrap();
        let target2_port = target2.local_addr().unwrap().port();

        let cfg_file = write_temp_file(
            format!(
                r#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0

                [[rbn_uplink]]
                enabled = true
                target_host = "127.0.0.1"
                target_port = {target1_port}

                [[rbn_uplink]]
                enabled = true
                target_host = "127.0.0.1"
                target_port = {target2_port}
                "#
            )
            .as_bytes(),
        );

        let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
        let (rt, _server) = start_spot_server(
            loaded.server.unwrap(),
            loaded.rbn_uplink,
            SourceInfo {
                name: "file",
                sample_rate_hz: 96_000.0,
                dial_freq_hz: 14_000_000.0,
                rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
                freq_calibration: 1.0,                 // --freq-correction-ppm 0
            },
            std::time::SystemTime::UNIX_EPOCH,
            0,
            std::sync::Arc::new(manta_spot::cty::Table::bundled()),
        )
        .unwrap();

        async fn wait_for_accept(listener: &std::net::TcpListener) -> bool {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if listener.accept().is_ok() {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or(false)
        }
        let (accepted1, accepted2) = rt
            .block_on(async { tokio::join!(wait_for_accept(&target1), wait_for_accept(&target2)) });
        assert!(accepted1, "first configured target must be connected to");
        assert!(accepted2, "second configured target must be connected to");
    }

    #[test]
    fn live_source_spec_reports_reconnectable_only_for_live_kinds() {
        assert!(LiveSourceSpec::Kiwi(KiwiOpts {
            host: Some("h".into()),
            port: 8073,
            freq: Some(14_000_000.0),
            password: String::new(),
        })
        .is_reconnectable());
        assert!(LiveSourceSpec::AudioDevice(None).is_reconnectable());
        assert!(!LiveSourceSpec::File {
            path: PathBuf::from("rec.wav"),
            source_iq: false,
        }
        .is_reconnectable());
    }

    #[test]
    fn live_source_spec_name_matches_metrics_label() {
        // MAN-73: `name()` feeds `set_source_health`'s label directly --
        // a mismatch here would silently split one source's health series
        // across two metric labels.
        assert_eq!(
            LiveSourceSpec::Kiwi(KiwiOpts {
                host: Some("h".into()),
                port: 8073,
                freq: Some(14_000_000.0),
                password: String::new(),
            })
            .name(),
            "kiwi"
        );
        assert_eq!(LiveSourceSpec::AudioDevice(None).name(), "audio");
        assert_eq!(
            LiveSourceSpec::File {
                path: PathBuf::from("rec.wav"),
                source_iq: false,
            }
            .name(),
            "file"
        );
    }

    fn open_error(spec: LiveSourceSpec) -> String {
        match spec.open(None, None) {
            Ok(_) => panic!("expected {} open to fail", spec.name()),
            Err(e) => e.to_string(),
        }
    }

    /// MAN-135 renamed the frequency/rate flags to `*-hz`; `run` opens its
    /// input through `LiveSourceSpec::open`, so its "missing flag" errors
    /// must name the same flags `doctor`/`soak` (`open_source`) do.
    #[test]
    fn live_source_spec_open_errors_name_the_hz_flags() {
        assert_eq!(
            open_error(LiveSourceSpec::Kiwi(KiwiOpts {
                host: Some("h".into()),
                port: 8073,
                freq: None,
                password: String::new(),
            })),
            "--kiwi-freq-hz is required with --kiwi-host"
        );
    }

    #[cfg(feature = "hpsdr")]
    #[test]
    fn live_source_spec_open_errors_name_the_hpsdr_hz_flags() {
        let hpsdr = |freq, rate| {
            LiveSourceSpec::Hpsdr(HpsdrOpts {
                host: Some("192.168.1.100".into()),
                port: manta_input::hpsdr::CONTROL_PORT,
                freq,
                rate,
            })
        };
        assert_eq!(
            open_error(hpsdr(None, None)),
            "--hpsdr-freq-hz is required with --hpsdr-host"
        );
        assert_eq!(
            open_error(hpsdr(Some(7_030_000.0), None)),
            "--hpsdr-rate-hz is required with --hpsdr-host"
        );
    }

    struct GapProbeSource {
        gap: Option<u64>,
    }

    impl IqSource for GapProbeSource {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            14_000_000.0
        }
        fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
            Ok(0)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.gap.take()
        }
    }

    /// MAN-73 (PR #207 review): during an outage `listen()` is blocked in
    /// the reconnecting `read()` and can't republish the active-track
    /// count, so the unhealthy transition itself must zero the shared
    /// gauge, or `manta_active_tracks` exports the pre-drop tracks until
    /// samples resume.
    #[test]
    fn source_health_sink_zeroes_active_tracks_when_the_source_goes_unhealthy() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let metrics = Arc::new(manta_server::metrics::Metrics::new());
        let gauge = Arc::new(AtomicU64::new(3));
        let mut sink = source_health_sink("kiwi", Some(metrics.clone()), Some(gauge.clone()));

        sink(true);
        assert_eq!(
            gauge.load(Ordering::Relaxed),
            3,
            "a healthy report leaves the live count alone"
        );
        sink(false);
        assert_eq!(
            gauge.load(Ordering::Relaxed),
            0,
            "an unhealthy source has no active tracks"
        );
        assert!(metrics
            .render_prometheus_text()
            .contains(r#"manta_source_health{source="kiwi"} 0"#));
    }

    /// MAN-96: a reconnecting source's healthy -> unhealthy -> healthy
    /// round trip through the real sink is one `manta_source_outages_total`
    /// increment.
    #[test]
    fn source_health_sink_counts_a_source_outage() {
        use std::sync::Arc;

        let metrics = Arc::new(manta_server::metrics::Metrics::new());
        let mut sink = source_health_sink("kiwi", Some(metrics.clone()), None);
        sink(true);
        sink(false);
        sink(true);
        let text = metrics.render_prometheus_text();
        assert!(
            text.contains(r#"manta_source_outages_total{source="kiwi"} 1"#),
            "{text}"
        );
    }

    #[test]
    fn fixed_center_freq_source_forwards_take_discontinuity() {
        let mut src = FixedCenterFreqSource {
            inner: Box::new(GapProbeSource { gap: Some(48_000) }),
            freq_hz: 14_000_000.0,
        };
        assert_eq!(src.take_discontinuity(), Some(48_000));
        assert_eq!(src.take_discontinuity(), None);
    }

    // MAN-56: input-layer health counters wiring.

    /// A wrapper `IqSource` that forgets to forward `health_counters`
    /// silently swallows the inner source's counters via the trait's
    /// `None` default -- the metrics would just be absent, with nothing
    /// failing loudly. Same hazard `confirmed_live_handle` carries; both
    /// are asserted here.
    #[test]
    fn fixed_center_freq_source_forwards_both_optional_trait_signals() {
        use manta_input::InputHealthCounters;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        struct StubSource {
            counters: Arc<InputHealthCounters>,
            live: Arc<AtomicBool>,
        }
        impl IqSource for StubSource {
            fn sample_rate(&self) -> f64 {
                48_000.0
            }
            fn center_freq_hz(&self) -> f64 {
                0.0
            }
            fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
                Ok(0)
            }
            fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
                Some(self.live.clone())
            }
            fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
                Some(self.counters.clone())
            }
        }

        let counters = Arc::new(InputHealthCounters::new());
        let live = Arc::new(AtomicBool::new(false));
        let wrapped = FixedCenterFreqSource {
            inner: Box::new(StubSource {
                counters: counters.clone(),
                live: live.clone(),
            }),
            freq_hz: 14_025_000.0,
        };

        assert!(Arc::ptr_eq(&wrapped.health_counters().unwrap(), &counters));
        assert!(Arc::ptr_eq(
            &wrapped.confirmed_live_handle().unwrap(),
            &live
        ));
    }

    #[test]
    fn input_health_of_snapshots_all_three_counters_without_transposing_them() {
        // Three same-typed u64s: a transposition would be invisible to any
        // test that used equal values (MAN-56 D7).
        let c = manta_input::InputHealthCounters::new();
        c.record_dropped(7);
        c.record_gap();
        c.record_gap();
        c.record_malformed();
        let h = input_health_of(&c);
        assert_eq!(h.dropped_packets, 7);
        assert_eq!(h.gaps_detected, 2);
        assert_eq!(h.malformed_packets, 1);
    }

    /// MAN-128: `daemon_build_info` must never fail or panic (it feeds a
    /// gauge any scrape can trigger), and `version` is the crate's own
    /// Cargo-reported version, not whatever happens to be in `Cargo.lock`.
    #[test]
    fn build_info_carries_the_crate_version_and_a_non_empty_git_sha() {
        let info = daemon_build_info();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(!info.git_sha.is_empty());
        assert!(!info.features.is_empty());
    }

    #[test]
    fn build_info_reports_none_when_no_optional_feature_is_compiled_in() {
        if !cfg!(feature = "hpsdr") && !cfg!(feature = "soapy") {
            assert_eq!(daemon_build_info().features, "none");
        }
    }

    #[test]
    fn latency_histogram_of_copies_the_engines_bucket_bounds() {
        let obs = manta_engine::DecodeLatencyObserver::new();
        obs.observe(std::time::Duration::from_millis(1));
        let h = latency_histogram_of(&obs.snapshot());
        assert_eq!(
            h.bounds_seconds,
            manta_engine::DECODE_LATENCY_BUCKETS_SECONDS
        );
        assert_eq!(h.bucket_counts.iter().sum::<u64>(), 1);
    }

    #[test]
    fn a_source_without_counters_publishes_no_input_health_series() {
        struct StubSourceNoCounters;
        impl IqSource for StubSourceNoCounters {
            fn sample_rate(&self) -> f64 {
                48_000.0
            }
            fn center_freq_hz(&self) -> f64 {
                0.0
            }
            fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
                Ok(0)
            }
        }

        let m = manta_server::metrics::Metrics::new();
        // Mirrors the wiring above: `None` means we never call
        // set_input_health.
        let src: Box<dyn IqSource> = Box::new(StubSourceNoCounters);
        if let Some(c) = src.health_counters() {
            m.set_input_health("file", input_health_of(&c));
        }
        assert!(!m
            .render_prometheus_text()
            .contains("manta_input_malformed_packets_total{"));
    }

    /// MAN-128: pins the label path for a non-HPSDR source. The daemon's
    /// `health_counters()` wiring (above `input_health_of`) is already
    /// generic over `source_name` -- this confirms a Kiwi-shaped source's
    /// counters reach `/metrics` under `source="kiwi"` with no CLI change.
    #[test]
    fn kiwi_like_source_counters_reach_metrics_under_kiwi_label() {
        let counters = manta_input::InputHealthCounters::new();
        counters.record_gap();
        counters.record_dropped(3);

        let m = manta_server::metrics::Metrics::new();
        m.set_input_health("kiwi", input_health_of(&counters));
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_input_gaps_detected_total{source="kiwi"} 1"#));
        assert!(text.contains(r#"manta_input_dropped_packets_total{source="kiwi"} 3"#));
    }

    // MAN-136 round-1 validate code-review finding 1: the increment
    // condition for `manta_spots_unresolved_geography_total` must match the
    // condition under which `SpotMessage::from_spot` emits the `UNKNOWN_*`
    // sentinels -- the RESOLVED ADIF entity number, not merely whether
    // `cty.lookup` returned an entry.

    const GEOGRAPHY_CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N;
";
    /// One `dxcc.tsv` row for the fixture above, in the vendored file's
    /// `<primary-prefix>\t<adif-number>\t<name>` shape.
    const GEOGRAPHY_DXCC_FIXTURE: &str = "K\t291\tUnited States\n";

    #[test]
    fn a_callsign_with_a_resolved_entity_number_is_not_counted_as_unresolved() {
        let cty =
            manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
        assert_eq!(cty.lookup("W1AW").and_then(|e| e.dxcc), Some(291));
        assert!(!geography_is_unresolved(&cty, "W1AW"));
    }

    #[test]
    fn an_unresolvable_callsign_is_counted_as_unresolved() {
        let cty =
            manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
        assert!(cty.lookup("QQ1AAA").is_none(), "test premise");
        assert!(geography_is_unresolved(&cty, "QQ1AAA"));
    }

    #[test]
    fn a_maritime_or_aeronautical_mobile_callsign_is_counted_as_unresolved() {
        // /MM and /AM resolve through the base prefix, so the entity-number
        // test alone reads them as resolved -- but `SpotMessage::from_spot`
        // emits UNKNOWN_CONTINENT/UNKNOWN_CQ_ZONE and null lat/lon for them,
        // so the counter must not sit at zero while those go out.
        let cty =
            manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
        assert_eq!(
            cty.lookup("W1AW/MM").and_then(|e| e.dxcc),
            Some(291),
            "test premise: the base prefix still resolves"
        );
        assert!(geography_is_unresolved(&cty, "W1AW/MM"));
        assert!(geography_is_unresolved(&cty, "W1AW/AM"));
        assert!(!geography_is_unresolved(&cty, "W1AW/P"));
    }

    #[test]
    fn a_cty_resolvable_callsign_with_no_dxcc_row_is_still_counted_as_unresolved() {
        // The cty.dat/dxcc.tsv drift state: `cty.dat` was hand-refreshed
        // (data/SOURCES.md has no refresh automation) without regenerating
        // the TSV, so geography resolves -- non-null dxLat/dxLon -- while
        // the entity number does not, and the spot goes out with
        // `dxDxcc: -1`. Counting `lookup().is_none()` missed exactly this.
        let cty = manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, "");
        let entry = cty.lookup("W1AW").expect("geography still resolves");
        assert_eq!(entry.dxcc, None, "test premise: only the number is missing");
        assert_eq!(entry.continent, "NA");
        assert!(
            geography_is_unresolved(&cty, "W1AW"),
            "a spot emitted with UNKNOWN_DXCC must be counted, even though cty.dat resolved it"
        );
    }

    // ---- MAN-261: CLI > env > file precedence (`resolve`) and coverage

    fn cli_overrides() -> CliOverrides {
        CliOverrides::none()
    }

    fn loaded_from(body: &str) -> config::Loaded {
        let f = write_temp_file(body.as_bytes());
        config::load(Some(f.path()), config::Env::Ignore).unwrap()
    }

    const KIWI_INPUT: &str =
        "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nfreq_hz = 14025000.0\n\
                              freq_correction_ppm = 2.5\ncenter_freq_hz = 14000000.0\n";

    #[test]
    fn cli_source_flag_discards_a_typed_input_layer_with_a_note() {
        let loaded = loaded_from(KIWI_INPUT);
        let cli = CliOverrides {
            source: Some(PathBuf::from("v1.wav")),
            ..cli_overrides()
        };
        let r = resolve(cli, &loaded).unwrap();
        assert!(matches!(r.spec, LiveSourceSpec::File { .. }));
        assert_eq!(r.freq_correction_ppm, 0.0);
        assert_eq!(r.dial_freq_hz, None);
        assert_eq!(r.notes.len(), 1);
        assert!(
            r.notes[0].contains("--source selects the source; ignoring [input] (type = \"kiwi\")")
        );
    }

    #[test]
    fn untyped_input_shared_keys_survive_a_cli_source() {
        let loaded = loaded_from("[input]\nfreq_correction_ppm = 2.5\n");
        let cli = CliOverrides {
            source: Some(PathBuf::from("v1.wav")),
            ..cli_overrides()
        };
        let r = resolve(cli, &loaded).unwrap();
        assert_eq!(r.freq_correction_ppm, 2.5);
        assert!(r.notes.is_empty());
    }

    #[test]
    fn config_source_is_used_when_no_cli_source_is_given() {
        let r = resolve(cli_overrides(), &loaded_from(KIWI_INPUT)).unwrap();
        let LiveSourceSpec::Kiwi(kiwi) = &r.spec else {
            panic!("expected the kiwi source from [input]");
        };
        assert_eq!(kiwi.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(kiwi.freq, Some(14_025_000.0));
        assert_eq!(r.freq_correction_ppm, 2.5);
        assert_eq!(r.dial_freq_hz, Some(14_000_000.0));
    }

    #[test]
    fn cli_ppm_beats_file_ppm() {
        let cli = CliOverrides {
            freq_correction_ppm: Some(4.0),
            ..cli_overrides()
        };
        let r = resolve(cli, &loaded_from(KIWI_INPUT)).unwrap();
        assert_eq!(r.freq_correction_ppm, 4.0);
    }

    #[test]
    fn explicit_cli_ppm_zero_beats_file_ppm() {
        let cli = CliOverrides {
            freq_correction_ppm: Some(0.0),
            ..cli_overrides()
        };
        let r = resolve(cli, &loaded_from(KIWI_INPUT)).unwrap();
        assert_eq!(r.freq_correction_ppm, 0.0);
    }

    #[test]
    fn source_iq_flag_sets_iq_on_a_config_file_source() {
        let cli = CliOverrides {
            source_iq: true,
            ..cli_overrides()
        };
        // Absolute on every OS: "/x/v1.wav" has no drive prefix, so Windows
        // treats it as relative and joins it onto the config dir.
        let wav = std::env::temp_dir().join("x").join("v1.wav");
        let r = resolve(
            cli,
            &loaded_from(&format!(
                "[input]\ntype = \"file\"\npath = '{}'\n",
                wav.display()
            )),
        )
        .unwrap();
        let LiveSourceSpec::File { path, source_iq } = &r.spec else {
            panic!("expected the file source from [input]");
        };
        assert_eq!(path, &wav);
        assert!(*source_iq);
    }

    #[test]
    fn cli_allowlist_replaces_file_allowlist() {
        let spot = config::SpotFile {
            allowlist: vec!["W1AW".into()],
            ..Default::default()
        };
        let r = resolve_spot(vec!["K1ABC".into()], None, None, None, None, &spot);
        assert_eq!(r.allowlist, vec!["K1ABC".to_string()]);
    }

    #[test]
    fn empty_cli_allowlist_keeps_file_allowlist() {
        let spot = config::SpotFile {
            allowlist: vec!["W1AW".into()],
            ..Default::default()
        };
        let r = resolve_spot(Vec::new(), None, None, None, None, &spot);
        assert_eq!(r.allowlist, vec!["W1AW".to_string()]);
    }

    #[test]
    fn cli_blocklist_beats_blocklist_path() {
        let spot = config::SpotFile {
            blocklist_path: Some(PathBuf::from("/cfg/bad.txt")),
            notch_path: Some(PathBuf::from("/cfg/n.txt")),
            ..Default::default()
        };
        let r = resolve_spot(
            Vec::new(),
            Some(PathBuf::from("cli.txt")),
            None,
            None,
            None,
            &spot,
        );
        assert_eq!(r.blocklist, Some(PathBuf::from("cli.txt")));
        assert_eq!(r.notch, Some(PathBuf::from("/cfg/n.txt")));
    }

    #[test]
    fn name_matches_today_source_names() {
        let kiwi = || KiwiOpts {
            host: Some("h".into()),
            port: 8073,
            freq: Some(7e6),
            password: String::new(),
        };
        assert_eq!(LiveSourceSpec::Kiwi(kiwi()).name(), "kiwi");
        assert_eq!(LiveSourceSpec::AudioDevice(None).name(), "audio");
        assert_eq!(
            LiveSourceSpec::File {
                path: PathBuf::from("x.wav"),
                source_iq: false
            }
            .name(),
            "file"
        );
        #[cfg(feature = "soapy")]
        assert_eq!(
            LiveSourceSpec::Soapy(SoapyOpts {
                driver: Some("d".into()),
                freq: Some(7e6),
                rate: Some(48e3),
                gain: None
            })
            .name(),
            "soapy"
        );
        #[cfg(feature = "hpsdr")]
        assert_eq!(
            LiveSourceSpec::Hpsdr(HpsdrOpts {
                host: Some("h".into()),
                port: 1024,
                freq: Some(7e6),
                rate: Some(48e3)
            })
            .name(),
            "hpsdr"
        );
    }

    #[cfg(not(feature = "soapy"))]
    #[test]
    fn soapy_input_type_needs_the_soapy_feature() {
        let loaded = loaded_from(
            "[input]\ntype = \"soapy\"\ndriver = \"d\"\nfreq_hz = 7e6\nrate_hz = 48000.0\n",
        );
        let err = resolve(cli_overrides(), &loaded).err().unwrap().to_string();
        assert!(
            err.contains("input.type = \"soapy\" needs a manta built with --features soapy"),
            "{err}"
        );
    }

    #[cfg(not(feature = "hpsdr"))]
    #[test]
    fn hpsdr_input_type_needs_the_hpsdr_feature() {
        let loaded = loaded_from(
            "[input]\ntype = \"hpsdr\"\nhost = \"h\"\nfreq_hz = 7e6\nrate_hz = 48000.0\n",
        );
        let err = resolve(cli_overrides(), &loaded).err().unwrap().to_string();
        assert!(
            err.contains("input.type = \"hpsdr\" needs a manta built with --features hpsdr"),
            "{err}"
        );
    }

    /// D10: every config-backed flag maps to the key it overrides.
    const FLAG_KEYS: &[(&str, &str)] = &[
        ("device", "input.device"),
        ("source", "input.path"),
        ("source_iq", "input.iq"),
        ("kiwi_host", "input.host"),
        ("kiwi_port", "input.port"),
        ("kiwi_freq_hz", "input.freq_hz"),
        ("kiwi_password", "input.password"),
        ("soapy_driver", "input.driver"),
        ("soapy_freq_hz", "input.freq_hz"),
        ("soapy_rate_hz", "input.rate_hz"),
        ("soapy_gain", "input.gain_db"),
        ("hpsdr_host", "input.host"),
        ("hpsdr_port", "input.port"),
        ("hpsdr_freq_hz", "input.freq_hz"),
        ("hpsdr_rate_hz", "input.rate_hz"),
        ("freq_correction_ppm", "input.freq_correction_ppm"),
        ("dial_freq_hz", "input.center_freq_hz"),
        ("capture_rate_hz", "input.capture_rate_hz"),
        ("replay_epoch", "input.replay_epoch"),
        ("allowlist", "spot.allowlist"),
        ("blocklist", "spot.blocklist_path"),
        ("notch", "spot.notch_path"),
        ("cty", "spot.cty_path"),
        ("scp", "spot.scp_path"),
        ("engine", "decode.engine"),
    ];
    /// Flags with no config key, by design.
    const CLI_ONLY: &[&str] = &[
        "json",
        "decoded_text",
        "duration",
        "config",
        "path",
        // `decode --center-freq-hz` (MAN-131 D8): a recording's centre, not
        // the live receiver's `input.center_freq_hz`.
        "center_freq_hz",
        "help",
        "version",
    ];

    #[test]
    fn every_config_backed_flag_maps_to_a_key() {
        use clap::CommandFactory;
        let cli = Cli::command();
        for name in ["run", "soak", "doctor", "decode", "check"] {
            let sub = cli.find_subcommand(name).unwrap();
            for arg in sub.get_arguments() {
                let id = arg.get_id().as_str();
                assert!(
                    FLAG_KEYS.iter().any(|(flag, _)| *flag == id) || CLI_ONLY.contains(&id),
                    "`manta {name} --{id}` has no config key: add it to FLAG_KEYS (and the \
                     loader) or to CLI_ONLY"
                );
            }
        }
    }

    #[test]
    fn every_flag_key_is_accepted_by_the_loader() {
        const TYPED: &[(&str, &str)] = &[
            ("audio", "type = \"audio\"\ndevice = \"d\"\n"),
            ("file", "type = \"file\"\npath = \"x.wav\"\niq = true\n"),
            (
                "kiwi",
                "type = \"kiwi\"\nhost = \"h\"\nport = 8073\nfreq_hz = 7e6\npassword = \"\"\n",
            ),
            (
                "soapy",
                "type = \"soapy\"\ndriver = \"d\"\nfreq_hz = 7e6\nrate_hz = 48000.0\ngain_db = 10.0\n",
            ),
            (
                "hpsdr",
                "type = \"hpsdr\"\nhost = \"h\"\nport = 1024\nfreq_hz = 7e6\nrate_hz = 48000.0\n",
            ),
        ];
        const SHARED: &[(&str, &str)] = &[
            (
                "input.freq_correction_ppm",
                "[input]\nfreq_correction_ppm = 1.0\n",
            ),
            ("input.center_freq_hz", "[input]\ncenter_freq_hz = 7e6\n"),
            (
                "input.capture_rate_hz",
                "[input]\ncapture_rate_hz = 6000.0\n",
            ),
            ("input.replay_epoch", "[input]\nreplay_epoch = 0\n"),
            ("spot.allowlist", "[spot]\nallowlist = [\"W1AW\"]\n"),
            (
                "spot.blocklist_path",
                "[spot]\nblocklist_path = \"b.txt\"\n",
            ),
            ("spot.notch_path", "[spot]\nnotch_path = \"n.txt\"\n"),
            ("spot.cty_path", "[spot]\ncty_path = 'cty.dat'\n"),
            ("spot.scp_path", "[spot]\nscp_path = 'MASTER.SCP'\n"),
            ("decode.engine", "[decode]\nengine = \"legacy\"\n"),
        ];
        for (_, key) in FLAG_KEYS {
            let body = match SHARED.iter().find(|(k, _)| k == key) {
                Some((_, body)) => body.to_string(),
                None => {
                    let field = key.strip_prefix("input.").unwrap();
                    let (_, table) = TYPED
                        .iter()
                        .find(|(_, t)| t.lines().any(|l| l.starts_with(&format!("{field} ="))))
                        .unwrap_or_else(|| panic!("no [input] type takes {key}"));
                    format!("[input]\n{table}")
                }
            };
            let f = write_temp_file(body.as_bytes());
            if let Err(e) = config::load(Some(f.path()), config::Env::Ignore) {
                panic!("{key}: {body:?} was rejected: {e:#}");
            }
        }
    }

    // MAN-86 review: the two ways the source-to-SETT wiring can advertise
    // coverage manta cannot hear. Both go through `station_profile`, the
    // exact code path `start_spot_server` uses, rather than calling
    // `segments_for_passband` directly.

    fn test_server_config(callsign: &str) -> manta_server::config::ServerConfig {
        let cfg_file = write_temp_file(
            format!(
                r#"
                [server]
                station_callsign = "{callsign}"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0
                "#
            )
            .as_bytes(),
        );
        let text = std::fs::read_to_string(cfg_file.path()).unwrap();
        let file: manta_server::config::DaemonConfigFile = toml::from_str(&text).unwrap();
        file.server
    }

    #[test]
    fn sett_advertises_a_rig_audio_source_only_above_its_dial_frequency() {
        // `--source` WAV / an audio device with --dial-freq-hz: analytic
        // audio carries spectrum ONLY above the dial, over the rig's ~3 kHz
        // AF passband -- not the +/-24 kHz its 48 kS/s stream could hold.
        let cfg = test_server_config("W3XYZ");
        let profile = station_profile(
            &cfg,
            14_027_000.0,
            (
                manta_input::AUDIO_PASSBAND_LO_HZ,
                manta_input::AUDIO_PASSBAND_HI_HZ,
            ),
            1.0,
        );
        assert_eq!(
            profile.sett.to_string(),
            "SETT: vlNormal 14027.3-14030.0",
            "audio coverage must be dial+0.3..dial+3.0 kHz"
        );
    }

    #[test]
    fn sett_advertises_the_frequency_corrected_passband_not_the_raw_one() {
        // --freq-correction-ppm moves every emitted spot; the advertised
        // bounds have to move with them.
        let cfg = test_server_config("W3XYZ");
        let factor = manta_spot::calibration_factor_from_ppm(1_000.0).unwrap();
        let corrected = station_profile(&cfg, 14_040_000.0, (-5_000.0, 5_000.0), factor);
        let raw = station_profile(&cfg, 14_040_000.0, (-5_000.0, 5_000.0), 1.0);
        assert_eq!(raw.sett.to_string(), "SETT: vlNormal 14035.0-14045.0");
        assert_eq!(corrected.sett.to_string(), "SETT: vlNormal 14049.0-14059.1");
    }

    // MAN-89 (PR #131 review, rounds 6 and 7): `station_geography_unresolved`
    // is precomputed from the operator's configured `station_callsign`, which
    // may carry an RBN `-N` per-band SSID. It must be classified through the
    // SAME string `SpotMessage::from_spot` resolves -- the SSID-stripped one
    // -- or a mobile node's spots go out with the de-side sentinels while
    // `manta_spots_unresolved_geography_total` stays at zero. These drive the
    // production entry point directly rather than stripping in the test, so
    // the strip cannot silently move back out to the call sites.

    #[test]
    fn an_ssid_bearing_mobile_station_callsign_is_counted_as_unresolved() {
        let cty =
            manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
        for call in ["K5ARH/MM-1", "K5ARH/AM-1", "K5ARH/MM-99"] {
            assert!(
                !geography_is_unresolved(&cty, call),
                "test premise: unstripped, {call} reads as resolved -- this is the bug"
            );
            assert!(
                station_geography_unresolved(&cty, call),
                "{call} carries the de-side sentinels and must be counted"
            );
        }
    }

    #[test]
    fn an_ssid_bearing_ordinary_station_callsign_is_not_counted_as_unresolved() {
        // The other direction: stripping must not turn a perfectly resolvable
        // node identity into a counted one.
        let cty =
            manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
        assert!(!station_geography_unresolved(&cty, "W1AW-1"));
        assert!(!station_geography_unresolved(&cty, "W1AW/P-2"));
        // An SSID-free identity is classified exactly as before.
        assert!(!station_geography_unresolved(&cty, "W1AW"));
        assert!(station_geography_unresolved(&cty, "W1AW/MM"));
    }

    // MAN-44: uplink health at a glance -- `manta status` + `GET /status`.

    #[test]
    fn every_configured_uplink_target_is_registered_even_when_disabled() {
        // Two [[rbn_uplink]] entries, one enabled=false: the daemon's
        // Metrics must report BOTH, the disabled one as
        // UplinkHealth::Disabled -- an operator must be able to see
        // "configured but off", not an empty list.
        let cfg_file = write_temp_file(
            br#"
            [server]
            station_callsign = "W3XYZ"
            bind_addr = "127.0.0.1"
            telnet_port = 0
            json_port = 0
            metrics_port = 0

            [[rbn_uplink]]
            enabled = true
            target_host = "127.0.0.1"
            target_port = 1

            [[rbn_uplink]]
            enabled = false
            target_host = "127.0.0.1"
            target_port = 2
            "#,
        );

        let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
        let (_rt, server) = start_spot_server(
            loaded.server.unwrap(),
            loaded.rbn_uplink,
            SourceInfo {
                name: "file",
                sample_rate_hz: 96_000.0,
                dial_freq_hz: 14_000_000.0,
                rf_passband_hz: (-48_000.0, 48_000.0),
                freq_calibration: 1.0,
            },
            std::time::SystemTime::UNIX_EPOCH,
            0,
            std::sync::Arc::new(manta_spot::cty::Table::bundled()),
        )
        .unwrap();

        let snap = server.metrics.uplink_snapshot();
        assert_eq!(
            snap.len(),
            2,
            "both configured targets must be registered, including the disabled one"
        );
        assert!(snap.iter().any(|t| t.label == "127.0.0.1:1" && t.enabled));
        assert!(snap.iter().any(|t| t.label == "127.0.0.1:2" && !t.enabled));
    }

    /// MAN-44 end-to-end: a real daemon (port 0, discovered via
    /// SpotServer::metrics_addr) answers `manta status`'s own fetch path.
    #[test]
    fn status_reports_a_configured_uplink_target_from_a_live_daemon() {
        let cfg_file = write_temp_file(
            br#"
            [server]
            station_callsign = "W3XYZ"
            bind_addr = "127.0.0.1"
            telnet_port = 0
            json_port = 0
            metrics_port = 0

            [[rbn_uplink]]
            enabled = true
            target_host = "127.0.0.1"
            target_port = 1
            "#,
        );

        let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
        let (rt, server) = start_spot_server(
            loaded.server.unwrap(),
            loaded.rbn_uplink,
            SourceInfo {
                name: "file",
                sample_rate_hz: 96_000.0,
                dial_freq_hz: 14_000_000.0,
                rf_passband_hz: (-48_000.0, 48_000.0),
                freq_calibration: 1.0,
            },
            std::time::SystemTime::UNIX_EPOCH,
            0,
            std::sync::Arc::new(manta_spot::cty::Table::bundled()),
        )
        .unwrap();

        let doc = rt
            .block_on(fetch_status(
                &[server.metrics_addr],
                std::time::Duration::from_secs(5),
            ))
            .unwrap();

        assert!(
            doc.uplink.targets.iter().any(|t| t.label == "127.0.0.1:1"),
            "expected the configured target to be visible, got: {doc:?}"
        );
        assert_eq!(
            status_exit_code(&doc),
            1,
            "a never-yet-connected enabled target must not read as healthy"
        );

        let _ = server.shutdown_tx.send(true);
        shutdown_runtime_after_drain(rt, &server.tasks);
    }

    fn cfg_with(metrics_bind_addr: &str, metrics_port: u16) -> manta_server::config::ServerConfig {
        manta_server::config::ServerConfig {
            metrics_bind_addr: metrics_bind_addr.to_string(),
            metrics_port,
            ..test_server_config("W3XYZ")
        }
    }

    /// MAN-132 made the metrics listener bind its own `metrics_bind_addr`
    /// (loopback by default) instead of `bind_addr`, so `manta status`
    /// must dial that address: a public `bind_addr` says nothing about
    /// where `/status` listens.
    #[test]
    fn status_address_follows_metrics_bind_addr_not_bind_addr() {
        let server = manta_server::config::ServerConfig {
            bind_addr: "10.0.0.5".to_string(),
            metrics_port: 17302,
            ..test_server_config("W3XYZ")
        };
        assert_eq!(server.metrics_bind_addr, "127.0.0.1", "MAN-132 default");
        assert_eq!(
            resolve_status_addr(None, Some(&server)).unwrap(),
            vec!["127.0.0.1:17302".parse().unwrap()]
        );
    }

    #[test]
    fn status_address_prefers_explicit_addr_then_config_then_default() {
        assert_eq!(
            resolve_status_addr(Some("1.2.3.4:9999"), None).unwrap(),
            vec!["1.2.3.4:9999".parse().unwrap()]
        );
        // bind_addr 0.0.0.0 in config means "listening everywhere"; the
        // CLI still has to DIAL something, and loopback is the only
        // address guaranteed to reach the local daemon.
        assert_eq!(
            resolve_status_addr(None, Some(&cfg_with("0.0.0.0", 17302))).unwrap(),
            vec!["127.0.0.1:17302".parse().unwrap()]
        );
        assert_eq!(
            resolve_status_addr(None, Some(&cfg_with("::", 17302))).unwrap(),
            vec!["[::1]:17302".parse().unwrap()]
        );
        assert_eq!(
            resolve_status_addr(None, Some(&cfg_with("10.0.0.5", 17302))).unwrap(),
            vec!["10.0.0.5:17302".parse().unwrap()]
        );
        assert_eq!(
            resolve_status_addr(None, None).unwrap(),
            vec!["127.0.0.1:7302".parse().unwrap()]
        );
    }

    /// MAN-44 remediate regression: `--addr` must accept a hostname too,
    /// the same way the `--server-config`/`bind_addr` path already does
    /// (`status_address_resolves_a_hostname_bind_addr_like_the_daemon_does`
    /// below) -- an explicit `--addr localhost:PORT` was being rejected
    /// outright by a literal `SocketAddr` parse, contradicting
    /// `docs/RUNBOOKS/uplink-health.md`'s documented cross-host invocation.
    #[test]
    fn status_address_resolves_a_hostname_passed_via_addr() {
        let addrs = resolve_status_addr(Some("localhost:17302"), None).unwrap();
        assert!(!addrs.is_empty(), "expected at least one resolved address");
        for addr in &addrs {
            assert!(
                addr.ip().is_loopback(),
                "expected localhost to resolve to a loopback address, got {addr}"
            );
            assert_eq!(addr.port(), 17302);
        }
    }

    /// MAN-44 CR-B regression: the daemon binds `bind_addr` via
    /// `TcpListener::bind((host, port))`, which resolves a hostname (not
    /// just a literal IP) through `ToSocketAddrs` -- so `bind_addr =
    /// "localhost"` is a config the daemon runs on happily. `manta status
    /// --server-config` must resolve it the same way instead of rejecting
    /// a config the daemon itself accepts.
    #[test]
    fn status_address_resolves_a_hostname_bind_addr_like_the_daemon_does() {
        let addrs = resolve_status_addr(None, Some(&cfg_with("localhost", 17302))).unwrap();
        assert!(!addrs.is_empty(), "expected at least one resolved address");
        for addr in &addrs {
            assert!(
                addr.ip().is_loopback(),
                "expected localhost to resolve to a loopback address, got {addr}"
            );
            assert_eq!(addr.port(), 17302);
        }
    }

    /// Reads the client's request through its blank line before a fake
    /// `/status` server answers it, as the real `metrics_http::read_headers`
    /// does (without that function's line-count and line-length bounds --
    /// the peer here is always this file's own `fetch_status`). A fake
    /// that closes with the request still unread makes the kernel send RST
    /// instead of FIN: macOS then fails the client's pending read with
    /// "Connection reset by peer (os error 54)" and drops the response it
    /// had not consumed yet, while Linux delivers the response first and
    /// hides the reset -- why these tests were green on ubuntu-latest and
    /// red on macos-latest.
    async fn read_request_head(socket: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if socket.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            head.push(byte[0]);
        }
    }

    /// Code-review regression (finding 1): a hostname that resolves to
    /// several addresses -- e.g. `localhost` returning `::1` before
    /// `127.0.0.1` on a dual-stack host -- must not make `manta status`
    /// give up after dialing only the FIRST candidate. The previous
    /// `resolve_status_addr`/`fetch_status_inner` kept only
    /// `to_socket_addrs().next()`, so a real daemon bound IPv4-only (the
    /// project's own default `bind_addr = "0.0.0.0"`) was reported as
    /// unreachable whenever the resolver listed an unreachable address
    /// first. This reproduces that shape directly -- a dead IPv6 loopback
    /// candidate followed by a live IPv4-only listener -- so it fails on
    /// any implementation that dials only the first address, regardless
    /// of what a given machine's real resolver happens to return for
    /// "localhost".
    #[tokio::test]
    async fn fetch_status_falls_back_past_an_unreachable_first_address() {
        // A closed port on ::1: nothing is listening, so connecting here
        // fails immediately (connection refused) rather than hanging.
        let dead = std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), 1);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap();
        let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled).to_json();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request_head(&mut socket).await;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let _ = socket.shutdown().await;
        });

        let doc = fetch_status(&[dead, live], std::time::Duration::from_secs(5))
            .await
            .expect("must fall back to the second address after the first refuses");
        assert_eq!(doc.schema_version, 1);
    }

    /// MAN-44 code review CR-1 regression: a black-holed first candidate
    /// (SYNs silently dropped, not refused) must not consume the WHOLE
    /// overall timeout budget before a later, live candidate is ever
    /// tried -- same shape and same reasoning as
    /// `uplink::connect_first_reachable_bounded_stops_at_the_overall_deadline`:
    /// a real SYN black hole depends on undocumented host/network behavior
    /// (a host with no route to a given block gets an immediate
    /// `NetworkUnreachable` instead of a hang), so this fakes an
    /// unconditionally hanging first attempt under paused tokio time
    /// instead of dialing a real address. `connect_any`'s previous bare
    /// `TcpStream::connect` with no per-candidate bound would have let
    /// address 1 alone eat the entire `overall_timeout` here, never
    /// reaching address 2.
    #[tokio::test(start_paused = true)]
    async fn connect_any_bounded_falls_through_a_stalled_first_candidate() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let overall_timeout = std::time::Duration::from_secs(10);
        let addrs: Vec<std::net::SocketAddr> = vec![
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
        ]; // never actually dialed -- `connect` below is faked

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_connect = attempts.clone();
        let connect = move |addr: std::net::SocketAddr| {
            let attempts = attempts_for_connect.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                if addr.port() == 1 {
                    std::future::pending::<std::io::Result<()>>().await
                } else {
                    Ok(())
                }
            }
        };

        let started = tokio::time::Instant::now();
        let (_stream, addr) = connect_any_bounded(&addrs, overall_timeout, connect)
            .await
            .expect("must fall through to the live second candidate");
        assert_eq!(addr.port(), 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            started.elapsed() < overall_timeout,
            "the stalled first candidate must not consume the whole overall budget: elapsed {:?}",
            started.elapsed()
        );
    }

    /// Codex review, PR #95: a candidate that fails fast must hand its
    /// unused slice of the budget to the candidates after it. Splitting
    /// `overall_timeout` evenly up front gave the live fourth address here
    /// only 5s / 4 = 1.25s, so a daemon that takes 3s to accept timed out
    /// (exit 2, "could not reach") with nearly the whole deadline unspent.
    #[tokio::test(start_paused = true)]
    async fn connect_any_bounded_carries_unused_time_forward_to_later_candidates() {
        let overall_timeout = std::time::Duration::from_secs(5);
        let addrs: Vec<std::net::SocketAddr> = (1..=4)
            .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port)))
            .collect(); // never actually dialed -- `connect` below is faked
        let connect = |addr: std::net::SocketAddr| async move {
            if addr.port() < 4 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "refused",
                ))
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                Ok(())
            }
        };

        let (_stream, addr) = connect_any_bounded(&addrs, overall_timeout, connect)
            .await
            .expect(
                "three instant refusals must leave the fourth candidate the rest of the deadline",
            );
        assert_eq!(addr.port(), 4);
    }

    /// Same terminal-injection concern as `status::render_human`'s label
    /// escaping (Codex review, PR #95), on the failure path: a spoofed
    /// endpoint's status line is quoted in the error `manta status` prints
    /// to stderr.
    #[tokio::test]
    async fn fetch_status_escapes_control_characters_in_a_peer_status_line() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request_head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 500 \x1b]0;pwned\x07Oops\r\n\r\n")
                .await
                .unwrap();
            let _ = socket.shutdown().await;
        });

        let err = fetch_status(&[addr], std::time::Duration::from_secs(5))
            .await
            .expect_err("a 500 status line must be an error");
        let message = status_failure_message(&err);
        assert!(
            !message.chars().any(char::is_control),
            "no peer-supplied control character may reach the terminal: {message:?}"
        );
        assert!(message.contains(r"\u{1b}]0;pwned\u{7}Oops"));
    }

    /// serde_json's data errors quote the offending value ("unknown variant
    /// `...`"), and that value comes from the peer -- same escaping as the
    /// status line above (Codex review, PR #95).
    #[test]
    fn parse_status_doc_escapes_control_characters_quoted_in_a_serde_error() {
        let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled)
            .to_json()
            .replace(r#""health": "disabled""#, r#""health": "\u001b[2J\npwned""#);
        let err = parse_status_doc(&body).expect_err("an unknown health variant must not parse");
        let message = status_failure_message(&err);
        assert!(
            message.contains("not a valid status document"),
            "{message:?}"
        );
        assert!(
            !message.chars().any(char::is_control),
            "no peer-supplied control character may reach the terminal: {message:?}"
        );
        assert!(message.contains(r"\u{1b}[2J\npwned"), "{message:?}");
    }

    /// The escaping above is applied where peer text enters the error, not
    /// to the whole chain at the stderr sink: a local `--config` TOML
    /// error's multi-line source snippet must still print as lines.
    #[test]
    fn status_config_parse_errors_keep_their_multi_line_snippet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "[server]\nbind_addr = \n").unwrap();
        let err = run_status(Some(&path), None, 5).expect_err("a broken config must not parse");
        let message = status_failure_message(&err);
        assert!(message.contains("TOML parse error"), "{message:?}");
        assert!(
            message.contains('\n') && !message.contains(r"\n"),
            "the TOML snippet must keep its real line breaks: {message:?}"
        );
    }

    fn doc_with(
        health: manta_server::metrics::OverallUplinkHealth,
    ) -> manta_server::status_doc::StatusDoc {
        manta_server::status_doc::StatusDoc {
            schema_version: 1,
            version: "test".to_string(),
            uptime_seconds: 0,
            spots_total: 0,
            telnet_clients: 0,
            json_clients: 0,
            ws_clients: 0,
            active_tracks: None,
            uplink: manta_server::status_doc::UplinkStatus {
                health,
                connected_targets: 0,
                enabled_targets: 0,
                sent_total: 0,
                suppressed_total: 0,
                reconnects_total: 0,
                reconnect_window_seconds: 300,
                flapping_threshold: 3,
                targets: vec![],
            },
        }
    }

    #[test]
    fn status_exit_code_is_zero_when_healthy_one_when_degraded() {
        use manta_server::metrics::OverallUplinkHealth;
        assert_eq!(status_exit_code(&doc_with(OverallUplinkHealth::Ok)), 0);
        assert_eq!(
            status_exit_code(&doc_with(OverallUplinkHealth::Disabled)),
            0
        );
        assert_eq!(
            status_exit_code(&doc_with(OverallUplinkHealth::Degraded)),
            1
        );
        assert_eq!(status_exit_code(&doc_with(OverallUplinkHealth::Down)), 1);
    }

    #[tokio::test]
    async fn fetch_status_parses_a_chunk_split_http_response() {
        // A body assembled from one read() would pass trivially and hide
        // a real framing bug -- the status line, headers, and body are
        // deliberately written in three separate writes here.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled).to_json();

        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request_head(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            socket
                .write_all(
                    format!(
                        "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            socket.write_all(body.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
        });

        let doc = fetch_status(&[addr], std::time::Duration::from_secs(5))
            .await
            .expect("must parse a response split across several writes");
        assert_eq!(doc.schema_version, 1);
    }

    #[tokio::test]
    async fn fetch_status_errors_cleanly_on_a_404_and_on_a_non_json_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request_head(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let _ = socket.shutdown().await;
        });
        let err = fetch_status(&[addr], std::time::Duration::from_secs(5))
            .await
            .expect_err("a 404 must be a clean error, not a panic");
        assert!(format!("{err:#}").contains("404"));

        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener2.accept().await.unwrap();
            read_request_head(&mut socket).await;
            let body = "not json";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let _ = socket.shutdown().await;
        });
        let err = fetch_status(&[addr2], std::time::Duration::from_secs(5))
            .await
            .expect_err("a non-JSON body must be a clean error, not a panic");
        assert!(format!("{err:#}").contains("status document"));
    }

    #[tokio::test]
    async fn fetch_status_times_out_instead_of_hanging_on_a_silent_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_socket, _peer) = listener.accept().await.unwrap();
            // Accepts, then never writes anything -- the socket stays open.
            std::future::pending::<()>().await
        });

        let started = std::time::Instant::now();
        let err = fetch_status(&[addr], std::time::Duration::from_millis(200))
            .await
            .expect_err("a silent server must time out, not hang forever");
        assert!(err.to_string().contains("timed out"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must not have hung past the configured timeout"
        );
    }
}
