//! Live/replayed real-audio IQ source: coppa-audio AudioSource -> Hilbert
//! transformer -> Complex32, matching IqSource. ARCHITECTURE §3, design
//! doc §2.
//!
//! M1 scope: no automatic resampling. Sources must already run at exactly
//! TARGET_RATE_HZ (48000 Hz) natively; a rate mismatch is a hard error, not
//! a resample attempt (coppa-audio's ResamplingSource is unreachable -- no
//! `rubato` dependency and no `mod resampler;` declaration upstream).
//!
//! An RF reference is optional but supported: `with_center_freq_hz` accepts
//! an operator-supplied dial frequency (MAN-34), superseding design doc
//! §2's original "audio has no RF reference" scope decision. Without it,
//! `center_freq_hz()` still reports `0.0` and downstream frequencies are
//! bare baseband offsets.
//!
//! Live devices (`from_device`) sit behind coppa's non-blocking callback
//! ring, where an empty read means "nothing captured yet", not end of
//! stream. `read` therefore waits for samples, and fails after
//! `DEVICE_STALL_TIMEOUT` with an error naming the device, the rate and
//! the likely cause, so the `IqSource` contract `0 = EOF` holds for every
//! source (MAN-131). File-backed sources keep returning 0 at end of file.

use crate::devices::quoted_name;
use crate::IqSource;
use anyhow::{anyhow, Context, Result};
use coppa_audio::AudioSource;
use manta_dsp::hilbert::HilbertTransformer;
use num_complex::Complex32;
use std::path::Path;
use std::time::{Duration, Instant};

/// Fixed target sample rate for M1 audio decode: 48000 / 93.75 = 512 (a
/// power of two), the constraint SingleChannelExtractor::new requires.
pub const TARGET_RATE_HZ: u32 = 48_000;

/// The rig-audio passband this source actually delivers, as offsets in Hz
/// ABOVE the dial frequency (`--dial-freq-hz`).
///
/// MAN-86 review: these are what `SKIMMER/SETT` advertises to Aggregator as
/// decodable coverage, and they are deliberately NOT derived from
/// `TARGET_RATE_HZ`. Two independent reasons: `read` Hilbert-transforms
/// real audio into an analytic signal, so all of its energy sits at
/// POSITIVE offsets from the dial -- the negative half of a
/// `+/- TARGET_RATE_HZ / 2` claim is on the wrong side of the dial
/// entirely; and a receiver's AF output is the ~3 kHz passband its IF
/// filter passes (ARCHITECTURE §3, "Degenerate ~3 kHz wideband mode"), not
/// the 24 kHz its sound card happens to sample.
///
/// The values are the nominal SSB/CW-wide AF passband a rig in its widest
/// setting delivers. A rig running a narrow CW filter passes less than
/// this, so the claim is a superset for that operator; making it exact
/// needs a config key for the rig's actual filter, which is out of MAN-86's
/// scope and is why these are named constants rather than literals.
pub const AUDIO_PASSBAND_LO_HZ: f64 = 300.0;
/// Upper edge of the rig-audio passband -- see `AUDIO_PASSBAND_LO_HZ`.
pub const AUDIO_PASSBAND_HI_HZ: f64 = 3_000.0;

/// How long a live device's `read` waits for a sample before failing
/// (MAN-131). Below the ~10 s network-source bound (hpsdr.rs/kiwi.rs):
/// local audio callbacks arrive every ~10 ms, and an operator whose device
/// delivers nothing should hear so quickly. A read in flight is not
/// interruptible, so this also bounds Ctrl-C latency during a stall.
pub const DEVICE_STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Sleep between polls of an empty capture ring (matches `manta check`).
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Exact-zero samples from open before the digital-silence notice fires:
/// two seconds, the length of `listen`'s startup window.
const SILENCE_NOTICE_SAMPLES: u64 = 2 * TARGET_RATE_HZ as u64;

