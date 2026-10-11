//! MAN-131 scenario 3 against the real `manta decode` binary: a recording
//! without a usable `<stem>.json` sidecar gets a stderr warning that its
//! frequencies are baseband offsets, and `--center-freq-hz` supplies (or
//! overrides) the centre frequency. `decode`'s stdout is unchanged.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary with every `MANTA_*` variable removed, as in `cli.rs`.
fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// `dir/v1.wav` plus its `v1.json` sidecar (`center_freq_hz` 14 000 000):
/// V1 shortened to 30 s, the same fixture as `cli.rs`'s `short_v1`, long
/// enough for its one W1AW spot.
fn v1_fixture(dir: &Path) -> PathBuf {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 30.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join(format!("{}.wav", spec.name))
}

fn decode(args: &[&str]) -> Output {
    manta().arg("decode").args(args).output().unwrap()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The report's `freq_hz` from `decode --json`'s single stdout object.
fn report_freq_hz(out: &Output) -> f64 {
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("decode --json prints one JSON object");
    report["freq_hz"].as_f64().expect("freq_hz is a number")
}

#[test]
fn decode_without_a_sidecar_warns_that_frequencies_are_baseband() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    std::fs::remove_file(dir.path().join("v1.json")).unwrap();

    let out = decode(&["--json", wav.to_str().unwrap()]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(report_freq_hz(&out) < 100_000.0, "{stderr}");
    for needle in [
        "warning: no sidecar",
        "v1.json",
        "baseband offsets",
        "--center-freq-hz",
    ] {
        assert!(stderr.contains(needle), "{needle:?} missing from {stderr}");
    }
}

#[test]
fn decode_with_a_sidecar_does_not_warn() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());

    let out = decode(&["--json", wav.to_str().unwrap()]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("baseband"), "{stderr}");
}

#[test]
fn decode_with_a_zero_center_sidecar_warns() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    std::fs::write(dir.path().join("v1.json"), r#"{"center_freq_hz": 0.0}"#).unwrap();

    let out = decode(&["--json", wav.to_str().unwrap()]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("center_freq_hz = 0"), "{stderr}");
    assert!(stderr.contains("baseband offsets"), "{stderr}");
}

#[test]
fn center_freq_hz_reproduces_the_sidecar_decode_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let copy = dir.path().join("copy.wav");
    std::fs::copy(&wav, &copy).unwrap();
    assert!(!dir.path().join("copy.json").exists());

    let with_sidecar = decode(&["--json", wav.to_str().unwrap()]);
    let with_flag = decode(&[
        "--json",
        "--center-freq-hz",
        "14000000",
        copy.to_str().unwrap(),
    ]);
    assert!(
        with_sidecar.status.success(),
        "{}",
        stderr_of(&with_sidecar)
    );
    assert!(with_flag.status.success(), "{}", stderr_of(&with_flag));
    assert!(!with_sidecar.stdout.is_empty());
    assert_eq!(
        with_sidecar.stdout, with_flag.stdout,
        "--center-freq-hz 14000000 must decode exactly as the 14 MHz sidecar does"
    );
    assert!(
        !stderr_of(&with_flag).contains("baseband"),
        "{}",
        stderr_of(&with_flag)
    );
}

#[test]
fn center_freq_hz_overrides_a_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());

    let sidecar = decode(&["--json", wav.to_str().unwrap()]);
    let overridden = decode(&[
        "--json",
        "--center-freq-hz",
        "7000000",
        wav.to_str().unwrap(),
    ]);
    assert!(sidecar.status.success(), "{}", stderr_of(&sidecar));
    assert!(overridden.status.success(), "{}", stderr_of(&overridden));
    let expected = report_freq_hz(&sidecar) - 7_000_000.0;
    let got = report_freq_hz(&overridden);
    assert!(
        (got - expected).abs() < 0.01,
        "freq_hz {got}, expected {expected}"
    );
    assert!(!stderr_of(&overridden).contains("baseband"));
}

#[test]
fn center_freq_hz_gets_past_a_malformed_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    std::fs::write(dir.path().join("v1.json"), "{\"other_tool\": true}").unwrap();

    let without_flag = decode(&["--json", wav.to_str().unwrap()]);
    assert!(!without_flag.status.success());
    assert!(stderr_of(&without_flag).contains("parse sidecar"));

    let with_flag = decode(&[
        "--json",
        "--center-freq-hz",
        "14000000",
        wav.to_str().unwrap(),
    ]);
    let stderr = stderr_of(&with_flag);
    assert!(with_flag.status.success(), "{stderr}");
    assert!(report_freq_hz(&with_flag) > 14_000_000.0, "{stderr}");
}

#[test]
fn center_freq_hz_rejects_bad_values() {
    // clap rejects the value before any I/O, so the WAV need not exist. The
    // `=` form keeps clap from reading a leading `-` as another flag.
    for bad in ["nan", "inf", "-inf", "0", "-14000000", "abc"] {
        let flag = format!("--center-freq-hz={bad}");
        let out = decode(&[&flag, "/nonexistent/man131/x.wav"]);
        let stderr = stderr_of(&out);
        assert_eq!(out.status.code(), Some(2), "{bad}: {stderr}");
        assert!(stderr.contains("--center-freq-hz"), "{bad}: {stderr}");
    }
}

#[test]
fn a_missing_wav_reports_the_open_error_without_a_sidecar_warning() {
    let out = decode(&["/nonexistent/man131/x.wav"]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("open WAV"), "{stderr}");
    assert!(!stderr.contains("sidecar"), "{stderr}");
}

#[test]
fn decode_help_documents_center_freq_hz() {
    let out = decode(&["--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--center-freq-hz"), "{stdout}");
}
