//! MAN-123 acceptance: `manta run`'s plain-text output with several tracks
//! decoding at once, against the real binary.
//!
//!   Scenario: Decoded text is grouped by track
//!   Scenario: Server mode is quiet by default
//!
//! stdout carries `SPOT:` lines only. Decoded text goes to stderr, one
//! labelled line per track, and a daemon (a `[server]` table) prints none of
//! it unless `--decoded-text` asks for it.

use manta_testkit::scene::{render_scene, SignalSpec};
use regex::Regex;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Output;

/// The three stations' callsigns, one per fixture signal.
const CALLS: [&str; 3] = ["W1AW", "K9ABC", "N0XYZ"];

/// 60 s of 48 kHz mono 16-bit audio with three looping CQ calls at 25 dB
/// SNR: W1AW at 600 Hz/20 WPM, K9ABC at 1300 Hz/24 WPM and N0XYZ at
/// 2000 Hz/28 WPM. Same construction as `node_health_acceptance.rs`'s
/// `write_cw_fixture`.
fn write_three_station_wav(dir: &Path) -> PathBuf {
    let fs = 48_000.0;
    let duration_s = 60.0;
    let station = |call: &str, offset_hz: f64, wpm: f32| SignalSpec {
        text: format!("CQ CQ DE {call} {call} K"),
        loop_text: true,
        wpm,
        offset_hz,
        snr_2500_db: 25.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    };
    let signals = [
        station(CALLS[0], 600.0, 20.0),
        station(CALLS[1], 1300.0, 24.0),
        station(CALLS[2], 2000.0, 28.0),
    ];
    let (scene, _texts) =
        render_scene(&signals, fs, duration_s, Some(42)).expect("render three-station scene");

    let path = dir.join("three.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: fs as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("create fixture WAV");
    for c in &scene {
        let v = (c.re * i16::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        writer.write_sample(v).expect("write fixture sample");
    }
    writer.finalize().expect("finalize fixture WAV");
    path
}

/// A `[server]` table on loopback with OS-chosen ports, as in
/// `startup_banner.rs`.
fn write_server_config(dir: &Path) -> PathBuf {
    let path = dir.join("server.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(
        br#"
        [server]
        station_callsign = "W3XYZ"
        bind_addr = "127.0.0.1"
        telnet_port = 0
        json_port = 0
        metrics_port = 0
        status_interval_secs = 0
        "#,
    )
    .unwrap();
    path
}

/// `manta run --source <wav> --dial-freq-hz 14060000 <extra...>`.
fn run(wav: &Path, extra: &[&str]) -> Output {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args([
            "run",
            // Pin the table to suppress the date-dependent cty age warning.
            "--cty",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../manta-spot/data/cty.dat"),
            "--source",
            wav.to_str().unwrap(),
            "--dial-freq-hz",
            "14060000",
        ])
        .args(extra)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "exit: {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// The decoded-text lines of a run's stderr.
fn track_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with("[track "))
        .collect()
}

/// stdout holds at least one line, and every line is a spot.
fn assert_stdout_is_spots_only(stdout: &str) {
    assert!(!stdout.is_empty(), "no spot line on stdout");
    for line in stdout.lines() {
        assert!(
            line.starts_with("SPOT: "),
            "stdout line is not a spot: {line:?}"
        );
    }
}

/// Every track line is labelled and holds one track's text: no line names
/// two stations, and every station appears intact in some line.
fn assert_grouped_per_track(lines: &[&str]) {
    assert!(!lines.is_empty(), "no decoded-text line on stderr");
    let label = Regex::new(r"^\[track \d+( -?\d+\.\d kHz)?( \d+ WPM)?\] \S").unwrap();
    for line in lines {
        assert!(label.is_match(line), "unlabelled track line: {line:?}");
        let calls: Vec<_> = CALLS.iter().filter(|c| line.contains(*c)).collect();
        assert!(calls.len() <= 1, "one line mixes {calls:?}: {line:?}");
    }
    for call in CALLS {
        assert!(
            lines.iter().any(|l| l.contains(call)),
            "{call} never appears intact in a track line:\n{}",
            lines.join("\n")
        );
    }
}

#[test]
fn text_mode_prints_decoded_text_one_line_per_track_and_spots_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let wav = write_three_station_wav(dir.path());

    let out = run(&wav, &[]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_stdout_is_spots_only(&stdout);
    let lines = track_lines(&stderr);
    assert_grouped_per_track(&lines);

    // File replay is deterministic, so the grouped text is too.
    let again = run(&wav, &[]);
    let again_stderr = String::from_utf8(again.stderr).unwrap();
    assert_eq!(lines, track_lines(&again_stderr));
    assert_eq!(stdout, String::from_utf8(again.stdout).unwrap());
}

#[test]
fn daemon_mode_prints_spots_on_stdout_and_no_decoded_text_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let wav = write_three_station_wav(dir.path());
    let cfg = write_server_config(dir.path());

    let out = run(&wav, &["--config", cfg.to_str().unwrap()]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_stdout_is_spots_only(&stdout);
    // Diagnostics still go to stderr.
    assert!(
        stderr.contains("listening:"),
        "no startup banner on stderr:\n{stderr}"
    );
    assert!(
        track_lines(&stderr).is_empty(),
        "a daemon printed decoded text:\n{stderr}"
    );
    for call in CALLS {
        assert!(
            !stderr.contains(call),
            "decoded {call} reached stderr:\n{stderr}"
        );
    }
}

#[test]
fn decoded_text_flag_prints_decoded_text_in_daemon_mode() {
    let dir = tempfile::tempdir().unwrap();
    let wav = write_three_station_wav(dir.path());
    let cfg = write_server_config(dir.path());

    let out = run(&wav, &["--config", cfg.to_str().unwrap(), "--decoded-text"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_stdout_is_spots_only(&stdout);
    assert_grouped_per_track(&track_lines(&stderr));
}

#[test]
fn decoded_text_conflicts_with_json() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args([
            "run",
            "--json",
            "--decoded-text",
            "--source",
            "/nonexistent.wav",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(
        stderr.contains("cannot be used with"),
        "not a clap conflict error:\n{stderr}"
    );
}
