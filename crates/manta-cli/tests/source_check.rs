//! MAN-125: source diagnostics through the real CLI, without receiver hardware.
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

fn manta() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            command.env_remove(key);
        }
    }
    command
}

fn wav(dir: &Path, name: &str, rate: u32, channels: u16, frames: usize, value: f32) -> PathBuf {
    let path = dir.join(name);
    let mut writer = hound::WavWriter::create(
        &path,
        hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for _ in 0..frames {
        writer.write_sample(value).unwrap();
        if channels == 2 {
            writer.write_sample(0.0_f32).unwrap();
        }
    }
    writer.finalize().unwrap();
    path
}

fn config(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn check(dir: &Path, args: &[&str]) -> Output {
    manta()
        .current_dir(dir)
        .arg("check")
        .args(args)
        .output()
        .unwrap()
}

fn status(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn report(output: &Output, code: i32) -> Value {
    status(output, code);
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)))
}

#[test]
fn positional_and_source_flag_report_identical_default_three_second_windows() {
    let dir = tempfile::tempdir().unwrap();
    let path = wav(dir.path(), "capture.wav", 96000, 2, 4 * 96000, 0.125);
    std::fs::write(
        path.with_extension("json"),
        r#"{"center_freq_hz":7030000.0}"#,
    )
    .unwrap();
    let positional = check(dir.path(), &["capture.wav", "--source-iq", "--json"]);
    let flagged = check(
        dir.path(),
        &["--source", "capture.wav", "--source-iq", "--json"],
    );
    let measured = report(&positional, 0);
    assert_eq!(measured, report(&flagged, 0));
    assert_eq!(measured["source"], "file");
    assert_eq!(measured["sample_rate_hz"], 96000.0);
    assert_eq!(measured["center_freq_hz"], 7030000.0);
    assert_eq!(measured["passband_offsets_hz"], json!([-48000.0, 48000.0]));
    assert_eq!(measured["samples_received"], 288000);
    assert_eq!(measured["requested_duration_seconds"], 3.0);
    assert_eq!(measured["observed_duration_seconds"], 3.0);
    assert_eq!(measured["stop_reason"], "sample_window_complete");
    assert!((measured["input_power_dbfs"].as_f64().unwrap() + 18.0618).abs() < 0.001);
    assert_eq!(measured["noise_floor_dbfs"]["channels"], 1024);
    assert_eq!(measured["digital_silence"], false);
}

#[test]
fn silence_is_reported_without_claiming_rf_health() {
    let dir = tempfile::tempdir().unwrap();
    wav(dir.path(), "silence.wav", 96000, 2, 96000, 0.0);
    let args = ["silence.wav", "--source-iq", "--duration", "1"];
    let text = check(dir.path(), &args);
    status(&text, 0);
    let text = String::from_utf8(text.stdout).unwrap();
    for expected in [
        "Source: file",
        "Stream sample rate: 96000 Hz",
        "Center frequency: unknown (baseband)",
        "Samples received: 96000 (1.000 s)",
        "Input power: digital silence",
        "Noise floor:",
        "Floor estimate: brief, per-channel lower quartile; not calibrated RF power.",
        "Stopped: sample window complete",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains("healthy"));
    let measured = report(
        &check(
            dir.path(),
            &["silence.wav", "--source-iq", "--duration", "1", "--json"],
        ),
        0,
    );
    assert_eq!(measured["digital_silence"], true);
    assert!(measured["input_power_dbfs"].is_null());
    assert!((measured["noise_floor_dbfs"]["median"].as_f64().unwrap() + 139.75).abs() < 0.01);
}

