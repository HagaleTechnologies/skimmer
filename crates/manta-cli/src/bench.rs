//! MAN-116: `manta bench sensitivity`, a recall/CER-vs-SNR sweep over synthetic
//! recordings. Scenes: manta_testkit::sensitivity. Decisions D1-D13 and the
//! metric definitions: docs/DECISIONS/2026-10-10-man116-sensitivity-benchmark.md.
//!
//! stdout carries only the result; it depends on the build, the flags and the
//! config file's `[decode]`/`[detector]` contents, never on `--jobs`, the
//! clock or the host (D8). Progress goes to stderr.

use anyhow::{anyhow, Context, Result};
use manta_decode::decoder::{events_to_text, Engine};
use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, DecodeReport, PipelineConfig};
use manta_server::rbn::RBN_REF_BW_CORRECTION_DB;
use manta_testkit::sensitivity::{self, Condition};
use num_complex::Complex32;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::PathBuf;

/// A spot or track counts for a station within this distance of its frequency.
pub(crate) const MATCH_HZ: f64 = 150.0;
/// A station is "copied cleanly" below this capped CER.
pub(crate) const GOOD_COPY_CER: f64 = 0.10;
/// Version of the `--json` report layout.
const FORMAT_VERSION: u32 = 1;
const COMMAND: &str = "manta bench sensitivity";

const DEFAULT_CONDITIONS: &str = "awgn,good,poor";
const DEFAULT_WPM: &str = "15,25,35";
const DEFAULT_SNR_DB: &str = "0,3,6,9,12,15,20,25,30";
const DEFAULT_DURATION_S: u32 = 60;
const DEFAULT_TRIALS: u32 = 1;
const DEFAULT_SEED: u64 = 1;

const WPM_RANGE: (u32, u32) = (5, 60);
const SNR_RANGE_DB: (f64, f64) = (-30.0, 60.0);
const DURATION_RANGE_S: (u32, u32) = (10, 300);
const TRIALS_RANGE: (u32, u32) = (1, 20);

/// `(name, description)` for the table legend, shared by the text and
/// Markdown renderers.
const LEGEND: [(&str, &str); 5] = [
    (
        "recall",
        "stations spotted with the right call, within 150 Hz of where they sent",
    ),
    (
        "bogus",
        "calls spotted that no station sent on that frequency",
    ),
    (
        "CER",
        "character error rate of each station's copy, capped at 1, averaged",
    ),
    (
        "CER<0.10",
        "stations copied with a character error rate under 0.10",
    ),
    (
        "spot SNR",
        "average SNR manta reported on its correct spots, in 500 Hz; - when none",
    ),
];
const SNR_NOTE: &str =
    "SNR: transmitted carrier against noise in 500 Hz, as RBN and CW Skimmer quote it";
const SCENE_NOTE: [&str; 3] = [
    "Each station sends \"CQ CQ DE <call> <call> K\" for the whole recording. AWGN is a",
    "steady signal in white noise; Watterson good and poor add two-path HF fading",
    "(0.5 ms / 0.1 Hz and 2 ms / 1 Hz) to each station independently.",
];

// Help text below is operator-facing copy (clap republishes `///`); keep
// provenance in `//` comments only.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct SensitivityArgs {
    /// Channel conditions to sweep, comma-separated: awgn (a steady signal in
    /// white noise), good and poor (two-path Watterson HF fading: 0.5 ms delay
    /// and 0.1 Hz Doppler spread for good, 2 ms and 1 Hz for poor)
    #[arg(long, value_name = "LIST", default_value = DEFAULT_CONDITIONS,
          value_parser = parse_conditions, allow_hyphen_values = true)]
    pub conditions: Conditions,
    /// Sending speeds to sweep, in words per minute, comma-separated. Whole
    /// numbers from 5 to 60
    #[arg(long, value_name = "LIST", default_value = DEFAULT_WPM,
          value_parser = parse_wpm_list, allow_hyphen_values = true)]
    pub wpm: WpmList,
    /// SNRs to sweep, in dB in a 500 Hz bandwidth, comma-separated. From -30
    /// to 60, at most one decimal place
    // D9: one comma-separated value through a newtype parser, because clap's
    // value_delimiter rejects `--snr-db -10,20` as an unknown `-1` flag.
    #[arg(long, value_name = "LIST", default_value = DEFAULT_SNR_DB,
          value_parser = parse_snr_list, allow_hyphen_values = true)]
    pub snr_db: SnrList,
    /// Length of each recording. Longer recordings give each station more
    /// chances to be spotted and take proportionally more time and memory.
    /// Whole seconds from 10 to 300
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_DURATION_S,
          value_parser = parse_duration)]
    pub duration_s: u32,
    /// Repeat every point this many times with fresh noise, fading and
    /// callsigns, and pool the results. From 1 to 20
    #[arg(long, value_name = "N", default_value_t = DEFAULT_TRIALS, value_parser = parse_trials)]
    pub trials: u32,
    /// Starting value for all generated noise and fading. Change it to see how
    /// much a result depends on one particular noise sample
    #[arg(long, value_name = "N", default_value_t = DEFAULT_SEED)]
    pub seed: u64,
    /// Decode engine: `legacy` (the default), `edge-legacy`, or `hsmm`.
    ///
    /// When --config also names an engine, this flag wins.
    #[arg(long, value_parser = crate::parse_engine)]
    pub engine: Option<Engine>,
    /// TOML config file. Its [decode] and [detector] settings apply; every
    /// other table is ignored, and the environment is never read
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// How many recordings to generate and decode at once. Each job needs
    /// about 150 MB of memory per minute of --duration-s. Defaults to the
    /// number of CPUs available
    // D7/D8: never part of the result, so never in `Regenerate:`.
    #[arg(long, value_name = "N", value_parser = parse_jobs)]
    pub jobs: Option<usize>,
    /// Print the full result, including every station, as one JSON object
    #[arg(long, help_heading = "Output", conflicts_with = "markdown")]
    pub json: bool,
    /// Print the table as Markdown, for a release note or a web page
    #[arg(long, help_heading = "Output")]
    pub markdown: bool,
}

/// `--conditions`, in canonical order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Conditions(pub Vec<Condition>);
/// `--wpm`, ascending.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WpmList(pub Vec<u32>);
/// `--snr-db` (500 Hz), ascending.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SnrList(pub Vec<f64>);

/// Splits one comma-separated flag value, rejecting an empty list or item.
fn split_list(s: &str) -> std::result::Result<Vec<&str>, String> {
    let items: Vec<&str> = s.split(',').map(str::trim).collect();
    if items.iter().any(|i| i.is_empty()) {
        return Err("the list has an empty item".to_string());
    }
    Ok(items)
}

/// Rejects the second occurrence of any value, naming it as typed.
fn reject_duplicates<T: PartialEq>(
    values: &[T],
    show: impl Fn(&T) -> String,
) -> std::result::Result<(), String> {
    for (i, v) in values.iter().enumerate() {
        if values[..i].contains(v) {
            return Err(format!("{} is listed twice", show(v)));
        }
    }
    Ok(())
}

fn parse_whole(s: &str) -> std::result::Result<u32, String> {
    s.parse::<u32>().map_err(|_| {
        if s.parse::<f64>().is_ok() {
            format!("{s} is not a whole number")
        } else {
            format!("`{s}` is not a number")
        }
    })
}

