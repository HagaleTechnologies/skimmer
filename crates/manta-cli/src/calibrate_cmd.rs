//! `manta calibrate`: measure the receiver's frequency error against a
//! known carrier and, once the operator confirms, save the
//! `freq_correction_ppm` that cancels it to `[input]` in their config file.

// MAN-127. The measurement is manta_engine::calibrate; the file edit is
// config_edit.rs. See docs/DECISIONS/2026-10-11-man127-calibrate-command.md.

use crate::{
    config, config_edit, prepare_source, CliOverrides, LiveSourceSpec, Resolved, SourcePrepared,
};
use anyhow::{bail, Context, Result};
use manta_engine::calibrate::{
    check_duration, plan_references, CalibrateOptions, CalibrationReport, PlannedReference,
    ReferenceResult, ReferenceStatus, MIN_AMBIGUITY_MARGIN_DB, MIN_VALID_SEGMENTS, SPREAD_WARN_HZ,
};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PPM_ENV_VAR: &str = "MANTA_INPUT_FREQ_CORRECTION_PPM";
/// The `MANTA_INPUT_*` variables that pick the receiver, as `--source`,
/// `--device`, `--kiwi-host`, `--soapy-driver` and `--hpsdr-host` do, plus
/// the port that, with the host, names a KiwiSDR or HPSDR receiver.
const SOURCE_ENV_VARS: &[&str] = &[
    "MANTA_INPUT_TYPE",
    "MANTA_INPUT_DEVICE",
    "MANTA_INPUT_PATH",
    "MANTA_INPUT_HOST",
    "MANTA_INPUT_PORT",
    "MANTA_INPUT_DRIVER",
];

/// Everything `manta calibrate` was given on the command line.
pub(crate) struct CalibrateArgs {
    pub duration_secs: u64,
    pub reference_hz: Option<f64>,
    pub search_ppm: f64,
    pub tune_hz: Option<f64>,
    pub write: bool,
    pub json: bool,
    pub config: Option<PathBuf>,
    pub cli: CliOverrides,
}

/// What happens to a measured value.
#[derive(Debug, PartialEq)]
pub(crate) enum SaveDecision {
    /// No config file was given.
    NoConfig,
    /// The named flag replaced the receiver `[input]` describes.
    OtherReceiver(&'static str),
    /// The file already holds the value.
    AlreadySet,
    /// Save without asking (`--write`).
    Save,
    /// `--json` without `--write`: report only.
    Skip,
    /// Nobody to ask.
    NotInteractive,
    /// Ask on the terminal.
    Ask,
}

/// The save rule, in priority order.
pub(crate) fn save_decision(
    has_config: bool,
    replaced_by: Option<&'static str>,
    already_set: bool,
    write: bool,
    json: bool,
    interactive: bool,
) -> SaveDecision {
    if !has_config {
        SaveDecision::NoConfig
    } else if let Some(flag) = replaced_by {
        SaveDecision::OtherReceiver(flag)
    } else if already_set {
        SaveDecision::AlreadySet
    } else if write {
        SaveDecision::Save
    } else if json {
        SaveDecision::Skip
    } else if !interactive {
        SaveDecision::NotInteractive
    } else {
        SaveDecision::Ask
    }
}

/// The source-selecting variable among `env_vars` when `file_text` has a
/// typed `[input]`: that variable, like a selector flag, means the receiver
/// being measured is not the one the file's `[input]` describes.
fn env_replaced_file_source(env_vars: &[String], file_text: &str) -> Result<Option<&'static str>> {
    let doc: toml::Table = toml::from_str(crate::strip_bom(file_text))?;
    if doc.get("input").and_then(|t| t.get("type")).is_none() {
        return Ok(None);
    }
    Ok(SOURCE_ENV_VARS
        .iter()
        .copied()
        .find(|v| env_vars.iter().any(|e| e == v)))
}