#[test]
fn empty_and_too_short_files_have_no_fabricated_floor() {
    let dir = tempfile::tempdir().unwrap();
    for (name, frames, value) in [("empty.wav", 0, 0.0), ("tiny.wav", 32, 0.25)] {
        wav(dir.path(), name, 96000, 2, frames, value);
        let measured = report(&check(dir.path(), &[name, "--source-iq", "--json"]), 1);
        assert_eq!(measured["samples_received"], frames);
        assert_eq!(
            measured["observed_duration_seconds"],
            frames as f64 / 96000.0
        );
        assert_eq!(measured["stop_reason"], "end_of_file");
        assert_eq!(measured["digital_silence"], false);
        assert!(measured["noise_floor_dbfs"].is_null());
        assert_eq!(measured["input_power_dbfs"].is_null(), frames == 0);
        let output = check(dir.path(), &[name, "--source-iq"]);
        status(&output, 1);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains("Noise floor: unavailable (not enough samples)"),
            "{text}"
        );
        assert!(text.contains("Stopped: end of file"), "{text}");
        assert!(!text.contains("Floor estimate:"), "{text}");
    }
}

#[test]
fn shorter_file_with_a_complete_hop_is_a_successful_honest_measurement() {
    let dir = tempfile::tempdir().unwrap();
    wav(dir.path(), "short.wav", 96000, 2, 24000, 0.25);
    let measured = report(
        &check(dir.path(), &["short.wav", "--source-iq", "--json"]),
        0,
    );
    assert_eq!(measured["samples_received"], 24000);
    assert_eq!(measured["observed_duration_seconds"], 0.25);
    assert_eq!(measured["requested_duration_seconds"], 3.0);
    assert_eq!(measured["stop_reason"], "end_of_file");
    assert!(measured["noise_floor_dbfs"].is_object());
}

#[test]
fn mono_audio_is_converted_and_iq_requires_stereo() {
    let dir = tempfile::tempdir().unwrap();
    wav(dir.path(), "audio.wav", 48000, 1, 48000, 0.25);
    let measured = report(
        &check(dir.path(), &["audio.wav", "--duration", "1", "--json"]),
        0,
    );
    assert_eq!(measured["sample_rate_hz"], 48000.0);
    let passband = measured["passband_offsets_hz"].as_array().unwrap();
    assert!(passband[0].as_f64().unwrap() >= 0.0);
    assert!(passband[1].as_f64().unwrap() < 24000.0);
    assert!(measured["noise_floor_dbfs"]["channels"].as_u64().unwrap() < 512);
    let output = check(dir.path(), &["audio.wav", "--source-iq"]);
    status(&output, 1);
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("2 channels"));
}

#[test]
fn decimation_and_dial_override_reach_the_report() {
    let dir = tempfile::tempdir().unwrap();
    wav(dir.path(), "capture.wav", 96000, 2, 96000, 0.125);
    let measured = report(
        &check(
            dir.path(),
            &[
                "capture.wav",
                "--source-iq",
                "--capture-rate-hz",
                "48000",
                "--dial-freq-hz",
                "14060000",
                "--json",
            ],
        ),
        0,
    );
    assert_eq!(measured["sample_rate_hz"], 48000.0);
    assert_eq!(measured["center_freq_hz"], 14060000.0);
    assert_eq!(measured["passband_offsets_hz"], json!([-24000.0, 24000.0]));
    assert!(measured["samples_received"].as_u64().unwrap() <= 48000);
    assert_eq!(
        measured["observed_duration_seconds"].as_f64().unwrap(),
        measured["samples_received"].as_f64().unwrap() / 48000.0
    );
}

#[test]
fn duration_bounds_are_usage_errors_and_valid_endpoints_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    wav(dir.path(), "short.wav", 96000, 2, 24000, 0.0);
    for duration in ["0", "0.9", "60.1", "61", "NaN", "inf", "-inf"] {
        let output = check(
            dir.path(),
            &[
                "short.wav",
                "--source-iq",
                &format!("--duration={duration}"),
            ],
        );
        status(&output, 2);
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("duration"));
    }
    for duration in ["1", "60"] {
        let measured = report(
            &check(
                dir.path(),
                &["short.wav", "--source-iq", "--duration", duration, "--json"],
            ),
            0,
        );
        assert_eq!(
            measured["requested_duration_seconds"].as_f64().unwrap(),
            duration.parse::<f64>().unwrap()
        );
    }
}