fn parse_conditions(s: &str) -> std::result::Result<Conditions, String> {
    let mut v = split_list(s)?
        .into_iter()
        .map(str::parse::<Condition>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    reject_duplicates(&v, |c| c.name().to_string())?;
    v.sort();
    Ok(Conditions(v))
}

fn parse_wpm_list(s: &str) -> std::result::Result<WpmList, String> {
    let mut v = Vec::new();
    for item in split_list(s)? {
        let w = parse_whole(item)?;
        if !(WPM_RANGE.0..=WPM_RANGE.1).contains(&w) {
            return Err(format!(
                "{w} is outside {} to {} WPM",
                WPM_RANGE.0, WPM_RANGE.1
            ));
        }
        v.push(w);
    }
    reject_duplicates(&v, |w| w.to_string())?;
    v.sort_unstable();
    Ok(WpmList(v))
}

fn parse_snr_list(s: &str) -> std::result::Result<SnrList, String> {
    let mut v = Vec::new();
    for item in split_list(s)? {
        let x: f64 = item
            .parse()
            .ok()
            .filter(|x: &f64| x.is_finite())
            .ok_or_else(|| format!("`{item}` is not a number"))?;
        let tenths = x * 10.0;
        if (tenths - tenths.round()).abs() > 1e-9 {
            return Err(format!("{item} has more than one decimal place"));
        }
        if !(SNR_RANGE_DB.0..=SNR_RANGE_DB.1).contains(&x) {
            return Err(format!(
                "{} is outside {} to {} dB",
                fmt_db(x),
                fmt_db(SNR_RANGE_DB.0),
                fmt_db(SNR_RANGE_DB.1)
            ));
        }
        // Canonical value: exact tenths, and `-0` is `0`.
        v.push(tenths.round() / 10.0 + 0.0);
    }
    reject_duplicates(&v, |x| fmt_db(*x))?;
    v.sort_by(f64::total_cmp);
    Ok(SnrList(v))
}

fn parse_duration(s: &str) -> std::result::Result<u32, String> {
    let d = parse_whole(s)?;
    if !(DURATION_RANGE_S.0..=DURATION_RANGE_S.1).contains(&d) {
        return Err(format!(
            "{d} is outside {} to {} s",
            DURATION_RANGE_S.0, DURATION_RANGE_S.1
        ));
    }
    Ok(d)
}

fn parse_trials(s: &str) -> std::result::Result<u32, String> {
    let t = parse_whole(s)?;
    if !(TRIALS_RANGE.0..=TRIALS_RANGE.1).contains(&t) {
        return Err(format!(
            "{t} is outside {} to {}",
            TRIALS_RANGE.0, TRIALS_RANGE.1
        ));
    }
    Ok(t)
}

fn parse_jobs(s: &str) -> std::result::Result<usize, String> {
    let j = parse_whole(s)?;
    if j < 1 {
        return Err("must be at least 1".to_string());
    }
    Ok(j as usize)
}

/// The grid a run sweeps.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SweepPlan {
    pub conditions: Vec<Condition>,
    pub wpm: Vec<u32>,
    pub snr_db: Vec<f64>,
    pub duration_s: u32,
    pub trials: u32,
    pub seed: u64,
}

impl SweepPlan {
    fn from_args(args: &SensitivityArgs) -> Self {
        SweepPlan {
            conditions: args.conditions.0.clone(),
            wpm: args.wpm.0.clone(),
            snr_db: args.snr_db.0.clone(),
            duration_s: args.duration_s,
            trials: args.trials,
            seed: args.seed,
        }
    }

    fn points(&self) -> usize {
        self.conditions.len() * self.wpm.len() * self.snr_db.len()
    }
}

/// What the decoder settings came from, for the header line.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    pub engine: Engine,
    /// The `--config` path as typed, if any.
    pub config: Option<String>,
}

/// Everything the renderers print besides the measured points.
#[derive(Clone, Debug)]
pub(crate) struct Header {
    pub settings: Settings,
    /// The `Regenerate:` command line (D8).
    pub command: String,
}

/// One transmitting station of one trial.
#[derive(Clone, Debug)]
pub(crate) struct Station {
    pub trial: u32,
    pub call: String,
    pub offset_hz: f64,
    pub freq_hz: f64,
    /// What the keyer actually sent over the whole recording.
    pub keyed_text: String,
}

/// One station's score at one point (D6). Serialized as-is into `--json`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct StationScore {
    pub trial: u32,
    pub call: String,
    pub offset_hz: f64,
    pub spotted: bool,
    /// Correct spots: the station's call within `MATCH_HZ` of its frequency.
    pub spots: usize,
    pub first_spot_s: Option<f64>,
    /// Mean SNR of the correct spots, in 500 Hz.
    pub spot_snr_db: Option<f64>,
    /// Capped at 1.
    pub cer: f64,
}

/// One decode's score.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PointScore {
    pub stations: Vec<StationScore>,
    pub bogus_calls: BTreeSet<String>,
}

/// One sweep point, pooled over every trial.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PointResult {
    pub condition: Condition,
    pub wpm: u32,
    /// 500 Hz.
    pub snr_db: f64,
    /// Every trial's stations, trial by trial.
    pub stations: Vec<StationScore>,
    /// Each trial's distinct bogus calls, pooled and sorted.
    pub bogus_calls: Vec<String>,
}

impl PointResult {
    fn new(condition: Condition, wpm: u32, snr_db: f64) -> Self {
        PointResult {
            condition,
            wpm,
            snr_db,
            stations: Vec::new(),
            bogus_calls: Vec::new(),
        }
    }

    /// Pools one trial's score into this point.
    pub fn add(&mut self, score: PointScore) {
        self.stations.extend(score.stations);
        self.bogus_calls.extend(score.bogus_calls);
        self.bogus_calls.sort();
    }

    pub fn spotted(&self) -> usize {
        self.stations.iter().filter(|s| s.spotted).count()
    }

    pub fn cer_mean(&self) -> f64 {
        mean(self.stations.iter().map(|s| s.cer)).unwrap_or(1.0)
    }

    pub fn good_copy(&self) -> usize {
        self.stations
            .iter()
            .filter(|s| s.cer < GOOD_COPY_CER)
            .count()
    }

    /// Mean over spotted stations of each one's mean spot SNR (500 Hz).
    pub fn spot_snr_db(&self) -> Option<f64> {
        mean(self.stations.iter().filter_map(|s| s.spot_snr_db))
    }

    fn snr_2500_db(&self) -> f64 {
        self.snr_db - RBN_REF_BW_CORRECTION_DB as f64
    }
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    (n > 0).then(|| sum / n as f64)
}

/// A whole sweep, points in canonical order (condition, speed, SNR).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SweepResult {
    pub plan: SweepPlan,
    pub points: Vec<PointResult>,
}

/// One finished decode, for the stderr progress line.
#[derive(Clone, Debug)]
pub(crate) struct Progress {
    pub index: usize,
    pub total: usize,
    pub condition: Condition,
    pub wpm: u32,
    pub snr_db: f64,
    pub trial: u32,
    pub trials: u32,
    pub spotted: usize,
    pub stations: usize,
    pub cer_mean: f64,
}