/// Write `prompt` and read one answer: true only for `y` or `yes`, in any
/// case, surrounded by any whitespace. End of input is a no.
pub(crate) fn confirm(out: &mut impl Write, input: &mut impl BufRead, prompt: &str) -> bool {
    let _ = write!(out, "{prompt} [y/N] ");
    let _ = out.flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}

/// The value that is saved: rounded to 0.01 ppm, never `-0.0`.
pub(crate) fn round_ppm(ppm: f64) -> f64 {
    let r = (ppm * 100.0).round() / 100.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// Whole dB, half away from zero, never `-0`.
fn db(v: f64) -> String {
    let r = v.round();
    format!("{}", if r == 0.0 { 0.0 } else { r })
}

/// Apply `--tune-hz` to the configured receiver's spec. A dial-frequency
/// override describes the old tuning, so it is dropped with it.
pub(crate) fn apply_tune_hz(resolved: &mut Resolved, tune_hz: f64) -> Result<()> {
    match &mut resolved.spec {
        LiveSourceSpec::Kiwi(kiwi) => kiwi.freq = Some(tune_hz),
        #[cfg(feature = "soapy")]
        LiveSourceSpec::Soapy(soapy) => soapy.freq = Some(tune_hz),
        #[cfg(feature = "hpsdr")]
        LiveSourceSpec::Hpsdr(hpsdr) => {
            hpsdr.freq = Some(
                crate::check_hpsdr_freq_hz("--tune-hz", tune_hz).map_err(|e| anyhow::anyhow!(e))?,
            )
        }
        LiveSourceSpec::AudioDevice(_) => bail!(
            "--tune-hz cannot retune a sound card; tune the radio so the reference sits 1 to 2 \
             kHz above its dial and pass --dial-freq-hz"
        ),
        LiveSourceSpec::File { .. } => bail!(
            "--tune-hz cannot retune a recording; record the receiver tuned near a reference \
             instead, or pass --reference-hz for a carrier the recording holds"
        ),
    }
    resolved.dial_freq_hz = None;
    Ok(())
}

/// Where the value `run` with the same flags would apply comes from.
fn configured_origin(
    resolved: &Resolved,
    loaded: &config::Loaded,
    config_path: Option<&Path>,
    file_value: Option<f64>,
) -> String {
    if resolved.replaced_file_source.is_none() && loaded.env_vars.iter().any(|v| v == PPM_ENV_VAR) {
        format!("from {PPM_ENV_VAR}")
    } else if let (None, Some(path), Some(_)) =
        (resolved.replaced_file_source, config_path, file_value)
    {
        format!("from {}", path.display())
    } else {
        "nothing set".to_string()
    }
}

fn reference_name(r: &ReferenceResult) -> String {
    format!("{:.1} Hz, {}", r.reference_hz, r.label)
}

fn segments_text(r: &ReferenceResult) -> String {
    format!(
        "carrier in {} of {} one-second segments",
        r.segments_valid, r.segments_total
    )
}

/// Why `r` was not measured.
fn failure_text(r: &ReferenceResult) -> String {
    match r.status {
        ReferenceStatus::Ambiguous => format!(
            "two carriers within {MIN_AMBIGUITY_MARGIN_DB:.0} dB of each other in the \
             ±{:.1} Hz search window ({:+.1} Hz and {:+.1} Hz); narrow --search-ppm or pass \
             --reference-hz",
            r.search_half_width_hz,
            r.peak_offset_hz.unwrap_or(0.0),
            r.runner_up_offset_hz.unwrap_or(0.0)
        ),
        _ => format!("{}, need {MIN_VALID_SEGMENTS}", segments_text(r)),
    }
}

/// The human report: everything but the prompt and the saved/not-saved line.
pub(crate) fn render_human(
    report: &CalibrationReport,
    configured_ppm: f64,
    configured_origin: &str,
) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(format!(
        "source: {:.0} Hz sample rate, centre {:.1} Hz, observed for {:.1} s",
        report.sample_rate_hz, report.center_freq_hz, report.duration_s
    ));
    let result = report.result();
    match result {
        Some(r) => {
            let error = r.error_hz.unwrap_or(0.0);
            line(format!("reference: {}", reference_name(r)));
            line(format!(
                "measured: {:.1} Hz, the receiver reads {:.1} Hz {}",
                r.measured_hz.unwrap_or(r.reference_hz),
                error.abs(),
                if error < 0.0 { "low" } else { "high" }
            ));
            line(format!(
                "correction: freq_correction_ppm = {:+.2} (±{:.2} ppm)",
                round_ppm(r.freq_correction_ppm.unwrap_or(0.0)),
                r.uncertainty_ppm.unwrap_or(0.0).max(0.01)
            ));
            line(format!(
                "evidence: {}, {} dB over the noise floor, spread {:.1} Hz",
                segments_text(r),
                db(r.peak_over_floor_db.unwrap_or(0.0)),
                r.spread_hz.unwrap_or(0.0)
            ));
            for other in report.references.iter().filter(|o| !std::ptr::eq(*o, r)) {
                match other.freq_correction_ppm {
                    Some(ppm) => line(format!(
                        "cross-check: {}: {:+.2} ppm, {}",
                        reference_name(other),
                        round_ppm(ppm),
                        segments_text(other)
                    )),
                    None => line(format!(
                        "cross-check: {}: not measured, {}",
                        reference_name(other),
                        failure_text(other)
                    )),
                }
            }
        }
        None => {
            line("reference: none measured".to_string());
            for r in &report.references {
                line(format!("tried: {}: {}", reference_name(r), failure_text(r)));
            }
        }
    }
    let configured = if configured_ppm == 0.0 {
        0.0
    } else {
        configured_ppm
    };
    line(format!(
        "configured: {configured:.2} ppm, {configured_origin}"
    ));
    out
}

