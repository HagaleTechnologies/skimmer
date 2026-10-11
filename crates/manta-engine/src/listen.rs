//! Streaming pipeline: live/replayed audio -> PFB channelizer ->
//! `TrackManager` (SPEC §2), run continuously until Ctrl-C or EOF, emitting
//! the merged multi-track decode event stream as it's produced. No actor/
//! ring-thread split; see design doc §4.

use crate::latency::DecodeLatencyObserver;
use crate::PipelineConfig;
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use manta_spot::{Blocklist, NotchList, Validator};
use num_complex::Complex32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One chunk read per loop iteration, in samples.
const CHUNK_SAMPLES: usize = 2048;
/// Seconds of audio buffered before the channelizer is built and streaming
/// begins. This buffer is no longer a one-shot channel-pick calibration
/// (`TrackManager` detects and tracks continuously, SPEC §2) -- it's fed
/// through `TrackManager::process_hops` like any other chunk, just like the
/// startup lead-in padding below it.
const CALIBRATION_SECONDS: f64 = 2.0;

/// Optional live handles into a running `listen()` loop, for a caller that
/// needs to observe engine-owned state the callbacks can't see.
///
/// MAN-45 (PR #63 round-9 finding): the daemon's `manta_active_tracks` gauge
/// reported a constant 0 because `listen()` owns its `TrackManager`
/// internally and exposed no live count -- `Metrics::set_active_tracks` had
/// no non-test caller. A shared atomic rather than another callback: the
/// consumer (`manta-cli`'s server runtime) polls on its own schedule and
/// must never be able to block the decode loop, which is exactly the shape
/// `IqSource::confirmed_live_handle` already uses for source liveness
/// (MAN-55).
#[derive(Clone, Default)]
pub struct ListenObservers {
    /// Updated after each processed chunk with `TrackManager::active_track_count()`.
    /// `None` (the default) skips the store entirely, so `listen()`'s
    /// existing callers -- including `soak()` and the CPU-budget bench --
    /// pay nothing.
    pub active_tracks: Option<Arc<AtomicU64>>,
    /// MAN-128: wall-clock time spent on one steady-state input chunk --
    /// channelize, track/decode, validate, `on_spot` -- measured from after
    /// `src.read()` returns until the chunk's processing ends. Excludes the
    /// source read wait and the startup calibration/padding blocks (a 2s
    /// block processed as one chunk would otherwise skew the distribution).
    /// `None` (the default) skips both the `Instant::now()` calls and the
    /// observation entirely, so `listen()`'s existing callers pay nothing.
    pub decode_latency: Option<Arc<DecodeLatencyObserver>>,
    /// MAN-78: the one inbound field -- replacement operator lists from a
    /// live config reload, taken before each decoder event reaches the
    /// running `Validator`. `None` (the default) skips the check
    /// entirely, so `listen()`'s existing callers pay nothing.
    pub operator_lists: Option<Arc<OperatorListsUpdate>>,
}

/// MAN-78: the operator lists a live reload replaces, as one unit.
#[derive(Debug, Clone, Default)]
pub struct OperatorLists {
    pub allowlist: Vec<String>,
    pub blocklist: Blocklist,
    pub notch: NotchList,
}

/// MAN-78: hands replacement lists from the reload thread to the decode
/// loop. The newest offer wins; the validator's next decoder event uses it.
#[derive(Default)]
pub struct OperatorListsUpdate {
    pending: AtomicBool,
    next: Mutex<Option<OperatorLists>>,
}

impl OperatorListsUpdate {
    /// Stores `lists` as the next update, replacing any not yet taken.
    pub fn offer(&self, lists: OperatorLists) {
        // Poison-tolerant: the slot holds plain data, so a panic in another
        // holder cannot leave it half-written.
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        *next = Some(lists);
        self.pending.store(true, Ordering::Release);
    }

    /// The newest offer not yet taken, if any. A relaxed load is the whole
    /// cost when nothing was offered.
    pub fn take(&self) -> Option<OperatorLists> {
        if !self.pending.load(Ordering::Relaxed) {
            return None;
        }
        if !self.pending.swap(false, Ordering::Acquire) {
            return None;
        }
        self.next.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Zeroes the active-track observer on EVERY exit path out of
/// `listen_with_observers`, not just the happy one.
///
/// MAN-45 (PR #63 round-19 finding, P2): `IqSource::read` failing after at
/// least one track had become active returned through `?` before
/// `tm.finish()` and the final `report_active_tracks`, leaving the observer
/// pinned at its last nonzero value while the `TrackManager` behind it was
/// already dropped -- a ghost count any long-lived consumer of
/// `listen_with_observers` would keep publishing after a device disconnect
/// or a file-read error. `manta-cli` happened to paper over this by zeroing
/// its own separate `Metrics` gauge; nothing made that true for other
/// callers. A `Drop` guard rather than cleanup appended to the end of the
/// function, because the error paths are precisely the ones that never
/// reached the end.
struct ActiveTracksGuard(Option<Arc<AtomicU64>>);

impl ActiveTracksGuard {
    /// Also zeroes on construction: a caller reusing one handle across runs
    /// (or one that seeded it with a nonzero value) must not see the
    /// previous run's tail count until the first chunk is processed.
    fn new(gauge: Option<Arc<AtomicU64>>) -> Self {
        let guard = Self(gauge);
        guard.clear();
        guard
    }

    fn clear(&self) {
        if let Some(gauge) = &self.0 {
            gauge.store(0, Ordering::Relaxed);
        }
    }
}

impl Drop for ActiveTracksGuard {
    fn drop(&mut self) {
        self.clear();
    }
}

/// One continuous decode segment: its own channelizer and track manager,
/// spanning the sample clock until the next discontinuity (MAN-73). The
/// `Validator` spans every segment of a `listen()` call; only the segment
/// itself (and its sample clock) restarts.
struct Segment {
    ch: manta_dsp::channelizer::Channelizer,
    tm: crate::track::TrackManager,
    hop: u64,
    pad_hops: u64,
}

fn new_segment(
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
    next_id: u32,
) -> Result<Segment> {
    let ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))?;
    let hop = ch.hop() as u64;
    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let mut tm = crate::track::TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        cfg.detector,
        cfg.decode.clone(),
    );
    tm.resume_track_ids_from(next_id);
    Ok(Segment {
        ch,
        tm,
        hop,
        pad_hops,
    })
}