#[test]
fn source_selectors_conflict_before_any_source_is_opened() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["missing.wav", "--source", "other.wav"],
        vec!["missing.wav", "--device", "dummy"],
        vec!["--source", "missing.wav", "--device", "dummy"],
        vec![
            "missing.wav",
            "--kiwi-host",
            "127.0.0.1",
            "--kiwi-freq-hz",
            "7030000",
        ],
    ] {
        let output = check(dir.path(), &args);
        status(&output, 2);
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    }
}

#[test]
fn missing_paths_and_invalid_metadata_fail_without_a_success_report() {
    let dir = tempfile::tempdir().unwrap();
    // SOURCE is a path, including the word audio; it is not a backend parser.
    for name in ["missing.wav", "audio"] {
        let output = check(dir.path(), &[name, "--source-iq"]);
        status(&output, 1);
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot open source"));
    }
    wav(dir.path(), "bad-rate.wav", 44100, 2, 44100, 0.25);
    let output = check(dir.path(), &["bad-rate.wav", "--source-iq", "--json"]);
    status(&output, 1);
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("source check failed"));
    wav(dir.path(), "nonfinite.wav", 96000, 2, 16000, f32::NAN);
    let output = check(dir.path(), &["nonfinite.wav", "--source-iq", "--json"]);
    status(&output, 1);
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("source check failed"));
}

#[test]
fn config_paths_are_relative_to_config_and_environment_overlays_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("config");
    std::fs::create_dir(&cfg_dir).unwrap();
    wav(&cfg_dir, "capture.wav", 96000, 2, 96000, 0.125);
    let cfg = config(
        &cfg_dir,
        "probe.toml",
        "[input]\ntype='file'\npath='capture.wav'\niq=true\ncenter_freq_hz=7030000.0\n",
    );
    let from_file = manta()
        .current_dir(dir.path())
        .args(["check", "--config"])
        .arg(&cfg)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(report(&from_file, 0)["center_freq_hz"], 7030000.0);
    let output = manta()
        .current_dir(dir.path())
        .env("MANTA_CONFIG", &cfg)
        .env("MANTA_INPUT_CENTER_FREQ_HZ", "14060000")
        .args(["check", "--json"])
        .output()
        .unwrap();
    let measured = report(&output, 0);
    assert_eq!(measured["center_freq_hz"], 14060000.0);
    assert_eq!(measured["samples_received"], 96000);
    let output = manta()
        .current_dir(dir.path())
        .env("MANTA_CONFIG", &cfg)
        .env("MANTA_INPUT_CENTER_FREQ_HZ", "14060000")
        .args(["check", "--dial-freq-hz", "21060000", "--json"])
        .output()
        .unwrap();
    assert_eq!(report(&output, 0)["center_freq_hz"], 21060000.0);
}

#[test]
fn config_flag_precedes_manta_config_and_no_implicit_cwd_config_is_read() {
    let dir = tempfile::tempdir().unwrap();
    wav(dir.path(), "capture.wav", 96000, 2, 96000, 0.125);
    config(dir.path(), "manta.toml", "[detector]\nunknown_key=1\n");
    let cfg = config(
        dir.path(),
        "valid.toml",
        "[input]\ntype='file'\npath='capture.wav'\niq=true\n",
    );
    let output = manta()
        .current_dir(dir.path())
        .env("MANTA_CONFIG", dir.path().join("absent.toml"))
        .args(["check", "--config"])
        .arg(&cfg)
        .arg("--json")
        .output()
        .unwrap();
    report(&output, 0);
    report(
        &check(dir.path(), &["capture.wav", "--source-iq", "--json"]),
        0,
    );
}

