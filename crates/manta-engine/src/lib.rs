//! M0 pipeline: WAV -> frequency estimate -> single channel -> decoder.
//! Grows into the PFB/track-manager engine at M2 (ARCHITECTURE §4, §10).

pub mod check;
pub mod config_file;
pub mod listen;
pub use listen::{listen, listen_with_observers, ListenObservers};
pub mod latency;
pub use latency::{DecodeLatencyObserver, DecodeLatencySnapshot, DECODE_LATENCY_BUCKETS_SECONDS};
pub mod calibrate;
pub use calibrate::{calibrate, CalibrateOptions, CalibrationReport};
pub mod doctor;
pub use doctor::{doctor, DoctorReport, Verdict, MAX_DURATION, MIN_DURATION};
pub mod soak;
pub use manta_spot::{Blocklist, NotchList, Spot, SpotType};
pub use soak::{soak, soak_passed, SoakReport};
pub mod soak_metrics;
pub use soak_metrics::{
    soak_metrics_passed, soak_with_metrics, SoakCloseCounts, SoakMetricsReport, SoakMetricsSample,
};

mod track;
pub use track::DetectorConfig;

use anyhow::{bail, Context, Result};
use manta_decode::decoder::{events_to_text, DecodeConfig};
use manta_decode::events::DecoderEvent;
use manta_input::{read_all, IqSource, WavIqSource};
use num_complex::Complex32;
use std::path::Path;
use std::sync::Arc;

/// Applies the calibration factor to every event variant that carries a
/// `freq_hz` (`TrackMeta`, `TrackPromoted`), leaving every other variant
/// untouched. Used to calibrate the public-facing event stream
/// (`DecodeReport::events`, `listen()`'s `on_event` callback) -- both
/// consumed directly by `decode --json`/`listen --json` -- WITHOUT
/// touching the copy fed to `manta_spot::Validator::ingest`, which already
/// applies its own calibration internally; applying it here too would
/// double-correct the validator's spot output (MAN-29 review round 3).
/// `TrackPromoted` calibration added in round 5 of the doctor review:
/// otherwise a promoted track's first-reported frequency is raw while
/// every later `TrackMeta`/spot for the same track is calibrated.
pub(crate) fn calibrate_freq_events(ev: &DecoderEvent, factor: f64) -> DecoderEvent {
    match ev {
        DecoderEvent::TrackMeta {
            track_id,
            sample_ts,
            snr_2500_db,
            freq_hz,
        } => DecoderEvent::TrackMeta {
            track_id: *track_id,
            sample_ts: *sample_ts,
            snr_2500_db: *snr_2500_db,
            freq_hz: freq_hz * factor,
        },
        DecoderEvent::TrackPromoted {
            track_id,
            sample_ts,
            freq_hz,
        } => DecoderEvent::TrackPromoted {
            track_id: *track_id,
            sample_ts: *sample_ts,
            freq_hz: freq_hz * factor,
        },
        other => other.clone(),
    }
}

/// `decode_samples`'s single-track selection: the lowest track_id among
/// events that represent real decoder output. `TrackPromoted` is a
/// detector-internal diagnostic signal (added for `manta_engine::doctor()`'s
/// NoSignal check, see docs/DECISIONS/2026-09-09-doctor-track-promoted-
/// event.md) with no decoder output of its own -- excluded here so an
/// early, low-track-id candidate that was promoted and then merged/
/// evicted/reached EOF before producing any real decoder event never gets
/// selected over a later track that actually decoded something (round-5
/// review finding). `None` means every event in `events` was a
/// `TrackPromoted` (or `events` was empty, already handled by the caller
/// before this is reached).
fn primary_track_id(events: &[DecoderEvent]) -> Option<u32> {
    events
        .iter()
        .filter(|e| !matches!(e, DecoderEvent::TrackPromoted { .. }))
        .map(track::event_track_id)
        .min()
}