/// Called at most once per opened device, with its quoted name. See
/// `AudioIqSource::with_silence_notice`.
pub type SilenceNotice = Box<dyn FnOnce(&str) + Send>;

/// What a live audio input most likely needs checked when it fails to open
/// or delivers nothing (MAN-131). macOS denies microphone access silently
/// -- the stream opens, then no callbacks arrive or every buffer is zero --
/// so permission is the likely cause there.
pub fn audio_input_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "on macOS this is usually microphone permission: allow the app \
         running manta in System Settings > Privacy & Security > Microphone, \
         quit and reopen that app, then confirm with `manta check`"
    } else {
        "check that the input is connected, unmuted and not held by another \
         program, then confirm with `manta check`"
    }
}

/// `--device NAME` matched no input.
fn no_matching_input(requested: &str) -> anyhow::Error {
    anyhow!(
        "no audio input matching {} (manta needs a {TARGET_RATE_HZ} Hz input) -- \
         run `manta devices` to list the inputs --device can select",
        quoted_name(requested)
    )
}

/// No `--device` and the host has no default input.
fn no_default_input() -> anyhow::Error {
    anyhow!(
        "no default audio input (manta needs a {TARGET_RATE_HZ} Hz input) -- \
         run `manta devices` to list the inputs --device can select"
    )
}

/// The device exists but its config query, stream build or start failed.
fn did_not_open(label: &str, cause: &anyhow::Error) -> anyhow::Error {
    anyhow!(
        "audio input {label} did not open at {TARGET_RATE_HZ} Hz: {cause:#} -- {}",
        audio_input_hint()
    )
}

/// The device's native rate is not `TARGET_RATE_HZ`; M1 does not resample.
fn wrong_rate(label: &str, native: u32) -> anyhow::Error {
    let where_to_set = if cfg!(target_os = "macos") {
        "the system's audio settings (Audio MIDI Setup on macOS)"
    } else {
        "the system's audio settings"
    };
    anyhow!(
        "audio input {label} runs at {native} Hz, but manta needs {TARGET_RATE_HZ} Hz \
         and does not resample -- set that input to {TARGET_RATE_HZ} Hz in \
         {where_to_set}, or choose another with --device"
    )
}

/// A live device delivered nothing for `waited`.
fn no_samples(label: &str, waited: Duration) -> anyhow::Error {
    anyhow!(
        "audio input {label} delivered no samples in {} s at {TARGET_RATE_HZ} Hz -- {}",
        waited.as_secs_f64(),
        audio_input_hint()
    )
}

/// A capture device behind a non-blocking callback ring (coppa's
/// `CpalSource`): `read` must wait for samples, because an empty ring is
/// not end of stream.
struct LiveDevice {
    /// The device's name as `manta devices` spells it (quoted).
    label: String,
    stall_timeout: Duration,
    /// Armed until it fires or a non-zero sample arrives.
    silence_notice: Option<SilenceNotice>,
    /// Exact-zero samples read since open, while the notice is armed.
    zero_samples: u64,
}

impl LiveDevice {
    /// Fire the silence notice once `SILENCE_NOTICE_SAMPLES` exact zeros
    /// have arrived since open; disarm it on the first non-zero sample.
    fn watch_for_silence(&mut self, samples: &[f32]) {
        if self.silence_notice.is_none() {
            return;
        }
        if samples.iter().any(|&s| s != 0.0) {
            self.silence_notice = None;
            return;
        }
        self.zero_samples += samples.len() as u64;
        if self.zero_samples >= SILENCE_NOTICE_SAMPLES {
            if let Some(notice) = self.silence_notice.take() {
                notice(&self.label);
            }
        }
    }
}

/// A real audio source (device or file) converted to analytic Complex32,
/// implementing IqSource. ARCHITECTURE §3 "Audio passband" input.
pub struct AudioIqSource {
    src: Box<dyn AudioSource>,
    hilbert: HilbertTransformer,
    /// Operator-supplied RF dial frequency this audio passband sits on, Hz.
    /// `0.0` = no RF reference supplied: reported frequencies are baseband
    /// offsets. See `with_center_freq_hz` (MAN-34).
    center_freq_hz: f64,
    /// `Some` only for `from_device`; file sources keep `0 = EOF`.
    live: Option<LiveDevice>,
}

