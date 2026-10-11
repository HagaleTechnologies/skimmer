# MAN-125: device listing and source diagnostics

Date: 2026-10-10

Operators need to distinguish absent samples from failed decoding. `manta devices`
identifies selectable inputs; `manta check [SOURCE]` measures received samples;
`manta doctor` then assesses the decode pipeline. These commands do not establish
that an antenna is connected or that a received signal contains decodable CW.

## Device identity

Enumeration uses coppa-audio input descriptors and feature-gated SoapySDR
`enumerate("")`. It never opens a capture stream or reads config. Audio output-only
devices are omitted; input and duplex devices retain exact names and input channel
counts, including duplicate names. Names are sorted and display-escaped. Selection
uses `--device NAME`, case-insensitive substring matching, with no numeric index.
The dependency's maximum rate mixes input/output capabilities and is not shown.
Soapy selectors preserve the complete owned argument string, including serial
identity, for `--soapy-driver ARGS`, sorted and escaped only for presentation.
HPSDR discovery is unsupported; use `--hpsdr-host` with the `hpsdr` feature.
KiwiSDR requires `--kiwi-host`.

Text and JSON share the inventory. JSON has audio/soapy status `ok` or `error`,
and Soapy may be `disabled`; HPSDR is `not_supported` and Kiwi is `explicit_host`.
A successful empty enumeration is `ok` with an empty devices array. Backend
failures retain healthy sections, report error text on stderr and exit 1;
empty lists and unavailable optional backends exit 0.

## Source selection and isolation

`check [SOURCE]` treats SOURCE as a WAV path, equivalent to `--source`; neither
accepts backend keywords. `--source-iq` selects complex stereo IQ instead of
48000 Hz mono audio. Existing hardware flags and their aliases also work, with
one explicit source selector at most. Positional SOURCE and `--source` conflict.
Explicit selectors replace typed `[input]` wholesale, including its shared fields.
Other precedence is CLI, environment, file, defaults. Config comes from `--config`
or `MANTA_CONFIG`; no implicit local config is loaded.

The shared preparation prelude validates typed TOML/environment values and
resolves the source. It does not read spot assets or require dial/station metadata
for services. The source opens exactly once through `LiveSourceSpec::open`, so
capture decimation and center overrides apply. Only a live audio device uses
transient-zero-read retries; audio files retain EOF behavior. No reconnect wrapper,
detector, decoder, validator, listener or uplink is created. `config check` keeps
its separate full configuration-validation behavior and never opens a source.

## Measurement

The default window is three seconds; `--duration` accepts finite values from
1 to 60 seconds. The report's rate and center come from the opened, composed
`IqSource`, not requested settings. Stream rate includes resampling and capture
decimation; Kiwi's 96000 Hz stream is not its native ADC rate. Unknown center is
zero in JSON and `unknown (baseband)` in text. Passband offsets are the declared
source passband intersected with delivered Nyquist.

Input power is `10 log10(mean(re² + im²))`, accumulated in f64 over delivered IQ.
Full-scale normalized power is 1.0. At least one sample with all I/Q values exactly
zero is digital silence, with null JSON power. No samples means unavailable power
and `digital_silence:false`.

The channelizer uses SPEC-decode-core §1's 93.75 Hz spacing. Each hop drives the
existing §2.1 FloorBank, whose 250-entry partial sliding window uses 0.5 dB
histogram bins. Noise floor is the raw per-channel lower quartile, not the
neighborhood-clamped effective floor used by detection. Eligible channel centers
are inside the clipped passband, lower bound inclusive and upper bound exclusive.
This excludes absent spectrum for narrow, positive audio passbands. The summary
contains minimum, median and maximum quartiles and channel count; an even median
averages the middle pair. Histogram endpoints saturate the estimate; silence
reports the actual minimum bin, approximately -139.75 dBFS.

These are per-channel dBFS values, not whole-band power, dBm, S-units or 500/2500 Hz
spot SNR. The first ten seconds use a partial window; all observations remain
estimates. No floor exists before the first complete channelizer hop. Startup
and EOF are never padded with zeros. Only bounded chunks and estimator state are
retained by the check, though the existing WAV backend still loads files eagerly.

## Completion and errors

JSON is one document on stdout with no progress output or wall-clock fields.
Text uses the same report. Notes and errors go to stderr. Source labels identify
the kind, never passwords or full configuration. Read errors, discontinuities,
invalid metadata and nonfinite sample values return errors without a report.
Opening failures say `cannot open source`; measurement failures say
`source check failed`. Malformed usage exits 2 through clap.

- A complete sample window with a measured floor exits 0, including silence.
- EOF with a floor exits 0, reporting the shorter actual count and duration.
- EOF without a floor exits 1, retaining measured power if samples arrived.
- A sampling deadline exits 1, retaining any measurements.

JSON stop reasons are `sample_window_complete`, `end_of_file` and
`sampling_deadline_reached`. Observed duration is sample count divided by stream
rate. The final read is limited to the remaining sample budget. Live audio zero
reads retry after 10 ms sleeps. The cooperative deadline is requested duration
plus five seconds after open, checked between reads. Native open/read calls can
exceed it because `IqSource` has no cancellable-read contract.

## Verification scope

Descriptor fixtures, scripted sources and generated WAVs cover the contracts
without sound hardware or public SDR hosts. Port/collector tests verify service
isolation. Physical audio/SDR smoke tests remain optional follow-up evidence;
this work does not satisfy the live soak or Pi4 budget gates.