/// M0 pipeline tunables. SPEC §5.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Classical decoder tunables. SPEC §5.
    pub decode: DecodeConfig,
    /// Real multi-track detector tunables. SPEC §9 `[detector]` table.
    pub detector: track::DetectorConfig,
    /// Per-source frequency-calibration correction, in ppm (config key
    /// `input.freq_correction_ppm`, SPEC-decode-core.md §1.4; 0.0 = no
    /// correction). Applied to both the top-level `DecodeReport::freq_hz`
    /// and every emitted spot's `freq_hz`, so the two never disagree.
    /// Corrects a drifted source clock/LO -- distinct from
    /// `manta-spot`'s ~10 Hz decode-accuracy figure (ARCHITECTURE §6 step
    /// 5), which is decode precision (MAN-29).
    pub freq_correction_ppm: f64,
    /// Operator Watch List (ARCHITECTURE §6, MAN-28): callsigns here
    /// bypass grammar/cty validation and the repetition gate entirely in
    /// the production validator, matching CW Skimmer's Watch List.
    pub allowlist: Vec<String>,
    /// Operator's bad-callsign blocklist (MAN-31). Empty by default -- no
    /// suppression until the operator supplies one.
    pub blocklist: Blocklist,
    /// Operator's notched-frequency list (MAN-31). Empty by default -- no
    /// suppression until the operator supplies one.
    pub notch: NotchList,
    /// Operator-supplied cty.dat, parsed once. None uses the bundled table.
    pub cty: Option<Arc<manta_spot::cty::Table>>,
    /// Operator-supplied MASTER.SCP, parsed once. None uses the bundled set.
    pub scp: Option<Arc<manta_spot::scp::Set>>,
}

impl PipelineConfig {
    /// The resolved country table, shared with the daemon's geography lookup.
    pub fn cty_table(&self) -> Arc<manta_spot::cty::Table> {
        self.cty
            .clone()
            .unwrap_or_else(|| Arc::new(manta_spot::cty::Table::bundled()))
    }
    /// The resolved known-callsign set.
    pub fn scp_set(&self) -> Arc<manta_spot::scp::Set> {
        self.scp
            .clone()
            .unwrap_or_else(|| Arc::new(manta_spot::scp::Set::bundled()))
    }
    /// Builds the validator used by every production decode path.
    pub fn validator(&self, fs: f64) -> Result<manta_spot::Validator> {
        // Keep the checked ppm constructor so callers cannot supply an
        // unchecked raw calibration factor (MAN-29).
        let mut validator =
            manta_spot::Validator::from_tables(fs, self.cty_table(), Some(self.scp_set()))
                .with_freq_correction_ppm(self.freq_correction_ppm)
                .map_err(|e| anyhow::anyhow!(e))?
                .with_blocklist(self.blocklist.clone())
                .with_notch(self.notch.clone());
        for call in &self.allowlist {
            validator.allowlist(call);
        }
        Ok(validator)
    }
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            decode: DecodeConfig::default(),
            detector: track::DetectorConfig::default(),
            freq_correction_ppm: 0.0,
            allowlist: Vec::new(),
            blocklist: Blocklist::default(),
            notch: NotchList::default(),
            cty: None,
            scp: None,
        }
    }
}

/// Result of decoding one signal from an IQ scene. SPEC §5.
#[derive(Debug, serde::Serialize)]
pub struct DecodeReport {
    /// Absolute spot frequency (center + estimated offset), full precision.
    /// SPEC §1.4: full Hz precision belongs to the JSON surface.
    pub freq_hz: f64,
    /// Most recently reported speed, if any.
    pub wpm: Option<f32>,
    /// Assembled plain text (see events_to_text).
    pub text: String,
    /// The full decoder event stream, for JSON output.
    pub events: Vec<DecoderEvent>,
    /// Validated spots (`manta-spot::Validator`, ARCHITECTURE §6), run
    /// over the full multi-track event stream above.
    pub spots: Vec<Spot>,
}