#[test]
fn cli_file_replaces_typed_input_and_its_shared_settings() {
    let dir = tempfile::tempdir().unwrap();
    wav(dir.path(), "capture.wav", 96000, 2, 96000, 0.125);
    let cfg = config(dir.path(), "probe.toml", "[input]\ntype='kiwi'\nhost='127.0.0.1'\nfreq_hz=7030000.0\npassword='SECRET-MAN125'\ncenter_freq_hz=14060000.0\ncapture_rate_hz=48000.0\nfreq_correction_ppm=20.0\n");
    for selection in [vec!["capture.wav"], vec!["--source", "capture.wav"]] {
        let output = manta()
            .current_dir(dir.path())
            .args(["check", "--config"])
            .arg(&cfg)
            .args(selection)
            .args(["--source-iq", "--json"])
            .output()
            .unwrap();
        let measured = report(&output, 0);
        assert_eq!(measured["sample_rate_hz"], 96000.0);
        assert_eq!(measured["center_freq_hz"], 0.0);
        assert_eq!(measured["passband_offsets_hz"], json!([-48000.0, 48000.0]));
        assert!(String::from_utf8_lossy(&output.stderr).contains("ignoring [input]"));
        for stream in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(stream).contains("SECRET-MAN125"));
        }
    }
}

#[test]
fn check_binds_no_server_and_connects_to_no_uplink_or_unused_assets() {
    let dir = tempfile::tempdir().unwrap();
    wav(dir.path(), "capture.wav", 96000, 2, 96000, 0.125);
    let held: Vec<_> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports: Vec<_> = held
        .iter()
        .map(|listener| listener.local_addr().unwrap().port())
        .collect();
    let collector = TcpListener::bind("127.0.0.1:0").unwrap();
    collector.set_nonblocking(true).unwrap();
    let collector_port = collector.local_addr().unwrap().port();
    let cfg = config(dir.path(), "probe.toml", &format!(
        "[server]\nstation_callsign='W1AW'\nbind_addr='127.0.0.1'\nmetrics_bind_addr='127.0.0.1'\ntelnet_port={}\njson_port={}\nmetrics_port={}\n\
         [[rbn_uplink]]\nenabled=true\ntarget_host='127.0.0.1'\ntarget_port={collector_port}\nlogin_callsign='W1AW'\ndry_run=false\n\
         [input]\ntype='file'\npath='capture.wav'\niq=true\n\
         [spot]\ncty_path='absent.dat'\nscp_path='absent.scp'\nblocklist_path='absent-blocklist.txt'\nnotch_path='absent-notches.txt'\n", ports[0], ports[1], ports[2]));
    let output = manta()
        .current_dir(dir.path())
        .args(["check", "--config"])
        .arg(cfg)
        .arg("--json")
        .output()
        .unwrap();
    report(&output, 0);
    match collector.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("check must not connect to an uplink: {other:?}"),
    }
}

#[test]
fn malformed_config_is_rejected_before_source_io() {
    let dir = tempfile::tempdir().unwrap();
    for invalid in [
        "[detector]\nunknown_key=1\n",
        "[detector]\non_snr_db=200.0\n",
    ] {
        let cfg = config(
            dir.path(),
            "invalid.toml",
            &format!("[input]\ntype='file'\npath='absent.wav'\niq=true\n{invalid}"),
        );
        let output = manta()
            .args(["check", "--config"])
            .arg(cfg)
            .arg("--json")
            .output()
            .unwrap();
        status(&output, 1);
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("detector"), "{error}");
        assert!(!error.contains("cannot open source"), "{error}");
    }
}