impl AudioIqSource {
    /// Wrap an already-started AudioSource at TARGET_RATE_HZ.
    pub fn new(src: Box<dyn AudioSource>) -> Result<Self> {
        if src.sample_rate() != TARGET_RATE_HZ {
            return Err(anyhow!(
                "AudioIqSource requires {TARGET_RATE_HZ} Hz, got {}",
                src.sample_rate()
            ));
        }
        Ok(AudioIqSource {
            src,
            hilbert: HilbertTransformer::new(),
            center_freq_hz: 0.0,
            live: None,
        })
    }

    /// Wrap a started live device: `read` waits up to `stall_timeout` for
    /// samples instead of reporting an empty ring as end of stream.
    fn live(src: Box<dyn AudioSource>, label: String, stall_timeout: Duration) -> Result<Self> {
        let mut source = Self::new(src)?;
        source.live = Some(LiveDevice {
            label,
            stall_timeout,
            silence_notice: None,
            zero_samples: 0,
        });
        Ok(source)
    }

    /// Call `notice` once, with the device's quoted name, if the first two
    /// seconds a live device delivers are all exact zeros -- on macOS the
    /// usual sign of a denied microphone permission (MAN-131). A no-op for
    /// file-backed sources: silent WAV fixtures are routine.
    pub fn with_silence_notice(mut self, notice: SilenceNotice) -> Self {
        if let Some(live) = &mut self.live {
            live.silence_notice = Some(notice);
        }
        self
    }

    /// Attach the operator-supplied RF dial frequency the rig is tuned to
    /// (MAN-34). An audio passband carries no RF reference of its own, so
    /// without this a track's reported frequency is a bare audio-tone
    /// offset (e.g. 700 Hz) rather than an absolute RBN frequency. This is
    /// the manual equivalent of entering the dial frequency once -- manta
    /// does not poll the rig (no CAT; README non-goals).
    ///
    /// Sideband convention: `center_freq_hz()` is added to the decoded
    /// audio-tone offset as-is, so pass the suppressed-carrier/USB dial
    /// reading. On a CW-mode dial display, subtract your sidetone pitch
    /// first, or the reported frequency reads high by the pitch amount.
    ///
    /// Rejects non-finite and non-positive values, matching the CLI's
    /// `--dial-freq-hz` parser: "no reference" is expressed by not calling
    /// this, not by passing 0.0.
    pub fn with_center_freq_hz(mut self, center_freq_hz: f64) -> Result<Self> {
        if !center_freq_hz.is_finite() || center_freq_hz <= 0.0 {
            return Err(anyhow!(
                "center frequency must be a finite, positive number of Hz, \
                 got {center_freq_hz}"
            ));
        }
        self.center_freq_hz = center_freq_hz;
        Ok(self)
    }

    /// Open the named input device (default device if `None`). Requires the
    /// device's native rate to be exactly TARGET_RATE_HZ (48000) -- M1 does
    /// not resample, and the rate is checked before the stream starts.
    ///
    /// Every failure names the device (as `manta devices` spells it, or
    /// what was requested), states the 48000 Hz requirement and says what
    /// to check next (MAN-131). The returned source is live: `read` waits
    /// for samples and fails after `DEVICE_STALL_TIMEOUT` of none.
    pub fn from_device(name: Option<&str>) -> Result<Self> {
        use cpal::traits::{DeviceTrait, HostTrait};
        let device = match name {
            Some(n) => {
                coppa_audio::find_input_device_by_name(n).ok_or_else(|| no_matching_input(n))?
            }
            None => cpal::default_host()
                .default_input_device()
                .ok_or_else(no_default_input)?,
        };
        let label = match device.description() {
            Ok(description) => quoted_name(description.name()),
            Err(_) => match name {
                Some(n) => format!("matching {}", quoted_name(n)),
                None => "(system default)".to_string(),
            },
        };
        let native_rate = device
            .default_input_config()
            .context("query its default input config")
            .map_err(|e| did_not_open(&label, &e))?
            .sample_rate();
        if native_rate != TARGET_RATE_HZ {
            return Err(wrong_rate(&label, native_rate));
        }
        let mut cpal_src = coppa_audio::CpalSource::from_device(device, native_rate, 8192)
            .map_err(|e| did_not_open(&label, &e))?;
        cpal_src.start().map_err(|e| did_not_open(&label, &e))?;
        AudioIqSource::live(Box::new(cpal_src), label, DEVICE_STALL_TIMEOUT)
    }

