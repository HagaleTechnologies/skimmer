//! MAN-127: `manta calibrate` end to end, on hermetic IQ WAV fixtures whose
//! carrier frequency is known exactly.

use num_complex::Complex32;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FS: f64 = 48_000.0;
const CENTRE: f64 = 9_998_500.0;
/// A 10 MHz carrier as a receiver 2.5 ppm fast would see it.
const CARRIER_2_5_PPM: f64 = 10e6 / (1.0 + 2.5e-6);
const SECS: f64 = 12.0;

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd.stdin(Stdio::null());
    cmd
}

/// Seeded xorshift64 + Box-Muller complex Gaussian noise, sigma per axis.
fn noise(n: usize, sigma: f64, seed: u64) -> Vec<Complex32> {
    let mut s = seed.max(1);
    let mut uniform = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    };
    (0..n)
        .map(|_| {
            let r = (-2.0 * uniform().ln()).sqrt() * sigma;
            let t = 2.0 * std::f64::consts::PI * uniform();
            Complex32::new((r * t.cos()) as f32, (r * t.sin()) as f32)
        })
        .collect()
}

/// `SECS` of a unit carrier at absolute `carrier_hz` (none when `None`)
/// plus noise at SNR +10 dB in 2500 Hz, as seen by a receiver at `centre`.
fn samples(carrier_hz: Option<f64>, centre: f64) -> Vec<Complex32> {
    let n = (SECS * FS) as usize;
    let sigma = (0.1 * FS / 2500.0 / 2.0f64).sqrt();
    let mut s = noise(n, sigma, 42);
    if let Some(hz) = carrier_hz {
        let f = hz - centre;
        for (i, x) in s.iter_mut().enumerate() {
            let phi = 2.0 * std::f64::consts::PI * f * i as f64 / FS;
            *x += Complex32::new(phi.cos() as f32, phi.sin() as f32);
        }
    }
    s
}

fn fixture(dir: &Path, carrier_hz: Option<f64>, centre: f64) -> PathBuf {
    manta_testkit::wav::write_fixture(dir, "ref", &samples(carrier_hz, centre), FS, centre).unwrap()
}

const FILE_INPUT: &str = "[input]\ntype = \"file\"\npath = \"ref.wav\"\niq = true\n";

fn write_cfg(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("manta.toml");
    std::fs::write(&path, body).unwrap();
    path
}

fn calibrate(cfg: &Path, extra: &[&str]) -> Output {
    manta()
        .args(["calibrate", "--duration", "12", "--config"])
        .arg(cfg)
        .args(extra)
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn calibrate_reports_the_ppm_of_the_configured_receiver() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &[]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    for needle in [
        "reference: 10000000.0 Hz, time standard (WWV, WWVH, BPM)",
        "the receiver reads 25.0 Hz low",
        "correction: freq_correction_ppm = +2.50",
        "not saved: no terminal to confirm on",
    ] {
        assert!(stdout.contains(needle), "no {needle:?} in {stdout}");
    }
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), FILE_INPUT);
}

#[test]
fn calibrate_json_reports_the_measurement() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &["--json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ppm = v["result"]["freq_correction_ppm"].as_f64().unwrap();
    assert!((ppm - 2.5).abs() < 0.02, "{ppm}");
    assert_eq!(v["references"][0]["status"], "measured");
    assert_eq!(v["written"], false);
    assert_eq!(v["config_path"], cfg.display().to_string());
}