fn emit(
    events: Vec<DecoderEvent>,
    validator: &mut Validator,
    lists: Option<&OperatorListsUpdate>,
    calibration_factor: f64,
    on_event: &mut impl FnMut(&DecoderEvent),
    on_spot: &mut impl FnMut(&crate::Spot),
) {
    for ev in events {
        // MAN-78: the one place the validator is fed, so the one place a
        // reload's lists are taken, for calibration, steady state, outage
        // and end-of-stream batches alike.
        if let Some(l) = lists.and_then(OperatorListsUpdate::take) {
            validator.replace_operator_lists(&l.allowlist, l.blocklist, l.notch);
        }
        on_event(&crate::calibrate_freq_events(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
}

/// Run the streaming decode loop against `src` until `read` returns 0 (EOF,
/// file replay) or `stop` is set (Ctrl-C, live audio). Each decoded event is
/// passed to `on_event` as it's produced. Design doc §4.
///
/// MAN-73: a live source may report a discontinuity via
/// `IqSource::take_discontinuity()` (e.g. after a reconnect). When it does,
/// the current segment (channelizer + track manager) is closed -- every
/// open track gets its `TrackClosed`, as `ClosureKind::Bookkeeping` rather
/// than `SignalEnded`, since the transmission may still be on the air --
/// and a fresh segment starts, with its sample clock advanced by the
/// reported gap and its track ids continuing from the closed segment's.
/// This keeps spot timestamps
/// wall-clock-true across the outage without splicing pre-outage audio
/// onto post-outage audio or zero-filling the gap (zero-fill pins the
/// noise floor and floods false tracks on resume -- see
/// `IqSource::take_discontinuity`'s doc comment). File replay never
/// reports a discontinuity, so this is a no-op there.
///
/// Unchanged entry point: `listen_with_observers` with no observers. Kept so
/// MAN-45's engine addition costs its four existing call sites nothing.
pub fn listen(
    src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    on_event: impl FnMut(&DecoderEvent),
    on_spot: impl FnMut(&crate::Spot),
) -> Result<()> {
    listen_with_observers(
        src,
        cfg,
        stop,
        ListenObservers::default(),
        on_event,
        on_spot,
        |_n| {},
    )
}

/// Like `listen`, but additionally publishes engine-owned live state (the
/// active-track count) to the caller two ways: into `observers`, a shared
/// atomic a consumer on another thread can poll on its own schedule
/// (MAN-45), and to `on_tracks`, a synchronous per-batch callback (MAN-122).
/// Both carry the same number; they exist side by side because they answer
/// different questions -- see `ListenObservers`'s doc comment for why the
/// gauge is an atomic, and the `on_tracks` paragraphs below for why the
/// daemon additionally needs the per-batch *edge*.
///
/// `on_tracks` is called with
/// `TrackManager::decoding_track_count()` after every batch the pipeline
/// processes, including the final `finish()`, which reports 0.
///
/// MAN-122 review round 2: it fires on EVERY batch, not only when the
/// count changes. The call is the daemon's only per-batch signal that the
/// synchronous decode loop is still turning over -- a status line derived
/// from change-only notifications cannot tell "quiet band, count steady at
/// 2" from "`IqSource::read` wedged, count frozen at 2 since an hour ago",
/// which is exactly the question the line exists to answer. Suppressing
/// repeats is the caller's business now (`manta-cli` keeps the gauge store
/// unconditional -- one relaxed atomic per ~43 ms chunk is nothing against
/// the per-batch DSP work it follows).
///
/// MAN-122 review round 1: the daemon's `manta_active_tracks` gauge and its
/// status line's `tracks=` field must come from the manager's own lifecycle
/// state, not from the `DecoderEvent` stream `on_event` already sees. A
/// track that `TrackManager` has promoted but whose demodulator has not
/// latched emits no events at all -- `TrackDecoder` withholds `TrackMeta`
/// until `snr_2500_db()` is `Some`, and a silent ACTIVE track survives to
/// the ~30 s `gc_hops` GC -- so an event-derived count reports zero while
/// real decoders are running on weak or unmodulated signals. This is the
/// count that cannot lie about that.
///
/// Kept as a separate entry point rather than extra parameters on
/// `listen()` so the existing callers and tests are untouched; `listen()`
/// is now a no-op-observer wrapper over this.
pub fn listen_with_observers(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    observers: ListenObservers,
    mut on_event: impl FnMut(&DecoderEvent),
    mut on_spot: impl FnMut(&crate::Spot),
    mut on_tracks: impl FnMut(usize),
) -> Result<()> {
    // Declared before the first `?` below so that EVERY early return from
    // here on clears the observer -- see `ActiveTracksGuard`'s doc comment.
    let _active_tracks_guard = ActiveTracksGuard::new(observers.active_tracks.clone());

    // Validated up front so a bad config value fails fast, before spending
    // CALIBRATION_SECONDS reading from a live device (MAN-29).
    let calibration_factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;

    let fs = src.sample_rate();
    let center_freq_hz = src.center_freq_hz();

    let mut validator = cfg.validator(fs)?;

    // One relaxed store over a count that is a single filtered pass across
    // the (cap-bounded) track map, once per processed chunk and skipped
    // entirely when no observer is registered -- immaterial against the Pi4
    // CPU budget and against the channelizer + TrackManager work it follows.
    //
    // MAN-122: the published number is `decoding_track_count()`, NOT
    // `active_track_count()`. MAN-45 introduced this gauge against the
    // latter (every entry in `tracks`, unconfirmed CANDIDATEs included);
    // candidates are mostly noise-blip rise crossings that close within
    // `confirm_hops` without ever leasing a decoder, so counting them
    // inflates an operator-facing "is it decoding?" reading with signals
    // nothing is decoding. See `TrackManager::decoding_track_count`'s doc
    // comment for the full argument. `active_track_count` keeps its
    // existing meaning for `soak_metrics`' peak/eviction accounting.
    let report_active_tracks = |n_tracks: usize| {
        if let Some(gauge) = &observers.active_tracks {
            gauge.store(n_tracks as u64, Ordering::Relaxed);
        }
    };

    let mut seg = new_segment(fs, center_freq_hz, cfg, 1)?;
    let mut seg_base: u64 = 0;

    let padding = vec![Complex32::new(0.0, 0.0); seg.ch.filter_len()];

    let calib_n = (fs * CALIBRATION_SECONDS).round() as usize;
    let mut calib = vec![Complex32::new(0.0, 0.0); calib_n];
    let mut filled = 0;
    while filled < calib_n {
        // MAN-122 review round 7: checked before EVERY calibration read, not
        // only once the decode loop below starts. The daemon installs its
        // Ctrl-C handler before it logs the `listening:` banner, so a stop
        // request can land at any point while this two-second buffer fills;
        // without this check it was honoured only once the buffer was full.
        // A read already in flight is not interrupted; it returns with
        // whatever the source yields next, or fails after that source's own
        // stall bound (`IqSource::read`'s implementations in `manta-input`).
        // `break`, not `return`: whatever was read is still processed below
        // and flushed by `finish()`, so a watchdog-bounded caller (`doctor`,
        // `soak`) still analyses every sample it read, and the decode loop's
        // own `stop` check then ends the run without another read.
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = src.read(&mut calib[filled..])?;
        if n == 0 {
            anyhow::bail!("audio source ended during startup calibration");
        }
        if let Some(gap) = src.take_discontinuity() {
            // A discontinuity during calibration: the pre-gap partial
            // buffer was never processed through the channelizer, so it's
            // simply discarded (not spliced) -- keep the post-gap samples
            // just read, advance the sample clock by the discarded samples
            // plus the gap, and keep filling from there.
            calib.copy_within(filled..filled + n, 0);
            seg_base += filled as u64 + gap;
            filled = n;
            continue;
        }
        filled += n;
    }
    // The startup lead-in padding is processed only once the calibration
    // fill has returned, not before it: its `on_tracks` call is the first
    // per-batch callback, and the daemon reads that first call as "the
    // calibration read has returned" (MAN-122's `ready: decoding` event).
    let events = seg.tm.process_hops(&seg.ch.process(&padding), |m| {
        seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
    });
    emit(
        events,
        &mut validator,
        observers.operator_lists.as_deref(),
        calibration_factor,
        &mut on_event,
        &mut on_spot,
    );
    let n_tracks = seg.tm.decoding_track_count();
    report_active_tracks(n_tracks);
    on_tracks(n_tracks);
    // `..filled`: the whole buffer unless a stop request cut the fill short.
    let events = seg.tm.process_hops(&seg.ch.process(&calib[..filled]), |m| {
        seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
    });
    emit(
        events,
        &mut validator,
        observers.operator_lists.as_deref(),
        calibration_factor,
        &mut on_event,
        &mut on_spot,
    );
    let n_tracks = seg.tm.decoding_track_count();
    report_active_tracks(n_tracks);
    on_tracks(n_tracks);
    let mut seg_consumed: u64 = filled as u64;

    let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // A read error must not escape via `?` before the gauge is
        // zeroed. The end-of-stream `on_tracks(0)` below is only reached
        // on a clean EOF or a `stop` request, so a mid-stream failure --
        // an SDR disconnecting, say -- would otherwise leave
        // `manta_active_tracks` (and the status line's `tracks=`) frozen
        // at its last nonzero value for the whole shutdown drain, up to
        // `SHUTDOWN_DRAIN_DEADLINE` (25 s), while the metrics listener is
        // still answering scrapes with decoders that no longer exist.
        // Publish 0 first, then propagate the original error unchanged.
        let n = match src.read(&mut chunk) {
            Ok(n) => n,
            Err(e) => {
                report_active_tracks(0);
                on_tracks(0);
                return Err(e);
            }
        };
        if n == 0 {
            break;
        }
        if let Some(gap) = src.take_discontinuity() {
            // Not `finish()`: an outage is not an end of signal (see
            // `TrackManager::finish_for_discontinuity`).
            let finish_events = seg.tm.finish_for_discontinuity();
            emit(
                finish_events,
                &mut validator,
                observers.operator_lists.as_deref(),
                calibration_factor,
                &mut on_event,
                &mut on_spot,
            );
            let next_id = seg.tm.next_track_id();
            seg_base += seg_consumed + gap;
            seg_consumed = 0;
            seg = new_segment(fs, center_freq_hz, cfg, next_id)?;
            let events = seg.tm.process_hops(&seg.ch.process(&padding), |m| {
                seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
            });
            emit(
                events,
                &mut validator,
                observers.operator_lists.as_deref(),
                calibration_factor,
                &mut on_event,
                &mut on_spot,
            );
        }
        // MAN-128: steady-state chunk latency, timed from here (after the
        // read and after any outage-segment rebuild above, so a one-off
        // `new_segment` does not skew the distribution) to the end of the
        // chunk's processing.
        let t0 = observers
            .decode_latency
            .as_ref()
            .map(|_| std::time::Instant::now());
        let events = seg.tm.process_hops(&seg.ch.process(&chunk[..n]), |m| {
            seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
        });
        emit(
            events,
            &mut validator,
            observers.operator_lists.as_deref(),
            calibration_factor,
            &mut on_event,
            &mut on_spot,
        );
        let n_tracks = seg.tm.decoding_track_count();
        report_active_tracks(n_tracks);
        on_tracks(n_tracks);
        if let (Some(obs), Some(t0)) = (&observers.decode_latency, t0) {
            obs.observe(t0.elapsed());
        }
        seg_consumed += n as u64;
    }
    let events = seg.tm.finish();
    emit(
        events,
        &mut validator,
        observers.operator_lists.as_deref(),
        calibration_factor,
        &mut on_event,
        &mut on_spot,
    );
    // `finish()` flushes and drops every decoder and closes every remaining
    // track: nothing is being decoded once the stream has ended, so both
    // observers must settle back to 0 rather than be left holding the last
    // live value after a source disconnects or a replay hits EOF.
    report_active_tracks(0);
    on_tracks(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn listen_spots_a_call_only_an_override_cty_allocates() {
        let mut spec = manta_testkit::vectors::v1();
        spec.duration_s = 30.0;
        spec.signals[0].text = "CQ CQ DE QQ9ZZZ QQ9ZZZ K".into();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let calls = |cfg: &PipelineConfig| {
            let src = Box::new(FixedFreqSource {
                samples: rendered.samples.clone(),
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            });
            let mut calls = Vec::new();
            listen(
                src,
                cfg,
                Arc::new(AtomicBool::new(false)),
                |_| {},
                |s| calls.push(s.callsign.clone()),
            )
            .unwrap();
            calls
        };
        assert!(!calls(&PipelineConfig::default()).contains(&"QQ9ZZZ".into()));
        let table = manta_spot::cty::Table::parse(&format!(
            "{}Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n",
            manta_spot::CTY_DAT
        ));
        let cfg = PipelineConfig {
            cty: Some(Arc::new(table)),
            ..Default::default()
        };
        assert!(calls(&cfg).contains(&"QQ9ZZZ".into()));
    }

    /// A minimal in-memory IqSource for testing, reporting a fixed,
    /// caller-chosen `center_freq_hz` (unlike `AudioIqSource`, which always
    /// reports 0.0) -- this is what proves `listen()` actually reads
    /// `src.center_freq_hz()` instead of hardcoding 0.0.
    struct FixedFreqSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl manta_input::IqSource for FixedFreqSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    /// MAN-73 test double: serves `samples` on a loop. Once the cumulative
    /// number of samples served reaches `arm_at` (default: the full
    /// vector length, i.e. "after exhaustion"), the *next* `read()` call
    /// rewinds to the start of `samples` and arms a one-shot
    /// `take_discontinuity()` of `gap` samples -- simulating a
    /// `ReconnectingSource` that lost the connection, waited out `gap`
    /// worth of wall-clock time, and resumed producing real samples.
    /// `chunk_cap` bounds how many samples a single `read()` call serves,
    /// so a caller reading into a buffer much larger than `chunk_cap`
    /// (e.g. the startup calibration buffer) still sees the gap arm
    /// partway through, not only at a whole-buffer boundary.
    struct GapSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
        gap: u64,
        arm_at: usize,
        chunk_cap: usize,
        served: usize,
        armed: bool,
        pending_gap: Option<u64>,
        /// Set once `take_discontinuity()` has handed the gap to `listen()`,
        /// so a test can tell the events `listen()` emits after the
        /// discontinuity from those before it.
        gap_taken: Arc<AtomicBool>,
    }

    impl GapSource {
        fn new(samples: Vec<Complex32>, fs: f64, center_freq_hz: f64, gap_samples: u64) -> Self {
            let arm_at = samples.len();
            GapSource {
                chunk_cap: samples.len().max(1),
                samples,
                cursor: 0,
                fs,
                center_freq_hz,
                gap: gap_samples,
                arm_at,
                served: 0,
                armed: false,
                pending_gap: None,
                gap_taken: Arc::new(AtomicBool::new(false)),
            }
        }

        /// Arm the gap after `k` cumulative samples served instead of at
        /// exhaustion -- used to simulate a discontinuity arriving
        /// partway through startup calibration.
        fn arm_after_samples(mut self, k: usize) -> Self {
            self.arm_at = k;
            self.chunk_cap = (k / 4).max(64);
            self
        }
    }

    impl manta_input::IqSource for GapSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if !self.armed && self.served >= self.arm_at {
                self.cursor = 0;
                self.armed = true;
                self.pending_gap = Some(self.gap);
            }
            let avail = self.samples.len() - self.cursor;
            let n = buf.len().min(avail).min(self.chunk_cap);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            self.served += n;
            Ok(n)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            let gap = self.pending_gap.take();
            if gap.is_some() {
                self.gap_taken.store(true, Ordering::Relaxed);
            }
            gap
        }
    }

    /// MAN-73: a discontinuity reported mid-stream closes the current
    /// segment (every open track gets `TrackClosed`), starts a fresh one
    /// whose sample clock is advanced by the gap, and never re-reads any
    /// pre-gap data -- so no false spot is produced by splicing pre- and
    /// post-gap audio together, track ids are never reused, and spot
    /// timestamps land on the correct (post-gap) side of the clock.
    #[test]
    fn listen_restarts_the_segment_on_a_discontinuity_and_advances_sample_ts() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let len = rendered.samples.len() as u64;
        let gap = (spec.fs as u64) * 3600; // 1 hour
        let src: Box<dyn manta_input::IqSource> = Box::new(GapSource::new(
            rendered.samples.clone(),
            spec.fs,
            spec.center_freq_hz,
            gap,
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        let mut track_meta_ids: Vec<(u64, u32)> = Vec::new(); // (approx order, track_id)
        let mut closed_before_first_post_gap_char = true;
        let mut seen_post_gap_char = false;
        let mut closed_ids: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        let mut order = 0u64;
        manta_engine_listen_with_discontinuity_probe(src, &stop, &mut spots, |ev| {
            order += 1;
            match ev {
                DecoderEvent::TrackMeta { track_id, .. } => {
                    track_meta_ids.push((order, *track_id));
                }
                DecoderEvent::TrackClosed { track_id, .. } => {
                    closed_ids.insert(*track_id);
                }
                DecoderEvent::CharDecoded { track_id, .. } => {
                    if *track_id > 1 && !closed_ids.contains(&1) {
                        // A post-gap track emitted a char before the
                        // pre-gap track (id 1) was ever closed.
                        closed_before_first_post_gap_char = false;
                    }
                    if *track_id > 1 {
                        seen_post_gap_char = true;
                    }
                }
                _ => {}
            }
        })
        .unwrap();
        let _ = stop;

        assert!(!spots.is_empty(), "expected at least one spot");
        assert!(
            spots.iter().all(|s: &crate::Spot| s.callsign == "W1AW"),
            "every spot must be W1AW (no false spot from splicing pre/post-gap audio): {spots:?}"
        );
        assert!(
            spots.iter().any(|s| s.sample_ts < len),
            "expected at least one pre-gap spot, got {spots:?}"
        );
        assert!(
            spots.iter().any(|s| s.sample_ts >= len + gap),
            "expected at least one post-gap spot with sample_ts >= {}, got {spots:?}",
            len + gap
        );

        let pre_gap_ids: std::collections::BTreeSet<u32> = track_meta_ids
            .iter()
            .filter(|(_, id)| *id == 1)
            .map(|(_, id)| *id)
            .collect();
        let post_gap_ids: std::collections::BTreeSet<u32> = track_meta_ids
            .iter()
            .map(|(_, id)| *id)
            .filter(|id| !pre_gap_ids.contains(id))
            .collect();
        assert!(
            !post_gap_ids.is_empty(),
            "expected at least one post-gap track id, got {track_meta_ids:?}"
        );
        assert!(
            post_gap_ids.iter().all(|id| pre_gap_ids.iter().all(|p| id > p)),
            "every post-gap track id must be greater than every pre-gap id: pre={pre_gap_ids:?} post={post_gap_ids:?}"
        );
        assert!(seen_post_gap_char, "expected a post-gap CharDecoded event");
        assert!(
            closed_before_first_post_gap_char,
            "the pre-gap track's TrackClosed must arrive before any post-gap CharDecoded"
        );
    }

    /// MAN-73 (PR #207 review): a transport outage is not an end of
    /// signal. Every track the discontinuity tears down must close as
    /// `Bookkeeping { survivor_track_id: None }`, never `SignalEnded`:
    /// `SignalEnded` tells `manta-spot`'s `Validator` the transmission
    /// ended, so the post-reconnect track's next call utterance of the same
    /// CQ would count as a distinct message, and pending beacons would
    /// resolve early.
    #[test]
    fn a_discontinuity_closes_open_tracks_as_bookkeeping_not_signal_ended() {
        use manta_decode::events::ClosureKind;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        // 20 s of V1's looped CQ with the gap armed 12 s in: past startup
        // calibration and mid-transmission, so a track is open at the gap.
        let samples = rendered.samples[..(spec.fs * 20.0) as usize].to_vec();
        let src = GapSource::new(samples, spec.fs, spec.center_freq_hz, spec.fs as u64)
            .arm_after_samples((spec.fs * 12.0) as usize);
        let gap_taken = src.gap_taken.clone();

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        let mut seen_before_gap: std::collections::BTreeSet<u32> = Default::default();
        let mut closed_at_gap: Vec<(u32, ClosureKind)> = Vec::new();
        manta_engine_listen_with_discontinuity_probe(Box::new(src), &stop, &mut spots, |ev| {
            let id = crate::track::event_track_id(ev);
            if !gap_taken.load(Ordering::Relaxed) {
                seen_before_gap.insert(id);
            } else if let DecoderEvent::TrackClosed { track_id, closure } = ev {
                // Only the pre-gap segment's tracks: its `TrackManager` is
                // dropped right after the teardown, so any of its tracks
                // closing after the gap was closed BY the teardown.
                if seen_before_gap.contains(track_id) {
                    closed_at_gap.push((*track_id, *closure));
                }
            }
        })
        .unwrap();

        assert!(
            !closed_at_gap.is_empty(),
            "test setup: a track must still be open when the gap arrives"
        );
        assert!(
            closed_at_gap.iter().all(|(_, closure)| *closure
                == ClosureKind::Bookkeeping {
                    survivor_track_id: None
                }),
            "a discontinuity must close tracks as Bookkeeping, not SignalEnded: {closed_at_gap:?}"
        );
    }

    /// Thin wrapper around `listen()` used only by the discontinuity tests
    /// above/below, so the on_event closure can observe every event
    /// (including TrackClosed) while still routing spots to a Vec the way
    /// the other tests in this module do.
    fn manta_engine_listen_with_discontinuity_probe(
        src: Box<dyn manta_input::IqSource>,
        stop: &Arc<AtomicBool>,
        spots: &mut Vec<crate::Spot>,
        mut on_event: impl FnMut(&DecoderEvent),
    ) -> Result<()> {
        listen(
            src,
            &PipelineConfig::default(),
            stop.clone(),
            |ev| on_event(ev),
            |spot| spots.push(spot.clone()),
        )
    }

    /// MAN-73: a discontinuity reported *during* startup calibration
    /// discards the pre-gap partial calibration buffer (it's never
    /// processed through the channelizer) rather than splicing it onto
    /// post-gap data, and still advances the sample clock correctly.
    #[test]
    fn listen_handles_a_discontinuity_during_startup_calibration() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let gap = (spec.fs as u64) * 5; // 5 s
        let arm_after = (spec.fs * 0.5).round() as usize; // 0.5 s into the 2 s calibration fill
        let src: Box<dyn manta_input::IqSource> = Box::new(
            GapSource::new(rendered.samples.clone(), spec.fs, spec.center_freq_hz, gap)
                .arm_after_samples(arm_after),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(!spots.is_empty(), "expected at least one spot");
        let min_expected_ts = arm_after as u64 + gap;
        assert!(
            spots.iter().all(|s| s.sample_ts >= min_expected_ts),
            "every spot's sample_ts must be >= {min_expected_ts} (discarded pre-gap calibration \
             samples + the gap), got {spots:?}"
        );
    }

    /// MAN-73: a source that never reports a discontinuity (every source
    /// today, and file replay always) must decode identically across runs
    /// -- this is the regression the golden/determinism suites already
    /// pin globally; this test documents the contract locally for this
    /// module.
    #[test]
    fn listen_without_discontinuity_is_unchanged() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();

        let run = || {
            let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
                samples: rendered.samples.clone(),
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            });
            let mut spots = Vec::new();
            listen(
                src,
                &PipelineConfig::default(),
                Arc::new(AtomicBool::new(false)),
                |_ev| {},
                |spot| spots.push((spot.callsign.clone(), spot.sample_ts, spot.track_id)),
            )
            .unwrap();
            spots
        };

        assert_eq!(run(), run(), "identical input must decode identically");
    }

    #[test]
    fn operator_lists_update_hands_over_only_the_newest_offer() {
        assert!(OperatorListsUpdate::default().take().is_none());
        let update = OperatorListsUpdate::default();
        update.offer(OperatorLists {
            allowlist: vec!["A".into()],
            ..OperatorLists::default()
        });
        update.offer(OperatorLists {
            allowlist: vec!["B".into()],
            ..OperatorLists::default()
        });
        assert_eq!(update.take().unwrap().allowlist, vec!["B".to_string()]);
        assert!(update.take().is_none());
    }

    /// Vector v7 through `listen_with_observers`, offering `lists` from
    /// `on_spot` the first time N2BB spots (sample 1,407,232; N1AA follows
    /// at 1,555,968, a later chunk). Returns every spotted callsign.
    fn v7_with_lists_offered_at_n2bb(cfg: &PipelineConfig, lists: OperatorLists) -> Vec<String> {
        let spec = manta_testkit::vectors::v7();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let update = Arc::new(OperatorListsUpdate::default());
        let observers = ListenObservers {
            operator_lists: Some(update.clone()),
            ..ListenObservers::default()
        };
        let mut lists = Some(lists);
        let mut spots = Vec::new();
        listen_with_observers(
            src,
            cfg,
            Arc::new(AtomicBool::new(false)),
            observers,
            |_ev| {},
            |spot| {
                if spot.callsign == "N2BB" {
                    if let Some(l) = lists.take() {
                        update.offer(l);
                    }
                }
                spots.push(spot.callsign.clone());
            },
            |_n| {},
        )
        .unwrap();
        spots
    }

    fn v7_spots(cfg: &PipelineConfig) -> Vec<String> {
        let spec = manta_testkit::vectors::v7();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let mut spots = Vec::new();
        listen(
            src,
            cfg,
            Arc::new(AtomicBool::new(false)),
            |_ev| {},
            |spot| spots.push(spot.callsign.clone()),
        )
        .unwrap();
        spots
    }

    /// Offers replacement lists from inside a read, including the EOF read.
    struct OfferingSource {
        inner: FixedFreqSource,
        update: Arc<OperatorListsUpdate>,
        offer_at: Option<u64>,
        reads: Arc<AtomicU64>,
    }

    impl IqSource for OfferingSource {
        fn sample_rate(&self) -> f64 {
            self.inner.sample_rate()
        }

        fn center_freq_hz(&self) -> f64 {
            self.inner.center_freq_hz()
        }

        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let k = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
            if self.offer_at == Some(k) {
                self.update.offer(OperatorLists {
                    allowlist: vec!["W1AW".into()],
                    blocklist: Blocklist::parse("W1AW\n"),
                    ..Default::default()
                });
            }
            self.inner.read(buf)
        }
    }

    fn fast_w1aw(
        wpm: f32,
        truncate: Option<usize>,
        offer_at: Option<u64>,
    ) -> (Vec<(String, u64)>, u64) {
        let mut spec = manta_testkit::vectors::v1();
        spec.duration_s = 3.0;
        spec.signals[0].wpm = wpm;
        spec.signals[0].text = "W1AW W1AW W1AW W1AW W1AW".into();
        let mut samples = manta_testkit::vectors::render(&spec).unwrap().samples;
        if let Some(n) = truncate {
            samples.truncate(n);
        }
        let update = Arc::new(OperatorListsUpdate::default());
        let reads = Arc::new(AtomicU64::new(0));
        let src = OfferingSource {
            inner: FixedFreqSource {
                samples,
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            },
            update: update.clone(),
            offer_at,
            reads: reads.clone(),
        };
        let mut cfg = PipelineConfig {
            allowlist: vec!["W1AW".into()],
            ..Default::default()
        };
        cfg.detector.warmup_hops = 0;
        let mut spots = Vec::new();
        listen_with_observers(
            Box::new(src),
            &cfg,
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                operator_lists: Some(update),
                ..Default::default()
            },
            |_| {},
            |s| spots.push((s.callsign.clone(), reads.load(Ordering::Relaxed))),
            |_| {},
        )
        .unwrap();
        (spots, reads.load(Ordering::Relaxed))
    }

    #[test]
    fn lists_offered_during_calibration_apply_to_the_calibration_buffer() {
        let (control, _) = fast_w1aw(72.0, None, None);
        assert_eq!(control, vec![("W1AW".to_string(), 1)]);
        let (spots, _) = fast_w1aw(72.0, None, Some(1));
        assert!(spots.is_empty(), "got {spots:?}");
    }

    #[test]
    fn lists_offered_during_a_read_apply_to_its_chunk() {
        let (control, _) = fast_w1aw(60.0, None, None);
        let k = control.first().expect("control spots W1AW").1;
        assert!(
            k > 1,
            "spot must come from a steady-state chunk, got {control:?}"
        );
        let (spots, _) = fast_w1aw(60.0, None, Some(k));
        assert!(spots.is_empty(), "got {spots:?}");
    }

    #[test]
    fn lists_offered_during_the_eof_read_apply_to_the_final_flush() {
        let (control, total) = fast_w1aw(60.0, Some(205_000), None);
        assert_eq!(control, vec![("W1AW".to_string(), total)]);
        let (spots, _) = fast_w1aw(60.0, Some(205_000), Some(total));
        assert!(spots.is_empty(), "got {spots:?}");
    }

    /// MAN-78: lists offered mid-run reach the live validator before a
    /// later call is evaluated.
    #[test]
    fn lists_offered_mid_run_block_a_later_call() {
        let control = v7_spots(&PipelineConfig::default());
        assert!(
            control.iter().any(|c| c == "N1AA") && control.iter().any(|c| c == "N2BB"),
            "control run must spot both N2BB and N1AA, got {control:?}"
        );
        let spots = v7_with_lists_offered_at_n2bb(
            &PipelineConfig::default(),
            OperatorLists {
                blocklist: Blocklist::parse("N1AA\n"),
                ..OperatorLists::default()
            },
        );
        assert_eq!(spots, vec!["N2BB".to_string()]);
    }

    #[test]
    fn lists_offered_mid_run_lift_a_block() {
        let cfg = PipelineConfig {
            blocklist: Blocklist::parse("N1AA\n"),
            ..PipelineConfig::default()
        };
        assert!(
            !v7_spots(&cfg).iter().any(|c| c == "N1AA"),
            "control: the startup blocklist must hold N1AA back"
        );
        let spots = v7_with_lists_offered_at_n2bb(&cfg, OperatorLists::default());
        assert!(spots.iter().any(|c| c == "N1AA"), "got {spots:?}");
    }

    /// MAN-45 (PR #63 round-9 finding): `manta_active_tracks` reported a
    /// constant 0 on every production run because `listen()` exposed no
    /// live track count to its caller -- the manager's own count existed but
    /// was reachable only from inside the engine. This proves the observer
    /// handle tracks the real count during the run and settles at 0
    /// afterward (`TrackManager::finish()` closes every track). MAN-122: the
    /// number published here is `decoding_track_count()`, so an unconfirmed
    /// noise candidate can never lift it off 0.
    #[test]
    fn listen_with_observers_publishes_a_live_active_track_count() {
        use std::sync::atomic::AtomicU64;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let gauge = Arc::new(AtomicU64::new(0));
        let observed = gauge.clone();
        let mut peak = 0u64;
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                active_tracks: Some(gauge.clone()),
                ..Default::default()
            },
            |_ev| peak = peak.max(observed.load(Ordering::Relaxed)),
            |_spot| {},
            |_n| {},
        )
        .unwrap();

        assert!(
            peak >= 1,
            "V1's single strong signal must show as an active track mid-run"
        );
        assert_eq!(
            gauge.load(Ordering::Relaxed),
            0,
            "finish() closes every track"
        );
    }

    /// MAN-45 (PR #63 round-19 finding, P2): an `IqSource::read` error after
    /// tracks had become active used to return through `?` with the observer
    /// still holding that nonzero count, so a caller polling the handle kept
    /// reporting ghost tracks for a `TrackManager` that no longer existed.
    #[test]
    fn the_active_track_observer_is_cleared_when_listen_exits_with_an_error() {
        use std::sync::atomic::AtomicU64;

        /// Fails its `read` as soon as the observer reports at least one
        /// active track -- exactly the "device disconnected mid-run" shape
        /// the finding describes.
        struct FailsOnceTracksAreActive {
            inner: FixedFreqSource,
            observed: Arc<AtomicU64>,
        }

        impl manta_input::IqSource for FailsOnceTracksAreActive {
            fn sample_rate(&self) -> f64 {
                self.inner.sample_rate()
            }
            fn center_freq_hz(&self) -> f64 {
                self.inner.center_freq_hz()
            }
            fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
                if self.observed.load(Ordering::Relaxed) > 0 {
                    anyhow::bail!("simulated device disconnect");
                }
                self.inner.read(buf)
            }
        }

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let gauge = Arc::new(AtomicU64::new(0));
        let src: Box<dyn manta_input::IqSource> = Box::new(FailsOnceTracksAreActive {
            inner: FixedFreqSource {
                samples: rendered.samples,
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            },
            observed: gauge.clone(),
        });

        let err = listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                active_tracks: Some(gauge.clone()),
                ..Default::default()
            },
            |_ev| {},
            |_spot| {},
            |_n| {},
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("simulated device disconnect"),
            "the read error must propagate, not be swallowed: {err}"
        );
        assert_eq!(
            gauge.load(Ordering::Relaxed),
            0,
            "the observer must not keep reporting tracks whose TrackManager is gone"
        );
    }

    /// The default path must stay exactly as cheap as before --
    /// `listen()`'s own signature and behavior are unchanged, and its four
    /// existing call sites (main.rs, soak.rs, two integration tests) do not
    /// move.
    #[test]
    fn plain_listen_still_runs_with_no_observers() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(!spots.is_empty(), "V1's repeated W1AW should have spotted");
    }

    #[test]
    fn listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero() {
        // A real V1-style golden signal (clean +20 dB tone), but fed through
        // a source that reports a nonzero center_freq_hz -- if listen() were
        // still hardcoding 0.0, every TrackMeta.freq_hz would come back as
        // just the +12.34 kHz baseband offset, not centered on 14 MHz.
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |ev| {
                if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                    last_freq_hz = Some(*freq_hz);
                }
            },
            |_spot| {},
        )
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        assert!(
            (freq_hz - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "freq_hz {freq_hz} should be near {} (center_freq_hz + V1's known offset), not near 12340 \
             (which is what a hardcoded center_freq_hz=0.0 would produce)",
            spec.center_freq_hz + 12_340.0
        );
    }

    /// MAN-122 review round 1: the track-count observer reports
    /// `TrackManager`'s promoted-track count, rises above zero while a real
    /// signal is being decoded, and is driven back to zero by `finish()` so
    /// the daemon's gauge doesn't stay stuck at the last live value after
    /// EOF or an SDR disconnect. Review round 2: it also fires once per
    /// processed batch, repeats included -- that per-batch edge is what the
    /// daemon's status line uses to tell a wedged decode loop from a quiet
    /// band.
    #[test]
    fn listen_reports_the_managers_track_count_and_clears_it_at_end_of_stream() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut counts: Vec<usize> = Vec::new();
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            stop,
            ListenObservers::default(),
            |_ev| {},
            |_spot| {},
            |n| counts.push(n),
        )
        .unwrap();

        assert!(
            counts.iter().any(|&n| n > 0),
            "V1 is a clean +20 dB tone -- the observer must see a promoted track at some point, got {counts:?}"
        );
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "finish() drops every decoder, so the final reported count must be 0, got {counts:?}"
        );
        // One call per batch: the padding batch, the calibration batch, one
        // per CHUNK_SAMPLES-sized read, and one final zero from finish().
        // Far more calls than there are distinct values -- the point being
        // that a steady count still produces a steady stream of calls.
        assert!(
            counts.len()
                > counts
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
            "the observer must fire per batch, repeats included, got {counts:?}"
        );
    }

    /// A source that replays `samples` and then FAILS instead of reporting
    /// EOF -- an SDR disconnecting mid-stream, not a file running out.
    struct FailsAtEndSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl manta_input::IqSource for FailsAtEndSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if self.cursor >= self.samples.len() {
                anyhow::bail!("source disconnected");
            }
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    /// MAN-122 review round 3: a mid-stream `IqSource::read` failure exits
    /// `listen_with_observers` before the end-of-stream `on_tracks(0)`,
    /// so without an explicit zero on the error path the daemon's
    /// `manta_active_tracks` gauge (and the status line's `tracks=`) would
    /// stay frozen at its last nonzero value for the whole shutdown drain
    /// while the metrics listener still answers scrapes.
    #[test]
    fn listen_zeroes_the_track_count_when_a_read_fails_mid_stream() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FailsAtEndSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut counts: Vec<usize> = Vec::new();
        let err = listen_with_observers(
            src,
            &PipelineConfig::default(),
            stop,
            ListenObservers::default(),
            |_ev| {},
            |_spot| {},
            |n| counts.push(n),
        )
        .expect_err("the source fails instead of reaching EOF, so listen must propagate the error");
        assert!(
            err.to_string().contains("source disconnected"),
            "the original read error must be propagated unchanged, got {err}"
        );

        assert!(
            counts.iter().any(|&n| n > 0),
            "V1 is a clean +20 dB tone -- a track must have been promoted before the failure, got {counts:?}"
        );
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "a failed read must publish 0 before propagating, or the gauge stays frozen through shutdown drain, got {counts:?}"
        );
    }

    /// A live-shaped source: short reads that never reach EOF, and a Ctrl-C
    /// that lands during its first read -- i.e. while `listen` is still
    /// filling its startup calibration buffer.
    struct StopsDuringCalibrationSource {
        stop: Arc<AtomicBool>,
        reads: Arc<AtomicU64>,
    }

    impl manta_input::IqSource for StopsDuringCalibrationSource {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            14_000_000.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
            let n = buf.len().min(64);
            buf[..n].fill(Complex32::new(0.0, 0.0));
            Ok(n)
        }
    }

    /// Runs `listen_with_observers` against `StopsDuringCalibrationSource`
    /// with `stop` initially `stop_before_start`, returning how many reads
    /// it issued and every `on_tracks` value.
    fn run_stopping_during_calibration(stop_before_start: bool) -> (u64, Vec<usize>) {
        let stop = Arc::new(AtomicBool::new(stop_before_start));
        let reads = Arc::new(AtomicU64::new(0));
        let src: Box<dyn manta_input::IqSource> = Box::new(StopsDuringCalibrationSource {
            stop: stop.clone(),
            reads: reads.clone(),
        });
        let mut counts = Vec::new();
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            stop,
            ListenObservers::default(),
            |_ev| {},
            |_spot| {},
            |n| counts.push(n),
        )
        .expect("a stop request during calibration is a clean shutdown, not an error");
        (reads.load(Ordering::Relaxed), counts)
    }

    /// MAN-122 review round 7: the daemon installs its Ctrl-C handler
    /// before it logs the `listening:` banner, so a signal can land while
    /// `listen` is still filling the two-second calibration buffer. The
    /// calibration loop must check `stop` before each read, not keep
    /// reading until the buffer is full and only check `stop` once the
    /// decode loop starts. What was read is still processed and flushed,
    /// so the run ends through `finish()` and settles the count at 0.
    #[test]
    fn listen_stops_during_startup_calibration_without_filling_the_buffer() {
        let (reads, counts) = run_stopping_during_calibration(false);
        assert_eq!(
            reads, 1,
            "listen must not issue another read once stop is set during calibration"
        );
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "the stopped run must still end through finish(), got {counts:?}"
        );
    }

    /// The same, for a signal that lands before `listen` starts at all --
    /// e.g. at the daemon's `listening:` banner: no read is issued, so a
    /// source that would block cannot hold up shutdown.
    #[test]
    fn listen_issues_no_read_when_stop_is_already_set() {
        let (reads, counts) = run_stopping_during_calibration(true);
        assert_eq!(reads, 0, "stop was set before listen started");
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "the stopped run must still end through finish(), got {counts:?}"
        );
    }

    /// MAN-29: `PipelineConfig::freq_correction_ppm` reaches the emitted
    /// spot's `freq_hz`, corrected by the configured ppm -- end-to-end
    /// through `listen()`, not just the `manta-spot::Validator` unit.
    #[test]
    fn listen_applies_freq_correction_ppm_to_emitted_spot_freq_hz() {
        const PPM: f64 = 10.0; // ~140 Hz at 14 MHz.
        let factor = 1.0 + PPM * 1e-6;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let cfg = PipelineConfig {
            freq_correction_ppm: PPM,
            ..Default::default()
        };

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(src, &cfg, stop, |_ev| {}, |spot| spots.push(spot.clone())).unwrap();

        assert!(!spots.is_empty(), "V1's repeated W1AW should have spotted");
        for spot in &spots {
            let uncorrected = spot.freq_hz / factor;
            assert!(
                (uncorrected - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
                "spot.freq_hz {} divided back by the calibration factor should land near the \
                 raw decoded frequency {}, proving the correction was applied once, multiplicatively",
                spot.freq_hz,
                spec.center_freq_hz + 12_340.0
            );
        }
    }

    /// MAN-29 review round 3: the `TrackMeta` events `listen()` passes to
    /// `on_event` (consumed directly by `listen --json`) must be
    /// calibrated too, not just the emitted spots.
    #[test]
    fn listen_calibrates_track_meta_events_passed_to_on_event() {
        const PPM: f64 = 10.0;
        let factor = 1.0 + PPM * 1e-6;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let cfg = PipelineConfig {
            freq_correction_ppm: PPM,
            ..Default::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(
            src,
            &cfg,
            stop,
            |ev| {
                if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                    last_freq_hz = Some(*freq_hz);
                }
            },
            |_spot| {},
        )
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        let uncorrected = freq_hz / factor;
        assert!(
            (uncorrected - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "on_event's TrackMeta.freq_hz {freq_hz} divided back by the calibration factor \
             should land near the raw decoded frequency {}",
            spec.center_freq_hz + 12_340.0
        );
    }

    /// MAN-29 review: an invalid `freq_correction_ppm` must fail `listen()`
    /// up front rather than silently poisoning spot output.
    #[test]
    fn listen_rejects_an_invalid_freq_correction_ppm() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let cfg = PipelineConfig {
            freq_correction_ppm: f64::NAN,
            ..Default::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        assert!(listen(src, &cfg, stop, |_ev| {}, |_spot| {}).is_err());
    }

    /// MAN-31: `listen()` is the other production call site that must
    /// apply an operator-supplied suppression list.
    #[test]
    fn listen_suppresses_a_blocklisted_callsign() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let cfg = PipelineConfig {
            blocklist: manta_spot::Blocklist::parse("W1AW\n"),
            ..Default::default()
        };
        let mut spots = Vec::new();
        listen(
            src,
            &cfg,
            Arc::new(AtomicBool::new(false)),
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(
            spots.is_empty(),
            "blocklisted callsign must never be spotted, got {spots:?}"
        );
    }

    /// MAN-128: one latency sample per steady-state chunk, none for
    /// calibration/padding. `N = calib_n + 3*CHUNK_SAMPLES + 100` reads as
    /// 2048, 2048, 2048, 100 (then EOF) -- 4 steady-state chunks.
    #[test]
    fn listen_with_observers_records_one_latency_sample_per_steady_state_chunk() {
        use crate::latency::DecodeLatencyObserver;

        let fs = 48_000.0;
        let calib_n = (fs * CALIBRATION_SECONDS).round() as usize;
        let n_samples = calib_n + 3 * CHUNK_SAMPLES + 100;
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: vec![Complex32::new(0.0, 0.0); n_samples],
            cursor: 0,
            fs,
            center_freq_hz: 14_000_000.0,
        });

        let obs = Arc::new(DecodeLatencyObserver::new());
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                decode_latency: Some(obs.clone()),
                ..Default::default()
            },
            |_ev| {},
            |_spot| {},
            |_n| {},
        )
        .unwrap();

        assert_eq!(obs.snapshot().count, 4);
    }

    /// The default path must stay exactly as cheap as before: no observer
    /// means no `Instant::now()` call and no observation recorded.
    #[test]
    fn plain_listen_records_nothing() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        listen(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            |_ev| {},
            |_spot| {},
        )
        .unwrap();
        // No observer was supplied -- nothing to assert beyond "this
        // compiles and runs with ListenObservers::default()", which is the
        // whole point: `listen()`'s existing callers pay nothing new.
    }
}