    /// Open a WAV file, replayed as an audio source (soak harness / `listen
    /// --source`). Requires the file's rate to be exactly TARGET_RATE_HZ.
    pub fn from_wav_file(path: &Path) -> Result<Self> {
        let wav_src = coppa_audio::WavSource::open(path)?;
        AudioIqSource::new(Box::new(wav_src))
    }
}

impl IqSource for AudioIqSource {
    fn sample_rate(&self) -> f64 {
        TARGET_RATE_HZ as f64
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz // 0.0 when no dial frequency was supplied -- MAN-34.
    }

    /// The rig's AF passband, ABOVE the dial frequency only -- never
    /// `+/- TARGET_RATE_HZ / 2`. See `AUDIO_PASSBAND_LO_HZ`.
    fn rf_passband_hz(&self) -> (f64, f64) {
        (AUDIO_PASSBAND_LO_HZ, AUDIO_PASSBAND_HI_HZ)
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let mut real = vec![0.0f32; buf.len()];
        let got = self.read_real(&mut real)?;
        if got == 0 {
            return Ok(0);
        }
        if let Some(live) = &mut self.live {
            live.watch_for_silence(&real[..got]);
        }
        let analytic = self.hilbert.process(&real[..got]);
        buf[..got].copy_from_slice(&analytic);
        Ok(got)
    }
}