/// `manta bench sensitivity`: load the config, sweep, print.
pub(crate) fn sensitivity(args: SensitivityArgs) -> Result<()> {
    let (cfg, settings, note) = pipeline_config(&args)?;
    if let Some(note) = note {
        eprintln!("{note}");
    }
    let plan = SweepPlan::from_args(&args);
    let jobs = args.jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    eprintln!("{}", start_line(&plan, jobs));
    let started = std::time::Instant::now();
    let result = run_sweep(&plan, &cfg, jobs, |p| eprintln!("{}", progress_line(p)))?;
    let header = Header {
        settings,
        command: regenerate(&args),
    };
    let out = if args.json {
        format!("{}\n", render_json(&result, &header)?)
    } else if args.markdown {
        render_markdown(&result, &header)
    } else {
        render_table(&result, &header)
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    stdout.flush()?;
    eprintln!("done in {} s", started.elapsed().as_secs());
    Ok(())
}

/// D10: `[decode]` and `[detector]` from `--config` (never the environment),
/// `--engine` over the file's engine. Returns the note naming any other
/// table present, rather than printing it, so tests can see it.
pub(crate) fn pipeline_config(
    args: &SensitivityArgs,
) -> Result<(PipelineConfig, Settings, Option<String>)> {
    let loaded = crate::config::load(args.config.as_deref(), crate::config::Env::Ignore)?;
    let note = crate::ignored_tables_note(&loaded, "bench sensitivity", &["decode", "detector"]);
    let decode = crate::merge_cli_engine(args.engine, loaded.decode.clone());
    let settings = Settings {
        engine: decode.engine,
        config: args.config.as_ref().map(|p| p.display().to_string()),
    };
    let cfg = PipelineConfig {
        decode,
        detector: loaded.detector,
        ..PipelineConfig::default()
    };
    Ok((cfg, settings, note))
}

/// D4/D7: render each series once, scale it per SNR, decode in chunks of
/// `jobs`. Summation and collection order never depend on `jobs`.
pub(crate) fn run_sweep(
    plan: &SweepPlan,
    cfg: &PipelineConfig,
    jobs: usize,
    mut progress: impl FnMut(&Progress),
) -> Result<SweepResult> {
    let jobs = jobs.max(1);
    let duration_s = plan.duration_s as f64;
    let mut points: Vec<PointResult> = Vec::with_capacity(plan.points());
    for &condition in &plan.conditions {
        for &wpm in &plan.wpm {
            for &snr_db in &plan.snr_db {
                points.push(PointResult::new(condition, wpm, snr_db));
            }
        }
    }
    let total = plan.points() * plan.trials as usize;
    let mut done = 0usize;
    for trial in 0..plan.trials {
        let noise = sensitivity::render_noise(duration_s, plan.seed, trial)
            .with_context(|| format!("generating noise (trial {})", trial + 1))?;
        let calls = sensitivity::series_calls(trial);
        let mut point_index = 0usize;
        for &condition in &plan.conditions {
            for &wpm in &plan.wpm {
                let series = sensitivity::series_signals(condition, wpm as f32, trial, plan.seed);
                let (clean, texts) = sensitivity::render_series_chunked(&series, duration_s, jobs)
                    .with_context(|| {
                        format!(
                            "generating {}, {wpm} WPM (trial {})",
                            condition.label(),
                            trial + 1
                        )
                    })?;
                let stations: Vec<Station> = calls
                    .iter()
                    .zip(texts)
                    .enumerate()
                    .map(|(j, (call, keyed_text))| {
                        let offset_hz = sensitivity::station_offset_hz(j);
                        Station {
                            trial,
                            call: call.clone(),
                            offset_hz,
                            freq_hz: sensitivity::CENTER_FREQ_HZ + offset_hz,
                            keyed_text,
                        }
                    })
                    .collect();
                for chunk in plan.snr_db.chunks(jobs) {
                    let (clean, noise, stations) = (&clean, &noise, &stations);
                    let scored: Vec<Result<PointScore>> = std::thread::scope(|s| {
                        let handles: Vec<_> = chunk
                            .iter()
                            .map(|&snr_db| {
                                s.spawn(move || {
                                    let gain = sensitivity::gain_for_snr_2500(
                                        snr_db - RBN_REF_BW_CORRECTION_DB as f64,
                                    );
                                    let iq = sensitivity::compose(clean, noise, gain);
                                    let report = decode_point(&iq, cfg)?;
                                    Ok(score_point(stations, report.as_ref()))
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .map(|h| {
                                h.join()
                                    .unwrap_or_else(|_| Err(anyhow!("the decode thread panicked")))
                            })
                            .collect()
                    });
                    for (&snr_db, score) in chunk.iter().zip(scored) {
                        let score = score.with_context(|| {
                            format!(
                                "decoding {}, {wpm} WPM, {} dB (trial {})",
                                condition.label(),
                                fmt_db(snr_db),
                                trial + 1
                            )
                        })?;
                        done += 1;
                        progress(&Progress {
                            index: done,
                            total,
                            condition,
                            wpm,
                            snr_db,
                            trial,
                            trials: plan.trials,
                            spotted: score.stations.iter().filter(|s| s.spotted).count(),
                            stations: score.stations.len(),
                            cer_mean: mean(score.stations.iter().map(|s| s.cer)).unwrap_or(1.0),
                        });
                        points[point_index].add(score);
                        point_index += 1;
                    }
                }
            }
        }
    }
    Ok(SweepResult {
        plan: plan.clone(),
        points,
    })
}

/// D6: `Ok(None)` when the engine finds no signal at all (an empty decode,
/// scored as zero recall), `Err` for any other decode failure.
pub(crate) fn decode_point(iq: &[Complex32], cfg: &PipelineConfig) -> Result<Option<DecodeReport>> {
    match decode_samples(
        iq,
        sensitivity::SAMPLE_RATE_HZ,
        sensitivity::CENTER_FREQ_HZ,
        cfg,
    ) {
        Ok(report) => Ok(Some(report)),
        Err(e) if e.to_string().starts_with("no signal found") => Ok(None),
        Err(e) => Err(e),
    }
}

fn event_track_id(e: &DecoderEvent) -> u32 {
    match e {
        DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. }
        | DecoderEvent::TrackPromoted { track_id, .. }
        | DecoderEvent::TrackClosed { track_id, .. } => *track_id,
    }
}

/// D6: score one decode against the stations that were sent.
pub(crate) fn score_point(stations: &[Station], report: Option<&DecodeReport>) -> PointScore {
    let Some(report) = report else {
        return PointScore {
            stations: stations
                .iter()
                .map(|st| StationScore {
                    trial: st.trial,
                    call: st.call.clone(),
                    offset_hz: st.offset_hz,
                    spotted: false,
                    spots: 0,
                    first_spot_s: None,
                    spot_snr_db: None,
                    cer: 1.0,
                })
                .collect(),
            bogus_calls: BTreeSet::new(),
        };
    };

    // A track's frequency: its last TrackMeta, else its TrackPromoted.
    let mut meta_freq: BTreeMap<u32, f64> = BTreeMap::new();
    let mut promoted_freq: BTreeMap<u32, f64> = BTreeMap::new();
    let mut track_events: BTreeMap<u32, Vec<DecoderEvent>> = BTreeMap::new();
    for ev in &report.events {
        let id = event_track_id(ev);
        match ev {
            DecoderEvent::TrackMeta { freq_hz, .. } => {
                meta_freq.insert(id, *freq_hz);
            }
            DecoderEvent::TrackPromoted { freq_hz, .. } => {
                promoted_freq.insert(id, *freq_hz);
            }
            _ => {}
        }
        track_events.entry(id).or_default().push(ev.clone());
    }
    let tracks: Vec<(f64, String)> = track_events
        .iter()
        .filter_map(|(id, evs)| {
            let freq = meta_freq.get(id).or_else(|| promoted_freq.get(id))?;
            Some((*freq, events_to_text(evs)))
        })
        .collect();

    let is_correct = |st: &Station, call: &str, freq_hz: f64| {
        call == st.call && (freq_hz - st.freq_hz).abs() <= MATCH_HZ
    };
    let scores = stations
        .iter()
        .map(|st| {
            let copy: Vec<&str> = tracks
                .iter()
                .filter(|(f, text)| (f - st.freq_hz).abs() <= MATCH_HZ && !text.is_empty())
                .map(|(_, text)| text.as_str())
                .collect();
            let cer = manta_testkit::cer::cer(&st.keyed_text, &copy.join(" ")).min(1.0);
            let correct: Vec<_> = report
                .spots
                .iter()
                .filter(|sp| is_correct(st, &sp.callsign, sp.freq_hz))
                .collect();
            StationScore {
                trial: st.trial,
                call: st.call.clone(),
                offset_hz: st.offset_hz,
                spotted: !correct.is_empty(),
                spots: correct.len(),
                first_spot_s: correct
                    .iter()
                    .map(|sp| sp.sample_ts)
                    .min()
                    .map(|ts| ts as f64 / sensitivity::SAMPLE_RATE_HZ),
                spot_snr_db: mean(
                    correct
                        .iter()
                        .map(|sp| sp.snr_db as f64 + RBN_REF_BW_CORRECTION_DB as f64),
                ),
                cer,
            }
        })
        .collect();
    let bogus_calls = report
        .spots
        .iter()
        .filter(|sp| {
            !stations
                .iter()
                .any(|st| is_correct(st, &sp.callsign, sp.freq_hz))
        })
        .map(|sp| sp.callsign.clone())
        .collect();
    PointScore {
        stations: scores,
        bogus_calls,
    }
}

/// Whole dB as an integer, anything else with one decimal.
fn fmt_db(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{}", x as i64)
    } else {
        format!("{x:.1}")
    }
}

/// A measured SNR, rounded to whole dB (`-0` prints as `0`), or `-`.
fn fmt_spot_snr(x: Option<f64>) -> String {
    match x {
        Some(v) => format!("{} dB", v.round() as i64),
        None => "-".to_string(),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Legacy => "legacy",
        Engine::EdgeLegacy => "edge-legacy",
        Engine::Hsmm => "hsmm",
    }
}

/// The stderr line printed before the sweep starts.
fn start_line(plan: &SweepPlan, jobs: usize) -> String {
    let trials = if plan.trials > 1 {
        format!(" x {}", plural(plan.trials as usize, "trial", "trials"))
    } else {
        String::new()
    };
    format!(
        "sweeping {} ({} x {} x {}){trials}, {} stations each, {} s recordings, {}",
        plural(plan.points(), "point", "points"),
        plural(plan.conditions.len(), "condition", "conditions"),
        plural(plan.wpm.len(), "speed", "speeds"),
        plural(plan.snr_db.len(), "SNR", "SNRs"),
        sensitivity::STATIONS_PER_RECORDING,
        plan.duration_s,
        plural(jobs, "job", "jobs"),
    )
}

/// One stderr progress line.
fn progress_line(p: &Progress) -> String {
    let width = p.total.to_string().len();
    let trial = if p.trials > 1 {
        format!(" (trial {} of {})", p.trial + 1, p.trials)
    } else {
        String::new()
    };
    format!(
        "[{:>width$}/{}] {}, {} WPM, {} dB: recall {}/{}, CER {:.2}{trial}",
        p.index,
        p.total,
        p.condition.label(),
        p.wpm,
        fmt_db(p.snr_db),
        p.spotted,
        p.stations,
        p.cer_mean,
    )
}

/// The decoder-settings phrase of the header.
fn settings_phrase(settings: &Settings) -> String {
    let source = match &settings.config {
        Some(path) => format!("decoder settings from {path}"),
        None => "default decoder settings".to_string(),
    };
    format!("{} engine, {source}", engine_name(settings.engine))
}

fn header_facts(plan: &SweepPlan) -> [String; 4] {
    [
        format!(
            "{} stations per point",
            sensitivity::STATIONS_PER_RECORDING * plan.trials as usize
        ),
        format!("{} s recordings", plan.duration_s),
        plural(plan.trials as usize, "trial", "trials"),
        format!("seed {}", plan.seed),
    ]
}

/// The table's cells for one point, in column order.
fn row_cells(p: &PointResult) -> [String; 8] {
    let n = p.stations.len();
    [
        p.condition.label().to_string(),
        format!("{} WPM", p.wpm),
        format!("{} dB", fmt_db(p.snr_db)),
        format!("{}/{n}", p.spotted()),
        p.bogus_calls.len().to_string(),
        format!("{:.2}", p.cer_mean()),
        format!("{}/{n}", p.good_copy()),
        fmt_spot_snr(p.spot_snr_db()),
    ]
}

const COLUMNS: [&str; 8] = [
    "condition",
    "speed",
    "SNR",
    "recall",
    "bogus",
    "CER",
    "CER<0.10",
    "spot SNR",
];

fn text_row(c: &[String; 8]) -> String {
    format!(
        "{:<15} {:>6}  {:>6}  {:>6}  {:>5}  {:>4}  {:>8}  {:>8}",
        c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]
    )
}

/// Design §3: the default stdout table.
pub(crate) fn render_table(result: &SweepResult, header: &Header) -> String {
    let mut out = String::new();
    let mut line = |s: &str| {
        out.push_str(s);
        out.push('\n');
    };
    line(&format!(
        "manta {} sensitivity sweep: {}",
        env!("CARGO_PKG_VERSION"),
        settings_phrase(&header.settings)
    ));
    line(&header_facts(&result.plan).join(", "));
    line(SNR_NOTE);
    line("");
    line(&text_row(&COLUMNS.map(String::from)));
    for p in &result.points {
        line(&text_row(&row_cells(p)));
    }
    line("");
    for (name, text) in LEGEND {
        line(&format!("{name:<9} {text}"));
    }
    for l in SCENE_NOTE {
        line(l);
    }
    line(&format!("Regenerate: {}", header.command));
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Design §5: the same rows and legend as a Markdown table.
pub(crate) fn render_markdown(result: &SweepResult, header: &Header) -> String {
    let mut out = String::new();
    let mut line = |s: &str| {
        out.push_str(s);
        out.push('\n');
    };
    line(&format!(
        "**manta {} sensitivity sweep**: {} · {}",
        env!("CARGO_PKG_VERSION"),
        settings_phrase(&header.settings),
        header_facts(&result.plan).join(" · ")
    ));
    line("");
    line(&format!("{SNR_NOTE}."));
    line("");
    line(&format!("| {} |", COLUMNS.map(capitalize).join(" | ")));
    line("|---|---:|---:|---:|---:|---:|---:|---:|");
    for p in &result.points {
        line(&format!("| {} |", row_cells(p).join(" | ")));
    }
    line("");
    for (name, text) in LEGEND {
        line(&format!("- **{}**: {text}.", capitalize(name)));
    }
    line("");
    line(
        &SCENE_NOTE
            .join(" ")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
    );
    line("");
    line(&format!("Regenerate: `{}`", header.command));
    out
}

#[derive(serde::Serialize)]
struct JsonReport<'a> {
    report: &'static str,
    format_version: u32,
    manta_version: &'static str,
    engine: &'static str,
    config_file: Option<&'a str>,
    command: &'a str,
    snr_ref_hz: u32,
    sample_rate_hz: u32,
    center_freq_hz: u64,
    stations_per_trial: usize,
    duration_s: u32,
    trials: u32,
    seed: u64,
    payload: &'static str,
    match_hz: u32,
    points: Vec<JsonPoint<'a>>,
}

#[derive(serde::Serialize)]
struct JsonPoint<'a> {
    condition: &'static str,
    wpm: u32,
    snr_db: f64,
    snr_2500_db: f64,
    stations: usize,
    spotted: usize,
    bogus_calls: &'a [String],
    cer_mean: f64,
    good_copy: usize,
    spot_snr_db: Option<f64>,
    per_station: &'a [StationScore],
}

/// Design §6: the full result as one line of JSON.
pub(crate) fn render_json(result: &SweepResult, header: &Header) -> Result<String> {
    let plan = &result.plan;
    let report = JsonReport {
        report: COMMAND,
        format_version: FORMAT_VERSION,
        manta_version: env!("CARGO_PKG_VERSION"),
        engine: engine_name(header.settings.engine),
        config_file: header.settings.config.as_deref(),
        command: &header.command,
        snr_ref_hz: 500,
        sample_rate_hz: sensitivity::SAMPLE_RATE_HZ as u32,
        center_freq_hz: sensitivity::CENTER_FREQ_HZ as u64,
        stations_per_trial: sensitivity::STATIONS_PER_RECORDING,
        duration_s: plan.duration_s,
        trials: plan.trials,
        seed: plan.seed,
        payload: sensitivity::PAYLOAD_TEMPLATE,
        match_hz: MATCH_HZ as u32,
        points: result
            .points
            .iter()
            .map(|p| JsonPoint {
                condition: p.condition.name(),
                wpm: p.wpm,
                snr_db: p.snr_db,
                snr_2500_db: p.snr_2500_db(),
                stations: p.stations.len(),
                spotted: p.spotted(),
                bogus_calls: &p.bogus_calls,
                cer_mean: p.cer_mean(),
                good_copy: p.good_copy(),
                spot_snr_db: p.spot_snr_db(),
                per_station: &p.stations,
            })
            .collect(),
    };
    Ok(serde_json::to_string(&report)?)
}

/// Quotes a path for a shell command line when it needs it.
fn shell_word(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=+,@%".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// D8: the command that regenerates this result: only flags that differ
/// from their defaults, in a fixed order, lists canonical. `--jobs` never
/// appears because it cannot change the result.
pub(crate) fn regenerate(args: &SensitivityArgs) -> String {
    let mut parts = vec![COMMAND.to_string()];
    let defaults = |list: &str| list.to_string();
    let conditions: Vec<&str> = args.conditions.0.iter().map(|c| c.name()).collect();
    if conditions.join(",") != defaults(DEFAULT_CONDITIONS) {
        parts.push(format!("--conditions {}", conditions.join(",")));
    }
    let wpm: Vec<String> = args.wpm.0.iter().map(u32::to_string).collect();
    if wpm.join(",") != defaults(DEFAULT_WPM) {
        parts.push(format!("--wpm {}", wpm.join(",")));
    }
    let snr: Vec<String> = args.snr_db.0.iter().map(|x| fmt_db(*x)).collect();
    if snr.join(",") != defaults(DEFAULT_SNR_DB) {
        parts.push(format!("--snr-db {}", snr.join(",")));
    }
    if args.duration_s != DEFAULT_DURATION_S {
        parts.push(format!("--duration-s {}", args.duration_s));
    }
    if args.trials != DEFAULT_TRIALS {
        parts.push(format!("--trials {}", args.trials));
    }
    if args.seed != DEFAULT_SEED {
        parts.push(format!("--seed {}", args.seed));
    }
    if let Some(engine) = args.engine {
        parts.push(format!("--engine {}", engine_name(engine)));
    }
    if let Some(path) = &args.config {
        parts.push(format!(
            "--config {}",
            shell_word(&path.display().to_string())
        ));
    }
    if args.json {
        parts.push("--json".to_string());
    }
    if args.markdown {
        parts.push("--markdown".to_string());
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use manta_decode::tree::Glyph;
    use manta_engine::{Spot, SpotType};

    fn parse(args: &[&str]) -> std::result::Result<SensitivityArgs, clap::Error> {
        let argv = ["manta", "bench", "sensitivity"]
            .iter()
            .chain(args)
            .copied();
        match crate::Cli::try_parse_from(argv)?.command {
            crate::Command::Bench(crate::BenchCommand::Sensitivity(a)) => Ok(a),
            _ => unreachable!("parsed as another command"),
        }
    }

    #[test]
    fn defaults_are_the_documented_grid() {
        let a = parse(&[]).unwrap();
        assert_eq!(a.conditions.0, Condition::ALL.to_vec());
        assert_eq!(a.wpm.0, vec![15, 25, 35]);
        assert_eq!(
            a.snr_db.0,
            vec![0.0, 3.0, 6.0, 9.0, 12.0, 15.0, 20.0, 25.0, 30.0]
        );
        assert_eq!((a.duration_s, a.trials, a.seed), (60, 1, 1));
        assert_eq!(
            (a.engine, a.jobs, a.json, a.markdown),
            (None, None, false, false)
        );
    }

    #[test]
    fn lists_accept_a_leading_negative_and_are_put_in_canonical_order() {
        let a = parse(&[
            "--snr-db",
            "-10,20,-3",
            "--wpm",
            "35,15",
            "--conditions",
            "poor,awgn",
        ])
        .unwrap();
        assert_eq!(a.snr_db.0, vec![-10.0, -3.0, 20.0]);
        assert_eq!(a.wpm.0, vec![15, 35]);
        assert_eq!(
            a.conditions.0,
            vec![Condition::Awgn, Condition::WattersonPoor]
        );
        assert!(parse(&["--snr-db=-10,20"]).is_ok());
        assert!(parse(&["--snr-db", "-10,20", "--json"]).unwrap().json);
        assert_eq!(
            parse(&["--snr-db", "4.5,-0"]).unwrap().snr_db.0,
            vec![0.0, 4.5]
        );
    }

    #[test]
    fn out_of_range_duplicate_and_malformed_values_are_usage_errors() {
        for (args, needle) in [
            (&["--wpm", "0"][..], "0 is outside 5 to 60 WPM"),
            (&["--wpm", "25.5"][..], "is not a whole number"),
            (&["--snr-db", "6,3,6"][..], "6 is listed twice"),
            (&["--snr-db", "4.25"][..], "more than one decimal place"),
            (&["--snr-db", "61"][..], "outside -30 to 60 dB"),
            (&["--snr-db", "3,x"][..], "`x` is not a number"),
            (&["--snr-db", "3,,6"][..], "empty item"),
            (
                &["--conditions", "moderate"][..],
                "unknown condition `moderate` (awgn, good or poor)",
            ),
            (&["--conditions", "awgn,awgn"][..], "awgn is listed twice"),
            (&["--duration-s", "5"][..], "outside 10 to 300 s"),
            (&["--trials", "21"][..], "outside 1 to 20"),
            (&["--jobs", "0"][..], "must be at least 1"),
        ] {
            let e = parse(args).unwrap_err();
            assert_eq!(
                e.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{args:?}"
            );
            assert!(e.to_string().contains(needle), "{args:?}: {e}");
        }
    }

    #[test]
    fn json_and_markdown_conflict() {
        let e = parse(&["--json", "--markdown"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    // Two expected stations: W2ABC at 14_000_000 - 36_000, K1XYZ at
    // 14_000_000 - 27_990.625.
    fn stations() -> Vec<Station> {
        ["W2ABC", "K1XYZ"]
            .iter()
            .enumerate()
            .map(|(j, call)| {
                let offset_hz = sensitivity::station_offset_hz(j);
                Station {
                    trial: 0,
                    call: call.to_string(),
                    offset_hz,
                    freq_hz: sensitivity::CENTER_FREQ_HZ + offset_hz,
                    keyed_text: format!("CQ CQ DE {call} {call} K"),
                }
            })
            .collect()
    }

    fn spot(call: &str, freq_hz: f64, snr_db: f32, sample_ts: u64) -> Spot {
        Spot {
            callsign: call.to_string(),
            freq_hz,
            snr_db,
            wpm: 25.0,
            spot_type: SpotType::Cq,
            confidence: 1.0,
            track_id: 0,
            sample_ts,
        }
    }

    fn text_events(track_id: u32, text: &str) -> Vec<DecoderEvent> {
        let mut evs = Vec::new();
        for (i, word) in text.split(' ').enumerate() {
            if i > 0 {
                evs.push(DecoderEvent::word_boundary(track_id, 0));
            }
            for c in word.chars() {
                evs.push(DecoderEvent::char_decoded(track_id, 0, Glyph::Char(c), 1.0));
            }
        }
        evs
    }

    fn meta(track_id: u32, freq_hz: f64) -> DecoderEvent {
        DecoderEvent::TrackMeta {
            track_id,
            sample_ts: 0,
            snr_2500_db: 10.0,
            freq_hz,
        }
    }

    fn report(events: Vec<DecoderEvent>, spots: Vec<Spot>) -> DecodeReport {
        DecodeReport {
            freq_hz: 0.0,
            wpm: None,
            text: String::new(),
            events,
            spots,
        }
    }

    fn f(j: usize) -> f64 {
        sensitivity::CENTER_FREQ_HZ + sensitivity::station_offset_hz(j)
    }

    #[test]
    fn a_station_counts_as_spotted_only_with_its_call_within_150_hz() {
        let st = stations();
        let s = score_point(
            &st,
            Some(&report(vec![], vec![spot("W2ABC", f(0) + 100.0, 10.0, 0)])),
        );
        assert!(s.stations[0].spotted && !s.stations[1].spotted);
        assert!(s.bogus_calls.is_empty());
        let s = score_point(
            &st,
            Some(&report(vec![], vec![spot("W2ABC", f(0) + 500.0, 10.0, 0)])),
        );
        assert!(!s.stations[0].spotted);
        assert_eq!(s.bogus_calls.iter().collect::<Vec<_>>(), ["W2ABC"]);
    }

    #[test]
    fn bogus_calls_are_distinct_sorted_and_exclude_correct_spots() {
        let s = score_point(
            &stations(),
            Some(&report(
                vec![],
                vec![
                    spot("N0PE", f(0), 1.0, 0),
                    spot("N0PE", f(1), 1.0, 0),
                    spot("AA0AA", f(1), 1.0, 0),
                    spot("K1XYZ", f(1), 1.0, 0),
                ],
            )),
        );
        assert_eq!(
            s.bogus_calls.into_iter().collect::<Vec<_>>(),
            ["AA0AA", "N0PE"]
        );
        assert!(s.stations[1].spotted);
    }

    #[test]
    fn copy_joins_every_track_within_150_hz_in_track_order() {
        let mut events = vec![
            meta(5, f(0) - 30.0),
            meta(3, f(0) + 20.0),
            meta(4, f(0) + 5000.0),
        ];
        events.extend(text_events(5, "W2ABC W2ABC K"));
        events.extend(text_events(4, "TEST"));
        events.extend(text_events(3, "CQ CQ DE"));
        let s = score_point(&stations(), Some(&report(events, vec![])));
        // "CQ CQ DE" + "W2ABC W2ABC K" == the keyed text exactly.
        assert_eq!(s.stations[0].cer, 0.0);
        // Reversed order would not be a perfect copy.
        assert!(manta_testkit::cer::cer("CQ CQ DE W2ABC W2ABC K", "W2ABC W2ABC K CQ CQ DE") > 0.0);
        // The far track is nobody's copy.
        assert_eq!(s.stations[1].cer, 1.0);
    }

    #[test]
    fn a_track_with_only_a_promotion_event_uses_the_promotion_frequency() {
        let mut events = vec![DecoderEvent::TrackPromoted {
            track_id: 7,
            sample_ts: 0,
            freq_hz: f(1) + 40.0,
        }];
        events.extend(text_events(7, "CQ CQ DE K1XYZ K1XYZ K"));
        // A track with neither frequency event is ignored.
        events.extend(text_events(8, "CQ CQ DE W2ABC W2ABC K"));
        let s = score_point(&stations(), Some(&report(events, vec![])));
        assert_eq!(s.stations[1].cer, 0.0);
        assert_eq!(s.stations[0].cer, 1.0);
    }

    #[test]
    fn cer_is_capped_at_one() {
        let mut events = vec![meta(1, f(0))];
        events.extend(text_events(1, &"EEEEEEEEEE ".repeat(20)));
        let s = score_point(&stations(), Some(&report(events, vec![])));
        assert_eq!(s.stations[0].cer, 1.0);
    }

    #[test]
    fn an_empty_decode_is_zero_recall_and_full_cer() {
        let s = score_point(&stations(), None);
        assert_eq!(s.stations.len(), 2);
        assert!(s
            .stations
            .iter()
            .all(|st| !st.spotted && st.cer == 1.0 && st.spots == 0 && st.spot_snr_db.is_none()));
        assert!(s.bogus_calls.is_empty());
    }

    #[test]
    fn spot_snr_is_the_station_weighted_mean_in_500_hz() {
        let s = score_point(
            &stations(),
            Some(&report(
                vec![],
                vec![
                    spot("W2ABC", f(0), 10.0, 96_000 * 3),
                    spot("W2ABC", f(0), 12.0, 96_000),
                    spot("K1XYZ", f(1), 20.0, 0),
                ],
            )),
        );
        let c = RBN_REF_BW_CORRECTION_DB as f64;
        assert_eq!(s.stations[0].spots, 2);
        assert_eq!(s.stations[0].first_spot_s, Some(1.0));
        assert!((s.stations[0].spot_snr_db.unwrap() - (11.0 + c)).abs() < 1e-9);
        let mut p = PointResult::new(Condition::Awgn, 25, 6.0);
        p.add(s);
        let want = ((11.0 + c) + (20.0 + c)) / 2.0;
        assert!((p.spot_snr_db().unwrap() - want).abs() < 1e-9);
    }

    fn station_score(trial: u32, spotted: bool, cer: f64) -> StationScore {
        StationScore {
            trial,
            call: "W2ABC".to_string(),
            offset_hz: 0.0,
            spotted,
            spots: usize::from(spotted),
            first_spot_s: spotted.then_some(1.0),
            spot_snr_db: spotted.then_some(10.0),
            cer,
        }
    }

    #[test]
    fn trials_pool_counts_and_average_cer_over_every_station() {
        let mut p = PointResult::new(Condition::Awgn, 25, 6.0);
        p.add(PointScore {
            stations: (0..10).map(|_| station_score(0, true, 0.05)).collect(),
            bogus_calls: BTreeSet::from(["N0PE".to_string()]),
        });
        p.add(PointScore {
            stations: (0..10).map(|_| station_score(1, false, 0.25)).collect(),
            bogus_calls: BTreeSet::from(["AA0AA".to_string()]),
        });
        assert_eq!(p.stations.len(), 20);
        assert_eq!(p.spotted(), 10);
        assert_eq!(p.good_copy(), 10);
        assert!((p.cer_mean() - 0.15).abs() < 1e-12);
        assert_eq!(p.bogus_calls, ["AA0AA", "N0PE"]);
    }

    #[test]
    fn no_signal_is_an_empty_decode_but_digital_silence_is_an_error() {
        let noise = sensitivity::render_noise(10.0, 1, 0).unwrap();
        assert!(decode_point(&noise, &PipelineConfig::default())
            .unwrap()
            .is_none());
        let zeros = vec![Complex32::new(0.0, 0.0); noise.len()];
        let e = decode_point(&zeros, &PipelineConfig::default()).unwrap_err();
        assert!(e.to_string().contains("digital silence"), "{e}");
    }

    #[test]
    fn sweep_result_does_not_depend_on_the_job_count() {
        let plan = SweepPlan {
            conditions: vec![Condition::Awgn],
            wpm: vec![25],
            snr_db: vec![-10.0, 20.0],
            duration_s: 12,
            trials: 1,
            seed: 1,
        };
        let cfg = PipelineConfig::default();
        let mut seen = Vec::new();
        let one = run_sweep(&plan, &cfg, 1, |p| seen.push(p.index)).unwrap();
        let three = run_sweep(&plan, &cfg, 3, |_| {}).unwrap();
        assert_eq!(one, three);
        assert_eq!(seen, [1, 2]);
        assert_eq!(one.points.len(), 2);
        assert!(one.points.iter().all(|p| p.stations.len() == 10));
    }

    fn hand_built() -> SweepResult {
        let plan = SweepPlan {
            conditions: vec![Condition::Awgn, Condition::WattersonGood],
            wpm: vec![25],
            snr_db: vec![4.5],
            duration_s: 30,
            trials: 1,
            seed: 1,
        };
        let mut a = PointResult::new(Condition::Awgn, 25, 4.5);
        a.add(PointScore {
            stations: vec![station_score(0, false, 1.0), station_score(0, false, 0.5)],
            bogus_calls: BTreeSet::new(),
        });
        let mut b = PointResult::new(Condition::WattersonGood, 25, 4.5);
        let mut s = station_score(0, true, 0.05);
        s.spot_snr_db = Some(-0.4);
        b.add(PointScore {
            stations: vec![s, station_score(0, false, 0.15)],
            bogus_calls: BTreeSet::from(["N0PE".to_string()]),
        });
        SweepResult {
            plan,
            points: vec![a, b],
        }
    }

    fn header(engine: Engine, config: Option<&str>) -> Header {
        Header {
            settings: Settings {
                engine,
                config: config.map(String::from),
            },
            command: "manta bench sensitivity --conditions awgn,good --wpm 25 --snr-db 4.5 --duration-s 30".to_string(),
        }
    }

    #[test]
    fn table_renders_header_rows_and_legend_exactly() {
        let got = render_table(&hand_built(), &header(Engine::Legacy, None));
        let want = format!(
            "\
manta {} sensitivity sweep: legacy engine, default decoder settings
10 stations per point, 30 s recordings, 1 trial, seed 1
SNR: transmitted carrier against noise in 500 Hz, as RBN and CW Skimmer quote it

condition        speed     SNR  recall  bogus   CER  CER<0.10  spot SNR
AWGN            25 WPM  4.5 dB     0/2      0  0.75       0/2         -
Watterson good  25 WPM  4.5 dB     1/2      1  0.10       1/2      0 dB

recall    stations spotted with the right call, within 150 Hz of where they sent
bogus     calls spotted that no station sent on that frequency
CER       character error rate of each station's copy, capped at 1, averaged
CER<0.10  stations copied with a character error rate under 0.10
spot SNR  average SNR manta reported on its correct spots, in 500 Hz; - when none
Each station sends \"CQ CQ DE <call> <call> K\" for the whole recording. AWGN is a
steady signal in white noise; Watterson good and poor add two-path HF fading
(0.5 ms / 0.1 Hz and 2 ms / 1 Hz) to each station independently.
Regenerate: manta bench sensitivity --conditions awgn,good --wpm 25 --snr-db 4.5 --duration-s 30
",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(got, want);
    }

    #[test]
    fn header_names_engine_settings_trials_and_seed() {
        let mut r = hand_built();
        let t = render_table(&r, &header(Engine::Hsmm, Some("cfg.toml")));
        assert!(
            t.contains("hsmm engine, decoder settings from cfg.toml"),
            "{t}"
        );
        let t = render_table(&r, &header(Engine::EdgeLegacy, None));
        assert!(
            t.contains("edge-legacy engine, default decoder settings"),
            "{t}"
        );
        r.plan.trials = 3;
        r.plan.seed = 7;
        let t = render_table(&r, &header(Engine::Legacy, None));
        assert!(
            t.contains("30 stations per point, 30 s recordings, 3 trials, seed 7"),
            "{t}"
        );
    }

    #[test]
    fn regenerate_line_lists_only_non_default_flags_canonically() {
        assert_eq!(regenerate(&parse(&[]).unwrap()), "manta bench sensitivity");
        assert_eq!(
            regenerate(
                &parse(&[
                    "--jobs",
                    "4",
                    "--wpm",
                    "25",
                    "--snr-db",
                    "20,-10",
                    "--conditions",
                    "awgn",
                    "--duration-s",
                    "30"
                ])
                .unwrap()
            ),
            "manta bench sensitivity --conditions awgn --wpm 25 --snr-db -10,20 --duration-s 30"
        );
        assert_eq!(
            regenerate(
                &parse(&[
                    "--markdown",
                    "--config",
                    "my cfg.toml",
                    "--engine",
                    "hsmm",
                    "--seed",
                    "9",
                    "--trials",
                    "3"
                ])
                .unwrap()
            ),
            "manta bench sensitivity --trials 3 --seed 9 --engine hsmm --config 'my cfg.toml' --markdown"
        );
    }

    fn temp_config(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f
    }

    #[test]
    fn config_decode_and_detector_apply_and_the_engine_flag_wins() {
        let file = temp_config("[decode]\nengine = \"hsmm\"\n\n[detector]\non_snr_db = 20.0\n");
        let path = file.path().to_str().unwrap();
        let (cfg, settings, note) = pipeline_config(&parse(&["--config", path]).unwrap()).unwrap();
        assert_eq!(cfg.decode.engine, Engine::Hsmm);
        assert_eq!(settings.engine, Engine::Hsmm);
        assert_eq!(settings.config.as_deref(), Some(path));
        assert_eq!(cfg.detector.on_snr_db, 20.0);
        assert_eq!(note, None);
        let (cfg, settings, _) =
            pipeline_config(&parse(&["--config", path, "--engine", "legacy"]).unwrap()).unwrap();
        assert_eq!(cfg.decode.engine, Engine::Legacy);
        assert_eq!(settings.engine, Engine::Legacy);
        let (cfg, settings, note) = pipeline_config(&parse(&[]).unwrap()).unwrap();
        assert_eq!(
            (cfg.decode.engine, settings.config, note),
            (Engine::Legacy, None, None)
        );
    }

    #[test]
    fn other_config_tables_are_reported_as_ignored() {
        let file = temp_config(
            "[spot]\nallowlist = [\"W1AW\"]\n\n[server]\nstation_callsign = \"W1AW\"\n",
        );
        let path = file.path().to_str().unwrap();
        let (cfg, _, note) = pipeline_config(&parse(&["--config", path]).unwrap()).unwrap();
        assert_eq!(
            note.as_deref(),
            Some(format!("note: bench sensitivity ignores [server], [spot] from {path}").as_str())
        );
        // An allowlist would bypass validation and inflate recall (D10).
        assert!(cfg.allowlist.is_empty());
    }

    #[test]
    fn json_has_the_documented_top_level_fields() {
        let h = header(Engine::Legacy, Some("cfg.toml"));
        let v: serde_json::Value =
            serde_json::from_str(&render_json(&hand_built(), &h).unwrap()).unwrap();
        assert_eq!(v["report"], "manta bench sensitivity");
        assert_eq!(v["format_version"], 1);
        assert_eq!(v["manta_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["engine"], "legacy");
        assert_eq!(v["config_file"], "cfg.toml");
        assert_eq!(v["command"], h.command.as_str());
        assert_eq!(v["snr_ref_hz"], 500);
        assert_eq!(v["sample_rate_hz"], 96000);
        assert_eq!(v["center_freq_hz"], 14000000);
        assert_eq!(v["stations_per_trial"], 10);
        assert_eq!(v["duration_s"], 30);
        assert_eq!(v["trials"], 1);
        assert_eq!(v["seed"], 1);
        assert_eq!(v["payload"], "CQ CQ DE <call> <call> K");
        assert_eq!(v["match_hz"], 150);
        assert_eq!(v["points"].as_array().unwrap().len(), 2);
        let none = header(Engine::Hsmm, None);
        let v: serde_json::Value =
            serde_json::from_str(&render_json(&hand_built(), &none).unwrap()).unwrap();
        assert!(v["config_file"].is_null());
        assert_eq!(v["engine"], "hsmm");
    }

    #[test]
    fn json_points_carry_both_snr_conventions_and_per_station_detail() {
        let out = render_json(&hand_built(), &header(Engine::Legacy, None)).unwrap();
        assert!(!out.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let p0 = &v["points"][0];
        assert_eq!(p0["condition"], "awgn");
        assert_eq!(p0["wpm"], 25);
        assert_eq!(p0["snr_db"], 4.5);
        assert_eq!(p0["snr_2500_db"], 4.5 - RBN_REF_BW_CORRECTION_DB as f64);
        assert_eq!(p0["stations"], 2);
        assert_eq!(p0["spotted"], 0);
        assert_eq!(p0["cer_mean"], 0.75);
        assert_eq!(p0["good_copy"], 0);
        assert!(p0["spot_snr_db"].is_null());
        assert!(p0["bogus_calls"].as_array().unwrap().is_empty());
        let st = &p0["per_station"][0];
        assert_eq!(st["trial"], 0);
        assert_eq!(st["call"], "W2ABC");
        assert_eq!(st["spotted"], false);
        assert!(st["first_spot_s"].is_null() && st["spot_snr_db"].is_null());
        let p1 = &v["points"][1];
        assert_eq!(p1["condition"], "good");
        assert_eq!(p1["bogus_calls"], serde_json::json!(["N0PE"]));
        assert_eq!(p1["per_station"].as_array().unwrap().len(), 2);
        assert_eq!(p1["per_station"][0]["first_spot_s"], 1.0);
        assert_eq!(p1["spot_snr_db"], -0.4);
    }

    #[test]
    fn markdown_renders_a_pipe_table_legend_and_regenerate_line() {
        let got = render_markdown(&hand_built(), &header(Engine::Legacy, None));
        let want = format!(
            "\
**manta {} sensitivity sweep**: legacy engine, default decoder settings · 10 stations per point · 30 s recordings · 1 trial · seed 1

SNR: transmitted carrier against noise in 500 Hz, as RBN and CW Skimmer quote it.

| Condition | Speed | SNR | Recall | Bogus | CER | CER<0.10 | Spot SNR |
|---|---:|---:|---:|---:|---:|---:|---:|
| AWGN | 25 WPM | 4.5 dB | 0/2 | 0 | 0.75 | 0/2 | - |
| Watterson good | 25 WPM | 4.5 dB | 1/2 | 1 | 0.10 | 1/2 | 0 dB |

- **Recall**: stations spotted with the right call, within 150 Hz of where they sent.
- **Bogus**: calls spotted that no station sent on that frequency.
- **CER**: character error rate of each station's copy, capped at 1, averaged.
- **CER<0.10**: stations copied with a character error rate under 0.10.
- **Spot SNR**: average SNR manta reported on its correct spots, in 500 Hz; - when none.

Each station sends \"CQ CQ DE &lt;call&gt; &lt;call&gt; K\" for the whole recording. AWGN is a steady signal in white noise; Watterson good and poor add two-path HF fading (0.5 ms / 0.1 Hz and 2 ms / 1 Hz) to each station independently.

Regenerate: `manta bench sensitivity --conditions awgn,good --wpm 25 --snr-db 4.5 --duration-s 30`
",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(got, want);
    }

    #[test]
    fn progress_and_start_lines_read_as_documented() {
        let plan = SweepPlan {
            conditions: Condition::ALL.to_vec(),
            wpm: vec![15, 25, 35],
            snr_db: vec![0.0; 9],
            duration_s: 60,
            trials: 1,
            seed: 1,
        };
        assert_eq!(
            start_line(&plan, 2),
            "sweeping 81 points (3 conditions x 3 speeds x 9 SNRs), 10 stations each, 60 s recordings, 2 jobs"
        );
        let p = Progress {
            index: 1,
            total: 81,
            condition: Condition::Awgn,
            wpm: 15,
            snr_db: 0.0,
            trial: 0,
            trials: 1,
            spotted: 0,
            stations: 10,
            cer_mean: 0.93,
        };
        assert_eq!(
            progress_line(&p),
            "[ 1/81] AWGN, 15 WPM, 0 dB: recall 0/10, CER 0.93"
        );
        let p = Progress {
            trial: 1,
            trials: 2,
            total: 162,
            ..p
        };
        assert!(progress_line(&p).ends_with("CER 0.93 (trial 2 of 2)"));
    }
}