/// M0 pipeline: estimate frequency, extract one channel, decode. SPEC
/// §1.3–§1.4, §3–§5.
pub fn decode_samples(
    iq: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
) -> Result<DecodeReport> {
    // Validated up front so a bad config value (NaN, infinite, or ppm so
    // negative it flips the factor negative/zero) fails fast rather than
    // after the channelizer/decoder work below (MAN-29).
    let calibration_factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;

    if iq.iter().all(|s| s.re == 0.0 && s.im == 0.0) {
        bail!("input is digital silence");
    }

    let mut ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))
        .context("channelizer")?;
    let hop = ch.hop() as u64;

    debug_assert!(
        (fs / hop as f64 - manta_decode::FO_HZ).abs() < 0.01,
        "channelizer hop rate {} Hz diverges from manta_decode::FO_HZ {}",
        fs / hop as f64,
        manta_decode::FO_HZ
    );

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let mut padded_iq = Vec::with_capacity(pad_samples + iq.len());
    padded_iq.resize(pad_samples, Complex32::new(0.0, 0.0));
    padded_iq.extend_from_slice(iq);

    let mut tm = track::TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        cfg.detector,
        cfg.decode.clone(),
    );
    // Feed the channelizer/track-manager in bounded chunks rather than one
    // `ch.process(&padded_iq)` + one `tm.process_hops(...)` call over the
    // whole file. `Channelizer::process` is chunk-size-invariant (proved by
    // `channelizer_chunking_determinism.rs`/`chunking_determinism.rs`), so
    // this doesn't change any hop's numeric output -- but `TrackManager`'s
    // SPEC §2.4 GC/silent-timer reset (`Lifecycle::note_char_decoded`) only
    // runs *after* `process_hops`'s `drain_pool()` call, once per call, for
    // whatever `CharDecoded` events that call's batch produced. Every hop in
    // between is driven by `step_hop` with `char_emitted = false` (the pool
    // hasn't run yet -- see `step_hop`'s doc), so a single whole-file
    // `process_hops` call lets `silent_count` climb unchecked for the
    // *entire* file before the one and only GC-timer reset ever happens;
    // any file longer than `gc_hops` (30 s default) force-closes its own
    // track partway through as `CloseReason::Silent`, even though the
    // signal never stopped -- discovered via a 120 s golden-vector decode
    // silently truncating to ~30 s of text. Chunking keeps `process_hops`
    // (and so `note_char_decoded`) running frequently enough that a
    // continuously-decoding track's GC timer never falsely expires.
    const CHUNK_SAMPLES: usize = 4096;
    let mut events = Vec::new();
    let mut any_hops = false;
    for chunk in padded_iq.chunks(CHUNK_SAMPLES) {
        let hops = ch.process(chunk);
        any_hops |= !hops.is_empty();
        events.extend(tm.process_hops(&hops, |m| (m.saturating_sub(pad_hops)) * hop));
    }
    if !any_hops {
        bail!("no signal found (input shorter than one filter length or empty)");
    }
    events.extend(tm.finish());

    if events.is_empty() {
        bail!("no signal found (input shorter than one filter length or empty)");
    }
    let Some(min_track_id) = primary_track_id(&events) else {
        bail!("no signal found (only detector promotion events, no decoder output)");
    };
    let this_track: Vec<DecoderEvent> = events
        .iter()
        .filter(|e| track::event_track_id(e) == min_track_id)
        .cloned()
        .collect();
    let freq_hz = this_track
        .iter()
        .rev()
        .find_map(|e| match e {
            DecoderEvent::TrackMeta { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .unwrap_or(center_freq_hz)
        * calibration_factor;
    let wpm = this_track.iter().rev().find_map(|e| match e {
        DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
        _ => None,
    });
    let text = events_to_text(&this_track);
    let mut validator = cfg.validator(fs)?;
    let mut spots = Vec::new();
    for ev in &events {
        spots.extend(validator.ingest(ev));
    }
    // Mutated in place, after the validator above has already ingested the
    // raw values (validator.ingest() applied its own correction to its
    // internal spot output, so this must not run before that loop) --
    // collecting into a second `Vec` here would clone every event while
    // Rust keeps the original alive until this function returns, doubling
    // peak memory on a long/dense offline decode for no reason (MAN-29
    // review round 4).
    for ev in events.iter_mut() {
        match ev {
            DecoderEvent::TrackMeta { freq_hz, .. }
            | DecoderEvent::TrackPromoted { freq_hz, .. } => {
                *freq_hz *= calibration_factor;
            }
            _ => {}
        }
    }
    Ok(DecodeReport {
        freq_hz,
        wpm,
        text,
        events,
        spots,
    })
}

/// decode_samples, sourced from a WAV file via manta-input. ARCHITECTURE
/// §3; SPEC §3–§5.
pub fn decode_wav(path: &Path, cfg: &PipelineConfig) -> Result<DecodeReport> {
    let mut src = WavIqSource::open(path)?;
    let fs = src.sample_rate();
    let center = src.center_freq_hz();
    let iq = read_all(&mut src)?;
    decode_samples(&iq, fs, center, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The QQ9ZZZ fixture `cli.rs`'s allowlist test uses: V1, 30 s, an
    /// unallocated-prefix call.
    fn qq9zzz_spec() -> manta_testkit::vectors::VectorSpec {
        let mut spec = manta_testkit::vectors::v1();
        spec.duration_s = 30.0;
        spec.signals[0].text = "CQ CQ DE QQ9ZZZ QQ9ZZZ K".into();
        spec
    }

    fn cty_with_qq9() -> Arc<manta_spot::cty::Table> {
        Arc::new(manta_spot::cty::Table::parse(&format!(
            "{}Test DXpedition:  14:  27:  EU:  50.0:  -5.0:  0.0:  QQ9:\n    QQ9;\n",
            manta_spot::CTY_DAT
        )))
    }

    #[test]
    fn decode_samples_spots_a_call_only_an_override_cty_allocates() {
        let spec = qq9zzz_spec();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let calls = |cfg: &PipelineConfig| -> Vec<String> {
            decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, cfg)
                .unwrap()
                .spots
                .into_iter()
                .map(|s| s.callsign)
                .collect()
        };
        assert!(!calls(&PipelineConfig::default()).contains(&"QQ9ZZZ".to_string()));
        let cfg = PipelineConfig {
            cty: Some(cty_with_qq9()),
            ..Default::default()
        };
        assert!(
            calls(&cfg).contains(&"QQ9ZZZ".to_string()),
            "override must let QQ9ZZZ spot"
        );
    }

    #[test]
    fn default_pipeline_config_uses_the_built_in_tables() {
        let cfg = PipelineConfig::default();
        assert!(cfg.cty_table().is_allocated("W1AW"));
        assert!(!cfg.cty_table().is_allocated("QQ9ZZZ"));
        assert_eq!(cfg.scp_set().len(), manta_spot::scp::Set::bundled().len());
    }

    #[test]
    fn an_override_table_is_shared_not_copied() {
        let table = cty_with_qq9();
        let cfg = PipelineConfig {
            cty: Some(table.clone()),
            ..Default::default()
        };
        assert!(Arc::ptr_eq(&cfg.cty_table(), &table));
    }

    #[test]
    fn pipeline_config_debug_stays_short_with_tables_loaded() {
        let cfg = PipelineConfig {
            cty: Some(Arc::new(manta_spot::cty::Table::bundled())),
            scp: Some(Arc::new(manta_spot::scp::Set::bundled())),
            ..Default::default()
        };
        // Loaded tables add a size summary, not ~50k callsigns.
        let baseline = format!("{:?}", PipelineConfig::default()).len();
        assert!(format!("{cfg:?}").len() < baseline + 200);
    }

    /// Regression (round-5 review, P1): an early low-track-id candidate
    /// that only ever produced a `TrackPromoted` (promoted, then merged/
    /// evicted/EOF'd before any real decoder output) must never be
    /// selected over a later track that actually decoded something.
    #[test]
    fn primary_track_id_skips_a_promotion_only_track() {
        let events = vec![
            DecoderEvent::TrackPromoted {
                track_id: 1,
                sample_ts: 0,
                freq_hz: 14_000_100.0,
            },
            DecoderEvent::TrackPromoted {
                track_id: 2,
                sample_ts: 10,
                freq_hz: 14_012_340.0,
            },
            DecoderEvent::TrackMeta {
                track_id: 2,
                sample_ts: 15,
                snr_2500_db: 20.0,
                freq_hz: 14_012_340.0,
            },
            DecoderEvent::CharDecoded {
                track_id: 2,
                sample_ts: 20,
                glyph: manta_decode::tree::Glyph::Char('W'),
                confidence: 1.0,
                alternatives: Vec::new(),
            },
        ];
        assert_eq!(primary_track_id(&events), Some(2));
    }

    #[test]
    fn primary_track_id_is_none_when_only_promotions_occurred() {
        let events = vec![
            DecoderEvent::TrackPromoted {
                track_id: 1,
                sample_ts: 0,
                freq_hz: 14_000_100.0,
            },
            DecoderEvent::TrackPromoted {
                track_id: 2,
                sample_ts: 5,
                freq_hz: 14_012_340.0,
            },
        ];
        assert_eq!(primary_track_id(&events), None);
    }

    /// MAN-31: `decode_samples` is one of the two production call sites
    /// that must apply an operator-supplied suppression list -- proves the
    /// `PipelineConfig` fields actually reach the `Validator`, not just the
    /// crate-level builders in isolation (those are already covered by
    /// `manta-spot`'s own golden_v16_v17 tests).
    #[test]
    fn decode_samples_suppresses_a_blocklisted_callsign() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let cfg = PipelineConfig {
            blocklist: manta_spot::Blocklist::parse("W1AW\n"),
            ..Default::default()
        };
        let report = decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, &cfg).unwrap();
        assert!(
            report.spots.is_empty(),
            "blocklisted callsign must never be spotted, got {:?}",
            report.spots
        );
    }

    #[test]
    fn decode_samples_suppresses_a_notched_frequency() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let signal_freq_hz = spec.center_freq_hz + spec.signals[0].offset_hz;
        let cfg = PipelineConfig {
            notch: manta_spot::NotchList::parse(&format!(
                "{}-{}\n",
                signal_freq_hz - 50.0,
                signal_freq_hz + 50.0
            )),
            ..Default::default()
        };
        let report = decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, &cfg).unwrap();
        assert!(
            report.spots.is_empty(),
            "signal inside a notched range must never be spotted, got {:?}",
            report.spots
        );
    }
}