#[test]
fn check_help_exposes_only_source_and_measurement_options() {
    let output = manta().args(["check", "--help"]).output().unwrap();
    status(&output, 0);
    let text = String::from_utf8(output.stdout).unwrap();
    for flag in [
        "[SOURCE]",
        "--device",
        "--source",
        "--source-iq",
        "--config",
        "--capture-rate-hz",
        "--dial-freq-hz",
        "--duration",
        "--json",
        "--kiwi-host",
        "--kiwi-password",
    ] {
        assert!(text.contains(flag), "missing {flag}: {text}");
    }
    for flag in [
        "--engine",
        "--reconnect",
        "--replay-epoch",
        "--allowlist",
        "--cty",
        "--on-snr-db",
    ] {
        assert!(!text.contains(flag), "unexpected {flag}: {text}");
    }
    assert!(
        text.contains("blocking") || text.contains("Native open/read calls can exceed"),
        "must explain native call timeout limitation: {text}"
    );
    assert_eq!(text.contains("--soapy-driver"), cfg!(feature = "soapy"));
    assert_eq!(text.contains("--hpsdr-host"), cfg!(feature = "hpsdr"));
}

#[test]
fn disabled_source_flags_are_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    for (enabled, flag) in [
        (cfg!(feature = "soapy"), "--soapy-driver"),
        (cfg!(feature = "hpsdr"), "--hpsdr-host"),
    ] {
        if !enabled {
            let output = check(dir.path(), &[flag, "unused"]);
            status(&output, 2);
            assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
        }
    }
}

#[test]
fn devices_json_smoke_is_inventory_independent_and_does_not_load_config() {
    let output = manta()
        .env("MANTA_CONFIG", "does-not-exist.toml")
        .args(["devices", "--json"])
        .output()
        .unwrap();
    let inventory: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut any_error = false;
    for backend in ["audio", "soapy"] {
        let section = &inventory[backend];
        let state = section["status"].as_str().unwrap();
        assert!(["ok", "error", "disabled"].contains(&state), "{inventory}");
        if backend == "audio" {
            assert_ne!(state, "disabled");
        } else {
            assert_eq!(state == "disabled", !cfg!(feature = "soapy"));
        }
        any_error |= state == "error";
        assert_eq!(section["error"].is_string(), state == "error");
        let devices = section["devices"].as_array().unwrap();
        if state != "ok" {
            assert!(devices.is_empty());
        }
        let key = if backend == "audio" { "name" } else { "args" };
        let names: Vec<_> = devices
            .iter()
            .map(|device| {
                assert!(device.get("max_sample_rate").is_none());
                assert!(device.get("index").is_none());
                if backend == "audio" {
                    assert!(device["input_channels"].as_u64().unwrap() > 0);
                }
                device[key].as_str().unwrap()
            })
            .collect();
        assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
    }
    assert_eq!(inventory["hpsdr"]["status"], "not_supported");
    assert_eq!(inventory["kiwi"]["status"], "explicit_host");
    status(&output, i32::from(any_error));
    let output = manta().arg("devices").output().unwrap();
    assert!(matches!(output.status.code(), Some(0 | 1)));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Audio inputs:"));
    assert!(text.contains("HPSDR: automatic discovery is not supported"));
    assert!(text.contains("KiwiSDR: use --kiwi-host HOST."));
}

#[test]
fn invalid_receiver_settings_do_not_echo_passwords() {
    let dir = tempfile::tempdir().unwrap();
    let output = check(
        dir.path(),
        &[
            "--kiwi-host",
            "127.0.0.1",
            "--kiwi-freq-hz",
            "NaN",
            "--kiwi-password",
            "SECRET-RECEIVER-PASSWORD",
            "--json",
        ],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    for stream in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains("SECRET-RECEIVER-PASSWORD"));
    }
}

#[test]
fn deprecated_receiver_frequency_alias_is_still_parsed() {
    let dir = tempfile::tempdir().unwrap();
    let output = check(
        dir.path(),
        &[
            "missing.wav",
            "--kiwi-host",
            "127.0.0.1",
            "--kiwi-freq",
            "7030000",
        ],
    );
    status(&output, 2);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("cannot be used with"),
        "alias must reach selector conflict: {error}"
    );
    assert!(!error.contains("unexpected argument"));
}