/// The `--json` report: the engine's report plus the outcome.
fn render_json(
    report: &CalibrationReport,
    configured_ppm: f64,
    config_path: Option<&Path>,
    written: bool,
) -> Result<String> {
    let mut value = serde_json::to_value(report)?;
    if let serde_json::Value::Object(ref mut map) = value {
        let result = match report.result() {
            Some(r) => serde_json::json!({
                "reference_hz": r.reference_hz,
                "freq_correction_ppm": round_ppm(r.freq_correction_ppm.unwrap_or(0.0)),
            }),
            None => serde_json::Value::Null,
        };
        map.insert("result".to_string(), result);
        map.insert(
            "configured_ppm".to_string(),
            serde_json::json!(configured_ppm),
        );
        map.insert(
            "config_path".to_string(),
            serde_json::json!(config_path.map(|p| p.display().to_string())),
        );
        map.insert("written".to_string(), serde_json::json!(written));
    }
    Ok(serde_json::to_string(&value)?)
}

fn measuring_line(planned: &[PlannedReference], duration: Duration) -> String {
    let name = |p: &PlannedReference| format!("{:.1} Hz, {}", p.reference.hz, p.reference.label);
    let mut s = format!(
        "measuring {} for {} s",
        name(&planned[0]),
        duration.as_secs()
    );
    if planned.len() > 1 {
        let rest: Vec<String> = planned[1..].iter().map(name).collect();
        s.push_str(&format!(", cross-checking {}", rest.join("; ")));
    }
    s
}

fn is_audio(spec: &LiveSourceSpec) -> bool {
    match spec {
        LiveSourceSpec::AudioDevice(_) => true,
        LiveSourceSpec::File { source_iq, .. } => !source_iq,
        _ => false,
    }
}