#[test]
fn calibrate_write_saves_only_that_key() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let original = "# The node's receiver.\n[input]\ntype = \"file\"   # a recording\n\
                    path = \"ref.wav\"\niq = true\n\n# end\n";
    let cfg = write_cfg(dir.path(), original);
    let out = calibrate(&cfg, &["--write"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(
        stdout.contains("saved: freq_correction_ppm = 2.5 in [input] of"),
        "{stdout}"
    );
    let edited = std::fs::read_to_string(&cfg).unwrap();
    let added: Vec<&str> = edited
        .lines()
        .filter(|l| !original.lines().any(|o| o == *l))
        .collect();
    assert_eq!(added, vec!["freq_correction_ppm = 2.5"], "{edited}");
    let kept: Vec<&str> = edited
        .lines()
        .filter(|l| *l != "freq_correction_ppm = 2.5")
        .collect();
    assert_eq!(kept, original.lines().collect::<Vec<_>>(), "{edited}");

    let check = manta()
        .args(["config", "check", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(check.status.success(), "{}", text(&check.stderr));
    assert!(
        text(&check.stdout).contains("freq_correction_ppm=2.5"),
        "{}",
        text(&check.stdout)
    );
}

#[test]
fn calibrate_write_replaces_rather_than_adds() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(
        dir.path(),
        &format!("{FILE_INPUT}freq_correction_ppm = 5.0\n"),
    );
    let out = calibrate(&cfg, &["--write"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(stdout.contains("(was 5.0)"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        format!("{FILE_INPUT}freq_correction_ppm = 2.5\n")
    );
}

#[test]
fn calibrate_reports_already_set() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let body = format!("{FILE_INPUT}freq_correction_ppm = 2.5\n");
    let cfg = write_cfg(dir.path(), &body);
    let out = calibrate(&cfg, &["--write"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(
        stdout.contains("not saved: ") && stdout.contains("already has freq_correction_ppm = 2.5"),
        "{stdout}"
    );
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);
}

#[test]
fn calibrate_write_without_config_fails_before_measuring() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let out = manta()
        .args([
            "calibrate",
            "--duration",
            "12",
            "--source-iq",
            "--write",
            "--source",
        ])
        .arg(&wav)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--write needs a config file"),
        "{}",
        text(&out.stderr)
    );
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
}

#[test]
fn calibrate_write_refuses_a_replaced_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let body = "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = 1\nfreq_hz = 7030000.0\n";
    let cfg = write_cfg(dir.path(), body);
    let out = calibrate(
        &cfg,
        &["--source-iq", "--write", "--source", wav.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("--write would save a measurement of the receiver --source names"),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);
}

#[test]
fn calibrate_write_refuses_a_receiver_the_environment_selects() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let body = "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = 1\nfreq_hz = 7030000.0\n";
    let cfg = write_cfg(dir.path(), body);
    let out = manta()
        .env("MANTA_INPUT_TYPE", "file")
        .env("MANTA_INPUT_PATH", &wav)
        .env("MANTA_INPUT_IQ", "true")
        .args(["calibrate", "--duration", "12", "--write", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("--write would save a measurement of the receiver MANTA_INPUT_TYPE names"),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);
}

#[test]
fn calibrate_explains_an_empty_passband() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), None, 7_030_000.0);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &[]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("no known reference frequency is usable") && stderr.contains("--tune-hz"),
        "{stderr}"
    );
}

#[test]
fn calibrate_failed_measurement_prints_the_report_and_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), None, CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &[]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("reference: none measured"), "{stdout}");
    assert!(stdout.contains("tried: 10000000.0 Hz"), "{stdout}");
    assert!(
        text(&out.stderr).contains("no reference carrier was measured"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_refuses_tune_hz_on_a_recording() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &["--tune-hz", "9998500"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--tune-hz cannot retune a recording"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_needs_an_absolute_frequency() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    std::fs::remove_file(dir.path().join("ref.json")).unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("absolute frequency"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_rejects_duration_below_10() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = manta()
        .args(["calibrate", "--duration", "5", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--duration must be between 10 and 3600 seconds, got 5"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_reference_hz_measures_an_operator_carrier() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(10_001_000.0), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &["--json", "--reference-hz", "10001000"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["references"].as_array().unwrap().len(), 1);
    assert_eq!(v["references"][0]["kind"], "operator");
    assert_eq!(v["references"][0]["status"], "measured");
    let ppm = v["result"]["freq_correction_ppm"].as_f64().unwrap();
    assert!(ppm.abs() < 0.02, "{ppm}");
}

#[test]
fn calibrate_is_deterministic_on_a_file() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let a = calibrate(&cfg, &["--json"]);
    let b = calibrate(&cfg, &["--json"]);
    assert!(a.status.success());
    assert!(!a.stdout.is_empty());
    assert_eq!(a.stdout, b.stdout);
}

#[test]
fn calibrate_warns_when_the_environment_overrides_the_file() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = manta()
        .env("MANTA_INPUT_FREQ_CORRECTION_PPM", "1")
        .args(["calibrate", "--duration", "12", "--write", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("MANTA_INPUT_FREQ_CORRECTION_PPM is set and overrides"),
        "{stderr}"
    );
    assert!(
        text(&out.stdout).contains("configured: 1.00 ppm, from MANTA_INPUT_FREQ_CORRECTION_PPM"),
        "{}",
        text(&out.stdout)
    );
    assert!(std::fs::read_to_string(&cfg)
        .unwrap()
        .contains("freq_correction_ppm = 2.5"));
}

#[cfg(unix)]
#[test]
fn calibrate_write_keeps_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = calibrate(&cfg, &["--write"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let mode = std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(std::fs::read_to_string(&cfg)
        .unwrap()
        .contains("freq_correction_ppm = 2.5"));
}

#[test]
fn top_level_help_lists_calibrate() {
    let out = manta().arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(
        text(&out.stdout)
            .contains("calibrate  Measure the receiver's frequency error against a known signal"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn calibrate_help_names_every_flag() {
    let out = manta().args(["calibrate", "--help"]).output().unwrap();
    assert!(out.status.success());
    let help = text(&out.stdout);
    for flag in [
        "--duration",
        "--reference-hz",
        "--search-ppm",
        "--tune-hz",
        "--write",
        "--json",
        "--config",
        "--source",
        "--kiwi-host",
        "--dial-freq-hz",
    ] {
        assert!(help.contains(flag), "no {flag} in {help}");
    }
}