impl AudioIqSource {
    /// Real samples from the underlying source. A file returns 0 at end of
    /// file; a live device polls its ring until at least one sample
    /// arrives, or fails after its stall timeout. An empty request never
    /// waits.
    fn read_real(&mut self, real: &mut [f32]) -> Result<usize> {
        let Some(live) = &self.live else {
            return self.src.read(real);
        };
        if real.is_empty() {
            return Ok(0);
        }
        let started = Instant::now();
        loop {
            let got = self.src.read(real)?;
            if got > 0 {
                return Ok(got);
            }
            if started.elapsed() >= live.stall_timeout {
                return Err(no_samples(&live.label, live.stall_timeout));
            }
            std::thread::sleep(DEVICE_POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// A fake capture ring with the semantics of coppa's `CpalSource`: each
    /// read pops the next `(count, value)` (count 0 = empty ring) and fills
    /// that many samples of `value`; an exhausted script reads as an empty
    /// ring forever, like a callback that never fires.
    struct ScriptedRing {
        script: VecDeque<(usize, f32)>,
    }

    impl AudioSource for ScriptedRing {
        fn read(&mut self, buf: &mut [f32]) -> Result<usize> {
            let (count, value) = self.script.pop_front().unwrap_or((0, 0.0));
            let n = count.min(buf.len());
            buf[..n].fill(value);
            Ok(n)
        }
        fn sample_rate(&self) -> u32 {
            TARGET_RATE_HZ
        }
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn scripted(script: &[(usize, f32)], stall: Duration) -> AudioIqSource {
        let ring = ScriptedRing {
            script: script.iter().copied().collect(),
        };
        AudioIqSource::live(Box::new(ring), "\"Test Mic\"".to_string(), stall).unwrap()
    }

    /// A notice that records each name it is called with.
    fn recording_notice() -> (SilenceNotice, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (
            Box::new(move |name: &str| sink.lock().unwrap().push(name.to_string())),
            seen,
        )
    }

    #[test]
    fn a_live_device_waits_out_an_empty_capture_ring() {
        let mut aiq = scripted(
            &[(0, 0.0), (0, 0.0), (0, 0.0), (480, 0.5)],
            Duration::from_secs(1),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        assert_eq!(aiq.read(&mut buf).unwrap(), 480);
    }

    #[test]
    fn a_live_device_that_never_delivers_names_itself_the_rate_and_the_hint() {
        let mut aiq = scripted(&[], Duration::from_millis(30));
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        let err = aiq.read(&mut buf).unwrap_err().to_string();
        assert!(
            err.contains("audio input \"Test Mic\" delivered no samples in 0.03 s at 48000 Hz -- "),
            "{err}"
        );
        assert!(err.ends_with(audio_input_hint()), "{err}");
    }

    #[test]
    fn an_empty_request_does_not_block_a_live_device() {
        let mut aiq = scripted(&[], Duration::from_secs(10));
        let started = Instant::now();
        assert_eq!(aiq.read(&mut []).unwrap(), 0);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn digital_silence_notice_fires_once_after_two_seconds_of_exact_zeros() {
        let (notice, seen) = recording_notice();
        let mut aiq =
            scripted(&[(4800, 0.0); 50], Duration::from_secs(1)).with_silence_notice(notice);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4800];
        for read in 1..=50u64 {
            assert_eq!(aiq.read(&mut buf).unwrap(), 4800);
            let fired = seen.lock().unwrap().len();
            // 20 reads x 4800 = 96 000 samples = 2 s at 48 kHz.
            let expected = usize::from(read * 4800 >= 2 * TARGET_RATE_HZ as u64);
            assert_eq!(fired, expected, "after read {read}");
        }
        assert_eq!(*seen.lock().unwrap(), ["\"Test Mic\""]);
    }

    #[test]
    fn digital_silence_notice_never_fires_after_a_nonzero_sample() {
        let (notice, seen) = recording_notice();
        let mut script = vec![(4800, 1e-4)];
        script.extend([(4800, 0.0); 50]);
        let mut aiq = scripted(&script, Duration::from_secs(1)).with_silence_notice(notice);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4800];
        for _ in 0..script.len() {
            assert_eq!(aiq.read(&mut buf).unwrap(), 4800);
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_file_backed_source_ignores_a_silence_notice() {
        let (notice, seen) = recording_notice();
        let samples = vec![0.0; 5 * TARGET_RATE_HZ as usize];
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            samples,
            TARGET_RATE_HZ,
        ));
        let mut aiq = AudioIqSource::new(src).unwrap().with_silence_notice(notice);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4800];
        while aiq.read(&mut buf).unwrap() > 0 {}
        assert!(seen.lock().unwrap().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_hint_names_the_macos_privacy_setting() {
        let hint = audio_input_hint();
        assert!(
            hint.contains("System Settings > Privacy & Security > Microphone"),
            "{hint}"
        );
        assert!(hint.contains("manta check"), "{hint}");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_hint_is_generic_off_macos() {
        let hint = audio_input_hint();
        assert!(hint.contains("manta check"), "{hint}");
        assert!(!hint.contains("Privacy"), "{hint}");
    }

    #[test]
    fn open_failure_messages_name_the_device_and_the_rate() {
        assert_eq!(
            no_matching_input("NoSuchCard").to_string(),
            "no audio input matching \"NoSuchCard\" (manta needs a 48000 Hz input) -- \
             run `manta devices` to list the inputs --device can select"
        );
        assert_eq!(
            no_default_input().to_string(),
            "no default audio input (manta needs a 48000 Hz input) -- \
             run `manta devices` to list the inputs --device can select"
        );
        let rate = wrong_rate("\"Mic\"", 44_100).to_string();
        assert!(
            rate.starts_with(
                "audio input \"Mic\" runs at 44100 Hz, but manta needs 48000 Hz \
                 and does not resample -- set that input to 48000 Hz in "
            ),
            "{rate}"
        );
        assert!(
            rate.ends_with(", or choose another with --device"),
            "{rate}"
        );
        assert_eq!(
            rate.contains("Audio MIDI Setup"),
            cfg!(target_os = "macos"),
            "{rate}"
        );
        assert_eq!(
            did_not_open("\"Mic\"", &anyhow!("boom")).to_string(),
            format!(
                "audio input \"Mic\" did not open at 48000 Hz: boom -- {}",
                audio_input_hint()
            )
        );
        // A context chain is flattened onto the one line.
        let chained = anyhow!("not available").context("query its default input config");
        assert!(did_not_open("\"Mic\"", &chained).to_string().contains(
            "did not open at 48000 Hz: query its default input config: not available -- "
        ));
    }

    #[test]
    fn converts_real_samples_to_analytic_iq() {
        let fs = TARGET_RATE_HZ;
        let f = 1_000.0;
        let samples: Vec<f32> = (0..4000)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / fs as f64).cos() as f32)
            .collect();
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(samples, fs));
        let mut aiq = AudioIqSource::new(src).unwrap();
        assert_eq!(aiq.sample_rate(), fs as f64);
        assert_eq!(aiq.center_freq_hz(), 0.0);
        // MAN-86 review: coverage is the rig's AF passband above the dial,
        // NOT the analytic stream's +/- 24 kHz Nyquist span.
        assert_eq!(
            aiq.rf_passband_hz(),
            (AUDIO_PASSBAND_LO_HZ, AUDIO_PASSBAND_HI_HZ)
        );
        assert!(
            aiq.rf_passband_hz().0 > 0.0,
            "analytic audio carries no spectrum below the dial frequency"
        );
        assert!(aiq.rf_passband_hz().1 < aiq.sample_rate() / 2.0);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4000];
        let n = aiq.read(&mut buf).unwrap();
        assert!(n > 0);
        // Well past the Hilbert filter's transient, magnitude should be ~unit.
        assert!(
            (buf[2000].norm() - 1.0).abs() < 0.1,
            "norm={}",
            buf[2000].norm()
        );
    }

    #[test]
    fn rejects_mismatched_sample_rate() {
        let src: Box<dyn AudioSource> =
            Box::new(coppa_audio::WavSource::from_samples(vec![0.0; 10], 44_100));
        assert!(AudioIqSource::new(src).is_err());
    }

    #[test]
    fn reports_eof_as_zero_read() {
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 5],
            TARGET_RATE_HZ,
        ));
        let mut aiq = AudioIqSource::new(src).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 5];
        assert_eq!(aiq.read(&mut buf).unwrap(), 5);
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn center_freq_hz_defaults_to_zero_without_an_rf_reference() {
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 10],
            TARGET_RATE_HZ,
        ));
        assert_eq!(AudioIqSource::new(src).unwrap().center_freq_hz(), 0.0);
    }

    #[test]
    fn with_center_freq_hz_is_reported_as_the_sources_rf_reference() {
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 10],
            TARGET_RATE_HZ,
        ));
        let aiq = AudioIqSource::new(src)
            .unwrap()
            .with_center_freq_hz(14_030_000.0)
            .unwrap();
        assert_eq!(aiq.center_freq_hz(), 14_030_000.0);
    }

    #[test]
    fn with_center_freq_hz_rejects_non_finite_and_non_positive_values() {
        for bad in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -14_030_000.0,
        ] {
            let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
                vec![0.0; 10],
                TARGET_RATE_HZ,
            ));
            assert!(
                AudioIqSource::new(src)
                    .unwrap()
                    .with_center_freq_hz(bad)
                    .is_err(),
                "with_center_freq_hz({bad}) should have been rejected"
            );
        }
    }
}