pub(crate) fn run(args: CalibrateArgs) -> Result<()> {
    let duration = Duration::from_secs(args.duration_secs);
    check_duration(duration)?;
    let opts = CalibrateOptions {
        duration,
        search_ppm: args.search_ppm,
        reference_hz: args.reference_hz,
    };
    let SourcePrepared {
        config_path,
        loaded,
        mut resolved,
    } = prepare_source(args.cli, args.config)?;
    let replaced_by = match (resolved.replaced_file_source, config_path.as_deref()) {
        (Some(flag), _) => Some(flag),
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config file {}", path.display()))?;
            env_replaced_file_source(&loaded.env_vars, &text)
                .with_context(|| format!("parsing config file {}", path.display()))?
        }
        (None, None) => None,
    };
    if let Some(tune_hz) = args.tune_hz {
        apply_tune_hz(&mut resolved, tune_hz)?;
    }
    if args.write {
        let Some(path) = config_path.as_deref() else {
            bail!("--write needs a config file: pass --config or set MANTA_CONFIG");
        };
        if let Some(flag) = replaced_by {
            let drop = if flag.starts_with("MANTA_") {
                "unset"
            } else {
                "drop"
            };
            bail!(
                "--write would save a measurement of the receiver {flag} names into [input] of \
                 {}, which describes a different receiver; {drop} {flag} and use --tune-hz to \
                 point the configured receiver at a reference",
                path.display()
            );
        }
    }
    let spec = &resolved.spec;
    if !spec.is_rf_aware() && resolved.dial_freq_hz.is_none() {
        bail!(
            "calibrate needs the receiver's absolute frequency; pass --dial-freq-hz with the \
             radio's dial frequency"
        );
    }
    let src = spec.open(resolved.capture_rate_hz, resolved.dial_freq_hz)?;
    let planned = plan_references(
        src.center_freq_hz(),
        src.rf_passband_hz(),
        is_audio(spec),
        &opts,
    )?;
    eprintln!("{}", measuring_line(&planned, duration));
    let report = manta_engine::calibrate(src, &planned, &opts)?;

    // The file's own value, read from its text: the loaded config already
    // carries the environment overlay.
    let file_value = match config_path.as_deref() {
        Some(path) => {
            let text = std::fs::read_to_string(path)?;
            config_edit::set_freq_correction_ppm(&text, 0.0)?.1
        }
        None => None,
    };
    let origin = configured_origin(&resolved, &loaded, config_path.as_deref(), file_value);
    let configured = resolved.freq_correction_ppm;
    let measured = report
        .result()
        .and_then(|r| r.freq_correction_ppm)
        .map(round_ppm);

    if !args.json {
        print!("{}", render_human(&report, configured, &origin));
    }
    let mut written = false;
    let mut save_error = None;
    if let Some(value) = measured {
        if let Some(spread) = report.result().and_then(|r| r.spread_hz) {
            if spread > SPREAD_WARN_HZ {
                eprintln!(
                    "warning: the measured frequency wandered {spread:.1} Hz across the run; the \
                     receiver may still be warming up, or another signal shares the search window"
                );
            }
        }
        if let Some(path) = config_path.as_deref() {
            if replaced_by.is_none() && loaded.env_vars.iter().any(|v| v == PPM_ENV_VAR) {
                eprintln!(
                    "warning: {PPM_ENV_VAR} is set and overrides freq_correction_ppm from {} for \
                     run, soak and doctor; unset it to use the saved value",
                    path.display()
                );
            }
        }
        let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        let decision = save_decision(
            config_path.is_some(),
            replaced_by,
            file_value == Some(value),
            args.write,
            args.json,
            interactive,
        );
        let run_hint = format!("pass --freq-correction-ppm {value:?} to manta run");
        let outcome = match (decision, config_path.as_deref()) {
            (SaveDecision::Save, Some(path)) => {
                Some(save(path, value, &mut written, &mut save_error))
            }
            (SaveDecision::Ask, Some(path)) => {
                let prompt = format!(
                    "Save freq_correction_ppm = {value:?} to [input] in {}?",
                    path.display()
                );
                if confirm(
                    &mut std::io::stderr(),
                    &mut std::io::stdin().lock(),
                    &prompt,
                ) {
                    Some(save(path, value, &mut written, &mut save_error))
                } else {
                    Some("not saved: answered no".to_string())
                }
            }
            (SaveDecision::Skip, _) => None,
            (SaveDecision::AlreadySet, Some(path)) => Some(format!(
                "not saved: {} already has freq_correction_ppm = {value:?}",
                path.display()
            )),
            (SaveDecision::OtherReceiver(flag), Some(path)) => Some(format!(
                "not saved: {flag} replaced the receiver [input] describes in {}; {run_hint} \
                 with the same flags and environment",
                path.display()
            )),
            (SaveDecision::NotInteractive, _) => Some(format!(
                "not saved: no terminal to confirm on; re-run with --write to save it, or \
                 {run_hint}"
            )),
            _ => Some(format!(
                "not saved: no config file; pass --config or set MANTA_CONFIG to save it, or \
                 {run_hint}"
            )),
        };
        if let (Some(line), false) = (outcome, args.json) {
            println!("{line}");
        }
    }
    if args.json {
        println!(
            "{}",
            render_json(&report, configured, config_path.as_deref(), written)?
        );
    }
    if let Some(e) = save_error {
        return Err(e);
    }
    if measured.is_none() {
        bail!(
            "no reference carrier was measured; try a longer --duration, another time of day, \
             or a reference nearer your receiver"
        );
    }
    Ok(())
}

/// Save `value` to `path`, returning the line to print.
fn save(
    path: &Path,
    value: f64,
    written: &mut bool,
    save_error: &mut Option<anyhow::Error>,
) -> String {
    match config_edit::save_freq_correction_ppm(path, value) {
        Ok(previous) => {
            *written = true;
            format!(
                "saved: freq_correction_ppm = {value:?} in [input] of {} (was {}); restart manta \
                 to apply it",
                path.display(),
                previous.map_or("not set".to_string(), |p| format!("{p:?}"))
            )
        }
        Err(e) => {
            *save_error = Some(e.context(format!(
                "could not save freq_correction_ppm = {value:?}; set it in [input] of {} by hand",
                path.display()
            )));
            "not saved: the config file could not be written".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_engine::calibrate::ReferenceKind;

    #[test]
    fn confirm_accepts_only_y_or_yes() {
        for yes in ["y\n", "YES\n", " y \n", "Yes"] {
            let mut out = Vec::new();
            assert!(confirm(&mut out, &mut yes.as_bytes(), "Save?"), "{yes:?}");
            assert_eq!(String::from_utf8(out).unwrap(), "Save? [y/N] ");
        }
        for no in ["\n", "n\n", "yep\n", ""] {
            let mut out = Vec::new();
            assert!(!confirm(&mut out, &mut no.as_bytes(), "Save?"), "{no:?}");
        }
    }

    #[test]
    fn round_ppm_rounds_to_hundredths_and_normalises_negative_zero() {
        assert_eq!(round_ppm(-1.41999), -1.42);
        assert_eq!(round_ppm(2.4987), 2.5);
        assert_eq!(round_ppm(1.005_1), 1.01);
        let z = round_ppm(-0.001);
        assert_eq!(z, 0.0);
        assert!(z.is_sign_positive());
        assert_eq!(format!("{:?}", round_ppm(-0.004)), "0.0");
    }

    fn result(
        hz: f64,
        label: &'static str,
        status: ReferenceStatus,
        total: usize,
        valid: usize,
    ) -> ReferenceResult {
        ReferenceResult {
            reference_hz: hz,
            label,
            kind: ReferenceKind::TimeStandard,
            search_half_width_hz: 500.0,
            status,
            segments_total: total,
            segments_valid: valid,
            measured_hz: None,
            error_hz: None,
            freq_correction_ppm: None,
            uncertainty_ppm: None,
            peak_over_floor_db: None,
            spread_hz: None,
            ambiguity_margin_db: None,
            peak_offset_hz: None,
            runner_up_offset_hz: None,
        }
    }

    fn measured(
        hz: f64,
        label: &'static str,
        valid: usize,
        error: f64,
        ppm: f64,
    ) -> ReferenceResult {
        ReferenceResult {
            measured_hz: Some(hz + error),
            error_hz: Some(error),
            freq_correction_ppm: Some(ppm),
            uncertainty_ppm: Some(0.0035),
            peak_over_floor_db: Some(38.1),
            spread_hz: Some(0.21),
            ambiguity_margin_db: Some(24.5),
            ..result(hz, label, ReferenceStatus::Measured, 60, valid)
        }
    }

    fn report(references: Vec<ReferenceResult>) -> CalibrationReport {
        CalibrationReport {
            sample_rate_hz: 96_000.0,
            center_freq_hz: 9_998_500.0,
            duration_s: 60.0,
            search_ppm: 50.0,
            references,
        }
    }

    #[test]
    fn render_human_matches_the_design() {
        let ok = report(vec![
            measured(
                10_000_000.0,
                "time standard (WWV, WWVH, BPM)",
                58,
                14.2,
                -1.41999,
            ),
            measured(9_996_000.0, "time standard (RWM)", 41, 14.1, -1.41056),
        ]);
        assert_eq!(
            render_human(&ok, 0.0, "from /etc/manta/manta.toml"),
            "source: 96000 Hz sample rate, centre 9998500.0 Hz, observed for 60.0 s\n\
             reference: 10000000.0 Hz, time standard (WWV, WWVH, BPM)\n\
             measured: 10000014.2 Hz, the receiver reads 14.2 Hz high\n\
             correction: freq_correction_ppm = -1.42 (±0.01 ppm)\n\
             evidence: carrier in 58 of 60 one-second segments, 38 dB over the noise floor, \
             spread 0.2 Hz\n\
             cross-check: 9996000.0 Hz, time standard (RWM): -1.41 ppm, carrier in 41 of 60 \
             one-second segments\n\
             configured: 0.00 ppm, from /etc/manta/manta.toml\n"
        );
        let failed = report(vec![
            result(
                10_000_000.0,
                "time standard (WWV, WWVH, BPM)",
                ReferenceStatus::TooFewSegments,
                60,
                3,
            ),
            result(
                9_996_000.0,
                "time standard (RWM)",
                ReferenceStatus::TooFewSegments,
                60,
                0,
            ),
        ]);
        assert_eq!(
            render_human(&failed, 0.0, "from manta.toml"),
            "source: 96000 Hz sample rate, centre 9998500.0 Hz, observed for 60.0 s\n\
             reference: none measured\n\
             tried: 10000000.0 Hz, time standard (WWV, WWVH, BPM): carrier in 3 of 60 \
             one-second segments, need 10\n\
             tried: 9996000.0 Hz, time standard (RWM): carrier in 0 of 60 one-second \
             segments, need 10\n\
             configured: 0.00 ppm, from manta.toml\n"
        );
        let ambiguous = report(vec![ReferenceResult {
            search_half_width_hz: 705.0,
            ambiguity_margin_db: Some(2.6),
            peak_offset_hz: Some(12.3),
            runner_up_offset_hz: Some(410.8),
            ..result(
                14_100_000.0,
                "NCDXF beacon",
                ReferenceStatus::Ambiguous,
                60,
                40,
            )
        }]);
        assert!(render_human(&ambiguous, 0.0, "nothing set").contains(
            "tried: 14100000.0 Hz, NCDXF beacon: two carriers within 6 dB of each other in the \
             ±705.0 Hz search window (+12.3 Hz and +410.8 Hz); narrow --search-ppm or pass \
             --reference-hz"
        ));
    }

    #[test]
    fn save_decision_table() {
        use SaveDecision::*;
        assert_eq!(
            save_decision(false, None, false, true, false, true),
            NoConfig
        );
        assert_eq!(
            save_decision(true, Some("--kiwi-host"), false, true, false, true),
            OtherReceiver("--kiwi-host")
        );
        assert_eq!(
            save_decision(true, None, true, true, false, true),
            AlreadySet
        );
        assert_eq!(save_decision(true, None, false, true, true, false), Save);
        assert_eq!(save_decision(true, None, false, false, true, true), Skip);
        assert_eq!(
            save_decision(true, None, false, false, false, false),
            NotInteractive
        );
        assert_eq!(save_decision(true, None, false, false, false, true), Ask);
    }

    #[test]
    fn a_source_env_var_replaces_only_a_typed_file_input() {
        let vars = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let typed = "[input]\ntype = \"kiwi\"\nhost = \"a\"\nport = 1\nfreq_hz = 7030000.0\n";
        let host = vars(&["MANTA_INPUT_HOST"]);
        assert_eq!(
            env_replaced_file_source(&host, typed).unwrap(),
            Some("MANTA_INPUT_HOST")
        );
        assert_eq!(
            env_replaced_file_source(&vars(&["MANTA_INPUT_PATH", "MANTA_INPUT_TYPE"]), typed)
                .unwrap(),
            Some("MANTA_INPUT_TYPE")
        );
        assert_eq!(
            env_replaced_file_source(&vars(&["MANTA_INPUT_PORT"]), typed).unwrap(),
            Some("MANTA_INPUT_PORT")
        );
        assert_eq!(
            env_replaced_file_source(&vars(&["MANTA_INPUT_FREQ_HZ", PPM_ENV_VAR]), typed).unwrap(),
            None
        );
        assert_eq!(
            env_replaced_file_source(&host, "[input]\nfreq_correction_ppm = 1.0\n").unwrap(),
            None
        );
    }

    fn resolved_with(spec: LiveSourceSpec) -> Resolved {
        Resolved {
            spec,
            freq_correction_ppm: 0.0,
            dial_freq_hz: Some(7_000_000.0),
            capture_rate_hz: None,
            replay_epoch: None,
            spot: crate::resolve_spot(Vec::new(), None, None, None, None, &Default::default()),
            notes: Vec::new(),
            replaced_file_source: None,
        }
    }

    #[test]
    fn tune_hz_applies_to_kiwi_soapy_hpsdr_and_refuses_audio_and_file() {
        let mut r = resolved_with(LiveSourceSpec::Kiwi(crate::KiwiOpts {
            host: Some("h".into()),
            port: 8073,
            freq: Some(7_030_000.0),
            password: String::new(),
        }));
        apply_tune_hz(&mut r, 9_998_500.0).unwrap();
        match &r.spec {
            LiveSourceSpec::Kiwi(k) => assert_eq!(k.freq, Some(9_998_500.0)),
            _ => unreachable!(),
        }
        assert_eq!(r.dial_freq_hz, None);
        #[cfg(feature = "soapy")]
        {
            let mut r = resolved_with(LiveSourceSpec::Soapy(crate::SoapyOpts {
                driver: Some("driver=rtlsdr".into()),
                freq: Some(7_030_000.0),
                rate: Some(240_000.0),
                gain: None,
            }));
            apply_tune_hz(&mut r, 9_998_500.0).unwrap();
            match &r.spec {
                LiveSourceSpec::Soapy(s) => assert_eq!(s.freq, Some(9_998_500.0)),
                _ => unreachable!(),
            }
        }
        #[cfg(feature = "hpsdr")]
        {
            let mut r = resolved_with(LiveSourceSpec::Hpsdr(crate::HpsdrOpts {
                host: Some("h".into()),
                port: manta_input::hpsdr::CONTROL_PORT,
                freq: Some(7_030_000.0),
                rate: Some(192_000.0),
            }));
            apply_tune_hz(&mut r, 9_998_500.0).unwrap();
            match &r.spec {
                LiveSourceSpec::Hpsdr(h) => assert_eq!(h.freq, Some(9_998_500.0)),
                _ => unreachable!(),
            }
        }
        let mut r = resolved_with(LiveSourceSpec::AudioDevice(None));
        let e = apply_tune_hz(&mut r, 9_998_500.0).unwrap_err();
        assert!(e.to_string().contains("cannot retune a sound card"), "{e}");
        let mut r = resolved_with(LiveSourceSpec::File {
            path: "x.wav".into(),
            source_iq: true,
        });
        let e = apply_tune_hz(&mut r, 9_998_500.0).unwrap_err();
        assert!(e.to_string().contains("cannot retune a recording"), "{e}");
    }
}
